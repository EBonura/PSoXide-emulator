//! The one logical input model.
//!
//! Every host input source (the keyboard, a gilrs gamepad, a macOS
//! GameController device, a browser Gamepad) is only an *adapter*: it turns
//! raw device state into a [`PadSnapshot`] in the PlayStation pad layout and
//! announces the device with a [`DeviceIdentity`]. Nothing downstream (menu
//! navigation, hold-to-repeat, the held-at-open baseline, the noise guard,
//! port assignment, the guest feed) ever learns which kind of device a
//! snapshot came from, so it is impossible for one kind to work while
//! another does not.

use emulator_core::button;

/// Left-stick travel that starts counting as a D-pad press.
pub(crate) const STICK_DPAD_PRESS: f32 = 0.5;
/// Left-stick travel below which a held D-pad proxy lets go. Lower than
/// [`STICK_DPAD_PRESS`] so a stick resting near the threshold cannot flutter.
pub(crate) const STICK_DPAD_RELEASE: f32 = 0.35;

/// Which adapter produced a device. Informational only: logs and the
/// controller panel show it, no logic branches on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BackendKind {
    /// The host keyboard.
    Keyboard,
    /// A gamepad read through gilrs (HID / evdev / XInput).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    Gilrs,
    /// A controller read through Apple's GameController framework.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    GameController,
    /// A browser Gamepad API pad.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    Web,
}

impl BackendKind {
    /// Short label for logs and the controller panel.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Keyboard => "keyboard",
            Self::Gilrs => "gilrs",
            Self::GameController => "GameController",
            Self::Web => "web",
        }
    }
}

/// Who a device is: stable id within a session, display name, and where it
/// came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeviceIdentity {
    /// Unique within the session; the routing panel keys on it.
    pub(crate) id: String,
    /// Host-provided display name.
    pub(crate) name: String,
    /// The adapter that reads it.
    pub(crate) backend: BackendKind,
    /// USB vendor and product id when the platform exposes them.
    pub(crate) vid_pid: Option<(u16, u16)>,
    /// How the adapter interprets the device ("SDL mapping", "standard", ...).
    pub(crate) mapping: String,
    /// Whether the adapter knows this device's button layout. A raw HID pad
    /// without a mapping is listed (so it is visible) but its buttons mean
    /// nothing.
    pub(crate) mapped: bool,
}

impl DeviceIdentity {
    /// `name [backend, vid:pid, mapping]` for logs.
    pub(crate) fn describe(&self) -> String {
        let vid_pid = match self.vid_pid {
            Some((vid, pid)) => format!("{vid:04x}:{pid:04x}"),
            None => "no vid:pid".to_string(),
        };
        format!(
            "{} [{}, {}, {}]",
            self.name,
            self.backend.label(),
            vid_pid,
            self.mapping
        )
    }
}

/// What a device holds right now, in the PS1 pad layout.
///
/// `buttons` are the physical buttons only. `stick_dpad` is the digital proxy
/// of the left stick (adapters of devices with a real stick fill it with a
/// [`StickDpad`]): the menu and the shortcuts read it as D-pad presses, and
/// the guest reads it only while its pad is in digital mode, because in analog
/// mode the stick is an axis and must not also press the D-pad. Sticks are in
/// host orientation (positive Y is up), `-1.0..=1.0`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct PadSnapshot {
    /// Held PlayStation buttons ([`emulator_core::button`] bits).
    pub(crate) buttons: u16,
    /// D-pad bits derived from the left stick (see the type docs).
    pub(crate) stick_dpad: u16,
    /// Left stick.
    pub(crate) left: (f32, f32),
    /// Right stick.
    pub(crate) right: (f32, f32),
    /// The device's Analog / Mode / Home button is down.
    pub(crate) mode: bool,
}

/// Button names in the order the controller panel lists them.
const BUTTON_NAMES: [(u16, &str); 16] = [
    (button::UP, "Up"),
    (button::DOWN, "Down"),
    (button::LEFT, "Left"),
    (button::RIGHT, "Right"),
    (button::CROSS, "Cross"),
    (button::CIRCLE, "Circle"),
    (button::SQUARE, "Square"),
    (button::TRIANGLE, "Triangle"),
    (button::L1, "L1"),
    (button::R1, "R1"),
    (button::L2, "L2"),
    (button::R2, "R2"),
    (button::L3, "L3"),
    (button::R3, "R3"),
    (button::START, "Start"),
    (button::SELECT, "Select"),
];

impl PadSnapshot {
    /// One line for the controller panel: the held buttons and any stick
    /// pushed off centre, or "Idle (press a button)".
    pub(crate) fn describe(&self) -> String {
        let mut parts: Vec<String> = BUTTON_NAMES
            .iter()
            .filter(|(bit, _)| self.buttons & bit != 0)
            .map(|(_, name)| (*name).to_string())
            .collect();
        for (label, (x, y)) in [("L", self.left), ("R", self.right)] {
            if x.abs() > 0.15 || y.abs() > 0.15 {
                parts.push(format!("{label}({x:+.2},{y:+.2})"));
            }
        }
        if parts.is_empty() {
            "Idle (press a button)".to_string()
        } else {
            parts.join(" ")
        }
    }

    /// Whether the device shows any deliberate input: a button or a stick
    /// pushed clearly off centre. Used to decide when a device "wakes up".
    pub(crate) fn is_active(&self) -> bool {
        let far = |(x, y): (f32, f32)| x.abs() > 0.5 || y.abs() > 0.5;
        self.buttons != 0 || far(self.left) || far(self.right)
    }
}

/// What an adapter reports to the hub.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DeviceEvent {
    /// A device appeared (or reappeared after unplugging).
    Connected(DeviceIdentity),
    /// A device went away.
    Disconnected(String),
    /// The device's current state. Adapters send one per poll for every
    /// connected device; the hub derives all edges itself.
    Sample {
        /// Device id from [`DeviceIdentity::id`].
        id: String,
        /// Current state.
        snapshot: PadSnapshot,
    },
}

/// Turns a stick into D-pad bits with hysteresis: a direction presses at
/// [`STICK_DPAD_PRESS`] travel and lets go below [`STICK_DPAD_RELEASE`].
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct StickDpad {
    held: u16,
}

impl StickDpad {
    /// The D-pad bits for a stick at (`x`, `y`) (positive `y` is up).
    pub(crate) fn update(&mut self, x: f32, y: f32) -> u16 {
        for (bit, value) in [
            (button::UP, y),
            (button::DOWN, -y),
            (button::LEFT, -x),
            (button::RIGHT, x),
        ] {
            let threshold = if self.held & bit != 0 {
                STICK_DPAD_RELEASE
            } else {
                STICK_DPAD_PRESS
            };
            if value > threshold {
                self.held |= bit;
            } else {
                self.held &= !bit;
            }
        }
        self.held
    }
}

/// Bits that appear in `now` but not in `previous`.
pub(crate) fn rising_edges(previous: u16, now: u16) -> u16 {
    now & !previous
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rising_edges_reports_only_new_presses() {
        assert_eq!(
            rising_edges(0, button::LEFT | button::CROSS),
            button::LEFT | button::CROSS
        );
        assert_eq!(rising_edges(button::LEFT, button::LEFT), 0);
        assert_eq!(rising_edges(button::LEFT, 0), 0);
        assert_eq!(
            rising_edges(button::LEFT, button::LEFT | button::RIGHT),
            button::RIGHT
        );
    }

    #[test]
    fn a_stick_becomes_dpad_bits_only_when_pushed_firmly() {
        let mut stick = StickDpad::default();
        assert_eq!(stick.update(0.2, -0.2), 0);
        // A drifting or lightly held stick (-0.36 was enough before) is not a press.
        assert_eq!(stick.update(-0.4, 0.0), 0);
        assert_eq!(stick.update(0.9, -0.9), button::RIGHT | button::DOWN);
    }

    #[test]
    fn a_held_stick_direction_has_hysteresis() {
        let mut stick = StickDpad::default();
        assert_eq!(stick.update(0.0, 0.6), button::UP);
        // Easing off but still past the release point keeps it held...
        assert_eq!(stick.update(0.0, 0.4), button::UP);
        // ...and only letting go below it releases, and it must be pushed
        // firmly again to press.
        assert_eq!(stick.update(0.0, 0.3), 0);
        assert_eq!(stick.update(0.0, 0.4), 0);
        assert_eq!(stick.update(0.0, 0.55), button::UP);
        // Swinging through the middle changes direction cleanly.
        assert_eq!(stick.update(0.0, -0.8), button::DOWN);
    }

    #[test]
    fn the_panel_line_names_held_buttons_and_pushed_sticks() {
        assert_eq!(PadSnapshot::default().describe(), "Idle (press a button)");
        let snapshot = PadSnapshot {
            buttons: button::CROSS | button::UP,
            left: (0.5, -1.0),
            right: (0.05, 0.0),
            stick_dpad: 0,
            mode: false,
        };
        assert_eq!(snapshot.describe(), "Up Cross L(+0.50,-1.00)");
    }

    #[test]
    fn a_snapshot_is_active_for_buttons_or_a_clearly_pushed_stick() {
        assert!(!PadSnapshot::default().is_active());
        let drift = PadSnapshot {
            left: (0.2, -0.3),
            ..PadSnapshot::default()
        };
        assert!(!drift.is_active());
        let pushed = PadSnapshot {
            right: (0.0, 0.9),
            ..PadSnapshot::default()
        };
        assert!(pushed.is_active());
        let pressed = PadSnapshot {
            buttons: button::CROSS,
            ..PadSnapshot::default()
        };
        assert!(pressed.is_active());
    }
}
