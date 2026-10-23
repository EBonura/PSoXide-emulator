//! Hold-to-repeat for menu navigation, as a pure state machine.
//!
//! A direction fires on press, waits [`INITIAL_DELAY_MS`], then repeats at a
//! rate that climbs from [`START_RATE_HZ`] to [`END_RATE_HZ`] over
//! [`RAMP_MS`], so a long list scrolls slowly enough to stop on a row and
//! quickly enough to cross a library in a second or two. Releasing, or
//! pressing the other direction of the same axis, starts over.
//!
//! The machine only sees the filtered, merged button mask, so everything the
//! input hub guarantees applies to it: a direction held when the menu opens
//! is baselined away until released, and a quarantined (noisy) device
//! contributes nothing.

use emulator_core::button;

/// Time a direction must be held before it starts repeating.
pub(crate) const INITIAL_DELAY_MS: u64 = 300;
/// Repeat rate right after the initial delay, in steps per second.
pub(crate) const START_RATE_HZ: u64 = 8;
/// Fastest repeat rate, in steps per second.
pub(crate) const END_RATE_HZ: u64 = 30;
/// Time after the initial delay over which the rate climbs to [`END_RATE_HZ`].
pub(crate) const RAMP_MS: u64 = 1500;

/// One direction pair (up/down or left/right).
#[derive(Clone, Copy, Debug, Default)]
struct AxisRepeat {
    /// The held direction and when its repeat schedule runs.
    active: Option<Held>,
}

#[derive(Clone, Copy, Debug)]
struct Held {
    direction: u16,
    /// When the first repeat becomes due: press time plus the initial delay.
    repeat_from_ms: u64,
    /// When the next step is due.
    next_due_ms: u64,
}

impl AxisRepeat {
    /// Step the axis; returns the direction to fire this tick, or 0.
    fn update(&mut self, now_ms: u64, pair: u16, held: u16) -> u16 {
        let direction = held & pair;
        // Both sides at once cannot be resolved to a direction.
        if direction == 0 || direction == pair {
            self.active = None;
            return 0;
        }
        match self.active {
            Some(active) if active.direction == direction => {}
            _ => {
                self.active = Some(Held {
                    direction,
                    repeat_from_ms: now_ms + INITIAL_DELAY_MS,
                    next_due_ms: now_ms + INITIAL_DELAY_MS,
                });
                return direction;
            }
        }
        let Some(active) = self.active.as_mut() else {
            return 0;
        };
        if now_ms < active.next_due_ms {
            return 0;
        }
        // The step interval at this point of the ramp.
        let into_ramp = now_ms.saturating_sub(active.repeat_from_ms).min(RAMP_MS);
        let rate_milli_hz =
            START_RATE_HZ * 1000 + (END_RATE_HZ - START_RATE_HZ) * 1000 * into_ramp / RAMP_MS;
        let interval_ms = (1_000_000 / rate_milli_hz).max(1);
        // Keep the cadence steady, but never queue a burst after a stall.
        active.next_due_ms = (active.next_due_ms + interval_ms).max(now_ms + 1);
        direction
    }
}

/// Hold-to-repeat for the four menu directions.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct MenuRepeat {
    vertical: AxisRepeat,
    horizontal: AxisRepeat,
}

impl MenuRepeat {
    /// Step both axes with the currently held directions in `held` and
    /// return the direction bits that fire this tick.
    pub(crate) fn update(&mut self, now_ms: u64, held: u16) -> u16 {
        self.vertical
            .update(now_ms, button::UP | button::DOWN, held)
            | self
                .horizontal
                .update(now_ms, button::LEFT | button::RIGHT, held)
    }

    /// Forget any hold in progress.
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `held` over `[from_ms, to_ms]` at a fixed tick and collect the
    /// times at which `direction` fired.
    fn fires(
        repeat: &mut MenuRepeat,
        direction: u16,
        held: u16,
        from_ms: u64,
        to_ms: u64,
        tick_ms: u64,
    ) -> Vec<u64> {
        let mut out = Vec::new();
        let mut t = from_ms;
        while t <= to_ms {
            if repeat.update(t, held) & direction != 0 {
                out.push(t);
            }
            t += tick_ms;
        }
        out
    }

    #[test]
    fn a_tap_moves_exactly_once() {
        let mut r = MenuRepeat::default();
        let fired = fires(&mut r, button::DOWN, button::DOWN, 0, 200, 16);
        assert_eq!(fired, vec![0]);
        assert_eq!(r.update(216, 0), 0);
    }

    #[test]
    fn nothing_repeats_before_the_initial_delay() {
        let mut r = MenuRepeat::default();
        let fired = fires(
            &mut r,
            button::DOWN,
            button::DOWN,
            0,
            INITIAL_DELAY_MS - 1,
            4,
        );
        assert_eq!(fired, vec![0]);
    }

    #[test]
    fn repeats_start_slow_and_speed_up_to_the_cap() {
        let mut r = MenuRepeat::default();
        let fired = fires(&mut r, button::DOWN, button::DOWN, 0, 5000, 1);
        // First repeat is due right at the initial delay.
        assert_eq!(fired[0], 0);
        assert_eq!(fired[1], INITIAL_DELAY_MS);
        let gaps: Vec<u64> = fired.windows(2).map(|w| w[1] - w[0]).collect();
        // The gap after the first repeat is close to the slow rate...
        let slow = 1000 / START_RATE_HZ;
        assert!(
            gaps[1].abs_diff(slow) <= 5,
            "first repeat gap {} vs {slow}",
            gaps[1]
        );
        // ...gaps never grow while the hold lasts...
        for pair in gaps[1..].windows(2) {
            assert!(pair[1] <= pair[0] + 1, "gaps must shrink: {gaps:?}");
        }
        // ...and settle at the capped rate.
        let fast = 1000 / END_RATE_HZ;
        let last = *gaps.last().unwrap();
        assert!(last.abs_diff(fast) <= 2, "last gap {last} vs {fast}");
        // The cap is reached about RAMP_MS after the repeats begin.
        let capped_at = fired
            .iter()
            .zip(gaps.iter())
            .find(|(_, gap)| gap.abs_diff(fast) <= 1)
            .map(|(t, _)| *t)
            .expect("reaches the cap");
        assert!(
            capped_at.abs_diff(INITIAL_DELAY_MS + RAMP_MS) <= 200,
            "capped at {capped_at}"
        );
    }

    #[test]
    fn the_repeat_rate_never_exceeds_the_cap_even_with_fast_ticks() {
        let mut r = MenuRepeat::default();
        let fired = fires(&mut r, button::UP, button::UP, 0, 10_000, 1);
        let second_window = fired.iter().filter(|t| (6000..7000).contains(*t)).count() as u64;
        assert!(second_window <= END_RATE_HZ + 1, "{second_window} steps/s");
        assert!(second_window >= END_RATE_HZ - 2, "{second_window} steps/s");
    }

    #[test]
    fn a_slow_tick_never_moves_more_than_one_step_per_tick() {
        let mut r = MenuRepeat::default();
        // 100 ms ticks (a stalled UI): at most one step each, no burst after.
        let fired = fires(&mut r, button::DOWN, button::DOWN, 0, 3000, 100);
        for pair in fired.windows(2) {
            assert!(pair[1] - pair[0] >= 100);
        }
    }

    #[test]
    fn releasing_resets_the_schedule() {
        let mut r = MenuRepeat::default();
        let _ = fires(&mut r, button::DOWN, button::DOWN, 0, 2000, 4);
        assert_eq!(r.update(2004, 0), 0);
        // Pressed again: one step, then the full initial delay again.
        let fired = fires(&mut r, button::DOWN, button::DOWN, 2008, 2008 + 299, 4);
        assert_eq!(fired, vec![2008]);
    }

    #[test]
    fn changing_direction_fires_the_new_one_and_restarts() {
        let mut r = MenuRepeat::default();
        let _ = fires(&mut r, button::DOWN, button::DOWN, 0, 1000, 4);
        // Roll the thumb from down to up without a gap.
        assert_eq!(r.update(1004, button::UP), button::UP);
        assert_eq!(r.update(1008, button::UP), 0);
        let fired = fires(&mut r, button::UP, button::UP, 1012, 1004 + 299, 4);
        assert!(fired.is_empty(), "repeat waits the full delay again");
    }

    #[test]
    fn the_two_axes_are_independent() {
        let mut r = MenuRepeat::default();
        assert_eq!(
            r.update(0, button::DOWN | button::RIGHT),
            button::DOWN | button::RIGHT
        );
        // Releasing right does not disturb down's schedule.
        let fired = fires(&mut r, button::DOWN, button::DOWN, 4, 400, 4);
        assert_eq!(fired, vec![INITIAL_DELAY_MS]);
    }

    #[test]
    fn both_sides_of_an_axis_at_once_do_nothing() {
        let mut r = MenuRepeat::default();
        assert_eq!(r.update(0, button::UP | button::DOWN), 0);
        assert_eq!(r.update(500, button::UP | button::DOWN), 0);
    }
}
