//! Which host device drives which PlayStation controller port.
//!
//! The table only knows device ids and names. By default every pad starts
//! unassigned and claims the lowest free port on its first real press, so the
//! first device you touch drives port 1 and the next drives port 2. The
//! keyboard is an ordinary device that happens to start on port 1 and may
//! share a port with one pad. The controller panel can override any of this
//! per device.

use std::collections::BTreeMap;

use emulator_core::Bus;

use super::model::{BackendKind, DeviceIdentity};

/// Stable identifier used by the routing panel for keyboard input.
pub(crate) const KEYBOARD_DEVICE_ID: &str = "keyboard";

/// A host device's guest-controller destination.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum PsxPort {
    /// Device is visible to the emulator UI but not to the guest.
    #[default]
    Off,
    /// PlayStation controller port 1.
    One,
    /// PlayStation controller port 2.
    Two,
}

impl PsxPort {
    /// Compact label used by the controller panel.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::One => "Port 1",
            Self::Two => "Port 2",
        }
    }
}

/// What the panel's port picker offers: let the device claim a port on its
/// first press, or pin it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PortChoice {
    /// A pad takes the lowest free port on its first press; the keyboard
    /// stays on port 1.
    Auto,
    /// Pinned to a port (or to none).
    Pinned(PsxPort),
}

impl PortChoice {
    /// Label shown in the picker.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Pinned(port) => port.label(),
        }
    }

    /// Every choice, in picker order.
    pub(crate) const ALL: [Self; 4] = [
        Self::Auto,
        Self::Pinned(PsxPort::One),
        Self::Pinned(PsxPort::Two),
        Self::Pinned(PsxPort::Off),
    ];
}

/// What a routed host device presents to the guest on its port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PadProfile {
    /// An original digital pad: always ID 0x41, ignores the DualShock
    /// configuration commands.
    Digital,
    /// A DualShock as it powers up on hardware: ID 0x41 until the game sends
    /// the analog-mode handshake (or the Mode button is pressed).
    DualShock,
    /// A DualShock already switched to analog mode: ID 0x73 from the first
    /// poll, as if the Analog button had been pressed before the game began.
    Analog,
}

impl PadProfile {
    /// Short label of the controller panel's mode button.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Digital => "Digital",
            Self::DualShock => "DualShock",
            Self::Analog => "Analog",
        }
    }

    /// Hover text describing what the guest sees.
    pub(crate) const fn description(self) -> &'static str {
        match self {
            Self::Digital => "Expose an original digital pad (ID 0x41)",
            Self::DualShock => {
                "Expose a DualShock that starts in digital mode (ID 0x41) and goes analog \
                 (ID 0x73) when the game asks for it"
            }
            Self::Analog => "Expose a DualShock already in analog mode (ID 0x73)",
        }
    }

    /// The next profile in the panel's button cycle.
    pub(crate) const fn next(self) -> Self {
        match self {
            Self::Digital => Self::DualShock,
            Self::DualShock => Self::Analog,
            Self::Analog => Self::Digital,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RoutingEntry {
    pub(crate) identity: DeviceIdentity,
    pub(crate) port: PsxPort,
    pub(crate) profile: PadProfile,
    /// The user pinned this device in the panel: never auto-assign it.
    pub(crate) pinned: bool,
}

impl RoutingEntry {
    fn is_keyboard(&self) -> bool {
        self.identity.backend == BackendKind::Keyboard
    }
}

/// Host-device assignments, independent of how devices are read.
#[derive(Clone, Debug)]
pub(crate) struct RoutingTable {
    pub(crate) devices: BTreeMap<String, RoutingEntry>,
    pub(crate) generation: u64,
}

impl Default for RoutingTable {
    fn default() -> Self {
        let mut table = Self {
            devices: BTreeMap::new(),
            generation: 1,
        };
        table.devices.insert(
            KEYBOARD_DEVICE_ID.to_string(),
            RoutingEntry {
                identity: DeviceIdentity {
                    id: KEYBOARD_DEVICE_ID.to_string(),
                    name: "Keyboard".to_string(),
                    backend: BackendKind::Keyboard,
                    vid_pid: None,
                    mapping: "bindings".to_string(),
                    mapped: true,
                },
                port: PsxPort::One,
                profile: PadProfile::Digital,
                pinned: false,
            },
        );
        table
    }
}

impl RoutingTable {
    /// A device appeared. Pads start unassigned; they claim a port on their
    /// first real press ([`Self::claim`]).
    pub(crate) fn connect(&mut self, identity: DeviceIdentity) {
        if let Some(existing) = self.devices.get_mut(&identity.id) {
            existing.identity = identity;
            return;
        }
        self.devices.insert(
            identity.id.clone(),
            RoutingEntry {
                identity,
                port: PsxPort::Off,
                // A host controller appears as a DualShock the way one powers
                // up on hardware: digital (ID 0x41) until the game sends the
                // analog handshake, which every DualShock-aware guest does.
                // Starting in analog mode instead made digital-only retail
                // games such as Tomb Raider pause on "No Controller", because
                // they accept nothing but ID 0x41. The routing panel still
                // offers the always-analog profile for guests that never ask.
                profile: PadProfile::DualShock,
                pinned: false,
            },
        );
        self.bump();
    }

    /// A device went away; its port is free again.
    pub(crate) fn disconnect(&mut self, id: &str) {
        if id == KEYBOARD_DEVICE_ID || self.devices.remove(id).is_none() {
            return;
        }
        self.bump();
    }

    /// First real press of an unassigned pad: take the lowest port no other
    /// pad holds. Returns the port when one was taken.
    pub(crate) fn claim(&mut self, id: &str) -> Option<PsxPort> {
        let entry = self.devices.get(id)?;
        if entry.is_keyboard() || entry.pinned || entry.port != PsxPort::Off {
            return None;
        }
        let port = [PsxPort::One, PsxPort::Two]
            .into_iter()
            .find(|port| !self.pad_holds(*port))?;
        if let Some(entry) = self.devices.get_mut(id) {
            entry.port = port;
        }
        self.bump();
        Some(port)
    }

    /// A pad that is being ignored (noise) gives its port back, so it cannot
    /// hold a slot it is not using; it may claim one again once it is trusted.
    pub(crate) fn release_auto(&mut self, id: &str) {
        let Some(entry) = self.devices.get_mut(id) else {
            return;
        };
        if entry.is_keyboard() || entry.pinned || entry.port == PsxPort::Off {
            return;
        }
        entry.port = PsxPort::Off;
        self.bump();
    }

    /// Apply a panel choice. Pinning a pad to a port moves another pad off it
    /// (a port has one pad; the keyboard may share). Returns whether anything
    /// changed.
    pub(crate) fn set_choice(&mut self, id: &str, choice: PortChoice) -> bool {
        let Some(entry) = self.devices.get(id) else {
            return false;
        };
        let is_keyboard = entry.is_keyboard();
        let (was_port, was_pinned) = (entry.port, entry.pinned);
        match choice {
            PortChoice::Auto => {
                if !was_pinned {
                    return false;
                }
                // Back to the default: the keyboard on port 1, a pad off until
                // it is next pressed.
                let port = if is_keyboard {
                    PsxPort::One
                } else {
                    PsxPort::Off
                };
                if let Some(entry) = self.devices.get_mut(id) {
                    entry.pinned = false;
                    entry.port = port;
                }
            }
            PortChoice::Pinned(port) => {
                if was_pinned && was_port == port {
                    return false;
                }
                if port != PsxPort::Off && !is_keyboard {
                    self.clear_pad_port(port, id);
                }
                if let Some(entry) = self.devices.get_mut(id) {
                    entry.pinned = true;
                    entry.port = port;
                }
            }
        }
        self.bump();
        true
    }

    /// The choice the panel should show for a device.
    pub(crate) fn choice_for(&self, id: &str) -> PortChoice {
        match self.devices.get(id) {
            Some(entry) if entry.pinned => PortChoice::Pinned(entry.port),
            _ => PortChoice::Auto,
        }
    }

    pub(crate) fn set_profile(&mut self, id: &str, profile: PadProfile) -> bool {
        let Some(entry) = self.devices.get_mut(id) else {
            return false;
        };
        if entry.profile == profile {
            return false;
        }
        entry.profile = profile;
        self.bump();
        true
    }

    /// The Analog / Mode button: switch to analog, or back to the device's
    /// own digital identity (a keyboard's original pad, a controller's
    /// DualShock). Returns whether the device is analog afterwards.
    pub(crate) fn toggle_analog(&mut self, id: &str) -> bool {
        let Some(entry) = self.devices.get_mut(id) else {
            return false;
        };
        entry.profile = match entry.profile {
            PadProfile::Analog if entry.is_keyboard() => PadProfile::Digital,
            PadProfile::Analog => PadProfile::DualShock,
            PadProfile::Digital | PadProfile::DualShock => PadProfile::Analog,
        };
        let analog = entry.profile == PadProfile::Analog;
        self.bump();
        analog
    }

    pub(crate) fn port_for(&self, id: &str) -> PsxPort {
        self.devices
            .get(id)
            .map(|entry| entry.port)
            .unwrap_or(PsxPort::Off)
    }

    /// The identity the guest sees on `port`: a pad's wins over the keyboard
    /// sharing the port with it.
    pub(crate) fn profile_for(&self, port: PsxPort) -> Option<PadProfile> {
        let on_port = || {
            self.devices
                .values()
                .filter(move |entry| entry.port == port)
        };
        on_port()
            .find(|entry| !entry.is_keyboard())
            .or_else(|| on_port().next())
            .map(|entry| entry.profile)
    }

    /// Whether a pad (not the keyboard) holds `port`.
    fn pad_holds(&self, port: PsxPort) -> bool {
        self.devices
            .values()
            .any(|entry| !entry.is_keyboard() && entry.port == port)
    }

    fn clear_pad_port(&mut self, port: PsxPort, except_id: &str) {
        for (id, entry) in &mut self.devices {
            if id != except_id && !entry.is_keyboard() && entry.port == port {
                entry.port = PsxPort::Off;
            }
        }
    }

    fn bump(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }
}

/// Apply a port's controller identity to the guest bus.
pub(crate) fn configure_port(bus: &mut Bus, port: PsxPort, profile: Option<PadProfile>) {
    match (port, profile) {
        (PsxPort::One, None) => bus.detach_pad_port1(),
        (PsxPort::One, Some(PadProfile::Digital)) => bus.attach_original_digital_pad_port1(),
        (PsxPort::One, Some(PadProfile::DualShock)) => bus.attach_digital_pad_port1(),
        (PsxPort::One, Some(PadProfile::Analog)) => {
            bus.attach_digital_pad_port1();
            let _ = bus.force_port1_analog_mode();
        }
        (PsxPort::Two, None) => bus.detach_pad_port2(),
        (PsxPort::Two, Some(PadProfile::Digital)) => bus.attach_original_digital_pad_port2(),
        (PsxPort::Two, Some(PadProfile::DualShock)) => bus.attach_digital_pad_port2(),
        (PsxPort::Two, Some(PadProfile::Analog)) => {
            bus.attach_digital_pad_port2();
            let _ = bus.force_port2_analog_mode();
        }
        (PsxPort::Off, _) => {}
    }
}

#[cfg(test)]
pub(crate) fn test_identity(id: &str, name: &str) -> DeviceIdentity {
    DeviceIdentity {
        id: id.to_string(),
        name: name.to_string(),
        backend: BackendKind::Gilrs,
        vid_pid: Some((0x045e, 0x0b13)),
        mapping: "test".to_string(),
        mapped: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl RoutingTable {
        fn set_pin(&mut self, id: &str, port: PsxPort) -> bool {
            self.set_choice(id, PortChoice::Pinned(port))
        }
    }

    fn pad(id: &str) -> DeviceIdentity {
        test_identity(id, id)
    }

    #[test]
    fn a_new_pad_waits_for_its_first_press_and_then_takes_port_one() {
        let mut routes = RoutingTable::default();
        routes.connect(pad("pad-a"));
        assert_eq!(routes.port_for("pad-a"), PsxPort::Off);
        // The keyboard still drives port 1 until a pad is actually used.
        assert_eq!(routes.port_for(KEYBOARD_DEVICE_ID), PsxPort::One);

        assert_eq!(routes.claim("pad-a"), Some(PsxPort::One));
        assert_eq!(routes.port_for("pad-a"), PsxPort::One);
        // The keyboard shares port 1 with it; it is a device like another.
        assert_eq!(routes.port_for(KEYBOARD_DEVICE_ID), PsxPort::One);
        // A pad on the port decides what the guest sees.
        assert_eq!(
            routes.profile_for(PsxPort::One),
            Some(PadProfile::DualShock)
        );
    }

    #[test]
    fn the_second_pad_to_press_takes_port_two_and_a_third_stays_off() {
        let mut routes = RoutingTable::default();
        for id in ["a", "b", "c"] {
            routes.connect(pad(id));
        }
        assert_eq!(routes.claim("b"), Some(PsxPort::One));
        assert_eq!(routes.claim("a"), Some(PsxPort::Two));
        assert_eq!(routes.claim("c"), None);
        assert_eq!(routes.port_for("c"), PsxPort::Off);
        // Claiming twice changes nothing.
        assert_eq!(routes.claim("b"), None);
    }

    #[test]
    fn disconnecting_a_pad_frees_its_port_for_the_next_press() {
        let mut routes = RoutingTable::default();
        routes.connect(pad("a"));
        routes.connect(pad("b"));
        assert_eq!(routes.claim("a"), Some(PsxPort::One));
        routes.disconnect("a");
        assert_eq!(routes.claim("b"), Some(PsxPort::One));
    }

    #[test]
    fn pinning_moves_another_pad_off_the_port_but_not_the_keyboard() {
        let mut routes = RoutingTable::default();
        routes.connect(pad("a"));
        routes.connect(pad("b"));
        routes.claim("a");
        assert!(routes.set_pin("b", PsxPort::One));
        assert_eq!(routes.port_for("a"), PsxPort::Off);
        assert_eq!(routes.port_for("b"), PsxPort::One);
        assert_eq!(routes.port_for(KEYBOARD_DEVICE_ID), PsxPort::One);
        // A pinned pad is never auto-assigned elsewhere.
        assert_eq!(routes.claim("b"), None);
        assert!(routes.set_pin("b", PsxPort::Off));
        assert_eq!(routes.claim("b"), None);
    }

    #[test]
    fn auto_puts_a_pad_back_to_first_press_and_the_keyboard_on_port_one() {
        let mut routes = RoutingTable::default();
        routes.connect(pad("a"));
        assert!(routes.set_pin(KEYBOARD_DEVICE_ID, PsxPort::Two));
        assert!(routes.set_pin("a", PsxPort::Two));
        assert_eq!(routes.choice_for("a"), PortChoice::Pinned(PsxPort::Two));
        assert!(routes.set_choice(KEYBOARD_DEVICE_ID, PortChoice::Auto));
        assert!(routes.set_choice("a", PortChoice::Auto));
        assert_eq!(routes.port_for(KEYBOARD_DEVICE_ID), PsxPort::One);
        assert_eq!(routes.port_for("a"), PsxPort::Off);
        assert_eq!(routes.claim("a"), Some(PsxPort::One));
    }

    #[test]
    fn a_noisy_pad_gives_its_port_back() {
        let mut routes = RoutingTable::default();
        routes.connect(pad("a"));
        routes.connect(pad("b"));
        routes.claim("a");
        routes.release_auto("a");
        assert_eq!(routes.claim("b"), Some(PsxPort::One));
    }

    #[test]
    fn analog_choice_is_part_of_the_port_profile() {
        let mut routes = RoutingTable::default();
        assert_eq!(routes.profile_for(PsxPort::One), Some(PadProfile::Digital));
        assert!(routes.set_profile(KEYBOARD_DEVICE_ID, PadProfile::Analog));
        assert_eq!(routes.profile_for(PsxPort::One), Some(PadProfile::Analog));
    }

    #[test]
    fn a_connected_controller_powers_up_as_a_digital_mode_dualshock() {
        use emulator_core::pad::PadMode;

        let mut routes = RoutingTable::default();
        routes.connect(pad("pad-a"));
        routes.claim("pad-a");
        assert_eq!(
            routes.profile_for(PsxPort::One),
            Some(PadProfile::DualShock)
        );

        // ID 0x41 at power-up: a digital-only retail game accepts it.
        let mut bus = Bus::new_without_bios();
        configure_port(&mut bus, PsxPort::One, routes.profile_for(PsxPort::One));
        assert_eq!(bus.port1_pad_mode(), Some(PadMode::Digital));

        // The Mode button goes analog and back to the controller's own
        // identity, never to the keyboard's original pad.
        assert!(routes.toggle_analog("pad-a"));
        assert_eq!(routes.profile_for(PsxPort::One), Some(PadProfile::Analog));
        assert!(!routes.toggle_analog("pad-a"));
        assert_eq!(
            routes.profile_for(PsxPort::One),
            Some(PadProfile::DualShock)
        );

        assert!(routes.toggle_analog(KEYBOARD_DEVICE_ID));
        assert!(!routes.toggle_analog(KEYBOARD_DEVICE_ID));
        assert_eq!(
            routes.devices.get(KEYBOARD_DEVICE_ID).map(|e| e.profile),
            Some(PadProfile::Digital)
        );
    }

    #[test]
    fn controller_profiles_rebuild_both_bus_ports() {
        use emulator_core::pad::PadMode;

        let mut bus = Bus::new_without_bios();
        configure_port(&mut bus, PsxPort::One, Some(PadProfile::Digital));
        configure_port(&mut bus, PsxPort::Two, Some(PadProfile::Analog));
        assert_eq!(bus.port1_pad_mode(), Some(PadMode::Digital));
        assert_eq!(bus.port2_pad_mode(), Some(PadMode::Analog));

        configure_port(&mut bus, PsxPort::One, None);
        configure_port(&mut bus, PsxPort::Two, None);
        assert_eq!(bus.port1_pad_mode(), None);
        assert_eq!(bus.port2_pad_mode(), None);
    }
}
