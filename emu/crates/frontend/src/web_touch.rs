//! Browser glue for the touch controller: screen facts, haptics, fullscreen,
//! per-device preferences, and the page events that mean "let go of every
//! button". Native builds have the same functions as no-ops so the shell
//! needs no cfg.

use crate::input::touch::layout::Insets;
use crate::input::touch::prefs::TouchPrefs;

/// Key of the preferences in the browser's local storage.
#[cfg(target_arch = "wasm32")]
const STORAGE_KEY: &str = "psoxide.touch.v1";

#[cfg(target_arch = "wasm32")]
mod imp {
    use std::cell::{Cell, RefCell};

    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::{JsCast, JsValue};

    use super::{Insets, TouchPrefs, STORAGE_KEY};

    thread_local! {
        static RELEASE: Cell<bool> = const { Cell::new(false) };
        static PROBE: RefCell<Option<web_sys::HtmlElement>> = const { RefCell::new(None) };
    }

    fn window() -> Option<web_sys::Window> {
        web_sys::window()
    }

    pub(super) fn coarse_pointer() -> bool {
        window()
            .and_then(|w| w.match_media("(pointer: coarse)").ok().flatten())
            .is_some_and(|query| query.matches())
    }

    /// Whether the player sits in a frame of another page (itch.io embeds it, and puts its own
    /// fullscreen button over the frame's bottom right corner).
    pub(super) fn in_frame() -> bool {
        window().is_some_and(|win| match win.top() {
            Ok(Some(top)) => JsValue::from(top) != JsValue::from(win),
            _ => false,
        })
    }

    /// CSS `env(safe-area-inset-*)` in CSS pixels, read through a hidden
    /// element whose padding uses them.
    pub(super) fn safe_insets_css() -> Insets {
        let Some(win) = window() else {
            return Insets::default();
        };
        // `?safe=top,right,bottom,left` (CSS px) stands in for a notch and a
        // home bar, so a desktop browser can show how the layout copes.
        if let Some(fake) = win.location().search().ok().and_then(|q| {
            q.trim_start_matches('?')
                .split('&')
                .find_map(|kv| kv.strip_prefix("safe=").map(str::to_string))
        }) {
            let v: Vec<f32> = fake.split(',').filter_map(|n| n.parse().ok()).collect();
            if let [top, right, bottom, left] = v[..] {
                return Insets {
                    top,
                    right,
                    bottom,
                    left,
                };
            }
        }
        let probe = PROBE.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_none() {
                let element = win
                    .document()
                    .and_then(|doc| doc.create_element("div").ok())
                    .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok());
                if let Some(element) = element {
                    let _ = element.set_attribute(
                        "style",
                        "position:fixed;left:0;top:0;width:0;height:0;visibility:hidden;\
                         pointer-events:none;padding:env(safe-area-inset-top) \
                         env(safe-area-inset-right) env(safe-area-inset-bottom) \
                         env(safe-area-inset-left)",
                    );
                    if let Some(body) = win.document().and_then(|d| d.body()) {
                        let _ = body.append_child(&element);
                    }
                    *slot = Some(element);
                }
            }
            slot.clone()
        });
        let Some(probe) = probe else {
            return Insets::default();
        };
        let Ok(Some(style)) = win.get_computed_style(&probe) else {
            return Insets::default();
        };
        let read = |name: &str| {
            style
                .get_property_value(name)
                .ok()
                .and_then(|v| v.trim_end_matches("px").parse::<f32>().ok())
                .unwrap_or(0.0)
        };
        Insets {
            top: read("padding-top"),
            right: read("padding-right"),
            bottom: read("padding-bottom"),
            left: read("padding-left"),
        }
    }

    /// `target[name](args...)`, only when `name` is a function there, with a
    /// JavaScript exception turned into `None`. Every optional browser API
    /// goes through this: WebKit has no `vibrate`, no wake lock on old
    /// versions and no Fullscreen API on an iPhone, and a missing method
    /// called through a plain web-sys binding throws out of the wasm and
    /// freezes the page.
    fn call_optional(target: &JsValue, name: &str, args: &[JsValue]) -> Option<JsValue> {
        let function = js_sys::Reflect::get(target, &JsValue::from_str(name)).ok()?;
        let function: js_sys::Function = function.dyn_into().ok()?;
        let args = js_sys::Array::from_iter(args.iter());
        let result = function.apply(target, &args).ok()?;
        // A returned promise may reject (fullscreen refused, orientation
        // lock unsupported); nobody is waiting on it.
        if let Some(promise) = result.dyn_ref::<js_sys::Promise>() {
            let swallow = Closure::once(|_: JsValue| {});
            let _ = promise.catch(&swallow);
            swallow.forget();
        }
        Some(result)
    }

    /// `target[name]`, or `undefined`.
    fn get_optional(target: &JsValue, name: &str) -> JsValue {
        js_sys::Reflect::get(target, &JsValue::from_str(name)).unwrap_or(JsValue::UNDEFINED)
    }

    fn document_value() -> Option<JsValue> {
        window()?.document().map(JsValue::from)
    }

    pub(super) fn vibrate(ms: u32) {
        if let Some(win) = window() {
            let navigator = JsValue::from(win.navigator());
            let _ = call_optional(&navigator, "vibrate", &[JsValue::from(ms)]);
        }
    }

    pub(super) fn fullscreen_available() -> bool {
        // The page's own handler gives the button a use on every browser.
        if window().is_some_and(|w| {
            get_optional(&JsValue::from(w), "psoxideToggleFullscreen").is_function()
        }) {
            return true;
        }
        let Some(doc) = document_value() else {
            return false;
        };
        let enabled = ["fullscreenEnabled", "webkitFullscreenEnabled"]
            .iter()
            .any(|name| get_optional(&doc, name).is_truthy());
        let root = get_optional(&doc, "documentElement");
        enabled
            && ["requestFullscreen", "webkitRequestFullscreen"]
                .iter()
                .any(|name| get_optional(&root, name).is_function())
    }

    pub(super) fn fullscreen_active() -> bool {
        document_value().is_some_and(|doc| {
            ["fullscreenElement", "webkitFullscreenElement"]
                .iter()
                .any(|name| !get_optional(&doc, name).is_falsy())
        })
    }

    pub(super) fn toggle_fullscreen(lock_landscape: bool) {
        // The page owns this (index.html): it enters fullscreen where the API
        // works, and where it does not (an iPhone) opens the player's own page
        // in a tab or explains Add to Home Screen, so the button is never dead.
        if let Some(win) = window() {
            let win = JsValue::from(win);
            if call_optional(
                &win,
                "psoxideToggleFullscreen",
                &[JsValue::from_bool(lock_landscape)],
            )
            .is_some()
            {
                return;
            }
        }
        let Some(doc) = document_value() else {
            return;
        };
        let orientation = window()
            .map(JsValue::from)
            .map(|w| get_optional(&w, "screen"))
            .map(|screen| get_optional(&screen, "orientation"));
        if fullscreen_active() {
            for name in ["exitFullscreen", "webkitExitFullscreen"] {
                if call_optional(&doc, name, &[]).is_some() {
                    break;
                }
            }
            if let Some(orientation) = &orientation {
                let _ = call_optional(orientation, "unlock", &[]);
            }
            return;
        }
        let root = get_optional(&doc, "documentElement");
        for name in ["requestFullscreen", "webkitRequestFullscreen"] {
            if call_optional(&root, name, &[]).is_some() {
                break;
            }
        }
        if lock_landscape {
            if let Some(orientation) = &orientation {
                let _ = call_optional(orientation, "lock", &[JsValue::from_str("landscape")]);
            }
        }
    }

    pub(super) fn load_prefs() -> TouchPrefs {
        window()
            .and_then(|w| w.local_storage().ok().flatten())
            .and_then(|s| s.get_item(STORAGE_KEY).ok().flatten())
            .map_or_else(TouchPrefs::default, |text| TouchPrefs::from_text(&text))
    }

    pub(super) fn local_text(key: &str) -> Option<String> {
        window()
            .and_then(|w| w.local_storage().ok().flatten())
            .and_then(|s| s.get_item(key).ok().flatten())
    }

    pub(super) fn set_local_text(key: &str, text: &str) -> bool {
        window()
            .and_then(|w| w.local_storage().ok().flatten())
            .is_some_and(|s| s.set_item(key, text).is_ok())
    }

    pub(super) fn save_prefs(prefs: &TouchPrefs) {
        if let Some(storage) = window().and_then(|w| w.local_storage().ok().flatten()) {
            let _ = storage.set_item(STORAGE_KEY, &prefs.to_text());
        }
    }

    pub(super) fn install_listeners() {
        let Some(win) = window() else { return };
        let release = Closure::<dyn FnMut(web_sys::Event)>::new(|_| RELEASE.with(|r| r.set(true)));
        let hidden = Closure::<dyn FnMut(web_sys::Event)>::new(|_| {
            if window()
                .and_then(|w| w.document())
                .is_some_and(|d| d.visibility_state() == web_sys::VisibilityState::Hidden)
            {
                RELEASE.with(|r| r.set(true));
            }
        });
        let _ = win.add_event_listener_with_callback("blur", release.as_ref().unchecked_ref());
        let _ = win.add_event_listener_with_callback("pagehide", release.as_ref().unchecked_ref());
        let _ = win.add_event_listener_with_callback(
            "orientationchange",
            release.as_ref().unchecked_ref(),
        );
        if let Some(doc) = win.document() {
            let _ = doc.add_event_listener_with_callback(
                "visibilitychange",
                hidden.as_ref().unchecked_ref(),
            );
        }
        release.forget();
        hidden.forget();
    }

    pub(super) fn take_release_request() -> bool {
        RELEASE.with(|r| r.replace(false))
    }

    pub(super) fn show_fatal(message: &str) {
        if let Some(win) = window() {
            let win = JsValue::from(win);
            let _ = call_optional(&win, "psoxideFatal", &[JsValue::from_str(message)]);
        }
    }

    thread_local! {
        static GUEST: Cell<(bool, u16, bool, bool, f32, f32)> =
            const { Cell::new((false, 0, false, false, 0.0, 0.0)) };
        static EVENTS: Cell<(u32, u32, f32, f32)> = const { Cell::new((0, 0, 0.0, 0.0)) };
    }

    pub(super) fn note_touch(owned: bool, x: f32, y: f32) {
        EVENTS.with(|e| {
            let (n, o, _, _) = e.get();
            e.set((n + 1, o + u32::from(owned), x, y));
        });
    }

    pub(super) fn note_guest(
        visible: bool,
        port1_mask: u16,
        analog: bool,
        menu_open: bool,
        left: (f32, f32),
    ) {
        GUEST.with(|g| g.set((visible, port1_mask, analog, menu_open, left.0, left.1)));
    }

    thread_local! {
        static UX: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
    }

    pub(super) fn note_ux(json: String) {
        UX.with(|u| *u.borrow_mut() = json);
    }

    /// What the page can ask about the player's transport (running, menu, loaded
    /// game, machine generation, port 1 mode), as JSON: for the browser journey
    /// tests in tools/web-journeys.
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = psoxideUxState)]
    pub fn ux_state() -> String {
        UX.with(|u| u.borrow().clone())
    }

    /// What the page can ask the emulator about the controller, as JSON: for
    /// headless tests that prove a tap reached the guest's port 1.
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = psoxideTouchState)]
    pub fn touch_state() -> String {
        let (visible, mask, analog, menu, lx, ly) = GUEST.with(Cell::get);
        let (events, owned, x, y) = EVENTS.with(Cell::get);
        format!(
            "{{\"visible\":{visible},\"port1_mask\":{mask},\"analog\":{analog},\"menu_open\":{menu},\
             \"left_x\":{lx:.2},\"left_y\":{ly:.2},\"touch_events\":{events},\"touch_owned\":{owned},\"last_x\":{x:.1},\"last_y\":{y:.1}}}"
        )
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use super::{Insets, TouchPrefs};

    pub(super) fn coarse_pointer() -> bool {
        false
    }
    pub(super) fn in_frame() -> bool {
        false
    }
    pub(super) fn safe_insets_css() -> Insets {
        Insets::default()
    }
    pub(super) fn vibrate(_ms: u32) {}
    pub(super) fn fullscreen_available() -> bool {
        false
    }
    pub(super) fn fullscreen_active() -> bool {
        false
    }
    pub(super) fn toggle_fullscreen(_lock_landscape: bool) {}
    pub(super) fn load_prefs() -> TouchPrefs {
        TouchPrefs::default()
    }
    #[allow(dead_code)]
    pub(super) fn local_text(_key: &str) -> Option<String> {
        None
    }
    #[allow(dead_code)]
    pub(super) fn set_local_text(_key: &str, _text: &str) -> bool {
        false
    }
    pub(super) fn save_prefs(_prefs: &TouchPrefs) {}
    pub(super) fn install_listeners() {}
    #[allow(dead_code)]
    pub(super) fn take_release_request() -> bool {
        false
    }
    #[allow(dead_code)]
    pub(super) fn show_fatal(_message: &str) {}
    pub(super) fn note_guest(
        _visible: bool,
        _port1_mask: u16,
        _analog: bool,
        _menu_open: bool,
        _left: (f32, f32),
    ) {
    }
    pub(super) fn note_touch(_owned: bool, _x: f32, _y: f32) {}
    #[allow(dead_code)]
    pub(super) fn note_ux(_json: String) {}
}

/// Whether the browser says the primary pointer is a finger.
pub(crate) fn coarse_pointer() -> bool {
    imp::coarse_pointer()
}

/// Whether the player is shown inside a frame of another page.
pub(crate) fn in_frame() -> bool {
    imp::in_frame()
}

/// Safe-area insets in CSS pixels (notch, rounded corners, home indicator).
pub(crate) fn safe_insets_css() -> Insets {
    imp::safe_insets_css()
}

/// A short vibration tick, where the browser supports it.
pub(crate) fn vibrate(ms: u32) {
    imp::vibrate(ms);
}

/// Whether the page can enter fullscreen (iPhone Safari cannot).
pub(crate) fn fullscreen_available() -> bool {
    imp::fullscreen_available()
}

/// Whether the page is fullscreen now.
#[allow(dead_code)]
pub(crate) fn fullscreen_active() -> bool {
    imp::fullscreen_active()
}

/// Enter or leave fullscreen; entering can ask for landscape.
pub(crate) fn toggle_fullscreen(lock_landscape: bool) {
    imp::toggle_fullscreen(lock_landscape);
}

/// The preferences saved on this device.
pub(crate) fn load_prefs() -> TouchPrefs {
    imp::load_prefs()
}

/// Save the preferences on this device.
pub(crate) fn save_prefs(prefs: &TouchPrefs) {
    imp::save_prefs(prefs);
}

/// Listen for blur, page hide and rotation, which must release every button.
pub(crate) fn install_listeners() {
    imp::install_listeners();
}

/// Whether a page event asked to release every held button since the last call.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn take_release_request() -> bool {
    imp::take_release_request()
}

/// Record what the guest's port 1 was fed this frame, for [`psoxideTouchState`]
/// (a no-op on native).
pub(crate) fn note_guest(
    visible: bool,
    port1_mask: u16,
    analog: bool,
    menu_open: bool,
    left: (f32, f32),
) {
    imp::note_guest(visible, port1_mask, analog, menu_open, left);
}

/// The touch controller's layout as JSON, in window pixels: the centre of every
/// pad button (`[mask, x, y]`), the D-pad (`[x, y, half_span]`), the sticks
/// (`[side, x, y, ring]`) and the interface buttons (`[id, x, y]`), for the
/// browser journey tests to aim at.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn layout_probe_json(
    layout: &crate::input::touch::layout::TouchLayout,
    safe: crate::input::touch::layout::Insets,
) -> String {
    use crate::input::touch::layout::{StickSide, UiId};
    let buttons = layout
        .buttons
        .iter()
        .map(|b| {
            let c = b.hit.center();
            format!("[{},{:.1},{:.1}]", b.mask, c.x, c.y)
        })
        .collect::<Vec<_>>()
        .join(",");
    let sticks = layout
        .sticks
        .iter()
        .map(|s| {
            let side = if s.side == StickSide::Left { "L" } else { "R" };
            format!("[\"{side}\",{:.1},{:.1},{:.1}]", s.rest.x, s.rest.y, s.ring)
        })
        .collect::<Vec<_>>()
        .join(",");
    let ui = layout
        .ui
        .iter()
        .map(|u| {
            let id = match u.id {
                UiId::Pause => "pause",
                UiId::Save => "save",
                UiId::Debug => "debug",
                UiId::Menu => "menu",
                UiId::Fullscreen => "fullscreen",
            };
            let c = u.hit.center();
            format!("[\"{id}\",{:.1},{:.1}]", c.x, c.y)
        })
        .collect::<Vec<_>>()
        .join(",");
    // What is drawn, as [mask-or-0, x0, y0, x1, y1] (pad buttons, then interface buttons).
    let rects = layout
        .buttons
        .iter()
        .map(|b| (b.mask, b.draw.bounds()))
        .chain(layout.ui.iter().map(|u| (0, u.draw)))
        .map(|(id, r)| {
            format!(
                "[{id},{:.1},{:.1},{:.1},{:.1}]",
                r.min.x, r.min.y, r.max.x, r.max.y
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"buttons\":[{buttons}],\"dpad\":[{:.1},{:.1},{:.1}],\"sticks\":[{sticks}],\"ui\":[{ui}],\
         \"rects\":[{rects}],\"safe\":[{:.1},{:.1},{:.1},{:.1}]}}",
        layout.dpad.center.x,
        layout.dpad.center.y,
        layout.dpad.half_span,
        safe.left,
        safe.top,
        safe.right,
        safe.bottom
    )
}

/// A string kept in the browser's `localStorage` (web only; `None` elsewhere or
/// when storage is blocked).
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn local_text(key: &str) -> Option<String> {
    imp::local_text(key)
}

/// Keep a string in `localStorage` (web only). Returns whether it was stored.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn set_local_text(key: &str, text: &str) -> bool {
    imp::set_local_text(key, text)
}

/// Publish the player's transport for [`psoxideUxState`] (a no-op on native).
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn note_ux(json: String) {
    imp::note_ux(json);
}

/// Count one touch event for [`psoxideTouchState`] (a no-op on native).
pub(crate) fn note_touch(owned: bool, x: f32, y: f32) {
    imp::note_touch(owned, x, y);
}

/// Show a fatal error on the page itself (web only), for failures on devices
/// with no console.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn show_fatal(message: &str) {
    imp::show_fatal(message);
}
