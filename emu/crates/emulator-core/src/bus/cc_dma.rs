//! The cache-control / DMA tripwire.
//!
//! On silicon, changing the CPU bus configuration while a DMA channel owns
//! the bus makes the next instruction fetch return the DMA's word. The
//! hazardous changes are a `cache_control` (`0xFFFE0130`) write that flips
//! bits 13..16 (bus grant, read priority, no-wait-state) or bit 3 or 7, and
//! a Status register write that sets `IsC` (isolate cache). Quake PSX froze
//! on a console that way: `FlushCache` wrote `cache_control = 0x804` while
//! DMA2 was walking the previous frame's list, and the CPU then executed a
//! GPU list header. The emulator ignores those bits and has no word-level
//! CPU/DMA arbitration, so it cannot reproduce the fetch; this check makes
//! the precondition itself a failure.
//!
//! `PSOXIDE_CHECK_CC_DMA=1` (or `fail`) panics at the first hit, `warn`
//! prints each hit (the first few in full) and counts them, anything else
//! is off. `psoxide-gate` turns it on for every run unless the caller set
//! the variable. `inject` is `warn` plus a model of the fault: the CPU's next
//! instruction fetch that reaches the bus (a cache hit never does) returns the
//! word at the busy channel's MADR instead of the real one, so the guest
//! executes a list header as code, as it did on the console.

use super::*;

/// `cache_control` bits whose change reconfigures the bus: 13..16 (the
/// bus-grant, read-priority and no-wait-state group), bit 7 and bit 3.
pub const CC_BUS_BITS: u32 = (0xF << 13) | (1 << 7) | (1 << 3);

/// COP0 Status bit 16, isolate cache.
const SR_ISC: u32 = 1 << 16;

/// What the tripwire does on a hit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CcDmaMode {
    /// No check.
    #[default]
    Off,
    /// Panic with the details.
    Fail,
    /// Print and count.
    Warn,
    /// Print and count like `Warn`, and hand the CPU the DMA's current word
    /// to execute as its next bus instruction fetch (see
    /// [`Bus::cc_dma_check`]): a headless model of the silicon fault.
    Inject,
}

impl CcDmaMode {
    /// Parse the value of `PSOXIDE_CHECK_CC_DMA`.
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("1" | "fail") => Self::Fail,
            Some("warn") => Self::Warn,
            Some("inject") => Self::Inject,
            _ => Self::Off,
        }
    }
}

pub(super) fn cc_dma_mode_from_env() -> CcDmaMode {
    CcDmaMode::parse(std::env::var("PSOXIDE_CHECK_CC_DMA").ok().as_deref())
}

/// A CPU action that reconfigures the bus.
#[derive(Clone, Copy, Debug)]
pub enum CcDmaEvent {
    /// A `cache_control` write from `old` to `new`.
    CacheControl {
        /// Register value before the write.
        old: u32,
        /// Value written.
        new: u32,
    },
    /// A Status register write from `old` to `new`.
    Status {
        /// Register value before the write.
        old: u32,
        /// Value written.
        new: u32,
    },
    /// The HLE kernel's `A(44h) FlushCache`, standing for the real BIOS
    /// routine that writes `cache_control = 0x804` and sets `IsC`. `ra` is
    /// the caller, `cc` the register as the guest left it.
    HleFlushCache {
        /// The caller's return address.
        ra: u32,
        /// `cache_control` as the guest left it.
        cc: u32,
    },
}

impl CcDmaEvent {
    fn hazardous(self) -> bool {
        match self {
            Self::CacheControl { old, new } => (old ^ new) & CC_BUS_BITS != 0,
            Self::Status { old, new } => new & SR_ISC != 0 && old & SR_ISC == 0,
            Self::HleFlushCache { .. } => true,
        }
    }

    fn describe(self) -> String {
        match self {
            Self::CacheControl { old, new } => format!(
                "cache_control {old:#07x} -> {new:#07x} (bus bits changed: {:#07x})",
                (old ^ new) & CC_BUS_BITS
            ),
            Self::Status { old, new } => format!("SR {old:#010x} -> {new:#010x} (IsC set)"),
            Self::HleFlushCache { ra, cc } => format!(
                "HLE A(44h) FlushCache from ra={ra:#010x} (the BIOS routine writes cache_control \
                 {cc:#07x} -> 0x00804 and sets SR.IsC)"
            ),
        }
    }
}

/// Printed in full this many times in `warn` mode; later hits get one line.
const WARN_PRINT_LIMIT: u64 = 8;

impl Bus {
    /// Set how the cache-control / DMA tripwire reacts.
    pub fn set_cc_dma_mode(&mut self, mode: CcDmaMode) {
        self.cc_dma_mode = mode;
    }

    /// Hits counted so far (`warn` mode, or `fail` mode before the panic).
    pub fn cc_dma_hits(&self) -> u64 {
        self.cc_dma_hits
    }

    /// Bit `n` set when DMA channel `n`'s CHCR busy/start bit (24) is set.
    pub fn busy_dma_channels(&self) -> u8 {
        (0..7).fold(0u8, |mask, ch| {
            mask | (u8::from(self.dma.channels[ch].channel_control & (1 << 24) != 0) << ch)
        })
    }

    /// Called by the CPU for every `cache_control` write, Status write and
    /// HLE `FlushCache`. `pc` is the executing instruction. Does nothing
    /// unless the mode is on, the event reconfigures the bus and a DMA
    /// channel is busy. In [`CcDmaMode::Inject`] a hit returns the word the
    /// CPU's next bus fetch must see (the lowest busy channel's current RAM
    /// word, DMA2's by preference).
    #[inline]
    #[must_use]
    pub fn cc_dma_check(&mut self, pc: u32, event: CcDmaEvent) -> Option<u32> {
        if self.cc_dma_mode == CcDmaMode::Off || !event.hazardous() {
            return None;
        }
        let busy = self.busy_dma_channels();
        if busy == 0 {
            return None;
        }
        self.cc_dma_hits += 1;
        let mut channels = String::new();
        for ch in 0..7 {
            if busy & (1 << ch) != 0 {
                channels.push_str(&format!(
                    " DMA{ch}[MADR={:#010x} BCR={:#010x} CHCR={:#010x}]",
                    self.dma.channels[ch].base,
                    self.dma.channels[ch].block_control,
                    self.dma.channels[ch].channel_control
                ));
            }
        }
        let message = format!(
            "CC_DMA_HAZARD: pc={pc:#010x} {} while a DMA channel is busy:{channels}. On \
             silicon the next instruction fetch returns the DMA's word (Quake PSX console \
             freeze, 2026-10-22). Idle every DMA channel and mask interrupts before \
             changing cache_control bits 13..16/7/3 or setting SR.IsC.",
            event.describe()
        );
        match self.cc_dma_mode {
            CcDmaMode::Fail => panic!("{message}"),
            CcDmaMode::Warn | CcDmaMode::Inject if self.cc_dma_hits <= WARN_PRINT_LIMIT => {
                eprintln!("[cc-dma] hit {}: {message}", self.cc_dma_hits);
            }
            // Later hits get a one-line form so a log can still be counted.
            CcDmaMode::Warn | CcDmaMode::Inject => {
                eprintln!(
                    "[cc-dma] hit {}: pc={pc:#010x} {} busy={busy:#04x}",
                    self.cc_dma_hits,
                    event.describe()
                );
            }
            CcDmaMode::Off => {}
        }
        if self.cc_dma_mode != CcDmaMode::Inject {
            return None;
        }
        let ch = if busy & (1 << 2) != 0 {
            2
        } else {
            busy.trailing_zeros() as usize
        };
        let word = read_ram_u32(&self.ram[..], self.dma.channels[ch].base & 0x001F_FFFC);
        if self.cc_dma_hits <= WARN_PRINT_LIMIT {
            eprintln!("[cc-dma] injecting DMA{ch} word {word:#010x} as the next bus fetch");
        }
        Some(word)
    }
}

#[cfg(test)]
impl Bus {
    /// Put channel `ch` into the busy state without running a transfer.
    pub(crate) fn fake_busy_dma_channel(&mut self, ch: usize, madr: u32, chcr: u32) {
        self.dma.channels[ch].base = madr;
        self.dma.channels[ch].channel_control = chcr;
    }
}
