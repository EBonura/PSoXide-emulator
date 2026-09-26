//! What the CPU's block executor (`cpu/block.rs`) needs from the bus to
//! account a run of register-only instructions at once.
//!
//! Between two bus accesses, an instruction's bus-side work is its issue
//! tick: move the clock, decay the GPU's busy credit, and drain any
//! scheduler event or SPU sample that fell due, plus the interrupt-line
//! sample and SPU catch-up at the start of the step. While the clock stays
//! below [`Bus::quiet_limit`], none of that does anything but add, so a run
//! can be charged in one go ([`Bus::advance_quiet`]) with exactly the same
//! result.

use super::*;

/// Write counts per 4 KiB page of main RAM (see `Bus::ram_pages`).
pub(crate) struct RamPages {
    /// Distinguishes this RAM from any other (a restored or new bus):
    /// stamps taken on another RAM never match.
    pub(crate) id: u64,
    counts: Vec<u32>,
}

impl RamPages {
    /// 4 KiB pages in main RAM.
    const PAGES: usize = memory::ram::SIZE >> 12;

    pub(crate) fn new() -> Self {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            counts: vec![0; Self::PAGES],
        }
    }

    /// Note a write at RAM byte offset `offset` (within one mirror).
    #[inline(always)]
    pub(crate) fn touch(&mut self, offset: usize) {
        let page = (offset >> 12) & (Self::PAGES - 1);
        self.counts[page] = self.counts[page].wrapping_add(1);
    }

    /// Note a write of `len` bytes from `offset`.
    pub(crate) fn touch_range(&mut self, offset: usize, len: usize) {
        if len == 0 {
            return;
        }
        let first = offset >> 12;
        let last = (offset + len - 1) >> 12;
        for page in first..=last {
            let page = page & (Self::PAGES - 1);
            self.counts[page] = self.counts[page].wrapping_add(1);
        }
    }

    /// Note a write anywhere.
    pub(crate) fn touch_all(&mut self) {
        for count in &mut self.counts {
            *count = count.wrapping_add(1);
        }
    }

    /// Write count of the page holding RAM byte offset `offset`.
    #[inline(always)]
    pub(crate) fn count(&self, offset: usize) -> u32 {
        self.counts[(offset >> 12) & (Self::PAGES - 1)]
    }
}

impl Bus {
    /// First bus cycle at which a tick could do more than move counters, or
    /// the current cycle when ticks are not plain counter updates at all.
    ///
    /// Below it: no scheduler event or SPU sample falls due, and advancing
    /// the clock ([`Bus::advance_quiet`]) is additive, i.e. two advances of
    /// `a` and `b` cycles leave the same state as one of `a + b`. That holds
    /// while the GPU's DMA-FIFO model and an in-flight linked-list walk can
    /// only decay credit (their own "quiet" spans), and never with a limit
    /// oracle freezing the clock or GPU DMA polling for its request, which
    /// act on every advance.
    #[doc(hidden)]
    #[inline(always)]
    pub fn quiet_limit(&self) -> u64 {
        if self.limits.frozen() || self.gpu_dma_waiting_for_request {
            return self.cycles;
        }
        let mut limit = self.scheduler.lowest_target().min(self.spu_sample_deadline);
        if self.experimental_gpu_list.is_some() {
            // Advancing the clock up to `gpu_quiet_until` (inclusive) is the
            // list walk's quiet batch; recompute it when not known.
            let quiet_until = if self.gpu_quiet_until > self.cycles {
                self.gpu_quiet_until
            } else {
                self.cycles
                    .saturating_add(u64::from(self.gpu_list_quiet_cycles(u32::MAX)))
            };
            limit = limit.min(quiet_until.saturating_add(1));
        } else if !self.gpu.decay_is_plain() {
            limit = limit.min(self.cycles.saturating_add(self.gpu.fifo_quiet_cycles()));
        }
        limit
    }

    /// Move the clock `n` cycles with no drain, as ticks and memory stalls
    /// do before the next step's drain. Batches issue cycles of steps that
    /// stay below [`Bus::quiet_limit`].
    #[doc(hidden)]
    #[inline(always)]
    pub fn advance_quiet(&mut self, n: u64) {
        debug_assert!(n == 0 || self.cycles + n < self.quiet_limit());
        self.advance_cycles(n as u32);
    }

    /// The interrupt-line sample of `n` consecutive quiet steps (see
    /// [`Bus::external_interrupt_pending`]): SPU catch-up has nothing to do
    /// below `quiet_limit`, and the line cannot change.
    #[inline(always)]
    pub(crate) fn external_interrupt_pending_quiet(&mut self, n: u64) -> bool {
        self.irq.pending_ticks(n)
    }

    /// The branch-boundary drain ([`Bus::drain_scheduler_events_post_op`])
    /// when it has nothing to do but record the boundary: no limit oracle
    /// waiting to start, no scheduler event or SPU sample due, no timer
    /// able to cross anything, and no CD-ROM deadline passed. Returns
    /// `false`, changing nothing, when the full drain is needed.
    #[inline(always)]
    pub(crate) fn post_op_quiet(&mut self) -> bool {
        let now = self.cycles;
        if self.limits.pending()
            || now >= self.scheduler.lowest_target().min(self.spu_sample_deadline)
            || now >= self.timers.quiet_until()
            || now > self.cdrom.idle_until()
        {
            return false;
        }
        self.last_post_op_cycle = now;
        true
    }

    /// Whether the word at `virt` in main RAM (or wherever
    /// [`Bus::peek_instruction`] reads) is a GTE command: the GTE interrupt
    /// hazard's look at the next instruction, with main RAM read directly.
    #[inline(always)]
    pub(crate) fn peek_is_gte_command(&self, virt: u32) -> bool {
        let phys = to_physical(virt);
        let word = if phys < memory::ram::MIRROR_END {
            let offset = (phys as usize) & (memory::ram::SIZE - 4);
            read_u32_le(&self.ram[offset..])
        } else {
            match self.peek_instruction(virt) {
                Some(word) => word,
                None => return false,
            }
        };
        word & 0xFE00_0000 == 0x4A00_0000
    }

    /// A CPU load from main RAM at `virt` with no limit oracle configured:
    /// exactly what `Cpu::charge_read` and the bus read do there (stalls,
    /// then the value and the data-bus latch). `width` is 1, 2 or 4 bytes;
    /// the address is aligned to it.
    #[inline(always)]
    pub(crate) fn cpu_ram_load(&mut self, virt: u32, width: u32) -> u32 {
        let stalls = self.ram_read_stalls(virt);
        self.add_cycles(stalls);
        let phys = to_physical(virt);
        let offset = (phys as usize) % memory::ram::SIZE;
        match width {
            4 => {
                let value = read_u32_le(&self.ram[offset..]);
                self.data_bus_latch = value;
                value
            }
            2 => {
                let value = u16::from_le_bytes([self.ram[offset], self.ram[offset + 1]]);
                let shift = (phys & 2) * 8;
                self.data_bus_latch =
                    (self.data_bus_latch & !(0xFFFF << shift)) | (u32::from(value) << shift);
                u32::from(value)
            }
            _ => {
                let value = self.ram[offset];
                let shift = (phys & 3) * 8;
                self.data_bus_latch =
                    (self.data_bus_latch & !(0xFF << shift)) | (u32::from(value) << shift);
                u32::from(value)
            }
        }
    }

    /// A CPU `SW` to main RAM at word-aligned `virt` with no limit oracle
    /// configured: exactly [`Bus::cpu_write32`] there (data-bus latch,
    /// write-buffer and refresh stalls, then the store).
    #[inline(always)]
    pub(crate) fn cpu_ram_store32(&mut self, virt: u32, value: u32) {
        self.data_bus_latch = value;
        let stall = self.ram_write_stalls(virt);
        self.add_cycles(stall);
        let offset = (to_physical(virt) as usize) % memory::ram::SIZE;
        self.ram[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        self.ram_pages.touch(offset);
    }

    /// A stamp of the RAM pages holding bytes `offset..offset + len`
    /// (within one mirror, at most two pages): their write counts and this
    /// RAM's id. Equal stamps mean nothing wrote those pages in between.
    #[inline(always)]
    pub(crate) fn ram_stamp(&self, offset: usize, len: usize) -> RamStamp {
        let last = (offset + len.max(1) - 1) % memory::ram::SIZE;
        RamStamp {
            id: self.ram_pages.id,
            first: self.ram_pages.count(offset),
            last: self.ram_pages.count(last),
        }
    }

    /// The word of main RAM at byte offset `offset` (any mirror).
    #[inline(always)]
    pub(crate) fn ram_word(&self, offset: usize) -> u32 {
        read_u32_le(&self.ram[(offset % memory::ram::SIZE) & !3..])
    }
}

/// See [`Bus::ram_stamp`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RamStamp {
    id: u64,
    first: u32,
    last: u32,
}
