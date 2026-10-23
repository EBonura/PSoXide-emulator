//! The input hub: the only place that knows what to do with device state.
//!
//! Adapters feed [`DeviceEvent`]s in; one [`InputFrame`] comes out per poll
//! with the menu navigation (including hold-to-repeat), the Select+Start menu
//! chord, the merged state for shortcuts and the freecam, and the feed for
//! guest ports 1 and 2. The hub treats every device the same: the keyboard
//! goes through the same filter, the same port claim and the same menu
//! edges as a gamepad.

use std::collections::BTreeMap;

use emulator_core::{button, Bus};

use super::filter::{ChatterChange, PadFilter};
use super::model::{rising_edges, BackendKind, DeviceEvent, DeviceIdentity, PadSnapshot};
use super::repeat::MenuRepeat;
use super::routing::{
    configure_port, PadProfile, PortChoice, PsxPort, RoutingTable, KEYBOARD_DEVICE_ID,
};
use super::socd::SocdFilter;

/// The Select+Start menu chord.
const CHORD_MASK: u16 = button::SELECT | button::START;
/// The four menu direction bits.
const DIRECTIONS: u16 = button::UP | button::DOWN | button::LEFT | button::RIGHT;

/// Buttons and sticks routed to one guest port this frame.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RoutedPadInput {
    /// Held PlayStation button bits (physical buttons only).
    pub(crate) mask: u16,
    /// [`Self::mask`] plus the D-pad bits derived from the left stick.
    pub(crate) mask_with_stick_dpad: u16,
    /// Left stick in host coordinates.
    pub(crate) left_stick: (f32, f32),
    /// Right stick in host coordinates.
    pub(crate) right_stick: (f32, f32),
}

impl RoutedPadInput {
    /// The buttons the guest should see: while its pad is in analog mode the
    /// stick is an axis and must not also press the D-pad; in digital mode
    /// (which carries no stick) the stick stands in for the D-pad.
    pub(crate) fn mask_for(&self, guest_pad_is_analog: bool) -> u16 {
        if guest_pad_is_analog {
            self.mask
        } else {
            self.mask_with_stick_dpad
        }
    }

    fn merge(&mut self, snapshot: &PadSnapshot) {
        self.mask |= snapshot.buttons;
        self.mask_with_stick_dpad |= snapshot.buttons | snapshot.stick_dpad;
        self.left_stick = merge_stick(self.left_stick, snapshot.left);
        self.right_stick = merge_stick(self.right_stick, snapshot.right);
    }
}

/// Combine two sticks per axis: the first one that is off centre wins.
fn merge_stick(first: (f32, f32), second: (f32, f32)) -> (f32, f32) {
    let axis = |a: f32, b: f32| if a != 0.0 { a } else { b };
    (axis(first.0, second.0), axis(first.1, second.1))
}

/// Something worth telling the user (and the log) about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum InputNotice {
    /// A device became available.
    Connected {
        /// Who it is and which adapter reads it.
        device: DeviceIdentity,
    },
    /// A previously tracked device disappeared.
    Disconnected {
        /// User-facing name captured before disconnect.
        name: String,
    },
    /// A device is reporting input faster than a person can press it (a
    /// stuck, failing or mis-parsed device), so it is ignored until it goes
    /// quiet. Without this its noise confirms menu rows and launches games.
    Ignored {
        /// Who it is.
        device: DeviceIdentity,
    },
    /// A device that was ignored for noise went quiet and is in use again.
    Restored {
        /// User-facing name.
        name: String,
    },
    /// A pad's first real press took it a controller port.
    Claimed {
        /// User-facing name.
        name: String,
        /// The port it now drives.
        port: PsxPort,
    },
    /// A device's Analog / Mode button switched its controller identity.
    ModeChanged {
        /// User-facing name.
        name: String,
        /// Whether it is now exposed as an analog pad.
        analog: bool,
    },
}

impl InputNotice {
    /// Short status-line text for the frontend toast.
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Connected { device } => {
                format!("Gamepad connected: {} ({})", device.name, device.mapping)
            }
            Self::Disconnected { name } => format!("Gamepad disconnected: {name}"),
            Self::Ignored { device } => format!(
                "Gamepad ignored: {} is sending constant input (stuck or faulty)",
                device.name
            ),
            Self::Restored { name } => format!("Gamepad back in use: {name}"),
            Self::Claimed { name, port } => format!("{name} drives {}", port.label()),
            Self::ModeChanged { name, analog } => format!(
                "{name} mode: {}",
                if *analog { "Analog" } else { "Digital" }
            ),
        }
    }

    /// One log line with the identity details (backend, vid:pid), for stderr.
    pub(crate) fn log_line(&self) -> String {
        match self {
            Self::Connected { device } => format!("[input] connected: {}", device.describe()),
            Self::Disconnected { name } => format!("[input] disconnected: {name}"),
            Self::Ignored { device } => format!(
                "[input] quarantined (constant input): {}",
                device.describe()
            ),
            Self::Restored { name } => format!("[input] back in use: {name}"),
            Self::Claimed { name, port } => format!("[input] {name} -> {}", port.label()),
            Self::ModeChanged { name, analog } => format!(
                "[input] {name} mode -> {}",
                if *analog { "analog" } else { "digital" }
            ),
        }
    }
}

/// Per-poll result of [`InputHub::step`].
#[derive(Clone, Debug, Default)]
pub(crate) struct InputFrame {
    /// Notices raised this poll.
    pub(crate) notices: Vec<InputNotice>,
    /// All devices merged (SOCD-resolved, menu chord removed): for freecam
    /// and shortcuts.
    pub(crate) mask: u16,
    /// Routed guest input for controller port 1.
    pub(crate) port1: RoutedPadInput,
    /// Routed guest input for controller port 2.
    pub(crate) port2: RoutedPadInput,
    /// Select+Start went down on some device (rising edge).
    pub(crate) toggle_menu: bool,
    /// A device's Analog/Mode button went down.
    pub(crate) analog_button: bool,
    /// Menu: up (press or hold-repeat step).
    pub(crate) menu_up: bool,
    /// Menu: down (press or hold-repeat step).
    pub(crate) menu_down: bool,
    /// Menu: left (press or hold-repeat step).
    pub(crate) menu_left: bool,
    /// Menu: right (press or hold-repeat step).
    pub(crate) menu_right: bool,
    /// Menu: confirm (Cross went down).
    pub(crate) menu_confirm: bool,
    /// Menu: back (Circle went down).
    pub(crate) menu_back: bool,
    /// First non-neutral left stick, for freecam.
    pub(crate) left_stick: (f32, f32),
    /// First non-neutral right stick, for freecam.
    pub(crate) right_stick: (f32, f32),
}

/// One row in the controller panel.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct InputDeviceInfo {
    /// Who it is.
    pub(crate) identity: DeviceIdentity,
    /// Current guest port.
    pub(crate) port: PsxPort,
    /// What the panel's picker shows (Auto or pinned).
    pub(crate) choice: PortChoice,
    /// Controller identity the guest sees on that port.
    pub(crate) profile: PadProfile,
    /// Ignored for constant input.
    pub(crate) ignored: bool,
    /// What the device holds right now, as the rest of the app sees it.
    pub(crate) live: PadSnapshot,
}

#[derive(Debug)]
struct DeviceState {
    identity: DeviceIdentity,
    filter: PadFilter,
    /// Latest raw snapshot from the adapter.
    snapshot: PadSnapshot,
    /// What the rest of the app sees after the guard.
    live: PadSnapshot,
    mode_down: bool,
    /// The adapter has sent at least one sample; until then there is no state to judge.
    sampled: bool,
}

impl DeviceState {
    fn new(identity: DeviceIdentity) -> Self {
        Self {
            identity,
            filter: PadFilter::new(),
            snapshot: PadSnapshot::default(),
            live: PadSnapshot::default(),
            mode_down: false,
            sampled: false,
        }
    }
}

/// Everything that turns device snapshots into menu actions and guest input.
pub(crate) struct InputHub {
    routing: RoutingTable,
    devices: BTreeMap<String, DeviceState>,
    repeat: MenuRepeat,
    socd_all: SocdFilter,
    socd_port1: SocdFilter,
    socd_port1_dpad: SocdFilter,
    socd_port2: SocdFilter,
    socd_port2_dpad: SocdFilter,
    previous_mask: u16,
    previous_chord: bool,
    pending_notices: Vec<InputNotice>,
}

impl Default for InputHub {
    fn default() -> Self {
        Self::new()
    }
}

impl InputHub {
    /// A hub that already knows the keyboard device.
    pub(crate) fn new() -> Self {
        let routing = RoutingTable::default();
        let mut devices = BTreeMap::new();
        if let Some(keyboard) = routing.devices.get(KEYBOARD_DEVICE_ID) {
            devices.insert(
                KEYBOARD_DEVICE_ID.to_string(),
                DeviceState::new(keyboard.identity.clone()),
            );
        }
        Self {
            routing,
            devices,
            repeat: MenuRepeat::default(),
            socd_all: SocdFilter::default(),
            socd_port1: SocdFilter::default(),
            socd_port1_dpad: SocdFilter::default(),
            socd_port2: SocdFilter::default(),
            socd_port2_dpad: SocdFilter::default(),
            previous_mask: 0,
            previous_chord: false,
            pending_notices: Vec::new(),
        }
    }

    /// Consume one poll's device events at `now_ms` (any monotonic clock).
    pub(crate) fn step(&mut self, now_ms: u64, events: Vec<DeviceEvent>) -> InputFrame {
        let mut frame = InputFrame {
            notices: std::mem::take(&mut self.pending_notices),
            ..InputFrame::default()
        };

        for event in events {
            match event {
                DeviceEvent::Connected(identity) => {
                    if let Some(known) = self.devices.get_mut(&identity.id) {
                        known.identity = identity.clone();
                    } else {
                        self.devices
                            .insert(identity.id.clone(), DeviceState::new(identity.clone()));
                        if identity.backend != BackendKind::Touch {
                            frame.notices.push(InputNotice::Connected {
                                device: identity.clone(),
                            });
                        }
                    }
                    let touch = identity.backend == BackendKind::Touch;
                    let id = identity.id.clone();
                    self.routing.connect(identity);
                    if touch {
                        // Showing the controller means the player will use
                        // it: give it a port now, so a game that wants a
                        // DualShock sees one before the first press.
                        self.routing.claim(&id);
                    }
                }
                DeviceEvent::Disconnected(id) => {
                    if id == KEYBOARD_DEVICE_ID {
                        continue;
                    }
                    if let Some(state) = self.devices.remove(&id) {
                        self.routing.disconnect(&id);
                        frame.notices.push(InputNotice::Disconnected {
                            name: state.identity.name,
                        });
                    }
                }
                DeviceEvent::Sample { id, snapshot } => {
                    if let Some(state) = self.devices.get_mut(&id) {
                        state.snapshot = snapshot;
                        state.sampled = true;
                    }
                }
            }
        }

        let mut merged = 0u16;
        for (id, state) in &mut self.devices {
            if !state.sampled {
                continue;
            }
            // Buttons and the stick's D-pad proxy share one guard, so a baseline
            // or a quarantine covers a stick pushed at connect or twitching.
            let filtered = state
                .filter
                .filter(now_ms, state.snapshot.buttons | state.snapshot.stick_dpad);
            match filtered.change {
                Some(ChatterChange::Ignored) => {
                    // A pad that is noise must not hold a controller port.
                    self.routing.release_auto(id);
                    frame.notices.push(InputNotice::Ignored {
                        device: state.identity.clone(),
                    });
                }
                Some(ChatterChange::Restored) => {
                    frame.notices.push(InputNotice::Restored {
                        name: state.identity.name.clone(),
                    });
                }
                None => {}
            }
            if state.filter.is_chattering() {
                state.live = PadSnapshot::default();
                state.mode_down = false;
                continue;
            }
            let live = PadSnapshot {
                buttons: filtered.mask & state.snapshot.buttons,
                stick_dpad: filtered.mask & state.snapshot.stick_dpad,
                ..state.snapshot
            };
            state.live = live;

            // The first deliberate input of an unassigned pad gives it a port.
            if live.is_active() {
                if let Some(port) = self.routing.claim(id) {
                    // Touching the screen is not worth a toast.
                    if state.identity.backend != BackendKind::Touch {
                        frame.notices.push(InputNotice::Claimed {
                            name: state.identity.name.clone(),
                            port,
                        });
                    }
                }
            }

            merged |= live.buttons | live.stick_dpad;
            frame.left_stick = merge_stick(frame.left_stick, live.left);
            frame.right_stick = merge_stick(frame.right_stick, live.right);
            match self.routing.port_for(id) {
                PsxPort::One => frame.port1.merge(&live),
                PsxPort::Two => frame.port2.merge(&live),
                PsxPort::Off => {}
            }

            if live.mode && !state.mode_down {
                let analog = self.routing.toggle_analog(id);
                frame.analog_button = true;
                frame.notices.push(InputNotice::ModeChanged {
                    name: state.identity.name.clone(),
                    analog,
                });
            }
            state.mode_down = live.mode;
        }

        // Select+Start on any device is the menu chord; it never reaches the
        // guest or the menu as two buttons.
        let chord = merged & CHORD_MASK == CHORD_MASK;
        frame.toggle_menu = chord && !self.previous_chord;
        self.previous_chord = chord;
        if chord {
            merged &= !CHORD_MASK;
            frame.port1.mask &= !CHORD_MASK;
            frame.port1.mask_with_stick_dpad &= !CHORD_MASK;
            frame.port2.mask &= !CHORD_MASK;
            frame.port2.mask_with_stick_dpad &= !CHORD_MASK;
        }

        let resolved = self.socd_all.resolve(merged);
        frame.mask = resolved;
        frame.port1.mask = self.socd_port1.resolve(frame.port1.mask);
        frame.port1.mask_with_stick_dpad = self
            .socd_port1_dpad
            .resolve(frame.port1.mask_with_stick_dpad);
        frame.port2.mask = self.socd_port2.resolve(frame.port2.mask);
        frame.port2.mask_with_stick_dpad = self
            .socd_port2_dpad
            .resolve(frame.port2.mask_with_stick_dpad);

        let edges = rising_edges(self.previous_mask, resolved);
        self.previous_mask = resolved;
        frame.menu_confirm = edges & button::CROSS != 0;
        frame.menu_back = edges & button::CIRCLE != 0;
        let steps = self.repeat.update(now_ms, resolved & DIRECTIONS);
        frame.menu_up = steps & button::UP != 0;
        frame.menu_down = steps & button::DOWN != 0;
        frame.menu_left = steps & button::LEFT != 0;
        frame.menu_right = steps & button::RIGHT != 0;
        frame
    }

    /// Whether a device other than `except` is being pressed right now.
    pub(crate) fn other_device_active(&self, except: &str) -> bool {
        self.devices
            .iter()
            .any(|(id, state)| id != except && state.live.is_active())
    }

    /// Make every device's currently held buttons a baseline that does
    /// nothing until released. Call when the menu opens or a game launches,
    /// so a button held across the change cannot act on the new screen.
    pub(crate) fn ignore_held_buttons(&mut self) {
        for state in self.devices.values_mut() {
            state.filter.rebaseline();
        }
        self.repeat.reset();
    }

    /// Current rows for the controller panel.
    pub(crate) fn devices(&self) -> Vec<InputDeviceInfo> {
        self.devices
            .iter()
            .filter_map(|(id, state)| {
                let entry = self.routing.devices.get(id)?;
                Some(InputDeviceInfo {
                    identity: state.identity.clone(),
                    port: entry.port,
                    choice: self.routing.choice_for(id),
                    profile: entry.profile,
                    ignored: state.filter.is_chattering(),
                    live: state.live,
                })
            })
            .collect()
    }

    /// Pin a device to a port, or let it pick one on its first press.
    pub(crate) fn set_device_choice(&mut self, id: &str, choice: PortChoice) -> bool {
        self.routing.set_choice(id, choice)
    }

    /// Select the controller identity a device presents to the guest.
    pub(crate) fn set_device_profile(&mut self, id: &str, profile: PadProfile) -> bool {
        self.routing.set_profile(id, profile)
    }

    /// Monotonic stamp changed by any route or mode edit.
    pub(crate) fn routing_generation(&self) -> u64 {
        self.routing.generation
    }

    /// Apply current Digital/Analog controller identities to a new layout.
    pub(crate) fn apply_layout(&self, bus: &mut Bus) {
        configure_port(bus, PsxPort::One, self.routing.profile_for(PsxPort::One));
        configure_port(bus, PsxPort::Two, self.routing.profile_for(PsxPort::Two));
    }
}

#[cfg(test)]
mod tests {
    use super::super::filter::tests::noisy_face_buttons;
    use super::super::model::StickDpad;
    use super::super::repeat::{END_RATE_HZ, INITIAL_DELAY_MS};
    use super::super::routing::test_identity;
    use super::*;

    const TICK_MS: u64 = 16;

    fn sample(id: &str, buttons: u16) -> DeviceEvent {
        DeviceEvent::Sample {
            id: id.to_string(),
            snapshot: PadSnapshot {
                buttons,
                ..PadSnapshot::default()
            },
        }
    }

    /// A hub with one pad "pad" connected and settled (the first idle poll of
    /// the pad and the keyboard has set their baselines).
    fn hub_with_pad() -> InputHub {
        let mut hub = InputHub::new();
        hub.step(0, vec![DeviceEvent::Connected(test_identity("pad", "Pad"))]);
        hub.step(
            TICK_MS,
            vec![sample("pad", 0), sample(KEYBOARD_DEVICE_ID, 0)],
        );
        hub
    }

    /// Drive a device through `script` (button mask per tick) and collect
    /// everything a person could observe. The keyboard and a pad must give
    /// the same transcript.
    fn transcript(hub: &mut InputHub, device: &str, script: &[u16]) -> Vec<Observed> {
        let mut now = 100;
        script
            .iter()
            .map(|buttons| {
                now += TICK_MS;
                let frame = hub.step(now, vec![sample(device, *buttons)]);
                Observed::of(&frame)
            })
            .collect()
    }

    #[derive(Debug, PartialEq)]
    struct Observed {
        nav: [bool; 4],
        confirm: bool,
        back: bool,
        toggle: bool,
        mask: u16,
        port1: u16,
    }

    impl Observed {
        fn of(frame: &InputFrame) -> Self {
            Self {
                nav: [
                    frame.menu_up,
                    frame.menu_down,
                    frame.menu_left,
                    frame.menu_right,
                ],
                confirm: frame.menu_confirm,
                back: frame.menu_back,
                toggle: frame.toggle_menu,
                mask: frame.mask,
                port1: frame.port1.mask,
            }
        }
    }

    /// A little of everything: a tap, a long hold, a direction change, the
    /// face buttons, the menu chord, and an idle tail.
    fn menu_script() -> Vec<u16> {
        let mut script = vec![0; 3];
        script.extend([button::DOWN, 0, 0]);
        script.extend(std::iter::repeat_n(button::DOWN, 150));
        script.extend([0, 0]);
        script.extend(std::iter::repeat_n(button::UP | button::RIGHT, 30));
        script.extend(std::iter::repeat_n(button::RIGHT, 5));
        script.extend([0, button::CROSS, button::CROSS, 0, button::CIRCLE, 0]);
        script.extend(std::iter::repeat_n(button::START | button::SELECT, 4));
        script.extend([0; 5]);
        script
    }

    fn keyboard_hub() -> InputHub {
        let mut hub = InputHub::new();
        hub.step(0, vec![sample(KEYBOARD_DEVICE_ID, 0)]);
        hub
    }

    #[test]
    fn a_touchscreen_takes_a_port_when_it_connects_so_a_dualshock_is_there_before_a_press() {
        let mut identity = test_identity("touch", "Touchscreen");
        identity.backend = BackendKind::Touch;
        let mut hub = InputHub::new();
        let frame = hub.step(0, vec![DeviceEvent::Connected(identity)]);
        let touch = hub.devices().into_iter().find(|d| d.identity.id == "touch");
        assert_eq!(touch.map(|d| d.port), Some(PsxPort::One));
        assert!(frame.notices.is_empty(), "no toast for the touchscreen");
        // A real pad still claims on its first press.
        hub.step(1, vec![DeviceEvent::Connected(test_identity("pad", "Pad"))]);
        let pad = hub.devices().into_iter().find(|d| d.identity.id == "pad");
        assert_eq!(pad.map(|d| d.port), Some(PsxPort::Off));
    }

    #[test]
    fn the_keyboard_and_a_pad_drive_the_menu_and_the_guest_identically() {
        let script = menu_script();
        let from_keyboard = transcript(&mut keyboard_hub(), KEYBOARD_DEVICE_ID, &script);
        let from_pad = transcript(&mut hub_with_pad(), "pad", &script);
        assert_eq!(from_keyboard, from_pad);

        // And the transcript is not trivially empty.
        let downs = from_pad.iter().filter(|o| o.nav[1]).count();
        assert!(downs > 8, "{downs} menu steps from a long hold");
        assert!(from_pad.iter().any(|o| o.confirm));
        assert!(from_pad.iter().any(|o| o.back));
        assert!(from_pad.iter().any(|o| o.toggle));
        assert!(from_pad.iter().any(|o| o.port1 & button::DOWN != 0));
    }

    #[test]
    fn a_press_on_a_pad_works_the_moment_it_is_made() {
        let mut hub = hub_with_pad();
        let frame = hub.step(100, vec![sample("pad", button::DOWN)]);
        assert!(frame.menu_down);
        assert!(frame.notices.iter().any(|n| matches!(
            n,
            InputNotice::Claimed {
                port: PsxPort::One,
                ..
            }
        )));
        // The same frame already feeds the guest.
        assert_eq!(frame.port1.mask, button::DOWN);
    }

    #[test]
    fn holding_down_scrolls_slowly_then_faster() {
        let mut hub = hub_with_pad();
        let mut steps = Vec::new();
        let mut now = 100;
        let start = now;
        while now < start + 3000 {
            let frame = hub.step(now, vec![sample("pad", button::DOWN)]);
            if frame.menu_down {
                steps.push(now - start);
            }
            now += TICK_MS;
        }
        assert_eq!(steps[0], 0);
        assert!(steps[1] >= INITIAL_DELAY_MS && steps[1] < INITIAL_DELAY_MS + 2 * TICK_MS);
        let early = steps.iter().filter(|t| (300..1300).contains(*t)).count();
        let late = steps.iter().filter(|t| (2000..3000).contains(*t)).count();
        assert!(late > early, "{late} late steps vs {early} early");
        assert!(late as u64 <= END_RATE_HZ + 1, "{late} steps in a second");
    }

    #[test]
    fn a_direction_held_when_the_menu_opens_waits_for_release() {
        let mut hub = hub_with_pad();
        // Mid-hold: scrolling is under way.
        let mut now = 100;
        for _ in 0..40 {
            hub.step(now, vec![sample("pad", button::DOWN)]);
            now += TICK_MS;
        }
        hub.ignore_held_buttons();
        let mut fired = 0;
        for _ in 0..120 {
            fired += usize::from(hub.step(now, vec![sample("pad", button::DOWN)]).menu_down);
            now += TICK_MS;
        }
        assert_eq!(fired, 0, "held through the open: no repeat");
        hub.step(now, vec![sample("pad", 0)]);
        now += TICK_MS;
        assert!(hub.step(now, vec![sample("pad", button::DOWN)]).menu_down);
    }

    #[test]
    fn the_same_holds_for_the_keyboard() {
        let mut hub = keyboard_hub();
        let mut now = 100;
        hub.ignore_held_buttons();
        let mut fired = 0;
        for _ in 0..80 {
            fired += usize::from(
                hub.step(now, vec![sample(KEYBOARD_DEVICE_ID, button::DOWN)])
                    .menu_down,
            );
            now += TICK_MS;
        }
        assert_eq!(fired, 0);
    }

    #[test]
    fn a_noisy_pad_cannot_navigate_scroll_or_hold_a_port() {
        let mut hub = hub_with_pad();
        let mut now = 100;
        let mut navigation_after_ignore = 0;
        let mut ignored = false;
        for poll in 0..400u64 {
            let noise = noisy_face_buttons(poll) | if poll % 3 == 0 { button::DOWN } else { 0 };
            let frame = hub.step(now, vec![sample("pad", noise)]);
            if frame
                .notices
                .iter()
                .any(|n| matches!(n, InputNotice::Ignored { .. }))
            {
                ignored = true;
            }
            if ignored {
                navigation_after_ignore += usize::from(
                    frame.menu_down
                        || frame.menu_confirm
                        || frame.menu_back
                        || frame.port1.mask != 0
                        || frame.mask != 0,
                );
            }
            now += TICK_MS;
        }
        assert!(ignored);
        assert_eq!(navigation_after_ignore, 0);
        let rows = hub.devices();
        let pad = rows
            .iter()
            .find(|r| r.identity.id == "pad")
            .expect("pad row");
        assert!(pad.ignored);
        assert_eq!(pad.port, PsxPort::Off);
    }

    #[test]
    fn a_pad_already_held_at_connect_does_not_claim_a_port_until_pressed_anew() {
        let mut hub = InputHub::new();
        hub.step(
            0,
            vec![
                DeviceEvent::Connected(test_identity("pad", "Pad")),
                sample("pad", button::CROSS),
            ],
        );
        for t in 1..20 {
            let frame = hub.step(t * 16, vec![sample("pad", button::CROSS)]);
            assert!(!frame.menu_confirm);
            assert_eq!(frame.port1.mask, 0);
        }
        assert_eq!(hub.devices()[1].port, PsxPort::Off);
        hub.step(400, vec![sample("pad", 0)]);
        let frame = hub.step(416, vec![sample("pad", button::CROSS)]);
        assert!(frame.menu_confirm);
        assert_eq!(frame.port1.mask, button::CROSS);
    }

    #[test]
    fn the_first_pad_pressed_drives_port_one_and_the_next_port_two() {
        let mut hub = InputHub::new();
        hub.step(
            0,
            vec![
                DeviceEvent::Connected(test_identity("a", "Pad A")),
                DeviceEvent::Connected(test_identity("b", "Pad B")),
            ],
        );
        hub.step(16, vec![sample("a", 0), sample("b", 0)]);
        // B is touched first.
        let frame = hub.step(32, vec![sample("b", button::CROSS)]);
        assert_eq!(frame.port1.mask, button::CROSS);
        let frame = hub.step(48, vec![sample("a", button::SQUARE), sample("b", 0)]);
        assert_eq!(frame.port2.mask, button::SQUARE);
        assert_eq!(frame.port1.mask, 0);
        // Both still reach the menu.
        assert_eq!(frame.mask, button::SQUARE);
    }

    #[test]
    fn the_keyboard_keeps_working_beside_a_pad() {
        let mut hub = hub_with_pad();
        hub.step(100, vec![sample("pad", button::CROSS)]);
        hub.step(116, vec![sample("pad", 0)]);
        let frame = hub.step(132, vec![sample(KEYBOARD_DEVICE_ID, button::DOWN)]);
        assert!(frame.menu_down);
        assert_eq!(frame.port1.mask, button::DOWN);
    }

    #[test]
    fn unplugging_the_pad_announces_it_and_frees_the_port() {
        let mut hub = hub_with_pad();
        hub.step(100, vec![sample("pad", button::CROSS)]);
        let frame = hub.step(116, vec![DeviceEvent::Disconnected("pad".to_string())]);
        assert!(frame
            .notices
            .iter()
            .any(|n| matches!(n, InputNotice::Disconnected { .. })));
        assert_eq!(hub.devices().len(), 1);
    }

    #[test]
    fn the_mode_button_switches_any_device_between_analog_and_its_own_identity() {
        for device in [KEYBOARD_DEVICE_ID, "pad"] {
            let mut hub = hub_with_pad();
            let mode = |down: bool| DeviceEvent::Sample {
                id: device.to_string(),
                snapshot: PadSnapshot {
                    mode: down,
                    ..PadSnapshot::default()
                },
            };
            assert!(hub.step(100, vec![mode(true)]).analog_button);
            let row = |hub: &InputHub| {
                hub.devices()
                    .into_iter()
                    .find(|r| r.identity.id == device)
                    .map(|r| r.profile)
            };
            assert_eq!(row(&hub), Some(PadProfile::Analog), "{device}");
            // Holding it does not toggle again; a second press does.
            assert!(!hub.step(116, vec![mode(true)]).analog_button);
            hub.step(132, vec![mode(false)]);
            assert!(hub.step(148, vec![mode(true)]).analog_button);
            assert_ne!(row(&hub), Some(PadProfile::Analog), "{device}");
        }
    }

    #[test]
    fn select_start_on_either_device_is_the_menu_chord_and_hides_both_buttons() {
        for device in [KEYBOARD_DEVICE_ID, "pad"] {
            let mut hub = hub_with_pad();
            hub.step(100, vec![sample(device, button::SELECT)]);
            let frame = hub.step(116, vec![sample(device, button::SELECT | button::START)]);
            assert!(frame.toggle_menu, "{device}");
            assert_eq!(frame.mask & CHORD_MASK, 0);
            assert_eq!(frame.port1.mask & CHORD_MASK, 0);
            assert!(
                !hub.step(132, vec![sample(device, button::SELECT | button::START)])
                    .toggle_menu
            );
        }
    }

    #[test]
    fn opposite_directions_from_two_devices_resolve_to_the_newer_one() {
        let mut hub = hub_with_pad();
        hub.step(100, vec![sample(KEYBOARD_DEVICE_ID, button::LEFT)]);
        let frame = hub.step(
            116,
            vec![
                sample(KEYBOARD_DEVICE_ID, button::LEFT),
                sample("pad", button::RIGHT),
            ],
        );
        assert_eq!(frame.mask & (button::LEFT | button::RIGHT), button::RIGHT);
    }

    fn stick_sample(id: &str, x: f32, y: f32) -> DeviceEvent {
        let mut stick = StickDpad::default();
        DeviceEvent::Sample {
            id: id.to_string(),
            snapshot: PadSnapshot {
                stick_dpad: stick.update(x, y),
                left: (x, y),
                ..PadSnapshot::default()
            },
        }
    }

    #[test]
    fn a_stick_navigates_the_menu_and_feeds_the_guest_as_a_dpad_only_in_digital_mode() {
        let mut hub = hub_with_pad();
        // A light lean is not a press anywhere.
        let frame = hub.step(100, vec![stick_sample("pad", -0.36, 0.0)]);
        assert!(!frame.menu_left);
        assert_eq!(frame.port1.mask_with_stick_dpad, 0);
        // Pushed firmly: the menu moves and a digital guest sees the D-pad.
        let frame = hub.step(116, vec![stick_sample("pad", -0.9, 0.0)]);
        assert!(frame.menu_left);
        assert_eq!(frame.port1.mask_for(false), button::LEFT);
        // An analog guest reads the axis and no D-pad press.
        assert_eq!(frame.port1.mask_for(true), 0);
        assert_eq!(frame.port1.left_stick, (-0.9, 0.0));
    }

    #[test]
    fn a_stick_held_at_connect_or_menu_open_waits_for_release_like_a_button() {
        let mut hub = hub_with_pad();
        hub.ignore_held_buttons();
        let mut fired = 0;
        for t in 0..30 {
            fired += usize::from(
                hub.step(100 + t * 16, vec![stick_sample("pad", 0.0, -1.0)])
                    .menu_down,
            );
        }
        assert_eq!(fired, 0);
    }

    #[test]
    fn sticks_from_several_devices_merge_per_axis() {
        assert_eq!(merge_stick((0.0, 1.0), (0.25, -0.5)), (0.25, 1.0));
        assert_eq!(merge_stick((-1.0, 0.0), (0.25, -0.5)), (-1.0, -0.5));
    }

    #[test]
    fn panel_rows_show_each_device_with_its_live_state() {
        let mut hub = hub_with_pad();
        hub.step(100, vec![sample("pad", button::CROSS | button::L1)]);
        let rows = hub.devices();
        assert_eq!(rows.len(), 2);
        let pad = rows.iter().find(|r| r.identity.id == "pad").unwrap();
        assert_eq!(pad.live.buttons, button::CROSS | button::L1);
        assert_eq!(pad.port, PsxPort::One);
        assert_eq!(pad.choice, PortChoice::Auto);
        assert!(hub.set_device_choice("pad", PortChoice::Pinned(PsxPort::Two)));
        let pad = hub
            .devices()
            .into_iter()
            .find(|r| r.identity.id == "pad")
            .unwrap();
        assert_eq!(pad.port, PsxPort::Two);
        assert_eq!(pad.choice, PortChoice::Pinned(PsxPort::Two));
    }
}
