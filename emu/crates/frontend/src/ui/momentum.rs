//! Touch scrolling that feels like a phone's: the content follows the finger, keeps coasting
//! after it lifts and slows smoothly, stretches against the ends and springs back, and a finger
//! put down on a moving list stops it without pressing anything under it.
//!
//! [`Momentum`] holds only the motion state. The scroll position stays with the page that owns
//! the list, as a plain `f32` in points (0 at the top, `max` at the bottom), so each surface keeps
//! drawing from the number it already had; this module moves that number. The position may sit a
//! little outside `0..=max` while the list is stretched or springing back, and drawing code must
//! accept that (the shell's lists clip to their view, the egui panels use [`scroll_area`]).
//!
//! The model follows iOS: velocity from the last 100 ms of finger travel, exponential
//! deceleration at the rate of `UIScrollView.DecelerationRate.normal` (0.998 per millisecond),
//! rubber-band resistance while pulling past an end, and a critically damped spring back.

use egui::{Id, Rect, Sense, Ui};

/// Deceleration, per second: velocity falls by `exp(-FRICTION * t)`. That is 0.998 per
/// millisecond, iOS's normal rate, so a flick of `v` points per second travels `v / FRICTION`.
const FRICTION: f32 = 2.0;
/// Speed below which a coast stops, points per second.
const STOP_SPEED: f32 = 8.0;
/// A release slower than this does not start a coast, points per second.
const MIN_FLING: f32 = 60.0;
/// The fastest coast, points per second.
const MAX_FLING: f32 = 7000.0;
/// Finger travel older than this does not count towards the release speed, seconds.
const VELOCITY_WINDOW: f64 = 0.1;
/// A finger that rested this long before lifting leaves no speed, seconds.
const REST_TIME: f64 = 0.08;
/// Spring back to an end: natural frequency, radians per second. Critically damped, so it
/// settles without crossing the end.
const SPRING: f32 = 13.0;
/// How far the content may be stretched past an end, as a share of the view.
const STRETCH_LIMIT: f32 = 0.35;
/// Stretch at which the finger gets no more give, as a share of the view: the stretch follows
/// `L * (1 - exp(-pulled / L))`, like iOS's rubber band.
const STRETCH_SOFTNESS: f32 = 0.4;
/// Samples kept for the release speed.
const SAMPLES: usize = 12;

/// Where the finger has been, to read its speed when it lifts.
#[derive(Clone, Copy, Debug, Default)]
struct Sample {
    time: f64,
    delta: f32,
}

/// The motion of one scrolling surface.
#[derive(Clone, Debug, Default)]
pub(crate) struct Momentum {
    /// Speed in points per second, positive towards the end of the content.
    vel: f32,
    /// The content is coasting or springing back.
    moving: bool,
    /// A finger is dragging it.
    dragging: bool,
    samples: [Sample; SAMPLES],
    count: usize,
    last_time: f64,
}

impl Momentum {
    /// Whether the content is moving on its own (a coast or a spring back): a finger put down now
    /// is a stop, not a tap.
    #[must_use]
    pub(crate) fn is_moving(&self) -> bool {
        self.moving
    }

    /// Whether a finger is dragging the content.
    #[must_use]
    pub(crate) fn is_dragging(&self) -> bool {
        self.dragging
    }

    /// Whether the position is under this object's control: left alone by keyboard focus
    /// following, and not snapped back by a clamp.
    #[must_use]
    pub(crate) fn is_active(&self) -> bool {
        self.moving || self.dragging
    }

    /// Stop everything where it is (a page change, the focus scrolling the list by key).
    pub(crate) fn stop(&mut self) {
        self.vel = 0.0;
        self.moving = false;
        self.dragging = false;
        self.count = 0;
    }

    /// A finger came down. Returns whether it caught a moving list: that touch only stops it and
    /// must not press anything.
    pub(crate) fn grab(&mut self) -> bool {
        let caught = self.moving;
        self.vel = 0.0;
        self.moving = false;
        self.dragging = false;
        self.count = 0;
        caught
    }

    /// The finger moved by `delta` points towards the end of the content (a finger moving up
    /// scrolls forward: pass the negated pointer travel) at `now` seconds. `max` is the furthest
    /// scroll position and `view` the height of the visible window.
    pub(crate) fn drag(&mut self, pos: &mut f32, delta: f32, now: f64, max: f32, view: f32) {
        if !self.dragging {
            self.dragging = true;
            self.moving = false;
            self.vel = 0.0;
            self.count = 0;
        }
        if self.count == SAMPLES {
            self.samples.copy_within(1.., 0);
            self.count -= 1;
        }
        self.samples[self.count] = Sample { time: now, delta };
        self.count += 1;
        self.last_time = now;
        // Past an end the content gives less and less; coming back it follows the finger.
        let over = stretch(*pos, max);
        let outward = over != 0.0 && over.signum() == delta.signum();
        let limit = (STRETCH_SOFTNESS * view).max(1.0);
        let give = if outward {
            (1.0 - over.abs() / limit).max(0.0)
        } else {
            1.0
        };
        *pos += delta * give;
    }

    /// The finger lifted at `now` seconds: start the coast, or the spring back.
    pub(crate) fn release(&mut self, pos: f32, now: f64, max: f32) {
        if !self.dragging {
            return;
        }
        self.dragging = false;
        let speed = self.release_speed(now);
        self.count = 0;
        if stretch(pos, max) != 0.0 {
            // Stretched at the end: the spring takes it from rest, whatever the finger did.
            self.vel = 0.0;
            self.moving = true;
        } else if speed.abs() >= MIN_FLING {
            self.vel = speed.clamp(-MAX_FLING, MAX_FLING);
            self.moving = true;
        } else {
            self.vel = 0.0;
            self.moving = false;
        }
    }

    /// Points per second over the last [`VELOCITY_WINDOW`] of travel; zero when the finger rested.
    fn release_speed(&self, now: f64) -> f32 {
        if now - self.last_time > REST_TIME {
            return 0.0;
        }
        let recent: Vec<Sample> = self.samples[..self.count]
            .iter()
            .copied()
            .filter(|s| now - s.time <= VELOCITY_WINDOW)
            .collect();
        let (Some(first), Some(last)) = (recent.first(), recent.last()) else {
            return 0.0;
        };
        let span = if recent.len() > 1 {
            // The frames' own spacing: the travel of the first sample began a frame earlier.
            (last.time - first.time) * recent.len() as f64 / (recent.len() - 1) as f64
        } else {
            1.0 / 60.0
        };
        let total: f32 = recent.iter().map(|s| s.delta).sum();
        total / span.max(1.0 / 240.0) as f32
    }

    /// The wheel or a trackpad scrolled by `delta` points: the content follows at once and any
    /// coast ends.
    pub(crate) fn wheel(&mut self, pos: &mut f32, delta: f32, max: f32) {
        self.stop();
        *pos = (*pos + delta).clamp(0.0, max.max(0.0));
    }

    /// Advance a coast or a spring back by `dt` seconds. Returns whether it is still moving (the
    /// caller keeps repainting).
    pub(crate) fn step(&mut self, pos: &mut f32, dt: f32, max: f32, view: f32) -> bool {
        if !self.moving {
            return false;
        }
        let dt = dt.clamp(0.0, 0.1);
        let max = max.max(0.0);
        let over = stretch(*pos, max);
        if over != 0.0 {
            // Critically damped spring towards the nearest end, in closed form so any frame
            // time is safe.
            let bound = if over > 0.0 { max } else { 0.0 };
            let e0 = *pos - bound;
            let c = self.vel + SPRING * e0;
            let decay = (-SPRING * dt).exp();
            let e = (e0 + c * dt) * decay;
            self.vel = (self.vel - SPRING * c * dt) * decay;
            *pos = bound + e;
            let limit = STRETCH_LIMIT * view.max(1.0);
            if e.abs() > limit {
                *pos = bound + limit * e.signum();
                self.vel = 0.0;
            }
            if e.abs() < 0.25 && self.vel.abs() < STOP_SPEED {
                *pos = bound;
                self.vel = 0.0;
                self.moving = false;
            }
        } else {
            let decay = (-FRICTION * dt).exp();
            *pos += self.vel * (1.0 - decay) / FRICTION;
            self.vel *= decay;
            if self.vel.abs() < STOP_SPEED {
                self.vel = 0.0;
                self.moving = false;
            }
        }
        self.moving
    }

    /// Hold `pos` inside the content unless this object is stretching it on purpose.
    pub(crate) fn limit(&self, pos: &mut f32, max: f32) {
        if !self.is_active() {
            *pos = pos.clamp(0.0, max.max(0.0));
        }
    }
}

/// How far `pos` is past an end: negative above the top, positive below the bottom, else zero.
#[must_use]
pub(crate) fn stretch(pos: f32, max: f32) -> f32 {
    if pos < 0.0 {
        pos
    } else if pos > max {
        pos - max
    } else {
        0.0
    }
}

/// Where a scroll area stands, for the browser journeys: the position, the furthest position and
/// whether it is coasting.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Reading {
    pub(crate) pos: f32,
    pub(crate) max: f32,
    pub(crate) moving: bool,
    /// The viewport's top and bottom edge, points.
    pub(crate) top: f32,
    pub(crate) bottom: f32,
    /// The viewport's left and right edge, points.
    pub(crate) left: f32,
    pub(crate) right: f32,
    /// The widest the content reached, points (wider than the viewport means it overflows).
    pub(crate) content_right: f32,
}

/// What a scroll area of the debug panels keeps between frames.
#[derive(Clone, Debug, Default)]
struct AreaState {
    momentum: Momentum,
    /// The scroll position, which may be stretched past the content.
    pos: f32,
    /// The viewport as last laid out, to catch fingers before the content is drawn.
    view: Option<Rect>,
    /// The content's own height as last measured, without the stretch padding.
    content: f32,
    /// The right edge of what the content drew, as last measured.
    content_right: f32,
}

/// A vertical egui scroll area with the same momentum, rubber band and tap-to-stop as the shell's
/// lists. `configure` sets the area's size and look (anything but its offset and scrolling
/// switches); `add` draws the content.
///
/// The area is dragged anywhere a widget inside does not take the drag, so sliders and buttons
/// keep working; the wheel and the scroll bar move it as in any egui area. While it coasts a transparent
/// cover takes the next touch, so the touch that stops a coast never lands on a button.
pub(crate) fn scroll_area<R>(
    ui: &mut Ui,
    id_salt: impl std::hash::Hash,
    configure: impl FnOnce(egui::ScrollArea) -> egui::ScrollArea,
    add: impl FnOnce(&mut Ui) -> R,
) -> (R, Reading) {
    scroll_area_if(ui, id_salt, true, configure, add)
}

/// [`scroll_area`] that a page can switch off (a dialog is up over it): a disabled area takes no
/// touch and ends any coast; `configure` should switch the egui area's own scrolling off too.
pub(crate) fn scroll_area_if<R>(
    ui: &mut Ui,
    id_salt: impl std::hash::Hash,
    enabled: bool,
    configure: impl FnOnce(egui::ScrollArea) -> egui::ScrollArea,
    add: impl FnOnce(&mut Ui) -> R,
) -> (R, Reading) {
    let id = ui.id().with(id_salt);
    let state_id = id.with("momentum");
    let ctx = ui.ctx().clone();
    let mut st: AreaState = ctx.data_mut(|d| d.get_temp(state_id)).unwrap_or_default();
    let (now, dt) = ctx.input(|i| (i.time, i.stable_dt));
    let max = (st.content - st.view.map_or(0.0, |v| v.height())).max(0.0);
    let view_h = st.view.map_or(1.0, |v| v.height());

    if !enabled {
        st.momentum.stop();
    }
    if let Some(view) = st.view.filter(|_| enabled) {
        // The lowest-priority claim on the area: widgets drawn later win the press first.
        let response = ui.interact(view, id.with("drag"), Sense::drag());
        let pressed_here = ctx.input(|i| {
            i.pointer.any_pressed() && i.pointer.interact_pos().is_some_and(|p| view.contains(p))
        });
        if pressed_here {
            st.momentum.grab();
        }
        // A chart that a finger started on hands its vertical travel up instead of scrubbing.
        let from_chart = psoxide_debug_ui::forwarded_scroll_drag(&ctx);
        if response.dragged() || from_chart.is_some() {
            let dy = -from_chart.unwrap_or_else(|| ctx.input(|i| i.pointer.delta().y));
            st.momentum.drag(&mut st.pos, dy, now, max, view_h);
        } else if st.momentum.is_dragging() {
            st.momentum.release(st.pos, now, max);
        }
    }
    if st.momentum.step(&mut st.pos, dt, max, view_h) {
        ctx.request_repaint();
    }
    st.momentum.limit(&mut st.pos, max);

    let over = stretch(st.pos, max);
    let top_pad = (-over).max(0.0);
    let bottom_pad = over.max(0.0);
    let want = st.pos.max(0.0);
    let area = configure(
        egui::ScrollArea::vertical()
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded),
    )
    .id_salt(id)
    .drag_to_scroll(false)
    .vertical_scroll_offset(want);
    let out = area.show(ui, |ui| {
        ui.add_space(top_pad);
        let start = ui.cursor().top();
        let result = add(ui);
        let height = ui.cursor().top() - start;
        let right = ui.min_rect().right();
        ui.add_space(bottom_pad);
        (result, height, right)
    });
    let (result, height, right) = out.inner;
    st.content = height;
    st.content_right = right;
    let view = out.inner_rect;
    st.view = Some(view);
    // Something other than this module moved the area (the wheel, the scroll bar, a
    // scroll_to_rect): take its position and end any coast.
    if (out.state.offset.y - want).abs() > 0.5 && !st.momentum.is_dragging() {
        st.momentum.stop();
        st.pos = out.state.offset.y;
    }
    if enabled && st.momentum.is_moving() {
        cover(ui, view, id.with("cover"));
    }
    let reading = Reading {
        pos: st.pos,
        max,
        moving: st.momentum.is_active(),
        top: view.top(),
        bottom: view.bottom(),
        left: view.left(),
        right: view.right(),
        content_right: st.content_right,
    };
    ctx.data_mut(|d| d.insert_temp(state_id, st));
    (result, reading)
}

/// An invisible widget over `view` that takes presses while the content coasts.
fn cover(ui: &mut Ui, view: Rect, id: Id) {
    ui.interact(view, id, Sense::click_and_drag());
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: f64 = 1.0 / 60.0;

    /// Drag at a steady `speed` (points per second) for `frames` frames, then lift.
    fn flick(m: &mut Momentum, pos: &mut f32, speed: f32, frames: usize, max: f32) -> f64 {
        let mut t = 10.0;
        for _ in 0..frames {
            t += FRAME;
            m.drag(pos, speed * FRAME as f32, t, max, 400.0);
        }
        m.release(*pos, t + 0.004, max);
        t + 0.004
    }

    fn coast(m: &mut Momentum, pos: &mut f32, max: f32, seconds: f32) -> Vec<f32> {
        let mut trace = Vec::new();
        for _ in 0..(seconds * 60.0) as usize {
            m.step(pos, FRAME as f32, max, 400.0);
            trace.push(*pos);
        }
        trace
    }

    #[test]
    fn a_flick_coasts_on_after_the_finger_lifts_and_slows_to_a_stop() {
        let (mut m, mut pos) = (Momentum::default(), 100.0);
        flick(&mut m, &mut pos, 1500.0, 8, 100_000.0);
        let lifted = pos;
        assert!(m.is_moving(), "a fast release starts a coast");
        let trace = coast(&mut m, &mut pos, 100_000.0, 6.0);
        assert!(!m.is_moving(), "the coast ends");
        let steps: Vec<f32> = std::iter::once(lifted)
            .chain(trace)
            .collect::<Vec<_>>()
            .windows(2)
            .map(|w| w[1] - w[0])
            .collect();
        assert!(steps.iter().all(|s| *s >= 0.0), "never goes backwards");
        assert!(
            steps.windows(2).all(|w| w[1] <= w[0] + 0.001),
            "every frame moves less than the last"
        );
        // Travel is about speed / friction.
        let travelled = pos - lifted;
        assert!((travelled - 750.0).abs() < 80.0, "travelled {travelled}");
    }

    #[test]
    fn a_slow_release_does_not_coast() {
        let (mut m, mut pos) = (Momentum::default(), 100.0);
        flick(&mut m, &mut pos, 30.0, 8, 100_000.0);
        assert!(!m.is_moving());
    }

    #[test]
    fn a_finger_that_rested_before_lifting_leaves_no_speed() {
        let (mut m, mut pos) = (Momentum::default(), 100.0);
        for i in 0..8 {
            m.drag(&mut pos, 25.0, 5.0 + i as f64 * FRAME, 100_000.0, 400.0);
        }
        m.release(pos, 5.0 + 8.0 * FRAME + 0.2, 100_000.0);
        assert!(!m.is_moving());
    }

    #[test]
    fn a_finger_down_on_a_coasting_list_stops_it_and_says_so() {
        let (mut m, mut pos) = (Momentum::default(), 100.0);
        flick(&mut m, &mut pos, 1500.0, 8, 100_000.0);
        coast(&mut m, &mut pos, 100_000.0, 0.2);
        assert!(m.is_moving());
        let before = pos;
        assert!(m.grab(), "the touch is a stop, not a tap");
        assert!(!m.is_moving());
        coast(&mut m, &mut pos, 100_000.0, 1.0);
        assert_eq!(pos, before, "it stays where the finger caught it");
        assert!(!m.grab(), "a touch on a list at rest is a tap");
    }

    #[test]
    fn a_flick_into_the_end_stretches_and_springs_back_without_crossing() {
        let (mut m, mut pos) = (Momentum::default(), 950.0);
        let max = 1000.0;
        flick(&mut m, &mut pos, 3000.0, 1, max);
        assert!(m.is_moving() && pos <= max, "released in bounds at speed");
        let trace = coast(&mut m, &mut pos, max, 3.0);
        let peak = trace.iter().copied().fold(f32::MIN, f32::max);
        assert!(peak > max + 5.0, "it stretches past the end, peak {peak}");
        assert!(peak < max + 0.35 * 400.0 + 1.0, "but only a little: {peak}");
        let after_peak: Vec<f32> = trace.iter().copied().skip_while(|p| *p < peak).collect();
        assert!(
            after_peak.iter().all(|p| *p >= max - 0.01),
            "the spring never crosses the end"
        );
        assert_eq!(pos, max, "it settles exactly on the end");
        assert!(!m.is_moving());
    }

    #[test]
    fn dragging_past_the_end_gives_less_and_less_and_lifting_springs_back() {
        let (mut m, mut pos) = (Momentum::default(), 0.0);
        let mut gains = Vec::new();
        for i in 0..40 {
            let before = pos;
            m.drag(&mut pos, -10.0, 1.0 + i as f64 * FRAME, 1000.0, 400.0);
            gains.push(before - pos);
        }
        assert!(pos < -40.0, "it stretched: {pos}");
        assert!(
            gains.windows(2).all(|w| w[1] <= w[0] + 1e-4),
            "ever stiffer"
        );
        assert!(pos > -0.4 * 400.0, "never past the limit: {pos}");
        m.release(pos, 1.0 + 40.0 * FRAME + 0.004, 1000.0);
        assert!(m.is_moving());
        coast(&mut m, &mut pos, 1000.0, 2.0);
        assert_eq!(pos, 0.0);
    }

    #[test]
    fn dragging_back_in_from_a_stretch_follows_the_finger() {
        let (mut m, mut pos) = (Momentum::default(), -50.0);
        m.drag(&mut pos, 20.0, 1.0, 1000.0, 400.0);
        assert!((pos + 30.0).abs() < 1e-4, "{pos}");
    }

    #[test]
    fn the_wheel_moves_the_content_and_ends_a_coast() {
        let (mut m, mut pos) = (Momentum::default(), 100.0);
        flick(&mut m, &mut pos, 1500.0, 8, 100_000.0);
        let before = pos;
        m.wheel(&mut pos, 40.0, 100_000.0);
        assert_eq!(pos, before + 40.0);
        assert!(!m.is_moving());
        m.wheel(&mut pos, -1e9, 100_000.0);
        assert_eq!(pos, 0.0, "the wheel never stretches");
    }

    #[test]
    fn a_long_frame_cannot_throw_the_content_away() {
        let (mut m, mut pos) = (Momentum::default(), 500.0);
        flick(&mut m, &mut pos, 5000.0, 8, 2000.0);
        for _ in 0..4 {
            m.step(&mut pos, 5.0, 2000.0, 400.0);
        }
        assert!(
            pos.is_finite() && pos >= 0.0 && pos <= 2000.0 + 0.35 * 400.0,
            "{pos}"
        );
    }

    #[test]
    fn limit_clamps_only_what_nobody_is_moving() {
        let mut m = Momentum::default();
        let mut pos = -30.0;
        m.limit(&mut pos, 100.0);
        assert_eq!(pos, 0.0);
        let mut pos = 500.0;
        m.drag(&mut pos, 1.0, 1.0, 100.0, 400.0);
        let before = pos;
        m.limit(&mut pos, 100.0);
        assert_eq!(pos, before, "a stretch under the finger stays");
    }
}
