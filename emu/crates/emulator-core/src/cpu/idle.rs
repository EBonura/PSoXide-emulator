//! Exact skipping of HLE kernel waits.

use super::*;

impl Cpu {
    /// Retire up to `max` retries of an HLE kernel call that is waiting
    /// (for example `_bu_init` waiting for the memory card), all at once,
    /// when that is exactly what [`Cpu::step`] would do one retry at a time:
    /// the call would retry with no side effect, no interrupt is taken,
    /// and nothing outside the CPU happens until the last of them (see
    /// [`Bus::skip_idle_steps`]). Returns the number of instructions
    /// retired, 0 when not idle. Callers that count `step` calls should
    /// count retired instructions ([`Cpu::tick`]) instead.
    pub fn skip_hle_wait(&mut self, bus: &mut Bus, max: u64) -> u64 {
        if max == 0
            || !bus.hle_bios_enabled
            || self.pending_pc.is_some()
            || self.branch_delay_next
            || self.pending_load.is_some()
            || self.cpu_cycle_profile_enabled
            || self.instruction_class_profile_enabled
            || self.instruction_cache_event_profile_enabled
            || (self.hle_exception_active
                && memory::to_physical(self.pc)
                    == memory::to_physical(crate::hle_bios::EXCEPTION_RETURN_STUB))
            || !crate::hle_bios::idle_wait(self.pc, bus, &self.gprs)
            || self.should_take_interrupt(bus)
        {
            return 0;
        }
        // Each retry charges the dispatch's two cycles, then the
        // branch-boundary drain and the interrupt check.
        let n = bus.skip_idle_steps(crate::hle_bios::RETRY_CYCLES, max);
        if n != 0 {
            if bus.idle_interrupt_samples(n) {
                self.irq_line_high_steps = self.irq_line_high_steps.saturating_add(n);
            }
            self.tick += n;
        }
        n
    }
}
