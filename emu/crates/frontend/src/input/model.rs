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

/// Left-stick deadzone for the digital D-pad proxy path.
pub(crate) const STICK_DEADZONE: f32 = 0.3;

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
/// `buttons` already contains the digital proxy of the left stick (adapters
/// that want it call [`add_stick_dpad`]), so a snapshot is complete: the menu
/// and the guest both read it as is. Sticks are in host orientation (positive
/// Y is up), `-1.0..=1.0`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct PadSnapshot {
    /// Held PlayStation buttons ([`emulator_core::button`] bits).
    pub(crate) buttons: u16,
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

/// Fold a stick into the D-pad bits (positive `y` is up).
pub(crate) fn add_stick_dpad(mask: &mut u16, x: f32, y: f32) {
    if y > STICK_DEADZONE {
        *mask |= button::UP;
    }
    if y < -STICK_DEADZONE {
        *mask |= button::DOWN;
    }
    if x < -STICK_DEADZONE {
        *mask |= button::LEFT;
    }
    if x > STICK_DEADZONE {
        *mask |= button::RIGHT;
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
    fn a_stick_pushed_past_the_deadzone_becomes_dpad_bits() {
        let mut mask = 0;
        add_stick_dpad(&mut mask, 0.2, -0.2);
        assert_eq!(mask, 0);
        add_stick_dpad(&mut mask, 0.9, -0.9);
        assert_eq!(mask, button::RIGHT | button::DOWN);
    }

    #[test]
    fn the_panel_line_names_held_buttons_and_pushed_sticks() {
        assert_eq!(PadSnapshot::default().describe(), "Idle (press a button)");
        let snapshot = PadSnapshot {
            buttons: button::CROSS | button::UP,
            left: (0.5, -1.0),
            right: (0.05, 0.0),
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
