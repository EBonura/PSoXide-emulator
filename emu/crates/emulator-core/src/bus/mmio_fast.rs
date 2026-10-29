//! Straight-line CPU accesses to the CD-ROM registers for the native tier's
//! batched device path.
//!
//! Guests spend a great deal of time polling the CD-ROM index/status and
//! interrupt-flag registers with byte loads and stores. The generic CPU
//! access ([`Bus::cpu_read_stalls`], [`Bus::read8`], [`Bus::cpu_write8`] and
//! [`Bus::cpu_write_narrow_mmio`]) finds the register by trying every device
//! range in turn. These two methods do exactly what that path does for an
//! address already known to be in the CD-ROM window, in the same order, and
//! nothing else. The caller guarantees what the generic path would have
//! skipped or branched on: no limit oracle is configured, so the limit
//! checks and the freeze thaw are no-ops.

use super::{
    Bus, Dma, IrqSource, Sio0, Timers, CODE_FILL_MMIO_WAIT_CAP, IRQ_MASK_ADDR, IRQ_STAT_ADDR,
};

/// The registers [`Bus::cpu_io_load32`] reads.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum IoWord {
    /// `I_STAT`.
    IrqStat,
    /// `I_MASK`.
    IrqMask,
    /// A root counter's counter, mode or target register.
    Timers,
    /// A DMA channel register or the DMA control registers.
    Dma,
    /// `GPUREAD` or `GPUSTAT`.
    Gpu,
    /// An SIO0 (controller port) register.
    Sio0,
}

/// Wait states of an internal I/O register read beyond the issue cycle
/// (`MemoryControl::read_stalls` answers four for the whole window except
/// the CD-ROM and SPU registers).
const INTERNAL_REGISTER_READ_STALLS: u32 = 4;

impl Bus {
    /// A CPU byte load from the CD-ROM register at `phys`
    /// (`CdRom::contains(phys)`): charge the read's wait states, then read.
    /// Same effects, in the same order, as the generic load for this
    /// address: the delay register's stall plus the wait for a streaming
    /// code fill (a CD-ROM read is not on-die, so it neither starts nor ends
    /// a load shadow), the clock moving by that, the GPU quiet span ending,
    /// the register read, and the data-bus latch taking the byte on its lane.
    #[inline(always)]
    pub(crate) fn cpu_cdrom_load8(&mut self, phys: u32) -> u8 {
        let fill_wait = self.code_fill_busy_until.saturating_sub(self.cycles);
        let stalls = self.memory_control.cdrom_byte_read_stalls()
            + fill_wait.min(CODE_FILL_MMIO_WAIT_CAP) as u32;
        self.advance_cycles(stalls);
        self.gpu_quiet_until = 0;
        let value = self.cdrom.read8(phys);
        let shift = (phys & 3) * 8;
        self.data_bus_latch =
            (self.data_bus_latch & !(0xff << shift)) | (u32::from(value) << shift);
        value
    }

    /// A CPU `SB` of `source` (the whole register) to the CD-ROM register
    /// at `phys`: the data bus takes the full word, a zero-cycle store
    /// charge (the delay register only slows SPU stores), the GPU quiet
    /// span ends, and the register sees the low byte at the current cycle,
    /// raising the CD-ROM interrupt when it asks to.
    #[inline(always)]
    pub(crate) fn cpu_cdrom_store8(&mut self, phys: u32, source: u32) {
        self.data_bus_latch = source;
        self.advance_cycles(0);
        self.gpu_quiet_until = 0;
        if self.cdrom.write8_at(phys, source as u8, self.cycles) {
            self.irq.raise(IrqSource::Cdrom);
        }
    }

    /// `I_STAT` as a word load reads it: the SPU first catches up to the
    /// clock (its interrupt may be due).
    #[inline(always)]
    pub(super) fn read32_irq_stat(&mut self) -> u32 {
        self.run_spu_to_current_cycle();
        self.irq.stat()
    }

    /// `I_MASK` as a word load reads it: the register in the low half, the
    /// data bus's previous value in the high half.
    #[inline(always)]
    pub(super) fn read32_irq_mask(&self) -> u32 {
        (self.data_bus_latch & 0xffff_0000) | self.irq.mask()
    }

    /// A root-counter register as a word load reads it: after the write
    /// buffer drains, with the bank caught up to the clock; the high half
    /// is the data bus's previous value.
    #[inline(always)]
    pub(super) fn read32_timers(&mut self, phys: u32) -> u32 {
        if self.ram_write_buffer_ready_cycle > self.cycles {
            self.advance_cycles((self.ram_write_buffer_ready_cycle - self.cycles) as u32);
        }
        self.ram_write_buffer_ready_cycle = self.cycles;
        self.service_timers();
        (self.data_bus_latch & 0xffff_0000) | self.timers.read32(phys)
    }

    /// A DMA register as a word load reads it: running block transfers
    /// first update their address and count.
    #[inline(always)]
    pub(super) fn read32_dma(&mut self, phys: u32) -> u32 {
        self.settle_dma_block_runs();
        self.dma.read32(phys)
    }

    /// An SIO0 register as a word load reads it: the port catches up to the
    /// clock first.
    #[inline(always)]
    pub(super) fn read32_sio0(&mut self, phys: u32) -> u32 {
        self.service_sio0();
        self.sio0.read32(phys).unwrap_or(0)
    }

    /// `I_STAT` as a word store writes it (acknowledging the bits written
    /// as zero): the SPU catches up first.
    #[inline(always)]
    pub(super) fn write32_irq_stat(&mut self, value: u32) {
        self.run_spu_to_current_cycle();
        self.irq.write_stat_at(value, self.cycles);
    }

    /// `I_MASK` as a word store writes it.
    #[inline(always)]
    pub(super) fn write32_irq_mask(&mut self, value: u32) {
        self.irq.write_mask_at(value, self.cycles);
    }

    /// A CPU word load from the interrupt, root-counter, DMA, GPU or SIO0
    /// registers (`phys` classified by [`Bus::io_word_register`]): charge the
    /// load's wait states, then read. Same effects, in the same order, as
    /// the generic load for these addresses: the internal registers' four
    /// wait states plus the wait for a streaming code fill, the load
    /// shadow flag (an on-die register's wait overlaps the instructions
    /// behind it), the clock (a root counter holds only the counter being
    /// read for it), the GPU quiet-span reset, the register read through the
    /// same arm of the bus's word-read dispatch, and the data-bus latch.
    #[inline(always)]
    pub(crate) fn cpu_io_load32(&mut self, register: IoWord, phys: u32) -> u32 {
        let fill_wait = self.code_fill_busy_until.saturating_sub(self.cycles);
        let stalls = INTERNAL_REGISTER_READ_STALLS + fill_wait.min(CODE_FILL_MMIO_WAIT_CAP) as u32;
        self.ram_load_from_cached_code =
            !self.code_fetch_on_ram_bus && self.cycles >= self.code_fill_busy_until;
        if register == IoWord::Timers {
            self.add_root_counter_read_stalls(phys, stalls);
        } else {
            self.advance_cycles(stalls);
        }
        self.gpu_quiet_until = 0;
        let value = match register {
            IoWord::IrqStat => self.read32_irq_stat(),
            IoWord::IrqMask => self.read32_irq_mask(),
            IoWord::Timers => self.read32_timers(phys),
            IoWord::Dma => self.read32_dma(phys),
            IoWord::Gpu => {
                let value = self.gpu.read32_at(phys, self.cycles);
                self.service_gpu_irq();
                value.expect("GP0 and GP1 are the GPU's readable registers")
            }
            IoWord::Sio0 => {
                // The bus's dispatch offers every address to the GPU before
                // the ports behind it; the GPU's only effect on one of
                // them is to apply the status writes that have matured.
                let _ = self.gpu.read32_at(phys, self.cycles);
                self.read32_sio0(phys)
            }
        };
        self.data_bus_latch = value;
        value
    }

    /// A CPU `SW` of `value` to `I_STAT` or `I_MASK` (`mask` says which):
    /// the data bus takes the word, the zero-cycle store charge, the GPU
    /// quiet span ends, the register is written.
    #[inline(always)]
    pub(crate) fn cpu_irq_store32(&mut self, mask: bool, value: u32) {
        self.data_bus_latch = value;
        self.advance_cycles(0);
        self.gpu_quiet_until = 0;
        if mask {
            self.write32_irq_mask(value);
        } else {
            self.write32_irq_stat(value);
        }
    }

    /// Which word-load register class `phys` is, if it is one of those
    /// [`Bus::cpu_io_load32`] serves.
    #[inline(always)]
    pub(crate) fn io_word_register(phys: u32) -> Option<IoWord> {
        match phys {
            IRQ_STAT_ADDR => Some(IoWord::IrqStat),
            IRQ_MASK_ADDR => Some(IoWord::IrqMask),
            crate::gpu::GP0_ADDR | crate::gpu::GP1_ADDR => Some(IoWord::Gpu),
            _ if Timers::contains(phys) => Some(IoWord::Timers),
            _ if Dma::contains(phys) => Some(IoWord::Dma),
            _ if Sio0::contains(phys) => Some(IoWord::Sio0),
            _ => None,
        }
    }
}
