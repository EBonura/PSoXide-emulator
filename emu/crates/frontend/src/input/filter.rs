//! Per-device guard between a raw snapshot and everything that acts on it.

use std::collections::VecDeque;

use super::model::rising_edges;

/// Window over which a device's presses are counted.
const CHATTER_WINDOW_MS: u64 = 1000;
/// Button presses inside the window that no person produces: a held-down
/// mash of four buttons at 8 Hz is already 32 a second.
const CHATTER_PRESS_LIMIT: usize = 32;
/// How long an ignored device must report no change before it counts again.
const CHATTER_QUIET_MS: u64 = 1500;

/// What [`PadFilter::filter`] decided about a device this poll.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FilteredPad {
    /// Buttons the rest of the app may see (empty while the device is ignored).
    pub(crate) mask: u16,
    /// The device crossed into or out of the ignored state on this poll.
    pub(crate) change: Option<ChatterChange>,
}

/// A transition of the noise guard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChatterChange {
    /// The device started chattering and is now ignored.
    Ignored,
    /// The device was quiet long enough and counts again.
    Restored,
}

/// Per-device guard between the raw snapshot and everything that acts on it
/// (menu, freecam shortcuts, guest ports). It is the same for the keyboard
/// and every pad.
///
/// Two protections, both because a host device must never be able to trap
/// the app:
/// - Buttons already down when the device connects, or when [`Self::rebaseline`]
///   is called (the menu opened), are ignored until released, so only fresh
///   presses act.
/// - A device whose presses outrun any person (a stuck or mis-parsed one; a
///   Switch Pro Controller on a Mac produced about 90 face-button presses a
///   second with nobody touching it, which confirmed the Library's first row
///   again and again) is ignored until it has been completely quiet for
///   [`CHATTER_QUIET_MS`].
#[derive(Debug, Default)]
pub(crate) struct PadFilter {
    /// Raw mask of the previous poll, for press counting.
    previous: u16,
    /// Press times (ms) inside the current window.
    presses: VecDeque<u64>,
    /// Buttons held at the last baseline, ignored until released.
    ignore: u16,
    /// Take the next sample as the new baseline.
    rebaseline: bool,
    /// Whether the device is currently ignored for chatter.
    chattering: bool,
    /// Time (ms) of the last raw-state change while ignored.
    last_change_ms: u64,
}

impl PadFilter {
    /// A filter for a device that has just connected: whatever it holds now is a baseline.
    pub(crate) fn new() -> Self {
        Self {
            rebaseline: true,
            ..Self::default()
        }
    }

    /// Treat the buttons held at the next poll as a baseline: they do nothing
    /// until released. Called when the menu opens or a game launches.
    pub(crate) fn rebaseline(&mut self) {
        self.rebaseline = true;
    }

    /// Whether the device is currently ignored for chatter.
    pub(crate) fn is_chattering(&self) -> bool {
        self.chattering
    }

    /// Filter one raw sample taken at `now_ms` (any monotonic clock).
    pub(crate) fn filter(&mut self, now_ms: u64, raw: u16) -> FilteredPad {
        let pressed = rising_edges(self.previous, raw);
        let changed = raw != self.previous;
        self.previous = raw;

        for _ in 0..pressed.count_ones() {
            self.presses.push_back(now_ms);
        }
        while self
            .presses
            .front()
            .is_some_and(|t| now_ms.saturating_sub(*t) > CHATTER_WINDOW_MS)
        {
            self.presses.pop_front();
        }

        let mut change = None;
        if self.chattering {
            if changed {
                self.last_change_ms = now_ms;
            } else if now_ms.saturating_sub(self.last_change_ms) >= CHATTER_QUIET_MS {
                self.chattering = false;
                self.presses.clear();
                self.rebaseline = true;
                change = Some(ChatterChange::Restored);
            }
        } else if self.presses.len() > CHATTER_PRESS_LIMIT {
            self.chattering = true;
            self.last_change_ms = now_ms;
            change = Some(ChatterChange::Ignored);
        }
        if self.chattering {
            return FilteredPad { mask: 0, change };
        }

        if self.rebaseline {
            self.rebaseline = false;
            self.ignore = raw;
        }
        // A baseline button counts again once it has been let go.
        self.ignore &= raw;
        FilteredPad {
            mask: raw & !self.ignore,
            change,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use emulator_core::button;

    /// A deterministic stand-in for the faulty controller seen on Manny's Mac:
    /// the four face buttons flip about 25 times a second each.
    pub(crate) fn noisy_face_buttons(poll: u64) -> u16 {
        let faces = [
            button::CROSS,
            button::CIRCLE,
            button::SQUARE,
            button::TRIANGLE,
        ];
        // Cheap xorshift so the pattern is irregular but reproducible.
        let mut x = poll.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        faces
            .iter()
            .enumerate()
            .filter(|(i, _)| (x >> (i * 8)) & 1 == 1)
            .fold(0, |mask, (_, bit)| mask | bit)
    }

    #[test]
    fn a_noisy_device_is_ignored_before_it_can_confirm_the_menu() {
        let mut filter = PadFilter::new();
        let mut previous = 0;
        let mut confirms = 0;
        let mut ignored_at = None;
        for poll in 0..600u64 {
            let now_ms = poll * 1000 / 60;
            let out = filter.filter(now_ms, noisy_face_buttons(poll));
            if out.change == Some(ChatterChange::Ignored) {
                ignored_at = Some(now_ms);
            }
            confirms += usize::from(rising_edges(previous, out.mask) & button::CROSS != 0);
            previous = out.mask;
        }
        let ignored_at = ignored_at.expect("noise must be recognised");
        assert!(ignored_at <= 1200, "ignored only after {ignored_at} ms");
        // Everything after that moment is silent: no menu confirm, no guest input.
        let confirms_after = {
            let mut f = PadFilter::new();
            let mut prev = 0;
            let mut n = 0;
            for poll in 0..600u64 {
                let out = f.filter(poll * 1000 / 60, noisy_face_buttons(poll));
                if f.is_chattering() {
                    assert_eq!(out.mask, 0);
                }
                if poll * 1000 / 60 > ignored_at {
                    n += usize::from(rising_edges(prev, out.mask) & button::CROSS != 0);
                }
                prev = out.mask;
            }
            n
        };
        assert_eq!(confirms_after, 0);
        assert!(confirms < 20, "{confirms} confirms leaked before the guard");
    }

    #[test]
    fn an_ignored_device_counts_again_after_it_goes_quiet() {
        let mut filter = PadFilter::new();
        let mut now = 0;
        for poll in 0..200u64 {
            now = poll * 1000 / 60;
            filter.filter(now, noisy_face_buttons(poll));
        }
        assert!(filter.is_chattering());
        // Quiet: no change at all for longer than the quiet period.
        let mut restored = false;
        for _ in 0..180 {
            now += 17;
            restored |= filter.filter(now, 0).change == Some(ChatterChange::Restored);
        }
        assert!(restored && !filter.is_chattering());
        // A real press now goes through.
        now += 17;
        assert_eq!(filter.filter(now, button::CROSS).mask, button::CROSS);
    }

    #[test]
    fn people_pressing_fast_are_not_mistaken_for_noise() {
        let mut filter = PadFilter::new();
        // One button mashed at 12 Hz for ten seconds.
        for poll in 0..600u64 {
            let now_ms = poll * 1000 / 60;
            let down = (poll / 2) % 5 < 2;
            let out = filter.filter(now_ms, if down { button::CROSS } else { 0 });
            assert!(out.change.is_none());
        }
        assert!(!filter.is_chattering());
    }

    #[test]
    fn buttons_held_at_connect_or_menu_open_wait_for_release() {
        let mut filter = PadFilter::new();
        // Connected with Cross already down: nothing acts until it is let go.
        assert_eq!(filter.filter(0, button::CROSS).mask, 0);
        assert_eq!(filter.filter(16, button::CROSS).mask, 0);
        assert_eq!(filter.filter(33, 0).mask, 0);
        assert_eq!(filter.filter(50, button::CROSS).mask, button::CROSS);
        // The menu opens while Cross is held: same rule.
        filter.rebaseline();
        assert_eq!(filter.filter(66, button::CROSS).mask, 0);
        assert_eq!(
            filter.filter(83, button::CROSS | button::LEFT).mask,
            button::LEFT
        );
        assert_eq!(filter.filter(100, button::LEFT).mask, button::LEFT);
    }
}
