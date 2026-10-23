//! Gamepads through gilrs (HID on macOS, evdev on Linux, XInput on Windows).

use std::collections::HashMap;

use emulator_core::button;
use gilrs::{Axis, Button, Event, EventType, Gamepad, GamepadId, Gilrs};

use crate::input::model::{BackendKind, DeviceEvent, DeviceIdentity, PadSnapshot, StickDpad};

use super::DeviceBackend;

/// Analog trigger activation threshold.
const TRIGGER_THRESHOLD: f32 = 0.5;

/// The gilrs adapter.
pub(crate) struct GilrsBackend {
    gilrs: Option<Gilrs>,
    /// Router id of every pad announced so far.
    pads: HashMap<GamepadId, (String, StickDpad)>,
}

impl GilrsBackend {
    /// Start gilrs; without it the backend simply reports nothing.
    pub(crate) fn new() -> Self {
        let gilrs = match Gilrs::new() {
            Ok(gilrs) => Some(gilrs),
            Err(error) => {
                eprintln!("[input] gilrs init failed: {error}");
                None
            }
        };
        Self {
            gilrs,
            pads: HashMap::new(),
        }
    }

    /// Pads already attached when gilrs started (most platforms report them
    /// as hot-plug events from the first poll instead).
    fn announce(&mut self, id: GamepadId, out: &mut Vec<DeviceEvent>) {
        let Some(gilrs) = self.gilrs.as_ref() else {
            return;
        };
        let gamepad = gilrs.gamepad(id);
        let route_id = format!("gilrs:{id}");
        self.pads
            .insert(id, (route_id.clone(), StickDpad::default()));
        out.push(DeviceEvent::Connected(identity(&route_id, &gamepad)));
    }
}

impl DeviceBackend for GilrsBackend {
    fn poll(&mut self, out: &mut Vec<DeviceEvent>) {
        let Some(gilrs) = self.gilrs.as_mut() else {
            return;
        };
        let mut connected = Vec::new();
        let mut disconnected = Vec::new();
        while let Some(Event { id, event, .. }) = gilrs.next_event() {
            match event {
                EventType::Connected => connected.push(id),
                EventType::Disconnected => disconnected.push(id),
                _ => {}
            }
        }
        // gilrs may also hold pads that never produced an event yet.
        if self.pads.is_empty() && connected.is_empty() {
            connected.extend(
                gilrs
                    .gamepads()
                    .filter(|(_, pad)| pad.is_connected())
                    .map(|(id, _)| id),
            );
        }
        for id in disconnected {
            if let Some((route_id, _)) = self.pads.remove(&id) {
                out.push(DeviceEvent::Disconnected(route_id));
            }
        }
        for id in connected {
            if !self.pads.contains_key(&id) {
                self.announce(id, out);
            }
        }
        let Some(gilrs) = self.gilrs.as_ref() else {
            return;
        };
        for (id, (route_id, stick)) in &mut self.pads {
            let gamepad = gilrs.gamepad(*id);
            if gamepad.is_connected() {
                out.push(DeviceEvent::Sample {
                    id: route_id.clone(),
                    snapshot: snapshot(&gamepad, stick),
                });
            }
        }
    }
}

fn identity(route_id: &str, gamepad: &Gamepad<'_>) -> DeviceIdentity {
    let mapped = gamepad.mapping_source() != gilrs::MappingSource::None;
    let mapping = if mapped {
        "SDL mapping"
    } else {
        "raw HID, no mapping"
    };
    let vid_pid = gamepad.vendor_id().zip(gamepad.product_id());
    DeviceIdentity {
        id: route_id.to_string(),
        name: gamepad.name().to_string(),
        backend: BackendKind::Gilrs,
        vid_pid,
        mapping: format!("{mapping}; os name {:?}", gamepad.os_name()),
        mapped,
    }
}

fn snapshot(gamepad: &Gamepad<'_>, stick: &mut StickDpad) -> PadSnapshot {
    let mut buttons = 0;
    for (pressed, bit) in [
        (gamepad.is_pressed(Button::South), button::CROSS),
        (gamepad.is_pressed(Button::East), button::CIRCLE),
        (gamepad.is_pressed(Button::West), button::SQUARE),
        (gamepad.is_pressed(Button::North), button::TRIANGLE),
        (gamepad.is_pressed(Button::LeftTrigger), button::L1),
        (gamepad.is_pressed(Button::RightTrigger), button::R1),
        (
            gamepad.is_pressed(Button::LeftTrigger2)
                || gamepad.value(Axis::LeftZ) > TRIGGER_THRESHOLD,
            button::L2,
        ),
        (
            gamepad.is_pressed(Button::RightTrigger2)
                || gamepad.value(Axis::RightZ) > TRIGGER_THRESHOLD,
            button::R2,
        ),
        (gamepad.is_pressed(Button::LeftThumb), button::L3),
        (gamepad.is_pressed(Button::RightThumb), button::R3),
        (gamepad.is_pressed(Button::DPadUp), button::UP),
        (gamepad.is_pressed(Button::DPadDown), button::DOWN),
        (gamepad.is_pressed(Button::DPadLeft), button::LEFT),
        (gamepad.is_pressed(Button::DPadRight), button::RIGHT),
        (gamepad.is_pressed(Button::Select), button::SELECT),
        (gamepad.is_pressed(Button::Start), button::START),
    ] {
        if pressed {
            buttons |= bit;
        }
    }
    let left = (
        gamepad.value(Axis::LeftStickX),
        gamepad.value(Axis::LeftStickY),
    );
    let right = (
        gamepad.value(Axis::RightStickX),
        gamepad.value(Axis::RightStickY),
    );
    PadSnapshot {
        buttons,
        stick_dpad: stick.update(left.0, left.1),
        left,
        right,
        mode: gamepad.is_pressed(Button::Mode),
    }
}
