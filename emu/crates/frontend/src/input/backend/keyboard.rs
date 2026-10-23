//! The keyboard as a device.
//!
//! The shell turns key events into the held PS1 buttons and emulated sticks
//! (using the user's bindings) and hands them over once per frame. Presses
//! are also latched, so a tap shorter than one frame still registers as a
//! press, exactly like a pad button would for a polled device.

use crate::input::model::{DeviceEvent, PadSnapshot};
use crate::input::routing::KEYBOARD_DEVICE_ID;

use super::DeviceBackend;

/// The keyboard adapter.
#[derive(Debug, Default)]
pub(crate) struct KeyboardDevice {
    held: PadSnapshot,
    /// Buttons pressed since the last poll, even if already released.
    tapped: u16,
    /// The Analog key was pressed since the last poll.
    mode_tapped: bool,
}

impl KeyboardDevice {
    /// Replace the held state (called every frame by the shell).
    pub(crate) fn set_held(&mut self, held: PadSnapshot) {
        self.held = held;
    }

    /// A bound key went down: latch it until the next poll.
    pub(crate) fn note_press(&mut self, buttons: u16, mode: bool) {
        self.tapped |= buttons;
        self.mode_tapped |= mode;
    }
}

impl DeviceBackend for KeyboardDevice {
    fn poll(&mut self, out: &mut Vec<DeviceEvent>) {
        let snapshot = PadSnapshot {
            buttons: self.held.buttons | self.tapped,
            mode: self.held.mode | self.mode_tapped,
            ..self.held
        };
        self.tapped = 0;
        self.mode_tapped = false;
        out.push(DeviceEvent::Sample {
            id: KEYBOARD_DEVICE_ID.to_string(),
            snapshot,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use emulator_core::button;

    fn poll(device: &mut KeyboardDevice) -> PadSnapshot {
        let mut out = Vec::new();
        device.poll(&mut out);
        match out.pop() {
            Some(DeviceEvent::Sample { snapshot, .. }) => snapshot,
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_tap_shorter_than_a_frame_is_still_seen_once() {
        let mut keyboard = KeyboardDevice::default();
        keyboard.note_press(button::DOWN, false);
        // Released again before the shell polled.
        keyboard.set_held(PadSnapshot::default());
        assert_eq!(poll(&mut keyboard).buttons, button::DOWN);
        assert_eq!(poll(&mut keyboard).buttons, 0);
    }

    #[test]
    fn held_keys_stay_down_and_the_analog_key_is_latched() {
        let mut keyboard = KeyboardDevice::default();
        keyboard.set_held(PadSnapshot {
            buttons: button::LEFT,
            left: (-1.0, 0.0),
            ..PadSnapshot::default()
        });
        keyboard.note_press(0, true);
        let first = poll(&mut keyboard);
        assert_eq!(first.buttons, button::LEFT);
        assert_eq!(first.left, (-1.0, 0.0));
        assert!(first.mode);
        let second = poll(&mut keyboard);
        assert_eq!(second.buttons, button::LEFT);
        assert!(!second.mode);
    }
}
