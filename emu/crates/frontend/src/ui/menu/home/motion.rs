//! Motion helpers for the shell's pages: a spring for things that slide (the tab highlight, the
//! segmented control), the staged reveal of a page's parts, and a way to move a group of already
//! painted shapes without rewriting the code that painted them.
//!
//! Everything here is a few multiplications per frame. Reduced motion (the setting, or the
//! browser's preference) snaps every spring and shows every part at once.

use egui::emath::TSTransform;
use egui::{Context, Painter, Vec2};

use super::tokens;

/// Natural frequency of the sliding highlights, radians per second.
const OMEGA: f32 = 15.0;
/// Damping ratio: 1 settles with no overshoot, less overshoots. 0.62 is a small, quick bounce.
const ZETA: f32 = 0.62;
/// The longest integration step, so a slow frame cannot blow the spring up.
const MAX_STEP: f32 = 1.0 / 120.0;

/// A value that chases a target the way a damped spring does.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Spring {
    pub pos: f32,
    vel: f32,
}

impl Spring {
    pub(super) fn at(pos: f32) -> Self {
        Self { pos, vel: 0.0 }
    }

    /// Move toward `target` over `dt` seconds; `snap` jumps there at once (reduced motion).
    pub(super) fn step(&mut self, target: f32, dt: f32, snap: bool) {
        if snap {
            *self = Self::at(target);
            return;
        }
        let mut left = dt.clamp(0.0, 0.25);
        while left > 1e-5 {
            let h = left.min(MAX_STEP);
            left -= h;
            let force = OMEGA * OMEGA * (target - self.pos) - 2.0 * ZETA * OMEGA * self.vel;
            self.vel += force * h;
            self.pos += self.vel * h;
        }
        if (self.pos - target).abs() < 0.0005 && self.vel.abs() < 0.005 {
            *self = Self::at(target);
        }
    }

    /// Whether it has arrived.
    #[cfg(test)]
    pub(super) fn settled(&self, target: f32) -> bool {
        (self.pos - target).abs() < 0.0005 && self.vel.abs() < 0.005
    }
}

/// Seconds between one part of a page starting to appear and the next.
const STAGE_STEP: f32 = 0.05;
/// Parts past this many steps appear together with the last one, so a long list never waits.
const STAGE_MAX_STEPS: usize = 8;
/// Seconds one part takes to arrive.
const STAGE_SECS: f32 = 0.34;

/// How far along part number `index` of a page is, 0 to 1, `age` seconds after the page opened.
/// Eased out, so a part lands softly.
pub(super) fn stage(age: f32, index: usize) -> f32 {
    let delay = STAGE_STEP * index.min(STAGE_MAX_STEPS) as f32;
    tokens::ease_out_expo(((age - delay) / STAGE_SECS).clamp(0.0, 1.0))
}

/// How long a page takes to be fully shown, in seconds.
#[cfg(test)]
pub(super) fn stage_total() -> f32 {
    STAGE_STEP * STAGE_MAX_STEPS as f32 + STAGE_SECS
}

/// Where in the painted shapes of a layer a group starts, to move it afterwards.
#[derive(Clone, Copy)]
pub(super) struct Mark(egui::layers::ShapeIdx);

/// Note where the next shape of `painter`'s layer will land.
pub(super) fn mark(ctx: &Context, painter: &Painter) -> Mark {
    Mark(ctx.graphics_mut(|g| g.entry(painter.layer_id()).next_idx()))
}

/// Move every shape painted since `from` by `by` (points).
pub(super) fn shift_since(ctx: &Context, painter: &Painter, from: Mark, by: Vec2) {
    if by == Vec2::ZERO {
        return;
    }
    ctx.graphics_mut(|g| {
        let list = g.entry(painter.layer_id());
        let end = list.next_idx();
        list.transform_range(from.0, end, TSTransform::from_translation(by));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spring_arrives_with_a_small_overshoot_and_settles() {
        let mut s = Spring::at(0.0);
        let mut peak = 0.0_f32;
        for _ in 0..120 {
            s.step(1.0, 1.0 / 60.0, false);
            peak = peak.max(s.pos);
        }
        assert!(peak > 1.0 && peak < 1.12, "a little overshoot, got {peak}");
        assert!(s.settled(1.0), "settled after two seconds: {s:?}");
    }

    #[test]
    fn a_slow_frame_does_not_blow_the_spring_up() {
        let mut s = Spring::at(0.0);
        s.step(3.0, 2.0, false);
        assert!(s.pos.is_finite() && (s.pos - 3.0).abs() < 0.5, "{s:?}");
    }

    #[test]
    fn snap_jumps_straight_to_the_target() {
        let mut s = Spring::at(0.0);
        s.step(4.0, 0.016, true);
        assert_eq!(s.pos, 4.0);
        assert!(s.settled(4.0));
    }

    #[test]
    fn parts_arrive_one_after_another_and_all_by_the_end() {
        assert_eq!(stage(0.0, 0), 0.0);
        assert!(stage(0.1, 0) > stage(0.1, 3), "an earlier part is further along");
        assert!(stage(stage_total(), 99) > 0.999);
        assert_eq!(stage(10.0, 0), 1.0);
    }
}
