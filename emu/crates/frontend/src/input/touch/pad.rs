//! The on-screen controller as a state machine: pointers in, buttons and
//! sticks out.
//!
//! Every finger is a pointer with a *grab* decided where it lands: the
//! D-pad, an analog stick, the field of buttons, or one of the player's own
//! interface buttons. A grab then follows the finger for its whole life, so a
//! thumb can slide across the D-pad or between face buttons without lifting,
//! and any number of fingers work at once.

use egui::{Pos2, Vec2};
use emulator_core::button;

use super::layout::{StickSide, TouchLayout, UiId};

/// D-pad travel (fraction of the grab radius) below which nothing presses.
const DPAD_DEAD: f32 = 0.16;
/// A direction presses when the finger's unit vector leans this far toward it.
const DPAD_PRESS: f32 = 0.40;
/// ...and stays pressed until it leans less than this.
const DPAD_RELEASE: f32 = 0.30;
/// Stick deflection below which the stick reads as centred.
const STICK_DEAD: f32 = 0.10;

/// What a pointer event is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    /// A finger touched down.
    Down,
    /// A finger moved.
    Move,
    /// A finger lifted.
    Up,
    /// The system took the finger away (gesture, notification, palm).
    Cancel,
}

/// A tap on one of the player's own buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UiAction {
    /// Pause or resume.
    Pause,
    /// Open the save-state sheet in Save mode.
    Save,
    /// Open the save-state sheet in Load mode.
    Load,
    /// Show or hide the debug panel.
    Debug,
    /// The menu button.
    Menu,
    /// The fullscreen button.
    Fullscreen,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Grab {
    Dpad,
    Stick(StickSide),
    Buttons,
    Ui(UiId),
    /// Landed on a control something else already holds: swallowed so it
    /// cannot click through to the interface underneath.
    Swallowed,
}

#[derive(Clone, Copy, Debug)]
struct Pointer {
    id: u64,
    grab: Grab,
    pos: Pos2,
}

#[derive(Clone, Copy, Debug, Default)]
struct StickState {
    /// Where the ring is centred while a finger holds it.
    center: Option<Pos2>,
    /// Deflection, `-1..=1`, screen orientation (y grows downward).
    deflection: Vec2,
}

/// The controller's live state.
#[derive(Debug, Default)]
pub(crate) struct TouchPad {
    layout: Option<TouchLayout>,
    pointers: Vec<Pointer>,
    dpad: u16,
    buttons: u16,
    latched: u16,
    sticks: [StickState; 2],
    pending: Vec<UiAction>,
    ui_down: Option<UiId>,
    pad_enabled: bool,
    fresh_press: bool,
}

fn stick_index(side: StickSide) -> usize {
    match side {
        StickSide::Left => 0,
        StickSide::Right => 1,
    }
}

impl TouchPad {
    /// A pad with no layout yet: it consumes nothing.
    pub(crate) fn new() -> Self {
        Self {
            pad_enabled: true,
            ..Self::default()
        }
    }

    /// Install a new layout. Fingers already down keep their grab, so a
    /// resize does not drop a held button; the caller releases everything on
    /// an orientation change.
    pub(crate) fn set_layout(&mut self, layout: Option<TouchLayout>) {
        if layout.is_none() {
            self.release_all();
        }
        self.layout = layout;
    }

    /// The current layout, when the controller is showing.
    pub(crate) fn layout(&self) -> Option<&TouchLayout> {
        self.layout.as_ref()
    }

    /// Whether the pad controls (not the interface buttons) take touches.
    /// Off while the emulator menu is open: the menu is tapped directly.
    pub(crate) fn set_pad_enabled(&mut self, enabled: bool) {
        if self.pad_enabled != enabled {
            self.pad_enabled = enabled;
            if !enabled {
                self.release_pad();
            }
        }
    }

    /// Let go of everything: focus lost, rotation, tab hidden.
    pub(crate) fn release_all(&mut self) {
        self.pointers.clear();
        self.ui_down = None;
        self.release_pad();
    }

    fn release_pad(&mut self) {
        self.pointers
            .retain(|pointer| matches!(pointer.grab, Grab::Ui(_) | Grab::Swallowed));
        self.dpad = 0;
        self.buttons = 0;
        self.sticks = [StickState::default(); 2];
    }

    /// Feed one pointer event. Returns whether the controller owns it (the
    /// interface underneath must not also see it).
    pub(crate) fn pointer(&mut self, id: u64, phase: Phase, pos: Pos2) -> bool {
        match phase {
            Phase::Down => self.down(id, pos),
            Phase::Move => self.moved(id, pos),
            Phase::Up => self.lift(id, pos, true),
            Phase::Cancel => self.lift(id, pos, false),
        }
    }

    fn down(&mut self, id: u64, pos: Pos2) -> bool {
        // A repeated Down for a pointer we already track is a lost Up.
        self.pointers.retain(|pointer| pointer.id != id);
        let Some(layout) = self.layout.as_ref() else {
            return false;
        };
        let grab = if let Some(ui) = layout.ui.iter().find(|ui| ui.hit.contains(pos)) {
            Some(Grab::Ui(ui.id))
        } else if !self.pad_enabled || layout.pad_hidden {
            None
        } else if let Some(stick) = layout.sticks.iter().find(|s| s.zone.contains(pos)) {
            Some(Grab::Stick(stick.side))
        } else if (pos - layout.dpad.center).length() <= layout.dpad.zone {
            Some(Grab::Dpad)
        } else if layout.buttons.iter().any(|b| b.hit.contains(pos)) {
            Some(Grab::Buttons)
        } else {
            None
        };
        let Some(mut grab) = grab else {
            return false;
        };
        // One finger per D-pad / stick / UI button.
        let taken = self
            .pointers
            .iter()
            .any(|pointer| match (pointer.grab, grab) {
                (Grab::Dpad, Grab::Dpad) => true,
                (Grab::Stick(a), Grab::Stick(b)) => a == b,
                (Grab::Ui(a), Grab::Ui(b)) => a == b,
                _ => false,
            });
        if taken {
            grab = Grab::Swallowed;
        }
        if let Grab::Stick(side) = grab {
            if let Some(plan) = self.layout.as_ref().and_then(|l| l.stick(side)) {
                // The ring plants where the thumb lands, kept far enough
                // inside the capture area that most of it stays on screen.
                let keep = (plan.zone.width() / 2.0 - 1.0)
                    .min(plan.ring * 0.5)
                    .max(0.0);
                let inner = plan.zone.shrink(keep);
                let center = Pos2::new(
                    pos.x.clamp(inner.min.x, inner.max.x),
                    pos.y.clamp(inner.min.y, inner.max.y),
                );
                self.sticks[stick_index(side)] = StickState {
                    center: Some(center),
                    deflection: Vec2::ZERO,
                };
            }
        }
        if let Grab::Ui(ui) = grab {
            self.ui_down = Some(ui);
            self.fresh_press = true;
        }
        self.pointers.push(Pointer { id, grab, pos });
        self.recompute();
        true
    }

    fn moved(&mut self, id: u64, pos: Pos2) -> bool {
        let Some(index) = self.pointers.iter().position(|pointer| pointer.id == id) else {
            return false;
        };
        self.pointers[index].pos = pos;
        if let Grab::Ui(ui) = self.pointers[index].grab {
            // Sliding off a menu / fullscreen button cancels the tap.
            let inside = self
                .layout
                .as_ref()
                .and_then(|l| l.ui.iter().find(|b| b.id == ui))
                .is_some_and(|b| b.hit.contains(pos));
            if !inside {
                self.pointers[index].grab = Grab::Swallowed;
                self.ui_down = None;
            }
        }
        self.recompute();
        true
    }

    fn lift(&mut self, id: u64, pos: Pos2, completed: bool) -> bool {
        let Some(index) = self.pointers.iter().position(|pointer| pointer.id == id) else {
            return false;
        };
        let pointer = self.pointers.remove(index);
        if let Grab::Ui(ui) = pointer.grab {
            self.ui_down = None;
            let inside = self
                .layout
                .as_ref()
                .and_then(|l| l.ui.iter().find(|b| b.id == ui))
                .is_some_and(|b| b.hit.contains(pos));
            if completed && inside {
                self.pending.push(match ui {
                    UiId::Pause => UiAction::Pause,
                    UiId::Save => UiAction::Save,
                    UiId::Load => UiAction::Load,
                    UiId::Debug => UiAction::Debug,
                    UiId::Menu => UiAction::Menu,
                    UiId::Fullscreen => UiAction::Fullscreen,
                });
            }
        }
        if let Grab::Stick(side) = pointer.grab {
            self.sticks[stick_index(side)] = StickState::default();
        }
        self.recompute();
        true
    }

    /// Recompute held buttons from the pointers.
    fn recompute(&mut self) {
        let Some(layout) = self.layout.as_ref() else {
            return;
        };
        let mut dpad = 0u16;
        let mut buttons = 0u16;
        for pointer in &self.pointers {
            match pointer.grab {
                Grab::Dpad => {
                    dpad |= dpad_bits(
                        pointer.pos - layout.dpad.center,
                        layout.dpad.zone,
                        self.dpad,
                    );
                }
                Grab::Buttons => {
                    for b in &layout.buttons {
                        if b.hit.contains(pointer.pos) {
                            buttons |= b.mask;
                        }
                    }
                }
                Grab::Stick(side) => {
                    let ring = layout.stick(side).map_or(1.0, |plan| plan.ring);
                    let state = &mut self.sticks[stick_index(side)];
                    if let Some(center) = state.center {
                        let delta = pointer.pos - center;
                        let length = delta.length();
                        // Dragging past the ring pulls the ring along, so the
                        // thumb never runs out of travel.
                        let center = if length > ring {
                            center + delta * ((length - ring) / length)
                        } else {
                            center
                        };
                        state.center = Some(center);
                        let raw = (pointer.pos - center) / ring;
                        state.deflection = clamp_unit(raw);
                    }
                }
                Grab::Ui(_) | Grab::Swallowed => {}
            }
        }
        let now = dpad | buttons;
        let before = self.dpad | self.buttons;
        let pressed = now & !before;
        if pressed != 0 {
            self.latched |= pressed;
            self.fresh_press = true;
        }
        self.dpad = dpad;
        self.buttons = buttons;
    }

    /// Buttons to report this poll: everything held, plus anything that went
    /// down and up again since the last poll. Clears the latch.
    pub(crate) fn take_buttons(&mut self) -> u16 {
        let held = self.dpad | self.buttons;
        let latched = std::mem::take(&mut self.latched);
        held | latched
    }

    /// Stick deflection in the host convention (`-1..=1`, positive Y up),
    /// with the dead zone applied.
    pub(crate) fn stick(&self, side: StickSide) -> (f32, f32) {
        let d = self.sticks[stick_index(side)].deflection;
        let length = d.length();
        if length < STICK_DEAD {
            return (0.0, 0.0);
        }
        let scaled = ((length - STICK_DEAD) / (1.0 - STICK_DEAD)).min(1.0) / length;
        (d.x * scaled, -d.y * scaled)
    }

    /// Where the ring of `side` is centred right now, when a finger holds it.
    pub(crate) fn stick_center(&self, side: StickSide) -> Option<Pos2> {
        self.sticks[stick_index(side)].center
    }

    /// Where the knob of `side` is drawn (the finger), when held.
    pub(crate) fn stick_knob(&self, side: StickSide) -> Option<Pos2> {
        let state = &self.sticks[stick_index(side)];
        let center = state.center?;
        let ring = self.layout.as_ref()?.stick(side)?.ring;
        Some(center + state.deflection * ring)
    }

    /// Buttons held right now (for drawing).
    pub(crate) fn held(&self) -> u16 {
        self.dpad | self.buttons
    }

    /// The interface button a finger is on, for drawing.
    pub(crate) fn ui_pressed(&self) -> Option<UiId> {
        self.ui_down
    }

    /// Interface taps since the last call.
    pub(crate) fn take_actions(&mut self) -> Vec<UiAction> {
        std::mem::take(&mut self.pending)
    }

    /// Whether a new press happened since the last call (for a haptic tick).
    pub(crate) fn take_fresh_press(&mut self) -> bool {
        std::mem::take(&mut self.fresh_press)
    }

    /// Whether any finger is down on the controller.
    pub(crate) fn is_touching(&self) -> bool {
        !self.pointers.is_empty()
    }
}

fn clamp_unit(v: Vec2) -> Vec2 {
    let length = v.length();
    if length > 1.0 {
        v / length
    } else {
        v
    }
}

/// D-pad bits for a finger at `offset` from the centre, with hysteresis
/// against the bits already `held`.
fn dpad_bits(offset: Vec2, zone: f32, held: u16) -> u16 {
    let length = offset.length();
    if length < zone * DPAD_DEAD {
        return 0;
    }
    let unit = offset / length;
    let mut bits = 0;
    for (bit, lean) in [
        (button::UP, -unit.y),
        (button::DOWN, unit.y),
        (button::LEFT, -unit.x),
        (button::RIGHT, unit.x),
    ] {
        let threshold = if held & bit != 0 {
            DPAD_RELEASE
        } else {
            DPAD_PRESS
        };
        if lean > threshold {
            bits |= bit;
        }
    }
    bits
}

#[cfg(test)]
mod tests {
    use egui::{pos2, vec2};

    use super::super::layout::tests::{params, DEVICES};
    use super::super::layout::{Glyph, Region};
    use super::*;

    fn pad(analog: bool) -> TouchPad {
        let mut pad = TouchPad::new();
        pad.set_layout(Some(TouchLayout::compute(&params(
            &DEVICES[1],
            true,
            analog,
        ))));
        pad
    }

    fn centre_of(pad: &TouchPad, mask: u16) -> Pos2 {
        pad.layout()
            .unwrap()
            .buttons
            .iter()
            .find(|b| b.mask == mask)
            .unwrap()
            .draw
            .center()
    }

    fn dpad_at(pad: &TouchPad, dx: f32, dy: f32) -> Pos2 {
        let d = &pad.layout().unwrap().dpad;
        d.center + vec2(dx, dy) * d.zone * 0.7
    }

    #[test]
    fn a_tap_on_a_button_presses_it_and_is_owned() {
        let mut pad = pad(false);
        let cross = centre_of(&pad, button::CROSS);
        assert!(pad.pointer(1, Phase::Down, cross));
        assert_eq!(pad.take_buttons(), button::CROSS);
        assert!(pad.pointer(1, Phase::Up, cross));
        assert_eq!(pad.take_buttons(), 0);
    }

    #[test]
    fn a_touch_on_empty_screen_is_not_owned() {
        let mut pad = pad(false);
        let empty = pos2(
            (pad.layout().unwrap().dpad.center.x + centre_of(&pad, button::CROSS).x) / 2.0,
            pad.layout().unwrap().dpad.center.y,
        );
        assert!(!pad.pointer(1, Phase::Down, empty));
        assert!(!pad.pointer(1, Phase::Move, empty + vec2(1.0, 1.0)));
        assert!(!pad.pointer(1, Phase::Up, empty + vec2(1.0, 1.0)));
        assert_eq!(pad.take_buttons(), 0);
    }

    #[test]
    fn a_tap_shorter_than_a_poll_still_reads_as_one_press() {
        let mut pad = pad(false);
        let circle = centre_of(&pad, button::CIRCLE);
        pad.pointer(1, Phase::Down, circle);
        pad.pointer(1, Phase::Up, circle);
        assert_eq!(pad.take_buttons(), button::CIRCLE);
        assert_eq!(pad.take_buttons(), 0);
    }

    #[test]
    fn dpad_and_face_buttons_work_at_the_same_time() {
        let mut pad = pad(false);
        let right = dpad_at(&pad, 1.0, 0.0);
        let cross = centre_of(&pad, button::CROSS);
        pad.pointer(1, Phase::Down, right);
        pad.pointer(2, Phase::Down, cross);
        assert_eq!(pad.take_buttons(), button::RIGHT | button::CROSS);
        pad.pointer(1, Phase::Up, right);
        assert_eq!(pad.take_buttons(), button::CROSS);
        pad.pointer(2, Phase::Up, cross);
        assert_eq!(pad.take_buttons(), 0);
    }

    #[test]
    fn sliding_across_the_dpad_changes_direction_without_lifting() {
        let mut pad = pad(false);
        pad.pointer(1, Phase::Down, dpad_at(&pad, 0.0, -1.0));
        assert_eq!(pad.take_buttons(), button::UP);
        pad.pointer(1, Phase::Move, dpad_at(&pad, 0.8, -0.8));
        assert_eq!(pad.take_buttons(), button::UP | button::RIGHT);
        pad.pointer(1, Phase::Move, dpad_at(&pad, 1.0, 0.0));
        assert_eq!(pad.take_buttons(), button::RIGHT);
        pad.pointer(1, Phase::Move, dpad_at(&pad, 0.0, 1.0));
        assert_eq!(pad.take_buttons(), button::DOWN);
        // Past the edge of the zone, the finger still steers the D-pad.
        let d = pad.layout().unwrap().dpad;
        pad.pointer(1, Phase::Move, d.center + vec2(-d.zone * 3.0, 0.0));
        assert_eq!(pad.take_buttons(), button::LEFT);
        // Back to the middle: nothing pressed.
        pad.pointer(1, Phase::Move, d.center);
        assert_eq!(pad.take_buttons(), 0);
    }

    #[test]
    fn a_direction_stays_pressed_a_little_past_where_it_started() {
        let mut pad = pad(false);
        let d = pad.layout().unwrap().dpad;
        let r = d.zone * 0.7;
        // Lean toward RIGHT at 0.45 (presses at 0.40), then ease to 0.34
        // (still above the release point 0.30).
        let at = |lean: f32| d.center + vec2(lean, -(1.0 - lean * lean).sqrt()) * r;
        pad.pointer(1, Phase::Down, at(0.45));
        assert_eq!(pad.take_buttons(), button::UP | button::RIGHT);
        pad.pointer(1, Phase::Move, at(0.34));
        assert_eq!(pad.take_buttons(), button::UP | button::RIGHT);
        pad.pointer(1, Phase::Move, at(0.25));
        assert_eq!(pad.take_buttons(), button::UP);
    }

    #[test]
    fn sliding_between_face_buttons_hands_the_press_over() {
        let mut pad = pad(false);
        let cross = centre_of(&pad, button::CROSS);
        let circle = centre_of(&pad, button::CIRCLE);
        pad.pointer(1, Phase::Down, cross);
        assert_eq!(pad.take_buttons(), button::CROSS);
        // Halfway between the two: both are under the thumb.
        pad.pointer(1, Phase::Move, cross.lerp(circle, 0.5));
        assert_eq!(pad.take_buttons(), button::CROSS | button::CIRCLE);
        pad.pointer(1, Phase::Move, circle);
        assert_eq!(pad.take_buttons(), button::CIRCLE);
        // Off the cluster: nothing pressed, but the finger is still tracked.
        pad.pointer(1, Phase::Move, pos2(circle.x, circle.y + 400.0));
        assert_eq!(pad.take_buttons(), 0);
        pad.pointer(1, Phase::Move, circle);
        assert_eq!(pad.take_buttons(), button::CIRCLE);
    }

    #[test]
    fn a_finger_that_started_off_the_controls_never_presses_anything() {
        let mut pad = pad(false);
        let cross = centre_of(&pad, button::CROSS);
        let empty = pos2(
            (pad.layout().unwrap().dpad.center.x + centre_of(&pad, button::CROSS).x) / 2.0,
            pad.layout().unwrap().dpad.center.y,
        );
        assert!(!pad.pointer(1, Phase::Down, empty));
        assert!(!pad.pointer(1, Phase::Move, cross));
        assert_eq!(pad.take_buttons(), 0);
    }

    #[test]
    fn lifting_several_fingers_together_releases_every_button() {
        let mut pad = pad(false);
        let spots = [
            (1, button::CROSS),
            (2, button::CIRCLE),
            (3, button::L1),
            (4, button::START),
        ];
        for (id, mask) in spots {
            pad.pointer(id, Phase::Down, centre_of(&pad, mask));
        }
        assert_eq!(
            pad.take_buttons(),
            button::CROSS | button::CIRCLE | button::L1 | button::START
        );
        for (id, mask) in spots {
            pad.pointer(id, Phase::Up, centre_of(&pad, mask));
        }
        assert_eq!(pad.take_buttons(), 0);
        assert!(!pad.is_touching());
    }

    #[test]
    fn a_cancelled_finger_releases_its_buttons() {
        let mut pad = pad(false);
        let cross = centre_of(&pad, button::CROSS);
        pad.pointer(7, Phase::Down, cross);
        pad.take_buttons();
        assert!(pad.pointer(7, Phase::Cancel, cross));
        assert_eq!(pad.take_buttons(), 0);
        assert_eq!(pad.held(), 0);
    }

    #[test]
    fn release_all_drops_everything_including_sticks() {
        let mut pad = pad(true);
        let stick = pad.layout().unwrap().stick(StickSide::Left).unwrap().rest;
        pad.pointer(1, Phase::Down, stick);
        let ring = pad.layout().unwrap().sticks[0].ring;
        pad.pointer(1, Phase::Move, stick + vec2(ring * 2.0, 0.0));
        pad.pointer(2, Phase::Down, centre_of(&pad, button::CROSS));
        assert!(pad.stick(StickSide::Left).0 > 0.5);
        pad.release_all();
        pad.take_buttons();
        assert_eq!(pad.take_buttons(), 0);
        assert_eq!(pad.stick(StickSide::Left), (0.0, 0.0));
        assert!(!pad.is_touching());
    }

    #[test]
    fn a_lost_up_is_repaired_by_the_next_down_of_the_same_finger() {
        let mut pad = pad(false);
        let cross = centre_of(&pad, button::CROSS);
        pad.pointer(1, Phase::Down, cross);
        pad.pointer(1, Phase::Down, centre_of(&pad, button::CIRCLE));
        assert_eq!(pad.held(), button::CIRCLE);
    }

    #[test]
    fn the_stick_centres_where_the_thumb_lands_and_reads_the_drag() {
        let mut pad = pad(true);
        let plan = *pad.layout().unwrap().stick(StickSide::Left).unwrap();
        let land = plan.rest + vec2(10.0, -8.0);
        assert!(pad.pointer(1, Phase::Down, land));
        // Landing alone is neutral, wherever in the zone it happened.
        assert_eq!(pad.stick(StickSide::Left), (0.0, 0.0));
        assert_eq!(pad.stick_center(StickSide::Left), Some(land));
        // A full-ring push right and a half-ring push up.
        pad.pointer(1, Phase::Move, land + vec2(plan.ring, 0.0));
        let (x, y) = pad.stick(StickSide::Left);
        assert!((x - 1.0).abs() < 1e-4 && y.abs() < 1e-4, "{x} {y}");
        pad.pointer(1, Phase::Move, land + vec2(0.0, -plan.ring * 0.5));
        let (x, y) = pad.stick(StickSide::Left);
        assert!(x.abs() < 1e-4 && y > 0.3 && y < 0.5, "{x} {y}");
        pad.pointer(1, Phase::Up, land);
        assert_eq!(pad.stick(StickSide::Left), (0.0, 0.0));
        assert_eq!(pad.stick_center(StickSide::Left), None);
    }

    #[test]
    fn dragging_past_the_ring_carries_the_ring_along() {
        let mut pad = pad(true);
        let plan = *pad.layout().unwrap().stick(StickSide::Right).unwrap();
        pad.pointer(1, Phase::Down, plan.rest);
        pad.pointer(1, Phase::Move, plan.rest + vec2(plan.ring * 3.0, 0.0));
        let (x, _) = pad.stick(StickSide::Right);
        assert!((x - 1.0).abs() < 1e-4);
        // Reversing needs only the ring's radius, not three rings of travel.
        pad.pointer(
            1,
            Phase::Move,
            plan.rest + vec2(plan.ring * 2.0 - plan.ring * 0.5, 0.0),
        );
        let (x, _) = pad.stick(StickSide::Right);
        assert!(x < 0.2, "{x}");
    }

    #[test]
    fn both_sticks_and_the_dpad_can_be_held_by_three_fingers() {
        let mut pad = pad(true);
        let l = pad.layout().unwrap().stick(StickSide::Left).unwrap().rest;
        let r = pad.layout().unwrap().stick(StickSide::Right).unwrap().rest;
        pad.pointer(1, Phase::Down, l);
        pad.pointer(2, Phase::Down, r);
        pad.pointer(3, Phase::Down, dpad_at(&pad, 0.0, 1.0));
        let ring = pad.layout().unwrap().sticks[0].ring;
        pad.pointer(1, Phase::Move, l + vec2(ring * 2.0, 0.0));
        pad.pointer(2, Phase::Move, r + vec2(0.0, ring * 2.0));
        assert!(pad.stick(StickSide::Left).0 > 0.9);
        assert!(pad.stick(StickSide::Right).1 < -0.9);
        assert_eq!(pad.take_buttons(), button::DOWN);
    }

    #[test]
    fn a_second_finger_on_a_taken_stick_is_swallowed_not_clicked_through() {
        let mut pad = pad(true);
        let rest = pad.layout().unwrap().stick(StickSide::Left).unwrap().rest;
        assert!(pad.pointer(1, Phase::Down, rest));
        assert!(pad.pointer(2, Phase::Down, rest + vec2(5.0, 5.0)));
        let ring = pad.layout().unwrap().sticks[0].ring;
        pad.pointer(2, Phase::Move, rest + vec2(ring * 0.6, 0.0));
        assert_eq!(pad.stick(StickSide::Left), (0.0, 0.0));
        pad.pointer(2, Phase::Up, rest);
        pad.pointer(1, Phase::Move, rest + vec2(ring * 2.0, 0.0));
        assert!(pad.stick(StickSide::Left).0 > 0.9);
    }

    #[test]
    fn there_are_no_stick_grabs_while_the_guest_pad_is_digital() {
        let mut pad = pad(false);
        // The place a stick would be is plain screen (not owned) when digital.
        let layout = TouchLayout::compute(&params(&DEVICES[1], true, true));
        let rest = layout.stick(StickSide::Left).unwrap().rest;
        assert!(!pad.pointer(1, Phase::Down, rest));
    }

    #[test]
    fn menu_and_fullscreen_buttons_fire_on_release_inside_only() {
        let mut pad = pad(false);
        let menu = pad
            .layout()
            .unwrap()
            .ui
            .iter()
            .find(|b| b.id == UiId::Menu)
            .unwrap()
            .draw
            .center();
        assert!(pad.pointer(1, Phase::Down, menu));
        assert_eq!(pad.ui_pressed(), Some(UiId::Menu));
        assert!(pad.take_actions().is_empty());
        pad.pointer(1, Phase::Up, menu);
        assert_eq!(pad.take_actions(), vec![UiAction::Menu]);
        // Sliding off cancels.
        pad.pointer(2, Phase::Down, menu);
        pad.pointer(2, Phase::Move, menu + vec2(0.0, 300.0));
        pad.pointer(2, Phase::Up, menu + vec2(0.0, 300.0));
        assert!(pad.take_actions().is_empty());
        // A system cancel never fires the action.
        pad.pointer(3, Phase::Down, menu);
        pad.pointer(3, Phase::Cancel, menu);
        assert!(pad.take_actions().is_empty());
        assert_eq!(pad.ui_pressed(), None);
    }

    #[test]
    fn with_the_pad_disabled_only_the_interface_buttons_take_touches() {
        let mut pad = pad(true);
        pad.pointer(1, Phase::Down, centre_of(&pad, button::CROSS));
        assert_eq!(pad.held(), button::CROSS);
        pad.set_pad_enabled(false);
        assert_eq!(pad.held(), 0);
        assert!(!pad.pointer(2, Phase::Down, centre_of(&pad, button::CROSS)));
        let menu = pad.layout().unwrap().ui[0].draw.center();
        assert!(pad.pointer(3, Phase::Down, menu));
        pad.set_pad_enabled(true);
        assert!(pad.pointer(4, Phase::Down, centre_of(&pad, button::CROSS)));
    }

    #[test]
    fn a_fresh_press_is_reported_once_for_the_haptic_tick() {
        let mut pad = pad(false);
        let cross = centre_of(&pad, button::CROSS);
        assert!(!pad.take_fresh_press());
        pad.pointer(1, Phase::Down, cross);
        assert!(pad.take_fresh_press());
        assert!(!pad.take_fresh_press());
        pad.pointer(1, Phase::Move, cross + vec2(1.0, 1.0));
        assert!(!pad.take_fresh_press(), "no new button, no tick");
        pad.pointer(1, Phase::Move, centre_of(&pad, button::CIRCLE));
        assert!(pad.take_fresh_press(), "sliding onto a new button ticks");
    }

    #[test]
    fn hit_testing_matches_the_layout_for_every_button() {
        let mut pad = pad(true);
        let buttons: Vec<_> = pad.layout().unwrap().buttons.clone();
        for b in buttons {
            // Dead centre of the drawn shape presses exactly that button
            // (face buttons also share overlap, but not at their centres).
            let c = b.draw.center();
            pad.release_all();
            pad.take_buttons();
            assert!(pad.pointer(9, Phase::Down, c), "{:?}", b.glyph);
            assert_eq!(pad.held(), b.mask, "{:?}", b.glyph);
            // Just outside the (larger) hit area presses nothing.
            let outside = match b.hit {
                Region::Circle { center, radius } => center + vec2(radius + 12.0, 0.0),
                Region::Rect(r) => pos2(r.center().x, r.max.y + 12.0),
            };
            pad.release_all();
            pad.take_buttons();
            pad.pointer(9, Phase::Down, outside);
            assert_eq!(pad.held() & b.mask, 0, "{:?}", b.glyph);
            let _ = matches!(b.glyph, Glyph::Label(_));
        }
    }
}
