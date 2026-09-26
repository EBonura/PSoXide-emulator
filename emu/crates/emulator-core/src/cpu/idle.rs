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
        if !self.hle_wait_skippable(bus, max) {
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

    /// [`Cpu::skip_hle_wait`] for [`Cpu::run`]: the retries are charged one
    /// at a time and `stop_after` is asked after each, exactly as `run`
    /// asks after every instruction, so a caller's stop condition (a cycle
    /// deadline, say) lands on the same instruction as without skipping.
    /// Returns the number retired and whether `stop_after` said stop.
    #[inline]
    pub(super) fn skip_hle_wait_stepwise(
        &mut self,
        bus: &mut Bus,
        max: u64,
        stop_after: &mut impl FnMut(&Bus) -> bool,
    ) -> (u64, bool) {
        // Every HLE hook lives below 64 KiB: the common case costs one
        // compare.
        if memory::to_physical(self.pc) >= 0x1_0000 || !self.hle_wait_skippable(bus, max) {
            return (0, false);
        }
        let n = bus.idle_window(crate::hle_bios::RETRY_CYCLES, max);
        let mut done = 0;
        while done < n {
            if bus.idle_interrupt_samples(1) {
                self.irq_line_high_steps = self.irq_line_high_steps.saturating_add(1);
            }
            bus.idle_step(crate::hle_bios::RETRY_CYCLES);
            self.tick += 1;
            done += 1;
            if stop_after(bus) {
                return (done, true);
            }
        }
        (done, false)
    }

    fn hle_wait_skippable(&self, bus: &mut Bus, max: u64) -> bool {
        !(max == 0
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
            || self.should_take_interrupt(bus))
    }
}
