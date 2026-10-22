//! How long the drive stays idle, for exact idle skipping.

use super::*;

impl CdRom {
    /// The latest cycle at which [`CdRom::tick_with_irq_pending`] still does
    /// nothing: no lid transition (fires past its deadline), no CD-DA seek
    /// completion or Stop spin-down end (both fire at their deadline), no
    /// queued event (fires past its deadline). `u64::MAX` when nothing is scheduled.
    #[inline(always)]
    pub(crate) fn idle_until(&self) -> u64 {
        debug_assert!(self.idle_bound == 0 || self.idle_bound == self.compute_idle_until());
        self.idle_bound
    }

    /// Recompute [`CdRom::idle_until`] from the deadlines it is the minimum of.
    /// Called wherever one of them changes.
    #[inline]
    pub(super) fn refresh_idle(&mut self) {
        self.idle_bound = self.compute_idle_until();
    }

    fn compute_idle_until(&self) -> u64 {
        let lid = self.lid_deadline.unwrap_or(u64::MAX);
        let seek = self
            .cdda_seek_done_at
            .map_or(u64::MAX, |at| at.saturating_sub(1));
        let spindown = self
            .spindown_until
            .map_or(u64::MAX, |until| until.saturating_sub(1));
        // The queue is kept sorted by deadline (`insert_pending_event`), so
        // its earliest event is the front one.
        let queued = self.pending.front().map_or(u64::MAX, |e| e.deadline);
        debug_assert!(self.pending.iter().all(|e| e.deadline >= queued));
        lid.min(seek).min(spindown).min(queued)
    }
}
