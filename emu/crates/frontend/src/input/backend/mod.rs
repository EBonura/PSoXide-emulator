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
