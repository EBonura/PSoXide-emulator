//! Controllers through Apple's GameController framework (macOS).
//!
//! The framework is where macOS keeps Xbox, DualShock 4, DualSense and
//! Switch Pro controllers, over USB and Bluetooth, already in one standard
//! layout; the raw HID view of the same device is a side door that needs
//! per-model mappings and, for some USB controllers, an initialisation
//! handshake the framework performs for us. This adapter polls the
//! framework's controller list, so it needs no notification plumbing.

use objc2::rc::Retained;
use objc2_game_controller::{GCController, GCDevice, GCExtendedGamepad};

use emulator_core::button;

use crate::input::model::{BackendKind, DeviceEvent, DeviceIdentity, PadSnapshot, StickDpad};

use super::DeviceBackend;

/// Trigger travel that counts as a press when the framework reports no
/// digital state of its own.
const TRIGGER_THRESHOLD: f32 = 0.5;

/// What one controller holds, in the framework's (positional) layout. Kept
/// separate from the Objective-C reads so the mapping is a plain function.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct GcReading {
    /// Bottom face button (Xbox A, PlayStation Cross).
    pub(crate) south: bool,
    /// Right face button (Xbox B, PlayStation Circle).
    pub(crate) east: bool,
    /// Left face button (Xbox X, PlayStation Square).
    pub(crate) west: bool,
    /// Top face button (Xbox Y, PlayStation Triangle).
    pub(crate) north: bool,
    pub(crate) left_shoulder: bool,
    pub(crate) right_shoulder: bool,
    pub(crate) left_trigger: f32,
    pub(crate) left_trigger_pressed: bool,
    pub(crate) right_trigger: f32,
    pub(crate) right_trigger_pressed: bool,
    pub(crate) left_stick_click: bool,
    pub(crate) right_stick_click: bool,
    pub(crate) up: bool,
    pub(crate) down: bool,
    pub(crate) left: bool,
    pub(crate) right: bool,
    /// Menu / Options / Start.
    pub(crate) menu: bool,
    /// View / Share / Back.
    pub(crate) options: bool,
    /// The system (Xbox / PS / Home) button.
    pub(crate) home: bool,
    pub(crate) left_stick: (f32, f32),
    pub(crate) right_stick: (f32, f32),
}

impl GcReading {
    /// The same controller in the PS1 layout.
    pub(crate) fn to_snapshot(self, stick: &mut StickDpad) -> PadSnapshot {
        let mut buttons = 0;
        for (down, bit) in [
            (self.south, button::CROSS),
            (self.east, button::CIRCLE),
            (self.west, button::SQUARE),
            (self.north, button::TRIANGLE),
            (self.left_shoulder, button::L1),
            (self.right_shoulder, button::R1),
            (
                self.left_trigger_pressed || self.left_trigger > TRIGGER_THRESHOLD,
                button::L2,
            ),
            (
                self.right_trigger_pressed || self.right_trigger > TRIGGER_THRESHOLD,
                button::R2,
            ),
            (self.left_stick_click, button::L3),
            (self.right_stick_click, button::R3),
            (self.up, button::UP),
            (self.down, button::DOWN),
            (self.left, button::LEFT),
            (self.right, button::RIGHT),
            (self.menu, button::START),
            (self.options, button::SELECT),
        ] {
            if down {
                buttons |= bit;
            }
        }
        PadSnapshot {
            buttons,
            stick_dpad: stick.update(self.left_stick.0, self.left_stick.1),
            left: self.left_stick,
            right: self.right_stick,
            mode: self.home,
        }
    }
}

fn read(pad: &GCExtendedGamepad) -> GcReading {
    // SAFETY: plain property reads on live framework objects.
    unsafe {
        let pressed = |b: &objc2_game_controller::GCControllerButtonInput| b.isPressed();
        let optional = |b: Option<Retained<objc2_game_controller::GCControllerButtonInput>>| {
            b.is_some_and(|b| b.isPressed())
        };
        let dpad = pad.dpad();
        let left = pad.leftThumbstick();
        let right = pad.rightThumbstick();
        let left_trigger = pad.leftTrigger();
        let right_trigger = pad.rightTrigger();
        GcReading {
            south: pressed(&pad.buttonA()),
            east: pressed(&pad.buttonB()),
            west: pressed(&pad.buttonX()),
            north: pressed(&pad.buttonY()),
            left_shoulder: pressed(&pad.leftShoulder()),
            right_shoulder: pressed(&pad.rightShoulder()),
            left_trigger: left_trigger.value(),
            left_trigger_pressed: left_trigger.isPressed(),
            right_trigger: right_trigger.value(),
            right_trigger_pressed: right_trigger.isPressed(),
            left_stick_click: optional(pad.leftThumbstickButton()),
            right_stick_click: optional(pad.rightThumbstickButton()),
            up: pressed(&dpad.up()),
            down: pressed(&dpad.down()),
            left: pressed(&dpad.left()),
            right: pressed(&dpad.right()),
            menu: pressed(&pad.buttonMenu()),
            options: optional(pad.buttonOptions()),
            home: optional(pad.buttonHome()),
            left_stick: (left.xAxis().value(), left.yAxis().value()),
            right_stick: (right.xAxis().value(), right.yAxis().value()),
        }
    }
}

struct Known {
    id: String,
    stick: StickDpad,
    controller: Retained<GCController>,
}

/// The GameController adapter.
pub(crate) struct GameControllerBackend {
    known: Vec<Known>,
    next_serial: u32,
}

impl GameControllerBackend {
    /// Ask the framework to deliver input while another app has focus, as the
    /// HID path always did.
    pub(crate) fn new() -> Self {
        // SAFETY: a class-level property setter, called before any use.
        unsafe { GCController::setShouldMonitorBackgroundEvents(true) };
        Self {
            known: Vec::new(),
            next_serial: 0,
        }
    }

    /// Number of controllers the framework currently reports.
    pub(crate) fn controller_count(&self) -> usize {
        self.known.len()
    }
}

impl DeviceBackend for GameControllerBackend {
    fn poll(&mut self, out: &mut Vec<DeviceEvent>) {
        // SAFETY: reads the framework's current controller list.
        let current = unsafe { GCController::controllers() };

        // Controllers that disappeared.
        self.known.retain(|known| {
            let still_there = current
                .iter()
                .any(|c| Retained::as_ptr(&c) == Retained::as_ptr(&known.controller));
            if !still_there {
                out.push(DeviceEvent::Disconnected(known.id.clone()));
            }
            still_there
        });

        for controller in current.iter() {
            // SAFETY: property reads on a live framework object.
            let Some(pad) = (unsafe { controller.extendedGamepad() }) else {
                // A remote or other device without the full layout.
                continue;
            };
            let existing = self.known.iter().position(|known| {
                Retained::as_ptr(&known.controller) == Retained::as_ptr(&controller)
            });
            let index = match existing {
                Some(index) => index,
                None => {
                    let id = format!("gc:{}", self.next_serial);
                    self.next_serial += 1;
                    // SAFETY: property reads on a live framework object.
                    let (name, category) = unsafe {
                        (
                            controller.vendorName().map(|n| n.to_string()),
                            controller.productCategory().to_string(),
                        )
                    };
                    out.push(DeviceEvent::Connected(DeviceIdentity {
                        id: id.clone(),
                        name: name.unwrap_or_else(|| "Game controller".to_string()),
                        backend: BackendKind::GameController,
                        vid_pid: None,
                        mapping: format!("GameController standard layout, {category}"),
                        mapped: true,
                    }));
                    self.known.push(Known {
                        id,
                        stick: StickDpad::default(),
                        controller: controller.clone(),
                    });
                    self.known.len() - 1
                }
            };
            let known = &mut self.known[index];
            out.push(DeviceEvent::Sample {
                id: known.id.clone(),
                snapshot: read(&pad).to_snapshot(&mut known.stick),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_standard_layout_maps_to_the_ps1_pad() {
        let reading = GcReading {
            south: true,
            east: true,
            west: true,
            north: true,
            left_shoulder: true,
            right_shoulder: true,
            left_trigger: 0.9,
            right_trigger_pressed: true,
            left_stick_click: true,
            right_stick_click: true,
            up: true,
            menu: true,
            options: true,
            home: true,
            right_stick: (0.0, -1.0),
            ..GcReading::default()
        };
        let snapshot = reading.to_snapshot(&mut StickDpad::default());
        assert_eq!(
            snapshot.buttons,
            button::CROSS
                | button::CIRCLE
                | button::SQUARE
                | button::TRIANGLE
                | button::L1
                | button::R1
                | button::L2
                | button::R2
                | button::L3
                | button::R3
                | button::UP
                | button::START
                | button::SELECT
        );
        assert!(snapshot.mode);
        assert_eq!(snapshot.right, (0.0, -1.0));
    }

    #[test]
    fn a_half_pulled_trigger_is_not_a_press_and_a_pushed_stick_is_a_stick_dpad_press_not_a_button()
    {
        let snapshot = GcReading {
            left_trigger: 0.3,
            left_stick: (0.0, 0.9),
            ..GcReading::default()
        }
        .to_snapshot(&mut StickDpad::default());
        assert_eq!(snapshot.buttons, 0);
        assert_eq!(snapshot.stick_dpad, button::UP);
        assert_eq!(snapshot.left, (0.0, 0.9));
    }

    #[test]
    fn an_idle_controller_reports_nothing() {
        assert_eq!(
            GcReading::default().to_snapshot(&mut StickDpad::default()),
            PadSnapshot::default()
        );
    }
}
