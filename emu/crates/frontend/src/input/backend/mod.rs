//! Device adapters: each turns one kind of host input into the logical
//! model in [`super::model`], and does nothing else.

use super::model::DeviceEvent;

#[cfg(any(target_os = "macos", test))]
pub(crate) mod dedupe;
pub(crate) mod keyboard;
pub(crate) mod touch;

#[cfg(target_os = "macos")]
pub(crate) mod game_controller;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod gilrs_pad;
#[cfg(target_os = "macos")]
pub(crate) mod mac;
#[cfg(target_arch = "wasm32")]
pub(crate) mod web;

/// A source of devices. `poll` reports hot-plug as [`DeviceEvent::Connected`] /
/// [`DeviceEvent::Disconnected`] and, for every connected device, one
/// [`DeviceEvent::Sample`] per call.
pub(crate) trait DeviceBackend {
    /// Append this poll's events.
    fn poll(&mut self, out: &mut Vec<DeviceEvent>);
}

/// Whether a controller is a Nintendo-layout one, from its product category, vendor or device
/// name: the Switch Pro Controller, a Joy-Con (pair or single) and the clones that present
/// themselves as one. Their face buttons are labelled the other way round from an Xbox pad's (A is
/// the right button, B the bottom one), and an adapter that reports them by label must read them by
/// position.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
pub(crate) fn is_nintendo_layout(names: &[&str]) -> bool {
    names.iter().any(|name| {
        let name = name.to_ascii_lowercase();
        ["nintendo", "switch", "joy-con", "joycon", "pro controller"]
            .iter()
            .any(|word| name.contains(word))
    })
}

/// The four face buttons by position, as the PS1 layout wants them: bottom (Cross), right
/// (Circle), left (Square), top (Triangle).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
pub(crate) struct FacePositions {
    pub(crate) south: bool,
    pub(crate) east: bool,
    pub(crate) west: bool,
    pub(crate) north: bool,
}

/// The four face buttons as the pad reports them by label.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
pub(crate) struct FaceLabels {
    pub(crate) a: bool,
    pub(crate) b: bool,
    pub(crate) x: bool,
    pub(crate) y: bool,
}

/// Face buttons read by label (`a`, `b`, `x`, `y`) turned into positions. An Xbox-style pad has A
/// at the bottom, B right, X left, Y top; a Nintendo-layout one has B at the bottom, A right,
/// Y left, X top.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
pub(crate) fn face_by_position(labels: FaceLabels, nintendo: bool) -> FacePositions {
    let FaceLabels { a, b, x, y } = labels;
    if nintendo {
        FacePositions {
            south: b,
            east: a,
            west: y,
            north: x,
        }
    } else {
        FacePositions {
            south: a,
            east: b,
            west: x,
            north: y,
        }
    }
}

/// Names of the pad adapters [`platform_pad_backends`] returns, for the log.
#[cfg(target_os = "macos")]
pub(crate) const PLATFORM_PAD_ADAPTERS: &str =
    "GameController (+ gilrs for HID pads it does not claim)";
/// Names of the pad adapters [`platform_pad_backends`] returns, for the log.
#[cfg(all(not(target_os = "macos"), not(target_arch = "wasm32")))]
pub(crate) const PLATFORM_PAD_ADAPTERS: &str = "gilrs";
/// Names of the pad adapters [`platform_pad_backends`] returns, for the log.
#[cfg(target_arch = "wasm32")]
pub(crate) const PLATFORM_PAD_ADAPTERS: &str = "web gamepad";

/// The pad adapters for this platform, in poll order.
pub(crate) fn platform_pad_backends() -> Vec<Box<dyn DeviceBackend>> {
    #[cfg(target_os = "macos")]
    {
        vec![Box::new(mac::MacPads::new())]
    }
    #[cfg(all(not(target_os = "macos"), not(target_arch = "wasm32")))]
    {
        vec![Box::new(gilrs_pad::GilrsBackend::new())]
    }
    #[cfg(target_arch = "wasm32")]
    {
        vec![Box::new(web::WebPadBackend::default())]
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    #[test]
    fn nintendo_pads_are_recognised_by_category_or_name_and_others_are_not() {
        for name in [
            "Switch Pro Controller",
            "Pro Controller",
            "Nintendo Switch Joy-Con (L/R)",
            "Joy-Con (R)",
            "Nintendo Switch Pro Controller",
        ] {
            assert!(is_nintendo_layout(&[name]), "{name}");
        }
        assert!(is_nintendo_layout(&[
            "Xbox Wireless Controller",
            "Switch Pro Controller"
        ]));
        for name in [
            "Xbox Wireless Controller",
            "Xbox 360 Controller",
            "DualSense Wireless Controller",
            "DUALSHOCK 4 Wireless Controller",
            "Generic USB Gamepad",
            "8BitDo SN30 Pro+",
            "",
        ] {
            assert!(!is_nintendo_layout(&[name]), "{name}");
        }
    }

    #[test]
    fn a_nintendo_pad_reads_its_face_buttons_by_position() {
        // Pressing the right button (Nintendo A) must be Circle, the bottom one (B) Cross, the
        // left (Y) Square and the top (X) Triangle.
        let by = |a, b, x, y| face_by_position(FaceLabels { a, b, x, y }, true);
        assert_eq!(
            by(true, false, false, false),
            FacePositions {
                east: true,
                ..Default::default()
            }
        );
        assert_eq!(
            by(false, true, false, false),
            FacePositions {
                south: true,
                ..Default::default()
            }
        );
        assert_eq!(
            by(false, false, true, false),
            FacePositions {
                north: true,
                ..Default::default()
            }
        );
        assert_eq!(
            by(false, false, false, true),
            FacePositions {
                west: true,
                ..Default::default()
            }
        );
        // Everything else keeps the Xbox-style reading.
        let by = |a, b, x, y| face_by_position(FaceLabels { a, b, x, y }, false);
        assert_eq!(
            by(true, false, false, false),
            FacePositions {
                south: true,
                ..Default::default()
            }
        );
        assert_eq!(
            by(false, true, false, false),
            FacePositions {
                east: true,
                ..Default::default()
            }
        );
        assert_eq!(
            by(false, false, true, false),
            FacePositions {
                west: true,
                ..Default::default()
            }
        );
        assert_eq!(
            by(false, false, false, true),
            FacePositions {
                north: true,
                ..Default::default()
            }
        );
    }
}
