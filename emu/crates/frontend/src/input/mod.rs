//! Host input: one logical model, many adapters.
//!
//! ```text
//!   keyboard ─┐
//!   GameController (macOS) ─┤                      ┌─ menu (nav, hold-to-repeat, confirm, back, chord)
//!   gilrs (HID/evdev/XInput) ─┼─ DeviceEvent ─ InputHub ─┼─ freecam / shortcuts
//!   Web Gamepad API ─┘                      └─ guest ports 1 and 2
//! ```
//!
//! Adapters ([`backend`]) only turn raw device state into a
//! [`model::PadSnapshot`] in the PlayStation layout plus a
//! [`model::DeviceIdentity`]. The [`hub::InputHub`] is the only code that
//! acts on snapshots, and it cannot tell a keyboard from a pad.

pub(crate) mod backend;
pub(crate) mod filter;
pub(crate) mod hub;
pub(crate) mod model;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod probe;
pub(crate) mod repeat;
pub(crate) mod routing;
pub(crate) mod socd;
pub(crate) mod touch;

use emulator_core::Bus;
use web_time::Instant;

pub(crate) use hub::{InputDeviceInfo, InputFrame, RoutedPadInput};
pub(crate) use model::PadSnapshot;
pub(crate) use routing::{PadProfile, PortChoice};

use backend::keyboard::KeyboardDevice;
use backend::touch::{TouchDevice, TOUCH_DEVICE_ID};
use backend::DeviceBackend;
use hub::InputHub;
use model::DeviceEvent;

/// The single input entry point of the shell: owns the adapters and the hub,
/// and is polled once per frame.
pub(crate) struct InputRouter {
    /// A key was pressed this session: a keyboard really is there.
    keyboard_seen: bool,
    keyboard: KeyboardDevice,
    touch: TouchDevice,
    backends: Vec<Box<dyn DeviceBackend>>,
    hub: InputHub,
    epoch: Instant,
}

impl InputRouter {
    /// A router with this platform's pad adapters.
    pub(crate) fn new() -> Self {
        eprintln!(
            "[input] adapters: keyboard + {}",
            backend::PLATFORM_PAD_ADAPTERS
        );
        Self::with_backends(backend::platform_pad_backends())
    }

    /// A router over explicit pad adapters.
    pub(crate) fn with_backends(backends: Vec<Box<dyn DeviceBackend>>) -> Self {
        Self {
            keyboard_seen: false,
            keyboard: KeyboardDevice::default(),
            touch: TouchDevice::default(),
            backends,
            hub: InputHub::new(),
            epoch: Instant::now(),
        }
    }

    /// A key went down (any key, bound or not): a keyboard is attached.
    pub(crate) fn note_key_event(&mut self) {
        self.keyboard_seen = true;
    }

    /// Whether a keyboard is really there. A desktop always has one; a phone or tablet (a finger
    /// is the primary pointer) has one only once a key has been pressed, so the Controls page
    /// never claims a keyboard nobody has attached.
    pub(crate) fn keyboard_present(&self) -> bool {
        self.keyboard_seen || !self.touch.touch_primary()
    }

    /// Hand over what the keyboard holds right now (once per frame).
    pub(crate) fn set_keyboard_held(&mut self, held: PadSnapshot) {
        self.keyboard.set_held(held);
    }

    /// A bound keyboard key went down (also covers a tap shorter than a frame).
    pub(crate) fn note_keyboard_press(&mut self, buttons: u16, mode: bool) {
        self.keyboard.note_press(buttons, mode);
    }

    /// Poll every adapter and run the hub once.
    pub(crate) fn poll(&mut self) -> InputFrame {
        let mut events: Vec<DeviceEvent> = Vec::new();
        self.keyboard.poll(&mut events);
        for backend in &mut self.backends {
            backend.poll(&mut events);
        }
        self.touch.poll(&mut events);
        let now_ms = self.now_ms();
        let frame = self.hub.step(now_ms, events);
        if self.hub.other_device_active(TOUCH_DEVICE_ID) {
            self.touch.note_other_input(now_ms);
        }
        frame
    }

    /// Milliseconds on the router's own monotonic clock.
    pub(crate) fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// The touchscreen device.
    pub(crate) fn touch(&self) -> &TouchDevice {
        &self.touch
    }

    /// The touchscreen device, for window events and layout.
    pub(crate) fn touch_mut(&mut self) -> &mut TouchDevice {
        &mut self.touch
    }

    /// Make every device's currently held buttons a baseline that does
    /// nothing until released. Call when the menu opens or a game launches.
    pub(crate) fn ignore_held_buttons(&mut self) {
        self.hub.ignore_held_buttons();
    }

    /// Current rows for the controller panel.
    pub(crate) fn devices(&self) -> Vec<InputDeviceInfo> {
        self.hub.devices()
    }

    /// Pin a device to a port, or let it pick one on its first press.
    pub(crate) fn set_device_choice(&mut self, id: &str, choice: PortChoice) -> bool {
        self.hub.set_device_choice(id, choice)
    }

    /// Select the controller identity a device presents to the guest.
    pub(crate) fn set_device_profile(&mut self, id: &str, profile: PadProfile) -> bool {
        self.hub.set_device_profile(id, profile)
    }

    /// Load the analog choices saved with the settings (call before pads connect).
    pub(crate) fn set_remembered_analog(
        &mut self,
        choices: std::collections::BTreeMap<String, bool>,
    ) {
        self.hub.set_remembered_analog(choices);
    }

    /// The analog choices, when one changed since the last call, to be saved.
    pub(crate) fn take_remembered_analog(
        &mut self,
    ) -> Option<std::collections::BTreeMap<String, bool>> {
        self.hub.take_remembered_analog()
    }

    /// Monotonic stamp changed by any route or mode edit.
    pub(crate) fn routing_generation(&self) -> u64 {
        self.hub.routing_generation()
    }

    /// Apply current Digital/Analog controller identities to a new layout.
    pub(crate) fn apply_layout(&self, bus: &mut Bus) {
        self.hub.apply_layout(bus);
    }
}

impl Default for InputRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use emulator_core::button;

    use super::model::{BackendKind, DeviceIdentity};
    use super::*;

    /// A pad adapter that replays a script of snapshots, one per poll.
    struct ScriptedPad {
        script: VecDeque<PadSnapshot>,
        announced: bool,
    }

    impl DeviceBackend for ScriptedPad {
        fn poll(&mut self, out: &mut Vec<DeviceEvent>) {
            if !self.announced {
                self.announced = true;
                out.push(DeviceEvent::Connected(DeviceIdentity {
                    id: "scripted".to_string(),
                    name: "Scripted pad".to_string(),
                    backend: BackendKind::Gilrs,
                    vid_pid: None,
                    mapping: "test".to_string(),
                    mapped: true,
                }));
            }
            let snapshot = self.script.pop_front().unwrap_or_default();
            out.push(DeviceEvent::Sample {
                id: "scripted".to_string(),
                snapshot,
            });
        }
    }

    /// What a person can observe on one poll.
    type Seen = (bool, bool, bool, u16);

    fn seen(frame: &InputFrame) -> Seen {
        (
            frame.menu_down,
            frame.menu_confirm,
            frame.menu_back,
            frame.port1.mask,
        )
    }

    fn steps() -> Vec<u16> {
        vec![
            0,
            0,
            button::DOWN,
            0,
            button::CROSS,
            button::CROSS,
            0,
            button::CIRCLE,
            0,
            0,
        ]
    }

    /// A router whose on-screen controller is showing on a landscape phone.
    fn touch_router() -> InputRouter {
        use super::touch::layout::{Insets, LayoutParams};
        use super::touch::prefs::{OverlayMode, TouchPrefs};

        let mut router = InputRouter::with_backends(Vec::new());
        router.touch_mut().set_prefs(TouchPrefs {
            mode: OverlayMode::On,
            ..TouchPrefs::default()
        });
        router.touch_mut().update_layout(
            LayoutParams {
                size: egui::vec2(2556.0, 1179.0),
                scale: 3.0,
                safe: Insets::default(),
                analog: false,
                size_pct: 100,
                left_handed: false,
                fullscreen_button: false,
                menu_open: false,
            },
            0,
            0.016,
        );
        router
    }

    #[test]
    fn a_phone_has_no_keyboard_until_a_key_is_pressed() {
        // A finger is the primary pointer: no keyboard is claimed.
        let mut phone = touch_router();
        phone.touch_mut().set_coarse_pointer(true);
        assert!(!phone.keyboard_present(), "a phone claimed a keyboard");
        phone.note_key_event();
        assert!(phone.keyboard_present(), "a key press proves one");
        // A desktop browser or the native app always has one.
        let desktop = InputRouter::with_backends(Vec::new());
        assert!(desktop.keyboard_present());
    }

    #[test]
    fn the_shell_over_a_game_takes_the_controllers_top_buttons_but_keeps_its_pad() {
        use super::touch::layout::{Insets, LayoutParams};
        let params = || LayoutParams {
            size: egui::vec2(2556.0, 1179.0),
            scale: 3.0,
            safe: Insets::default(),
            analog: false,
            size_pct: 100,
            left_handed: false,
            fullscreen_button: true,
            menu_open: true,
        };
        let mut router = touch_router();
        router.touch_mut().update_layout(params(), 0, 0.016);
        let before = router.touch().layout().expect("laid out").ui.len();
        assert!(before >= 4, "pause, save, debug, menu: {before}");
        router.touch_mut().set_shell_up(true);
        router.touch_mut().update_layout(params(), 0, 0.016);
        let layout = router.touch().layout().expect("the pad stays laid out");
        assert!(layout.ui.is_empty(), "no button of the controller over the shell");
        assert!(!layout.buttons.is_empty(), "the game keeps its panel");
        router.touch_mut().set_shell_up(false);
        router.touch_mut().update_layout(params(), 0, 0.016);
        assert_eq!(router.touch().layout().expect("laid out").ui.len(), before);
    }

    /// Where a finger goes to hold exactly `mask` (one button or none).
    fn finger_for(router: &InputRouter, mask: u16) -> egui::Pos2 {
        let layout = router.touch().layout().expect("controller is showing");
        if mask == button::DOWN {
            return layout.dpad.center + egui::vec2(0.0, layout.dpad.zone * 0.7);
        }
        layout
            .buttons
            .iter()
            .find(|b| b.mask == mask)
            .map(|b| b.draw.center())
            .expect("a button for the mask")
    }

    #[test]
    fn the_router_gives_the_keyboard_and_the_touchscreen_the_same_menu_and_guest_behaviour() {
        use super::touch::pad::Phase;

        let script = steps();

        let mut keyboard_router = InputRouter::with_backends(Vec::new());
        let mut from_keyboard = Vec::new();
        let mut held = 0u16;
        for buttons in &script {
            if buttons & !held != 0 {
                keyboard_router.note_keyboard_press(buttons & !held, false);
            }
            held = *buttons;
            keyboard_router.set_keyboard_held(PadSnapshot {
                buttons: held,
                ..PadSnapshot::default()
            });
            from_keyboard.push(seen(&keyboard_router.poll()));
        }

        // The same presses as fingers on the on-screen controller: lift the
        // old finger and put a new one down whenever the wanted button changes.
        let mut router = touch_router();
        let mut from_touch = Vec::new();
        let mut finger: Option<(u64, u16)> = None;
        let mut next_id = 1;
        for buttons in &script {
            if finger.map(|(_, mask)| mask) != Some(*buttons).filter(|b| *b != 0) {
                if let Some((id, mask)) = finger.take() {
                    let at = finger_for(&router, mask);
                    router.touch_mut().handle_touch(id, Phase::Up, at, 0);
                }
                if *buttons != 0 {
                    let at = finger_for(&router, *buttons);
                    assert!(router.touch_mut().handle_touch(next_id, Phase::Down, at, 0));
                    finger = Some((next_id, *buttons));
                    next_id += 1;
                }
            }
            from_touch.push(seen(&router.poll()));
        }

        assert_eq!(from_keyboard, from_touch);
        assert!(from_touch.iter().any(|s| s.0), "a menu step happened");
        assert!(from_touch.iter().any(|s| s.1), "a confirm happened");
        assert!(from_touch.iter().any(|s| s.2), "a back happened");
        assert!(from_touch.iter().any(|s| s.3 != 0), "the guest saw input");
    }

    #[test]
    fn a_tap_shorter_than_a_frame_still_confirms_through_the_touchscreen() {
        use super::touch::pad::Phase;

        let mut router = touch_router();
        // The first poll announces the device and is neutral.
        router.poll();
        router.poll();
        let at = finger_for(&router, button::CROSS);
        router.touch_mut().handle_touch(1, Phase::Down, at, 0);
        router.touch_mut().handle_touch(1, Phase::Up, at, 0);
        let frame = router.poll();
        assert!(frame.menu_confirm);
        assert_eq!(frame.port1.mask, button::CROSS);
        assert_eq!(router.poll().port1.mask, 0);
    }

    #[test]
    fn a_mouse_click_on_the_menu_button_asks_for_the_menu_like_a_tap() {
        use super::touch::layout::UiId;
        use super::touch::pad::{Phase, UiAction};

        let mut router = touch_router();
        let at = router
            .touch()
            .layout()
            .and_then(|l| l.ui.iter().find(|b| b.id == UiId::Menu))
            .map(|b| b.hit.center())
            .expect("a menu button");
        assert!(router.touch_mut().handle_mouse(Phase::Down, at, 0));
        // A drag that stays on the button is still a click.
        assert!(router.touch_mut().handle_mouse(Phase::Move, at, 1));
        assert!(router.touch_mut().handle_mouse(Phase::Up, at, 2));
        assert_eq!(router.touch_mut().take_actions(), vec![UiAction::Menu]);
        // Off the controller the mouse is not its business.
        assert!(!router
            .touch_mut()
            .handle_mouse(Phase::Down, egui::pos2(-50.0, -50.0), 3));
    }

    #[test]
    fn the_controller_steps_aside_while_only_the_shell_is_up() {
        let mut router = touch_router();
        assert!(router.touch().layout().is_some());
        router.touch_mut().set_shell_only(true);
        router.touch_mut().update_layout(
            super::touch::layout::LayoutParams {
                size: egui::vec2(2556.0, 1179.0),
                scale: 3.0,
                safe: super::touch::layout::Insets::default(),
                analog: false,
                size_pct: 100,
                left_handed: false,
                fullscreen_button: false,
                menu_open: false,
            },
            0,
            0.016,
        );
        assert!(
            router.touch().layout().is_none(),
            "no controller over the home screen"
        );
        router.touch_mut().set_shell_only(false);
        router.touch_mut().update_layout(
            super::touch::layout::LayoutParams {
                size: egui::vec2(2556.0, 1179.0),
                scale: 3.0,
                safe: super::touch::layout::Insets::default(),
                analog: false,
                size_pct: 100,
                left_handed: false,
                fullscreen_button: false,
                menu_open: false,
            },
            0,
            0.016,
        );
        assert!(router.touch().layout().is_some());
    }

    #[test]
    fn a_keyboard_press_hides_the_automatic_controller_and_a_touch_brings_it_back() {
        use super::touch::pad::Phase;
        use super::touch::prefs::OverlayMode;

        let mut router = InputRouter::with_backends(Vec::new());
        router.touch_mut().set_coarse_pointer(true);
        assert!(router.touch().visible());
        router.poll();
        router.note_keyboard_press(button::CROSS, false);
        router.set_keyboard_held(PadSnapshot {
            buttons: button::CROSS,
            ..PadSnapshot::default()
        });
        router.poll();
        assert!(!router.touch().visible(), "a key press hides it");
        router.set_keyboard_held(PadSnapshot::default());
        router.poll();
        router
            .touch_mut()
            .handle_touch(1, Phase::Down, egui::pos2(1.0, 1.0), 10_000);
        assert!(router.touch().visible(), "a touch shows it again");
        // Off stays off.
        router
            .touch_mut()
            .set_prefs(super::touch::prefs::TouchPrefs {
                mode: OverlayMode::Off,
                ..Default::default()
            });
        assert!(!router.touch().visible());
    }

    #[test]
    fn the_router_gives_the_keyboard_and_a_pad_the_same_menu_and_guest_behaviour() {
        let script = steps();

        // The keyboard, exactly as the shell drives it: held state once a
        // frame, and a latched press on each key-down.
        let mut keyboard_router = InputRouter::with_backends(Vec::new());
        let mut from_keyboard = Vec::new();
        let mut held = 0u16;
        for buttons in &script {
            if buttons & !held != 0 {
                keyboard_router.note_keyboard_press(buttons & !held, false);
            }
            held = *buttons;
            keyboard_router.set_keyboard_held(PadSnapshot {
                buttons: held,
                ..PadSnapshot::default()
            });
            from_keyboard.push(seen(&keyboard_router.poll()));
        }

        // The same presses from a pad adapter.
        let pad = ScriptedPad {
            script: script
                .iter()
                .map(|buttons| PadSnapshot {
                    buttons: *buttons,
                    ..PadSnapshot::default()
                })
                .collect(),
            announced: false,
        };
        let mut pad_router = InputRouter::with_backends(vec![Box::new(pad)]);
        let from_pad: Vec<Seen> = script.iter().map(|_| seen(&pad_router.poll())).collect();

        assert_eq!(from_keyboard, from_pad);
        assert!(from_pad.iter().any(|s| s.0), "a menu step happened");
        assert!(from_pad.iter().any(|s| s.1), "a confirm happened");
        assert!(from_pad.iter().any(|s| s.2), "a back happened");
        assert!(from_pad.iter().any(|s| s.3 != 0), "the guest saw input");
    }
}
