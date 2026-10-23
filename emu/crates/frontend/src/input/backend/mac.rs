//! macOS: GameController first, HID for what it does not claim.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use crate::input::model::{DeviceEvent, DeviceIdentity, PadSnapshot};

use super::dedupe::hidden_hid_twins;
use super::game_controller::GameControllerBackend;
use super::gilrs_pad::GilrsBackend;
use super::DeviceBackend;

/// How long a HID pad must be known before it is announced. The framework
/// can take a moment to list a controller that HID already shows, and
/// announcing the HID twin first would flash a connect/disconnect pair.
const HID_ANNOUNCE_GRACE: Duration = Duration::from_millis(600);

/// The pad adapters on macOS, combined so one controller shows up once.
pub(crate) struct MacPads {
    framework: GameControllerBackend,
    hid: GilrsBackend,
    /// Every HID pad gilrs reports, whether shown or hidden.
    hid_devices: BTreeMap<String, (DeviceIdentity, PadSnapshot)>,
    /// When each HID pad was first seen.
    first_seen: BTreeMap<String, Instant>,
    /// HID pads currently announced to the hub.
    shown: BTreeSet<String>,
}

impl MacPads {
    /// Start both adapters.
    pub(crate) fn new() -> Self {
        Self {
            framework: GameControllerBackend::new(),
            hid: GilrsBackend::new(),
            hid_devices: BTreeMap::new(),
            first_seen: BTreeMap::new(),
            shown: BTreeSet::new(),
        }
    }
}

impl DeviceBackend for MacPads {
    fn poll(&mut self, out: &mut Vec<DeviceEvent>) {
        self.framework.poll(out);

        let mut hid_events = Vec::new();
        self.hid.poll(&mut hid_events);
        for event in hid_events {
            match event {
                DeviceEvent::Connected(identity) => {
                    self.first_seen
                        .entry(identity.id.clone())
                        .or_insert_with(Instant::now);
                    self.hid_devices
                        .insert(identity.id.clone(), (identity, PadSnapshot::default()));
                }
                DeviceEvent::Disconnected(id) => {
                    self.first_seen.remove(&id);
                    self.hid_devices.remove(&id);
                }
                DeviceEvent::Sample { id, snapshot } => {
                    if let Some(entry) = self.hid_devices.get_mut(&id) {
                        entry.1 = snapshot;
                    }
                }
            }
        }

        let listing: Vec<(String, Option<(u16, u16)>)> = self
            .hid_devices
            .iter()
            .map(|(id, (identity, _))| (id.clone(), identity.vid_pid))
            .collect();
        let hidden = hidden_hid_twins(self.framework.controller_count(), &listing);

        // Announce, withdraw and sample the HID pads that should be visible.
        let gone: Vec<String> = self
            .shown
            .iter()
            .filter(|id| !self.hid_devices.contains_key(*id) || hidden.contains(*id))
            .cloned()
            .collect();
        for id in gone {
            self.shown.remove(&id);
            out.push(DeviceEvent::Disconnected(id));
        }
        for (id, (identity, snapshot)) in &self.hid_devices {
            let settled = self
                .first_seen
                .get(id)
                .is_some_and(|seen| seen.elapsed() >= HID_ANNOUNCE_GRACE);
            if hidden.contains(id) || (!settled && !self.shown.contains(id)) {
                continue;
            }
            if self.shown.insert(id.clone()) {
                out.push(DeviceEvent::Connected(identity.clone()));
            }
            out.push(DeviceEvent::Sample {
                id: id.clone(),
                snapshot: *snapshot,
            });
        }
    }
}
