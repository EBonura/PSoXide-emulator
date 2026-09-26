//! Exact idle skipping: charge many identical wait retries at once.

use super::*;

impl Bus {
    /// Charge up to `max` repetitions of an idle step that each advance the
    /// clock by `step` cycles and then run the branch-boundary drain
    /// ([`Bus::drain_scheduler_events_post_op`]), when doing them one by one
    /// would change nothing but the clock: no scheduler event or SPU sample
    /// falls due, the CD-ROM stays idle, the root counters stay inside their
    /// lazy quiet window ([`Timers::quiet_until`], where the drain skips
    /// them), and the clock is on its plain path. Returns how many were
    /// charged (possibly 0); the state afterwards is the state after that
    /// many single steps.
    pub(crate) fn skip_idle_steps(&mut self, step: u64, max: u64) -> u64 {
        let n = self.idle_window(step, max);
        if n != 0 {
            self.advance_cycles((n * step) as u32);
            // What the last step's drain records.
            self.last_post_op_cycle = self.cycles;
        }
        n
    }

    /// One of the steps [`Bus::idle_window`] allowed: the clock advance and
    /// what that step's drain records.
    pub(crate) fn idle_step(&mut self, step: u64) {
        self.advance_cycles(step as u32);
        self.last_post_op_cycle = self.cycles;
    }

    /// How many idle steps of `step` cycles (at most `max`) change nothing
    /// but the clock; see [`Bus::skip_idle_steps`]. Pure.
    pub(crate) fn idle_window(&self, step: u64, max: u64) -> u64 {
        // The clock's plain path: one advance of n cycles equals n advances
        // of one (no frozen limit range, no experimental GPU list walk, no
        // GPU DMA held back, plain GPU credit decay). No limit oracle may
        // be pending either, since the drain would activate it.
        let plain = !(self.limits.frozen()
            || self.experimental_gpu_list.is_some()
            || self.gpu_dma_waiting_for_request)
            && self.gpu.decay_is_plain();
        let limits_idle =
            !self.limits.on(u32::MAX) && !self.limits.pending() && !self.limits.tracks_pc();
        if step == 0 || !plain || !limits_idle {
            return 0;
        }
        // Every step k ends at cycles + k * step; each must stay below the
        // next scheduler target and SPU sample (where `tick` and the drain
        // start dispatching) and at or below the CD-ROM's idle bound.
        let bound = self
            .scheduler
            .lowest_target()
            .min(self.spu_sample_deadline)
            .saturating_sub(1)
            .min(self.cdrom.idle_until())
            // Each step's drain skips the root counters only below this.
            .min(self.timers.quiet_until().saturating_sub(1));
        (bound.saturating_sub(self.cycles) / step)
            .min(max)
            .min(u64::from(u32::MAX) / step)
    }

    /// The interrupt-line sample at the start of `n` idle steps: the SPU
    /// catch-up is a no-op inside the window, so only the diagnostic count
    /// moves. Returns whether an interrupt is pending.
    pub(crate) fn idle_interrupt_samples(&mut self, n: u64) -> bool {
        self.irq.pending_ticks(n)
    }
}
