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

use super::*;

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
}
