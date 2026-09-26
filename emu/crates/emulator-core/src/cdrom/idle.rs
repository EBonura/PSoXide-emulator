//! How long the drive stays idle, for exact idle skipping.

use super::*;

impl CdRom {
    /// The latest cycle at which [`CdRom::tick_with_irq_pending`] still does
    /// nothing: no lid transition (fires past its deadline), no CD-DA seek
    /// completion (fires at its deadline), no queued event (fires past its
    /// deadline). `u64::MAX` when nothing is scheduled.
    pub(crate) fn idle_until(&self) -> u64 {
        let lid = self.lid_deadline.unwrap_or(u64::MAX);
        let seek = self
            .cdda_seek_done_at
            .map_or(u64::MAX, |at| at.saturating_sub(1));
        let queued = self
            .pending
            .iter()
            .map(|e| e.deadline)
            .min()
            .unwrap_or(u64::MAX);
        lid.min(seek).min(queued)
    }
}
