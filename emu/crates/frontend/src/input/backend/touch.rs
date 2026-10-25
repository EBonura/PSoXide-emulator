//! The touchscreen as a device: the on-screen controller's state, reported
//! to the hub as an ordinary pad snapshot.

use egui::Pos2;

use crate::input::model::{BackendKind, DeviceEvent, DeviceIdentity, PadSnapshot, StickDpad};
use crate::input::touch::layout::{LayoutParams, Orientation, StickSide, TouchLayout};
use crate::input::touch::pad::{Phase, TouchPad, UiAction};
use crate::input::touch::prefs::{OverlayMode, TouchPrefs};

/// Stable id of the touchscreen in the routing table.
pub(crate) const TOUCH_DEVICE_ID: &str = "touch";
/// The mouse's id among the pointers, clear of every touch id.
const MOUSE_POINTER_ID: u64 = u64::MAX;
/// Idle time before the controller dims.
const DIM_AFTER_MS: u64 = 3500;
/// How far it dims (fraction of its normal opacity).
const DIM_LEVEL: f32 = 0.35;

/// Whether the controller should be on screen right now.
#[derive(Clone, Copy, Debug, Default)]
struct Presence {
    /// The browser says the primary pointer is a finger.
    coarse: bool,
    /// A touch has been seen.
    touch_seen: bool,
    last_touch_ms: u64,
    /// The last time a keyboard or gamepad did something.
    last_other_ms: Option<u64>,
}

impl Presence {
    fn visible(&self, mode: OverlayMode) -> bool {
        match mode {
            OverlayMode::On => true,
            OverlayMode::Off => false,
            OverlayMode::Auto => {
                (self.coarse || self.touch_seen)
                    && self
                        .last_other_ms
                        .is_none_or(|other| other < self.last_touch_ms)
            }
        }
    }
}

/// The touchscreen device.
pub(crate) struct TouchDevice {
    pad: TouchPad,
    prefs: TouchPrefs,
    presence: Presence,
    params: Option<LayoutParams>,
    stick_dpad: StickDpad,
    announced: bool,
    menu_open: bool,
    /// Only the shell is up (the home screen, no game behind it): the controller steps aside.
    shell_only: bool,
    /// The shell is up over a game: the controller keeps its place (the game keeps its panel)
    /// but its pause, save, debug, menu and fullscreen buttons step aside, so nothing of the
    /// controller overlaps the shell's header.
    shell_up: bool,
    /// The layout the pad holds was made with the buttons hidden for the shell.
    laid_for_shell: bool,
    /// Current fade, `DIM_LEVEL..=1`.
    fade: f32,
    now_ms: u64,
}

impl Default for TouchDevice {
    fn default() -> Self {
        Self {
            pad: TouchPad::new(),
            prefs: TouchPrefs::default(),
            presence: Presence::default(),
            params: None,
            stick_dpad: StickDpad::default(),
            announced: false,
            menu_open: false,
            shell_only: false,
            shell_up: false,
            laid_for_shell: false,
            fade: 1.0,
            now_ms: 0,
        }
    }
}

impl TouchDevice {
    /// The browser reports a coarse (finger) primary pointer.
    pub(crate) fn set_coarse_pointer(&mut self, coarse: bool) {
        self.presence.coarse = coarse;
    }

    /// Replace the preferences.
    pub(crate) fn set_prefs(&mut self, prefs: TouchPrefs) {
        self.prefs = prefs;
    }

    /// A keyboard or gamepad did something at `now_ms`.
    pub(crate) fn note_other_input(&mut self, now_ms: u64) {
        self.presence.last_other_ms = Some(now_ms);
    }

    /// Whether this looks like a phone or tablet: the browser says the primary pointer is a
    /// finger, or a touch has been seen. Such a device has no keyboard until one is used.
    pub(crate) fn touch_primary(&self) -> bool {
        self.presence.coarse || self.presence.touch_seen
    }

    /// Whether the controller is on screen.
    pub(crate) fn visible(&self) -> bool {
        self.presence.visible(self.prefs.mode)
    }

    /// Feed a touch from the window. Returns whether the controller owns it.
    pub(crate) fn handle_touch(&mut self, id: u64, phase: Phase, pos: Pos2, now_ms: u64) -> bool {
        if phase == Phase::Down {
            self.presence.touch_seen = true;
            self.presence.last_touch_ms = now_ms;
        }
        let owned = self.pad.pointer(id, phase, pos);
        if owned {
            self.presence.last_touch_ms = now_ms;
            self.fade = 1.0;
        }
        owned
    }

    /// Feed the mouse as one more pointer (`phase` Down on a press, Move while held, Up on the
    /// release). Unlike a touch it never counts as proof that this is a touch device. Returns
    /// whether the controller owns it.
    pub(crate) fn handle_mouse(&mut self, phase: Phase, pos: Pos2, now_ms: u64) -> bool {
        let owned = self.pad.pointer(MOUSE_POINTER_ID, phase, pos);
        if owned {
            self.presence.last_touch_ms = now_ms;
            self.fade = 1.0;
        }
        owned
    }

    /// Recompute the layout for this frame's window and guest state, and
    /// advance the fade. Releases everything when the controller hides or the
    /// window turns the other way.
    pub(crate) fn update_layout(&mut self, params: LayoutParams, now_ms: u64, dt: f32) {
        self.now_ms = now_ms;
        let visible = self.visible() && !self.shell_only;
        if !visible {
            self.params = None;
            self.pad.set_layout(None);
            return;
        }
        let params = LayoutParams {
            size_pct: self.prefs.size_pct,
            left_handed: self.prefs.left_handed,
            menu_open: self.menu_open,
            ..params
        };
        if self.params != Some(params) || self.laid_for_shell != self.shell_up {
            let turned = self
                .pad
                .layout()
                .is_some_and(|old| old.orientation != Orientation::from_size(params.size));
            if turned {
                self.pad.release_all();
            }
            let mut layout = TouchLayout::compute(&params);
            if self.shell_up {
                layout.ui.clear();
            }
            self.pad.set_layout(Some(layout));
            self.params = Some(params);
            self.laid_for_shell = self.shell_up;
        }
        let idle = now_ms.saturating_sub(self.presence.last_touch_ms) > DIM_AFTER_MS
            && !self.pad.is_touching();
        let target = if idle { DIM_LEVEL } else { 1.0 };
        let rate = if target > self.fade { 14.0 } else { 0.9 };
        self.fade += (target - self.fade) * (rate * dt).min(1.0);
    }

    /// Drop every held button (focus lost, tab hidden, rotation).
    pub(crate) fn release_all(&mut self) {
        self.pad.release_all();
    }

    /// Whether only the shell is on screen: the controller is not laid out at all, so the home
    /// screen shows its own header and nothing else.
    pub(crate) fn set_shell_only(&mut self, only: bool) {
        self.shell_only = only;
    }

    /// Whether the shell is up over a game: the controller's own buttons for the game (pause,
    /// save, debug, menu, fullscreen) are not laid out, and the shell shows its own.
    pub(crate) fn set_shell_up(&mut self, up: bool) {
        self.shell_up = up;
    }

    /// Whether the menu is up: the pad controls step aside for taps on it.
    pub(crate) fn set_menu_open(&mut self, open: bool) {
        self.menu_open = open;
        self.pad.set_pad_enabled(!open);
    }

    /// The layout, while visible.
    pub(crate) fn layout(&self) -> Option<&TouchLayout> {
        self.pad.layout()
    }

    /// Safe-area insets the layout was computed with, in window pixels.
    pub(crate) fn safe_px(&self) -> crate::input::touch::layout::Insets {
        self.params.map_or_else(Default::default, |p| p.safe)
    }

    /// The pad state, for drawing.
    pub(crate) fn pad(&self) -> &TouchPad {
        &self.pad
    }

    /// Opacity to draw with: the player's setting times the idle fade.
    pub(crate) fn alpha(&self) -> f32 {
        f32::from(self.prefs.opacity_pct) / 100.0 * self.fade
    }

    /// Taps on the menu / fullscreen buttons since the last call.
    pub(crate) fn take_actions(&mut self) -> Vec<UiAction> {
        self.pad.take_actions()
    }

    /// Whether a press happened that should tick the haptic motor.
    pub(crate) fn take_haptic(&mut self) -> bool {
        self.pad.take_fresh_press() && self.prefs.haptics
    }

    /// Append the device's events for this poll.
    pub(crate) fn poll(&mut self, out: &mut Vec<DeviceEvent>) {
        if !self.visible() {
            return;
        }
        if !self.announced {
            self.announced = true;
            out.push(DeviceEvent::Connected(DeviceIdentity {
                id: TOUCH_DEVICE_ID.to_string(),
                name: "Touchscreen".to_string(),
                backend: BackendKind::Touch,
                vid_pid: None,
                mapping: "on-screen controller".to_string(),
                mapped: true,
            }));
            // The first sample is neutral: a finger already down while the
            // device connects is a baseline, not a press, and the pad's
            // latch keeps it for the next poll.
            out.push(DeviceEvent::Sample {
                id: TOUCH_DEVICE_ID.to_string(),
                snapshot: PadSnapshot::default(),
            });
            return;
        }
        let left = self.pad.stick(StickSide::Left);
        let right = self.pad.stick(StickSide::Right);
        let snapshot = PadSnapshot {
            buttons: self.pad.take_buttons(),
            stick_dpad: self.stick_dpad.update(left.0, left.1),
            left,
            right,
            mode: false,
        };
        out.push(DeviceEvent::Sample {
            id: TOUCH_DEVICE_ID.to_string(),
            snapshot,
        });
    }
}
