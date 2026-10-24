//! Browser Gamepad API pads.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use emulator_core::button;

use crate::input::model::{BackendKind, DeviceEvent, DeviceIdentity, PadSnapshot, StickDpad};

use super::DeviceBackend;

thread_local! {
    /// Buttons seen down between two polls, by pad slot: a tap shorter than one frame (a page
    /// that gets a few frames a second misses most real presses) still counts as a press.
    static SEEN_DOWN: RefCell<BTreeMap<u32, u16>> = const { RefCell::new(BTreeMap::new()) };
    static WATCHING: Cell<bool> = const { Cell::new(false) };
}

/// Look at the pads on a timer of their own, so a press that starts and ends between two
/// frames is latched for the next poll. Frames pace the game; they must not pace the buttons.
fn watch_between_frames() {
    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::JsCast;

    if WATCHING.with(|w| w.replace(true)) {
        return;
    }
    let Some(window) = web_sys::window() else {
        return;
    };
    let tick = Closure::<dyn FnMut()>::new(|| {
        let pads = connected_web_gamepads();
        if pads.is_empty() {
            return;
        }
        SEEN_DOWN.with(|seen| {
            let mut seen = seen.borrow_mut();
            for (index, gamepad) in &pads {
                *seen.entry(*index).or_insert(0) |= pressed_buttons(gamepad);
            }
        });
    });
    let _ = window
        .set_interval_with_callback_and_timeout_and_arguments_0(tick.as_ref().unchecked_ref(), 20);
    // The page-long timer owns the closure.
    tick.forget();
}

/// The Web Gamepad adapter ("standard" mapping).
#[derive(Default)]
pub(crate) struct WebPadBackend {
    /// Announced pads by browser slot index.
    known: BTreeMap<u32, (String, StickDpad)>,
}

impl DeviceBackend for WebPadBackend {
    fn poll(&mut self, out: &mut Vec<DeviceEvent>) {
        watch_between_frames();
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

/// The buttons that are down on `gamepad` right now, as a pad mask.
fn pressed_buttons(gamepad: &web_sys::Gamepad) -> u16 {
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
    mask
}

fn sample_web_pad(gamepad: &web_sys::Gamepad, stick: &mut StickDpad) -> PadSnapshot {
    let pressed_now = pressed_buttons(gamepad);
    // What went down and up again since the last poll counts too, for one sample.
    let mask = pressed_now
        | SEEN_DOWN.with(|seen| seen.borrow_mut().remove(&gamepad.index()).unwrap_or(0));
    let pressed = |index: u32| {
        let value = gamepad.buttons().get(index);
        !value.is_null()
            && !value.is_undefined()
            && wasm_bindgen::JsCast::unchecked_into::<web_sys::GamepadButton>(value).pressed()
    };
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
