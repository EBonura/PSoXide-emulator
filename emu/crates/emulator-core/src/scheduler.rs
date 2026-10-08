//! Unified event scheduler: a deadline queue keyed by bus-cycle count.
//!
//! Every subsystem that needs something to happen "N cycles from now" (a
//! CD-ROM response, a DMA completion, an SPU mix tick, the end of a pad
//! byte, VBlank) registers one event here. The CPU loop asks the scheduler
//! which events are due at each instruction boundary and the bus dispatches
//! the handlers with a `match` on the returned [`EventSlot`].
//!
//! Design:
//!
//! - **One pending event per slot.** Each [`EventSlot`] names a
//!   subsystem-event kind and holds at most one deadline; scheduling it
//!   again replaces the old deadline. A fixed array indexed by slot plus a
//!   bitmap of the pending ones is cheaper than a heap at this size.
//! - **O(1) "is anything due".** The earliest pending deadline is cached, so
//!   the common case is a single comparison.
//! - **Deterministic order.** Due events come out earliest deadline first;
//!   two events with the same deadline come out in slot order (the slot's
//!   discriminant is its priority, lower first).
//! - **A deadline is due only strictly in the past.** An event for cycle `T`
//!   is visible to software from the first instruction boundary after the
//!   CPU clock has passed `T`, never on the boundary where it equals `T`.
//!   [`Scheduler::take_slot_due_inclusive`] is the exception for events that
//!   fire on equality (root counters, VBlank).
//! - **No handlers stored.** The scheduler is plain data and serialises
//!   with the rest of the machine state.
//!
//! ## Provenance
//!
//! Written from this project's own requirements: the event set, the slot
//! names, the priority order and the strict-deadline rule are fixed by the
//! gate suite (compat frame hashes, hardware-test records, ps1-tests), not
//! by an outside design. See `LICENSE` and `docs/license-audit.md`.

/// A scheduled-event slot. The discriminant is the slot's position in the
/// pending bitmap and its tie-break priority (lower fires first when two
/// events share a deadline); the numbering is part of the save-state format.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum EventSlot {
    /// Controller / memory-card port (SIO0) byte or ACK complete.
    Sio0 = 0,
    /// Serial port (SIO1).
    Sio1 = 1,
    /// CD-ROM command response (first and second response delays).
    CdResponse = 2,
    /// CD-ROM sector ready from ReadN / ReadS.
    CdSector = 3,
    /// GPU DMA (channel 2) complete.
    GpuDma = 4,
    /// MDEC output DMA (channel 1) complete.
    MdecOutDma = 5,
    /// SPU DMA (channel 4) complete.
    SpuDma = 6,
    /// MDEC input DMA (channel 0) complete.
    MdecInDma = 7,
    /// OTC DMA (channel 6) complete.
    OtcDma = 8,
    /// CD-ROM DMA (channel 3) complete.
    CdDma = 9,
    /// CD-DA play / stop completion.
    CdPlayStop = 10,
    /// CD-ROM decoded-buffer interrupt.
    CdBufferReady = 11,
    /// CD-ROM lid-open and rescan transitions.
    CdLid = 12,
    /// Periodic SPU mixing tick.
    SpuMix = 13,
    /// Vertical blank.
    VBlank = 14,
}

/// Total slots in the `targets` array and pending bitmap: one more than the
/// highest [`EventSlot`] discriminant, so a new slot never indexes out of
/// range.
pub const SLOT_COUNT: usize = 16;

impl EventSlot {
    /// Convert a raw slot index (from bitmap iteration) back to a slot.
    /// Returns `None` outside the defined range.
    pub fn from_index(idx: u32) -> Option<Self> {
        Some(match idx {
            0 => Self::Sio0,
            1 => Self::Sio1,
            2 => Self::CdResponse,
            3 => Self::CdSector,
            4 => Self::GpuDma,
            5 => Self::MdecOutDma,
            6 => Self::SpuDma,
            7 => Self::MdecInDma,
            8 => Self::OtcDma,
            9 => Self::CdDma,
            10 => Self::CdPlayStop,
            11 => Self::CdBufferReady,
            12 => Self::CdLid,
            13 => Self::SpuMix,
            14 => Self::VBlank,
            _ => return None,
        })
    }

    /// Bitmap position: `1 << self.bit()` is the slot's mask.
    #[inline]
    pub fn bit(self) -> u32 {
        self as u32
    }
}

/// The deadline queue. See the module documentation for the ordering rules.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Scheduler {
    /// Absolute bus cycle at which each slot's event is due; meaningful
    /// only while the slot's bit is set in `pending`.
    targets: [u64; SLOT_COUNT],
    /// Bitmap of slots with an event outstanding.
    pending: u32,
    /// Earliest deadline among pending slots (`u64::MAX` when none), cached
    /// so "nothing is due" is one comparison.
    earliest: u64,
    /// Number of [`Scheduler::schedule`] calls. Diagnostic, excluded from
    /// save states.
    #[serde(skip)]
    total_scheduled: u64,
    /// Number of events returned as due. Diagnostic, excluded from save
    /// states.
    #[serde(skip)]
    total_fired: u64,
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl Scheduler {
    /// A scheduler with nothing pending.
    pub const fn new() -> Self {
        Self {
            targets: [0; SLOT_COUNT],
            pending: 0,
            earliest: u64::MAX,
            total_scheduled: 0,
            total_fired: 0,
        }
    }

    /// Arrange for `slot` to fire at `now + delta`, replacing any event
    /// already pending for it (the old deadline is dropped without firing).
    pub fn schedule(&mut self, slot: EventSlot, now: u64, delta: u64) {
        let target = now.saturating_add(delta);
        self.targets[slot as usize] = target;
        self.pending |= 1 << slot.bit();
        self.earliest = self.earliest.min(target);
        self.total_scheduled = self.total_scheduled.saturating_add(1);
    }

    /// Is an event pending for `slot`?
    #[inline]
    pub fn is_pending(&self, slot: EventSlot) -> bool {
        self.pending & (1 << slot.bit()) != 0
    }

    /// Bitmap of all pending slots. Diagnostic.
    #[inline]
    pub fn pending_bitmap(&self) -> u32 {
        self.pending
    }

    /// The deadline for `slot`, or `None` if nothing is pending for it.
    pub fn target(&self, slot: EventSlot) -> Option<u64> {
        self.is_pending(slot).then(|| self.targets[slot as usize])
    }

    /// Drop the event pending for `slot`, if any. Used when a subsystem state
    /// change invalidates an in-flight event (a CD-ROM Pause cancels the
    /// pending sector-ready interrupt).
    pub fn cancel(&mut self, slot: EventSlot) {
        let bit = 1 << slot.bit();
        if self.pending & bit != 0 {
            self.pending &= !bit;
            self.refresh_earliest();
        }
    }

    /// Remove and return `slot`'s deadline when it is `<= now` (inclusive).
    /// For events that fire on the cycle itself rather than strictly after:
    /// root counters and VBlank.
    pub fn take_slot_due_inclusive(&mut self, slot: EventSlot, now: u64) -> Option<u64> {
        if !self.is_pending(slot) {
            return None;
        }
        let target = self.targets[slot as usize];
        if target > now {
            return None;
        }
        self.pending &= !(1 << slot.bit());
        self.refresh_earliest();
        self.total_fired = self.total_fired.saturating_add(1);
        Some(target)
    }

    /// The next event, by earliest deadline then slot priority, among the
    /// pending slots not in `excluded`; only when its deadline is strictly
    /// before `now`.
    fn next_due(&self, now: u64, excluded: u32) -> Option<(EventSlot, u64)> {
        let mut bits = self.pending & !excluded;
        let mut best: Option<(u64, u32)> = None;
        while bits != 0 {
            let idx = bits.trailing_zeros();
            bits &= bits - 1;
            let target = self.targets[idx as usize];
            // Slots are visited in priority order, so a later slot only wins
            // with a strictly earlier deadline.
            if best.map_or(true, |(t, _)| target < t) {
                best = Some((target, idx));
            }
        }
        let (target, idx) = best?;
        if target >= now {
            return None;
        }
        EventSlot::from_index(idx).map(|slot| (slot, target))
    }

    /// Remove and return the earliest due event with its original deadline,
    /// or `None` if nothing is strictly past due. Callers loop to drain every
    /// due event; periodic handlers (VBlank, SPU mix) reschedule from the
    /// returned deadline, not from `now`, so a late drain does not stretch
    /// the period. Events sharing a deadline come out in slot order.
    pub fn take_due(&mut self, now: u64) -> Option<(EventSlot, u64)> {
        self.take_due_excluding(now, 0)
    }

    /// Like [`Scheduler::take_due`] but never returns the slots in
    /// `excluded_mask`. Used by the per-instruction tick for events that are
    /// only serviced at branch boundaries.
    pub fn take_due_excluding(&mut self, now: u64, excluded_mask: u32) -> Option<(EventSlot, u64)> {
        // Nothing can be strictly past due unless the earliest deadline is.
        if now <= self.earliest {
            return None;
        }
        let (slot, target) = self.next_due(now, excluded_mask)?;
        self.pending &= !(1 << slot.bit());
        self.refresh_earliest();
        self.total_fired = self.total_fired.saturating_add(1);
        Some((slot, target))
    }

    /// What [`Scheduler::take_due`] would return next, without removing it.
    pub fn peek_due(&self, now: u64) -> Option<EventSlot> {
        self.next_due(now, 0).map(|(slot, _)| slot)
    }

    /// Earliest pending deadline, `u64::MAX` when nothing is pending.
    #[inline]
    pub fn lowest_target(&self) -> u64 {
        self.earliest
    }

    /// Number of events scheduled since construction. Diagnostic.
    pub fn total_scheduled(&self) -> u64 {
        self.total_scheduled
    }

    /// Number of events fired since construction. Diagnostic.
    pub fn total_fired(&self) -> u64 {
        self.total_fired
    }

    fn refresh_earliest(&mut self) {
        let mut earliest = u64::MAX;
        let mut bits = self.pending;
        while bits != 0 {
            earliest = earliest.min(self.targets[bits.trailing_zeros() as usize]);
            bits &= bits - 1;
        }
        self.earliest = earliest;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_scheduler_has_nothing_pending() {
        let s = Scheduler::new();
        assert_eq!(s.pending_bitmap(), 0);
        assert_eq!(s.lowest_target(), u64::MAX);
        assert!(s.peek_due(0).is_none());
        assert!(s.peek_due(u64::MAX).is_none());
    }

    #[test]
    fn schedule_marks_slot_pending_and_records_target() {
        let mut s = Scheduler::new();
        s.schedule(EventSlot::VBlank, 100, 500);
        assert!(s.is_pending(EventSlot::VBlank));
        assert_eq!(s.target(EventSlot::VBlank), Some(600));
        assert_eq!(s.lowest_target(), 600);
        assert_eq!(s.total_scheduled(), 1);
    }

    #[test]
    fn take_due_before_deadline_returns_none() {
        let mut s = Scheduler::new();
        s.schedule(EventSlot::CdResponse, 100, 500);
        assert!(s.take_due(599).is_none());
        // Still pending.
        assert!(s.is_pending(EventSlot::CdResponse));
    }

    #[test]
    fn take_due_strictly_after_deadline_fires_and_clears() {
        let mut s = Scheduler::new();
        s.schedule(EventSlot::CdResponse, 100, 500);
        assert!(
            s.take_due(600).is_none(),
            "exact target must wait one boundary"
        );
        assert_eq!(s.take_due(601), Some((EventSlot::CdResponse, 600)));
        assert!(!s.is_pending(EventSlot::CdResponse));
        // Draining again returns None.
        assert!(s.take_due(601).is_none());
        assert_eq!(s.total_fired(), 1);
    }

    #[test]
    fn multiple_slots_fire_in_target_order_not_bit_order() {
        // Schedule GpuDma (bit 4) first with a later target, then
        // Sio (bit 0) with an earlier one. take_due should return
        // Sio first because its target is smaller.
        let mut s = Scheduler::new();
        s.schedule(EventSlot::GpuDma, 100, 1000);
        s.schedule(EventSlot::Sio0, 100, 200);
        assert_eq!(s.take_due(5000), Some((EventSlot::Sio0, 300)));
        assert_eq!(s.take_due(5000), Some((EventSlot::GpuDma, 1100)));
        assert!(s.take_due(5000).is_none());
    }

    #[test]
    fn re_scheduling_same_slot_replaces_deadline() {
        let mut s = Scheduler::new();
        s.schedule(EventSlot::CdSector, 100, 1000);
        s.schedule(EventSlot::CdSector, 100, 500);
        assert_eq!(s.target(EventSlot::CdSector), Some(600));
        assert_eq!(s.lowest_target(), 600);
    }

    #[test]
    fn cancel_removes_slot_and_recomputes_lowest() {
        let mut s = Scheduler::new();
        s.schedule(EventSlot::Sio0, 0, 100);
        s.schedule(EventSlot::CdResponse, 0, 200);
        assert_eq!(s.lowest_target(), 100);
        s.cancel(EventSlot::Sio0);
        assert!(!s.is_pending(EventSlot::Sio0));
        assert_eq!(s.lowest_target(), 200);
    }

    #[test]
    fn cancel_of_non_pending_slot_is_noop() {
        let mut s = Scheduler::new();
        s.cancel(EventSlot::CdResponse);
        assert_eq!(s.pending_bitmap(), 0);
    }

    #[test]
    fn lowest_target_collapses_to_max_when_drained() {
        let mut s = Scheduler::new();
        s.schedule(EventSlot::VBlank, 0, 100);
        s.take_due(101);
        assert_eq!(s.lowest_target(), u64::MAX);
    }

    #[test]
    fn peek_due_does_not_mutate() {
        let mut s = Scheduler::new();
        s.schedule(EventSlot::GpuDma, 0, 50);
        assert!(
            s.peek_due(50).is_none(),
            "exact target must still look pending"
        );
        assert_eq!(s.peek_due(100), Some(EventSlot::GpuDma));
        // Still pending after peek.
        assert!(s.is_pending(EventSlot::GpuDma));
    }

    #[test]
    fn from_index_round_trips_all_defined_slots() {
        for raw in 0..=14u32 {
            let slot = EventSlot::from_index(raw).unwrap();
            assert_eq!(slot.bit(), raw);
        }
        assert!(EventSlot::from_index(15).is_none());
        assert!(EventSlot::from_index(99).is_none());
    }

    #[test]
    fn simultaneous_deadlines_both_fire() {
        let mut s = Scheduler::new();
        s.schedule(EventSlot::Sio0, 0, 100);
        s.schedule(EventSlot::VBlank, 0, 100);
        assert!(s.take_due(100).is_none(), "exact target must not fire yet");
        let first = s.take_due(101);
        let second = s.take_due(101);
        let third = s.take_due(101);
        assert!(first.is_some() && second.is_some());
        assert!(third.is_none());
        // Order is irrelevant -- both should have fired, and both
        // should report target 100.
        let mut fired = [first.unwrap().0, second.unwrap().0];
        fired.sort_by_key(|s| s.bit());
        assert_eq!(fired, [EventSlot::Sio0, EventSlot::VBlank]);
        assert_eq!(first.unwrap().1, 100);
        assert_eq!(second.unwrap().1, 100);
    }

    #[test]
    fn take_due_reports_original_target_for_periodic_reschedule() {
        // VBlank + SPU async need to reschedule from the *original*
        // target so long drain lags don't accumulate drift. If we
        // fire at now=700 but the target was 500, the next period
        // should start at 500, not 700.
        let mut s = Scheduler::new();
        s.schedule(EventSlot::VBlank, 0, 500);
        let fired = s.take_due(700).unwrap();
        assert_eq!(fired.0, EventSlot::VBlank);
        assert_eq!(fired.1, 500); // original target, not now=700
    }

    #[test]
    fn saturating_schedule_on_large_delta_does_not_wrap() {
        let mut s = Scheduler::new();
        s.schedule(EventSlot::SpuMix, u64::MAX - 10, 100);
        assert_eq!(s.target(EventSlot::SpuMix), Some(u64::MAX));
    }
}
