//! Exact idle skipping: charge many identical wait retries at once.

use super::*;

impl Bus {
    /// Charge up to `max` repetitions of an idle step that each advance the
    /// clock by `step` cycles and then run the branch-boundary drain
    /// ([`Bus::drain_scheduler_events_post_op`]), when doing them one by one
    /// would change nothing but the clock and the timer counters: no
    /// scheduler event or SPU sample falls due, the CD-ROM stays idle, no
    /// root counter fires, and the clock is on its plain path. Returns how
    /// many were charged (possibly 0); the state afterwards is the state
    /// after that many single steps.
    pub(crate) fn skip_idle_steps(&mut self, step: u64, max: u64) -> u64 {
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
            .min(self.cdrom.idle_until());
        let mut n = (bound.saturating_sub(self.cycles) / step)
            .min(max)
            .min(u64::from(u32::MAX) / step);
        let next_vblank = self
            .scheduler
            .target(crate::scheduler::EventSlot::VBlank)
            .unwrap_or(u64::MAX);
        while n != 0 {
            // Root counters advance lazily and are split-invariant: serviced
            // once at the end they reach the state of being serviced after
            // every step. They must not fire inside the window, since a fire
            // raises an interrupt at its own step.
            let mut timers = self.timers.clone();
            let fired = timers.advance_to_video(
                self.cycles + n * step,
                self.hsync_cycles,
                self.gpu.dot_clock_divisor(),
                next_vblank,
                self.vblank_period,
            );
            if fired == 0 {
                self.timers = timers;
                self.advance_cycles((n * step) as u32);
                return n;
            }
            n /= 2;
        }
        0
    }

    /// The interrupt-line sample at the start of `n` idle steps: the SPU
    /// catch-up is a no-op inside the window, so only the diagnostic count
    /// moves. Returns whether an interrupt is pending.
    pub(crate) fn idle_interrupt_samples(&mut self, n: u64) -> bool {
        self.irq.pending_ticks(n)
    }
}
