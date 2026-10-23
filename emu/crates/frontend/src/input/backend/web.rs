//! Browser Gamepad API pads.

use std::collections::BTreeMap;

use emulator_core::button;

use crate::input::model::{BackendKind, DeviceEvent, DeviceIdentity, PadSnapshot, StickDpad};

use super::DeviceBackend;

/// The Web Gamepad adapter ("standard" mapping).
#[derive(Default)]
pub(crate) struct WebPadBackend {
    /// Announced pads by browser slot index.
    known: BTreeMap<u32, (String, StickDpad)>,
}

impl DeviceBackend for WebPadBackend {
    fn poll(&mut self, out: &mut Vec<DeviceEvent>) {
        let connected = connected_web_gamepads();

        let removed: Vec<u32> = self
            .known
            .keys()
            .filter(|index| !connected.contains_key(index))
            .copied()
            .collect();
        for index in removed {
            if let Some((id, _)) = self.known.remove(&index) {
                out.push(DeviceEvent::Disconnected(id));
            }
        }
        for (index, gamepad) in &connected {
            let id = match self.known.get(index) {
                Some((id, _)) => id.clone(),
                None => {
                    let id = format!("web:{index}");
                    self.known
                        .insert(*index, (id.clone(), StickDpad::default()));
                    out.push(DeviceEvent::Connected(DeviceIdentity {
                        id: id.clone(),
                        name: gamepad.id(),
                        backend: BackendKind::Web,
                        vid_pid: None,
                        mapping: "web standard mapping".to_string(),
                        mapped: true,
                    }));
                    id
                }
            };
            if let Some((_, stick)) = self.known.get_mut(index) {
                out.push(DeviceEvent::Sample {
                    id,
                    snapshot: sample_web_pad(gamepad, stick),
                });
            }
        }
    }
}

fn connected_web_gamepads() -> BTreeMap<u32, web_sys::Gamepad> {
    use wasm_bindgen::JsCast;

    let mut connected = BTreeMap::new();
    let Some(window) = web_sys::window() else {
        return connected;
    };
    let Ok(gamepads) = window.navigator().get_gamepads() else {
        return connected;
    };
    for slot in 0..gamepads.length() {
        let value = gamepads.get(slot);
        if value.is_null() || value.is_undefined() {
            continue;
        }
        let gamepad: web_sys::Gamepad = value.unchecked_into();
        if gamepad.connected() {
            connected.insert(gamepad.index(), gamepad);
        }
    }
    connected
}

fn sample_web_pad(gamepad: &web_sys::Gamepad, stick: &mut StickDpad) -> PadSnapshot {
    use wasm_bindgen::JsCast;

    let buttons = gamepad.buttons();
    let pressed = |index: u32| {
        let value = buttons.get(index);
        !value.is_null()
            && !value.is_undefined()
            && value.unchecked_into::<web_sys::GamepadButton>().pressed()
    };
    let mut mask = 0;
    for (index, bit) in [
        (0, button::CROSS),
        (1, button::CIRCLE),
        (2, button::SQUARE),
        (3, button::TRIANGLE),
        (4, button::L1),
        (5, button::R1),
        (6, button::L2),
        (7, button::R2),
        (8, button::SELECT),
        (9, button::START),
        (10, button::L3),
        (11, button::R3),
        (12, button::UP),
        (13, button::DOWN),
        (14, button::LEFT),
        (15, button::RIGHT),
    ] {
        if pressed(index) {
            mask |= bit;
        }
    }
    let axes = gamepad.axes();
    let axis = |index: u32| -> f32 {
        let value = axes.get(index).as_f64().unwrap_or(0.0) as f32;
        if value.abs() < 0.15 {
            0.0
        } else {
            value
        }
    };
    let left = (axis(0), -axis(1));
    let right = (axis(2), -axis(3));
    PadSnapshot {
        buttons: mask,
        stick_dpad: stick.update(left.0, left.1),
        left,
        right,
        mode: pressed(16),
    }
}
