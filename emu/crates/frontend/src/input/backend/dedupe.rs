//! One physical controller must not appear twice.
//!
//! On macOS an Xbox, PlayStation or Switch controller is visible both to the
//! GameController framework and, as its HID side door, to gilrs. The
//! framework is the better view of those (standard layout, every
//! connection type), so each framework controller hides one HID twin from a
//! vendor the framework handles. A HID device the framework does not claim
//! (a clone, an arcade stick, an adapter) is left alone and still works,
//! behind the same noise guard.

use std::collections::BTreeSet;

/// USB vendor ids the GameController framework drives itself: Microsoft,
/// Sony and Nintendo.
const FRAMEWORK_VENDORS: [u16; 3] = [0x045e, 0x054c, 0x057e];

/// Ids of the HID devices to hide, given how many framework controllers are
/// connected. `hid` lists `(id, vid:pid)` of every HID pad gilrs reports.
pub(crate) fn hidden_hid_twins(
    framework_controllers: usize,
    hid: &[(String, Option<(u16, u16)>)],
) -> BTreeSet<String> {
    let mut candidates: Vec<&str> = hid
        .iter()
        .filter(|(_, vid_pid)| vid_pid.is_some_and(|(vid, _)| FRAMEWORK_VENDORS.contains(&vid)))
        .map(|(id, _)| id.as_str())
        .collect();
    candidates.sort_unstable();
    candidates
        .into_iter()
        .take(framework_controllers)
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hid(entries: &[(&str, Option<(u16, u16)>)]) -> Vec<(String, Option<(u16, u16)>)> {
        entries.iter().map(|(i, v)| (i.to_string(), *v)).collect()
    }

    #[test]
    fn a_framework_controller_hides_its_hid_twin() {
        let pads = hid(&[("gilrs:0", Some((0x045e, 0x0b13)))]);
        assert_eq!(hidden_hid_twins(1, &pads).len(), 1);
    }

    #[test]
    fn without_a_framework_controller_nothing_is_hidden() {
        let pads = hid(&[("gilrs:0", Some((0x045e, 0x0b13)))]);
        assert!(hidden_hid_twins(0, &pads).is_empty());
    }

    #[test]
    fn other_vendors_are_never_hidden() {
        let pads = hid(&[("gilrs:0", Some((0x0079, 0x0006))), ("gilrs:1", None)]);
        assert!(hidden_hid_twins(2, &pads).is_empty());
    }

    #[test]
    fn two_framework_controllers_hide_two_twins_and_leave_a_third_alone() {
        let pads = hid(&[
            ("gilrs:0", Some((0x054c, 0x0ce6))),
            ("gilrs:1", Some((0x045e, 0x0b12))),
            ("gilrs:2", Some((0x045e, 0x028e))),
        ]);
        let hidden = hidden_hid_twins(2, &pads);
        assert_eq!(hidden.len(), 2);
        assert!(!hidden.contains("gilrs:2"));
    }
}
