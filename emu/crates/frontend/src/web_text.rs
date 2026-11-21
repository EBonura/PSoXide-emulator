//! The phone's own keyboard for the home shelf's search box (web only).
//!
//! The shell is drawn on a canvas, and a browser only raises its on-screen keyboard for a
//! focused text field. So the page keeps one invisible `<input>` over the search box, and a
//! touch that ends on the box focuses it from inside the touch's own event handler (iOS raises
//! the keyboard only for a focus made there, never for one made later in a frame). What is
//! typed comes back as the field's whole value, Enter and the keyboard going away close the
//! box. A mouse and a real keyboard never use it: they type through the window as before.
//!
//! Native builds have the same functions as no-ops so the shell needs no cfg.

// The shell drives it from the web frame loop only.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

/// What the field reported since the last frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TextEvent {
    /// The field's whole text now.
    Value(String),
    /// Enter, or the keyboard was put away: close the box, keep the filter.
    Done,
}

#[cfg(target_arch = "wasm32")]
mod imp {
    use std::cell::{Cell, RefCell};

    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::JsCast;

    use super::TextEvent;

    const FIELD_ID: &str = "psoxide-search-field";

    thread_local! {
        /// The search box on the canvas this frame, in CSS pixels of the canvas: x, y, w, h.
        static TARGET: Cell<Option<[f32; 4]>> = const { Cell::new(None) };
        /// The query the shell holds, so a new focus starts from it.
        static QUERY: RefCell<String> = const { RefCell::new(String::new()) };
        static EVENTS: RefCell<Vec<TextEvent>> = const { RefCell::new(Vec::new()) };
        static FIELD: RefCell<Option<web_sys::HtmlInputElement>> = const { RefCell::new(None) };
        /// When a touch last focused the field (ms): the shell sees the tap a frame or two
        /// later, and until then a closed box must not put the keyboard away again.
        static FOCUSED_AT: Cell<f64> = const { Cell::new(0.0) };
    }

    fn document() -> Option<web_sys::Document> {
        web_sys::window()?.document()
    }

    fn push(event: TextEvent) {
        EVENTS.with(|e| e.borrow_mut().push(event));
    }

    fn field() -> Option<web_sys::HtmlInputElement> {
        FIELD.with(|f| f.borrow().clone())
    }

    /// The canvas's place on the page, to turn a touch's page position into canvas points.
    fn canvas_origin() -> (f64, f64) {
        document()
            .and_then(|d| d.query_selector("canvas").ok().flatten())
            .map_or((0.0, 0.0), |c| {
                let r = c.get_bounding_client_rect();
                (r.left(), r.top())
            })
    }

    fn make_field(doc: &web_sys::Document) -> Option<web_sys::HtmlInputElement> {
        let input: web_sys::HtmlInputElement = doc.create_element("input").ok()?.unchecked_into();
        input.set_id(FIELD_ID);
        input.set_type("search");
        for (name, value) in [
            ("inputmode", "search"),
            ("enterkeyhint", "search"),
            ("autocomplete", "off"),
            ("autocorrect", "off"),
            ("autocapitalize", "off"),
            ("spellcheck", "false"),
            ("aria-label", "Search games"),
            // 16px: iOS zooms the page into a field with smaller text.
            (
                "style",
                "position:fixed;left:0;top:0;width:1px;height:1px;opacity:0;border:0;\
                 padding:0;margin:0;font-size:16px;pointer-events:none;z-index:-1;",
            ),
        ] {
            input.set_attribute(name, value).ok()?;
        }
        doc.body()?.append_child(&input).ok()?;

        let on_input = Closure::<dyn FnMut(web_sys::Event)>::new(|_| {
            if let Some(f) = field() {
                push(TextEvent::Value(f.value()));
            }
        });
        let on_key =
            Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(|e: web_sys::KeyboardEvent| {
                if e.key() == "Enter" {
                    e.prevent_default();
                    if let Some(f) = field() {
                        let _ = f.blur();
                    }
                }
            });
        let on_blur = Closure::<dyn FnMut(web_sys::Event)>::new(|_| {
            typing(false);
            push(TextEvent::Done);
        });
        let _ = input.add_event_listener_with_callback("input", on_input.as_ref().unchecked_ref());
        let _ = input.add_event_listener_with_callback("keydown", on_key.as_ref().unchecked_ref());
        let _ = input.add_event_listener_with_callback("blur", on_blur.as_ref().unchecked_ref());
        on_input.forget();
        on_key.forget();
        on_blur.forget();
        Some(input)
    }

    /// Mark the page while the phone's keyboard types into the box: the page's notices step
    /// aside, so they never sit over the results above the keyboard.
    fn typing(on: bool) {
        if let Some(body) = document().and_then(|d| d.body()) {
            let _ = body.class_list().toggle_with_force("psoxide-typing", on);
        }
    }

    /// A touch that ends on the search box focuses the field, inside the touch's handler.
    fn on_touch_end(event: web_sys::TouchEvent) {
        if let Some(touch) = event.changed_touches().get(0) {
            focus_at(f64::from(touch.client_x()), f64::from(touch.client_y()));
        }
    }

    /// The finger's pointer-up comes before its touchend; either may be the first the shell
    /// sees, so both focus (the second finds the field focused already).
    fn on_pointer_up(event: web_sys::PointerEvent) {
        if matches!(event.pointer_type().as_str(), "touch" | "pen") {
            focus_at(f64::from(event.client_x()), f64::from(event.client_y()));
        }
    }

    fn focus_at(client_x: f64, client_y: f64) {
        let Some([x, y, w, h]) = TARGET.with(Cell::get) else {
            return;
        };
        let (ox, oy) = canvas_origin();
        let (tx, ty) = ((client_x - ox) as f32, (client_y - oy) as f32);
        if tx < x || tx > x + w || ty < y || ty > y + h {
            return;
        }
        if focused() {
            return;
        }
        if let Some(f) = field() {
            QUERY.with(|q| f.set_value(&q.borrow()));
            FOCUSED_AT.with(|t| t.set(js_sys::Date::now()));
            typing(true);
            let _ = f.focus();
        }
    }

    pub(super) fn install() {
        let Some(doc) = document() else { return };
        let Some(input) = make_field(&doc) else {
            return;
        };
        FIELD.with(|f| *f.borrow_mut() = Some(input));
        let touch = Closure::<dyn FnMut(web_sys::TouchEvent)>::new(on_touch_end);
        let opts = web_sys::AddEventListenerOptions::new();
        opts.set_capture(true);
        let _ = doc.add_event_listener_with_callback_and_add_event_listener_options(
            "touchend",
            touch.as_ref().unchecked_ref(),
            &opts,
        );
        touch.forget();
        let pointer = Closure::<dyn FnMut(web_sys::PointerEvent)>::new(on_pointer_up);
        let _ = doc.add_event_listener_with_callback_and_add_event_listener_options(
            "pointerup",
            pointer.as_ref().unchecked_ref(),
            &opts,
        );
        pointer.forget();
    }

    pub(super) fn take() -> Vec<TextEvent> {
        EVENTS.with(|e| std::mem::take(&mut *e.borrow_mut()))
    }

    pub(super) fn sync(editing: bool, query: &str, target: Option<[f32; 4]>) {
        TARGET.with(|t| t.set(target));
        QUERY.with(|q| {
            let mut q = q.borrow_mut();
            if *q != query {
                q.clear();
                q.push_str(query);
            }
        });
        let Some(f) = field() else { return };
        // Keep the field over the box, so the page never scrolls to bring it into view.
        if let Some([x, y, w, h]) = target {
            let (ox, oy) = canvas_origin();
            let style = f.style();
            let _ = style.set_property("left", &format!("{}px", ox + f64::from(x)));
            let _ = style.set_property("top", &format!("{}px", oy + f64::from(y)));
            let _ = style.set_property("width", &format!("{w}px"));
            let _ = style.set_property("height", &format!("{h}px"));
        }
        if focused() {
            let settled = js_sys::Date::now() - FOCUSED_AT.with(Cell::get) > 400.0;
            if f.value() != query {
                // The shell changed the text itself (its clear button).
                f.set_value(query);
            }
            if !editing && settled {
                // The shell closed the box (Back, a tap on a case or the clear button): put
                // the keyboard away.
                let _ = f.blur();
            }
        }
    }

    pub(super) fn focused() -> bool {
        document()
            .and_then(|d| d.active_element())
            .is_some_and(|e| e.id() == FIELD_ID)
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use super::TextEvent;

    pub(super) fn install() {}

    pub(super) fn take() -> Vec<TextEvent> {
        Vec::new()
    }

    pub(super) fn sync(_editing: bool, _query: &str, _target: Option<[f32; 4]>) {}

    pub(super) fn focused() -> bool {
        false
    }
}

/// Put the hidden field in the page and listen for touches on the search box.
pub(crate) fn install() {
    imp::install();
}

/// What the field reported since the last frame.
pub(crate) fn take() -> Vec<TextEvent> {
    imp::take()
}

/// Once a frame: whether the box is open, its text, and where it is (canvas points, which
/// are CSS pixels on the web).
pub(crate) fn sync(editing: bool, query: &str, target: Option<[f32; 4]>) {
    imp::sync(editing, query, target);
}

/// The phone's keyboard is typing into the box.
pub(crate) fn focused() -> bool {
    imp::focused()
}
