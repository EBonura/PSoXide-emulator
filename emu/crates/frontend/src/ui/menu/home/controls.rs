//! The Controls page: live device cards, the remappable keyboard map and the touch
//! controller, inside the shell (v9).
//!
//! Wide windows show the three groups side by side; a phone shows one at a time under a
//! segmented bar (Devices, Keyboard, Touch). Everything is live: a device that is plugged in
//! appears, and a card goes from grey to cyan while its buttons are held. Every control is a
//! focus stop, so a pad, the keyboard and a finger reach the same things.

use std::collections::HashMap;

use egui::{Align2, Color32, FontId, Pos2, Rect, Shape, Stroke, Vec2};

use super::scene::{fade, mix};
use super::{
    chamfer, draw_device_icon, focus_cage, motion, pages, tokens, Click, HomeState, LayoutKind,
    Page, PageCtx, Zone,
};
use crate::input::model::BackendKind;
use crate::input::{InputDeviceInfo, PadProfile, PortChoice};
use crate::ui::menu::{MenuAction, MenuInput, MenuState, PadBindTarget};
use crate::ui::momentum::Momentum;

/// The three groups, in the order of the segmented bar.
const TABS: [&str; 3] = ["Devices", "Keyboard", "Touch"];

/// A device card stays cyan this long after its last input (seconds), so a quick tap is seen.
const ACTIVE_HOLD: f64 = 0.6;

/// What the page asks the app to change on a device.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Command {
    /// Pin a device to a port, or let it choose.
    Choice(String, PortChoice),
    /// Present the device as a digital pad, a DualShock or an analog pad.
    Profile(String, PadProfile),
}

/// One focus stop or click target of the page.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Item {
    /// The segmented bar (phone): the number is the segment.
    Tab(usize),
    /// A device's port choice.
    Port(String),
    /// A device's analog or digital toggle.
    Mode(String),
    /// One key of the map.
    Key(PadBindTarget),
    /// A touch row; the number is the row's index on the Settings page.
    Touch(usize),
    /// Reset the keyboard map.
    Reset,
}

impl Item {
    /// A stable name for the browser journeys' hit-area probe.
    pub(super) fn probe_name(&self) -> String {
        match self {
            Self::Tab(i) => format!("ctl:tab:{}", TABS.get(*i).copied().unwrap_or("?")),
            Self::Port(id) => format!("ctl:port:{id}"),
            Self::Mode(id) => format!("ctl:mode:{id}"),
            Self::Key(target) => format!("ctl:key:{}", target.label()),
            Self::Touch(i) => format!("ctl:touch:{i}"),
            Self::Reset => "ctl:reset".to_string(),
        }
    }
}

#[derive(Clone, Debug)]
struct Stop {
    item: Item,
    /// Stops of one row share a number: up and down move between rows.
    row: u32,
}

/// The page's own state. The app feeds `devices` and drains `commands` each frame.
#[derive(Default)]
pub(super) struct ControlsState {
    pub(super) devices: Vec<InputDeviceInfo>,
    pub(super) commands: Vec<Command>,
    tab: usize,
    focus: usize,
    scroll: f32,
    /// Top and bottom of the scrolling area as last drawn.
    view: [f32; 2],
    /// The furthest scroll, as last drawn.
    max: f32,
    stops: Vec<Stop>,
    active_at: HashMap<String, f64>,
    /// The focus the last frame drew, to scroll to it only when it moved.
    seen_focus: Option<usize>,
    /// The highlight of the segmented bar, in segments from the first: it slides.
    seg: motion::Spring,
    /// The segment whose content is on screen, seconds since it came up, and which way the
    /// bar moved to get there (-1 left, 1 right, 0 none).
    shown_tab: usize,
    tab_age: f32,
    tab_dir: f32,
    /// Where the touch layout preview and its column were drawn, `[x0, x1, column x0, column x1]`
    /// (the browser journeys check it is centred); `None` when it is off screen.
    preview: Option<[f32; 4]>,
}

impl ControlsState {
    /// A finger moved the page by `delta` points.
    pub(super) fn drag(&mut self, kin: &mut Momentum, delta: f32, now: f64) {
        let view = self.view[1] - self.view[0];
        kin.drag(&mut self.scroll, delta, now, self.max, view);
    }

    /// The finger lifted: the page coasts on or springs back.
    pub(super) fn release(&self, kin: &mut Momentum, now: f64) {
        kin.release(self.scroll, now, self.max);
    }

    /// The page opens on its first stop.
    pub(super) fn reset(&mut self) {
        self.focus = 0;
        self.scroll = 0.0;
        self.seen_focus = None;
        self.seg = motion::Spring::at(self.tab as f32);
        self.shown_tab = self.tab;
        self.tab_age = f32::MAX;
        self.tab_dir = 0.0;
    }

    /// The page's state as JSON for the browser journeys: the segment, the focus, the key being
    /// bound, every binding label and the routed devices.
    pub(super) fn probe_json(&self, ms: &MenuState) -> String {
        let esc = |t: &str| t.replace('\\', "\\\\").replace('"', "\\\"");
        let keys: Vec<String> = PadBindTarget::ALL
            .iter()
            .map(|t| {
                let label = ms.controls_labels.get(t).map_or("", String::as_str);
                format!("\"{}\":\"{}\"", t.label(), esc(label))
            })
            .collect();
        let devices: Vec<String> = self
            .devices
            .iter()
            .map(|d| {
                format!(
                    "[\"{}\",\"{}\",\"{}\"]",
                    esc(&d.identity.id),
                    d.choice.label(),
                    d.profile.label()
                )
            })
            .collect();
        let touch: Vec<String> = pages::visible_rows(ms, Page::Settings)
            .iter()
            .filter(|r| {
                matches!(
                    r.action,
                    MenuAction::CycleTouchMode
                        | MenuAction::CycleTouchOpacity
                        | MenuAction::CycleTouchSize
                        | MenuAction::ToggleTouchHaptics
                        | MenuAction::ToggleTouchLeftHanded
                )
            })
            .map(|r| format!("\"{}\":\"{}\"", esc(&r.label), esc(&r.value)))
            .collect();
        let focus = self
            .stops
            .get(self.focus)
            .map_or(String::new(), |s| s.item.probe_name());
        format!(
            "{{\"preview\":{},\"tab\":{},\"seg\":{:.3},\"focus\":\"{}\",\"stops\":{},\"scroll\":{:.0},\"max\":{:.0},\"view\":[{:.0},{:.0}],\"capture\":{},\"keys\":{{{}}},\"touch\":{{{}}},\"devices\":[{}]}}",
            self.preview.map_or("null".to_string(), |p| format!("[{:.1},{:.1},{:.1},{:.1}]", p[0], p[1], p[2], p[3])),
            self.tab,
            self.seg.pos,
            esc(&focus),
            self.stops.len(),
            self.scroll,
            self.max,
            self.view[0],
            self.view[1],
            ms.controls_capture.map_or("null".to_string(), |t| format!("\"{}\"", t.label())),
            keys.join(","),
            touch.join(","),
            devices.join(",")
        )
    }

    /// Open on a segment of the phone's bar (screenshots).
    pub(super) fn show_tab(&mut self, tab: usize) {
        self.tab = tab.min(TABS.len() - 1);
        self.seg = motion::Spring::at(self.tab as f32);
        self.shown_tab = self.tab;
    }
}

/// Everything one frame's drawing shares.
#[derive(Clone, Copy)]
struct Frame<'a> {
    p: &'a egui::Painter,
    ctx: &'a egui::Context,
    u: f32,
    a: f32,
    /// The scrolling area, in screen space.
    view: Rect,
    scroll: f32,
    time: f64,
    /// The connected-device facts of the feed.
    keyboard: bool,
    touch: bool,
    /// The mouse, when it is over the window.
    hover: Option<Pos2>,
    /// Seconds since this content came up (a page opening or another segment), the way it came
    /// in (-1, 0, 1 along the segments) and whether motion is calmed.
    age: f32,
    dir: f32,
    calm: bool,
}

/// The facts of a frame that do not depend on the area it draws into.
#[derive(Clone, Copy)]
struct FrameBase {
    u: f32,
    a: f32,
    time: f64,
    keyboard: bool,
    touch: bool,
    age: f32,
    dir: f32,
    calm: bool,
}

impl FrameBase {
    fn at<'a>(
        &self,
        p: &'a egui::Painter,
        ctx: &'a egui::Context,
        view: Rect,
        scroll: f32,
    ) -> Frame<'a> {
        Frame {
            p,
            ctx,
            u: self.u,
            a: self.a,
            view,
            scroll,
            time: self.time,
            keyboard: self.keyboard,
            touch: self.touch,
            hover: ctx.input(|i| i.pointer.hover_pos()),
            age: self.age,
            dir: self.dir,
            calm: self.calm,
        }
    }
}

impl Frame<'_> {
    /// Whether the mouse is over `r` (and over the page, not under the nav).
    fn hovered(&self, r: Rect) -> bool {
        self.hover
            .is_some_and(|p| r.contains(p) && self.view.contains(p))
    }

    /// A rest border that lifts to the focus colour under the mouse.
    fn edge(&self, r: Rect, rest: Color32) -> Stroke {
        if self.hovered(r) {
            Stroke::new(1.3, fade(tokens::FOCUS, self.a * 0.8))
        } else {
            Stroke::new(1.0, fade(rest, self.a))
        }
    }

    /// Content y to screen y.
    fn sy(&self, y: f32) -> f32 {
        y - self.scroll
    }

    /// This frame with part `index` of the content revealed that far, and what is needed to
    /// move the part's shapes into place afterwards (`motion::shift_since`): the parts arrive
    /// one after another, sliding in along the way the content came.
    fn part(&self, index: usize) -> (Self, motion::Mark, Vec2) {
        let s = if self.calm {
            1.0
        } else {
            motion::stage(self.age, index)
        };
        let k = 1.0 - s;
        let by = if self.dir == 0.0 {
            Vec2::new(0.0, k * 12.0 * self.u)
        } else {
            Vec2::new(self.dir * k * 24.0 * self.u, 0.0)
        };
        let shown = Self {
            a: self.a * s,
            ..*self
        };
        (shown, motion::mark(self.ctx, self.p), by)
    }

    /// Move what a part painted since `from` into its place in the slide.
    fn settle(&self, from: motion::Mark, by: Vec2) {
        motion::shift_since(self.ctx, self.p, from, by);
    }
}

/// The key a binding label shows, short enough for a cap.
fn short_key(label: &str) -> String {
    let compact = match label {
        "Backspace" => "Bksp".to_string(),
        "Space" => "Space".to_string(),
        "Enter" => "Enter".to_string(),
        other => other
            .replace("Numpad", "Num")
            .replace("Arrow", "")
            .replace("Shift", "Shft"),
    };
    if compact.chars().count() > 6 {
        let head: String = compact.chars().take(5).collect();
        format!("{head}\u{2026}")
    } else {
        compact
    }
}

/// Which arrow, when the label names an arrow key.
fn arrow_of(label: &str) -> Option<usize> {
    match label {
        "ArrowUp" => Some(0),
        "ArrowRight" => Some(1),
        "ArrowDown" => Some(2),
        "ArrowLeft" => Some(3),
        _ => None,
    }
}

fn arrow_glyph(p: &egui::Painter, c: Pos2, r: f32, dir: usize, col: Color32) {
    let (dx, dy) = [(0.0, -1.0), (1.0, 0.0), (0.0, 1.0), (-1.0, 0.0)][dir];
    let fwd = Vec2::new(dx, dy);
    let side = Vec2::new(-dy, dx);
    p.add(Shape::convex_polygon(
        vec![
            c + fwd * r,
            c - fwd * r * 0.7 + side * r * 0.8,
            c - fwd * r * 0.7 - side * r * 0.8,
        ],
        col,
        Stroke::NONE,
    ));
}

/// One line of text cut with an ellipsis when it does not fit.
fn fit_job(text: &str, font: FontId, col: Color32, max_w: f32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), font, col);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_w.max(8.0));
    job
}

/// Text that wraps to two rows and is cut with an ellipsis only after that.
fn wrap_job(text: &str, font: FontId, col: Color32, max_w: f32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::simple(text.to_string(), font, col, max_w.max(8.0));
    job.wrap.max_rows = 2;
    job.wrap.break_anywhere = false;
    job
}

/// What the keyboard card says while no keyboard has been used.
const KEYBOARD_ABSENT: &str = "None attached. Press a key if you have one";

/// What a device is, for its icon and its wording.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Keyboard,
    Pad,
    Touch,
}

impl Kind {
    fn icon(self) -> u8 {
        match self {
            Self::Keyboard => 0,
            Self::Pad => 1,
            Self::Touch => 2,
        }
    }

    fn idle(self) -> &'static str {
        match self {
            Self::Keyboard => "Press a key to see it here",
            Self::Pad => "Press a button to see it here",
            Self::Touch => "Touch the screen to see it here",
        }
    }
}

/// One card of the device column.
struct Card<'a> {
    kind: Kind,
    title: String,
    line: String,
    chip: &'static str,
    connected: bool,
    active: bool,
    /// The device behind it, when it is plugged in and can be routed.
    info: Option<&'a InputDeviceInfo>,
}

/// The cards, in order: the keyboard, each controller (or a placeholder), the touch screen.
fn cards<'a>(
    devices: &'a [InputDeviceInfo],
    f: &Frame<'_>,
    touch_off: bool,
    active_at: &HashMap<String, f64>,
) -> Vec<Card<'a>> {
    let live = |info: &InputDeviceInfo, kind: Kind| -> (String, bool) {
        let recent = active_at
            .get(&info.identity.id)
            .is_some_and(|t| f.time - t < ACTIVE_HOLD);
        let active = info.live.is_active() || recent;
        let line = if !info.identity.mapped {
            "Unmapped device: its buttons are not recognised".to_string()
        } else if info.ignored {
            "Ignored: sending constant input".to_string()
        } else if info.live.is_active() {
            info.live.describe()
        } else {
            kind.idle().to_string()
        };
        (line, active)
    };
    let mut out = Vec::new();
    let keyboard = devices
        .iter()
        .find(|d| d.identity.backend == BackendKind::Keyboard);
    out.push(match keyboard {
        Some(info) => {
            let (line, active) = live(info, Kind::Keyboard);
            // A phone has no keyboard until a key is pressed: say so, and how to find out.
            let line = if f.keyboard {
                line
            } else {
                KEYBOARD_ABSENT.to_string()
            };
            Card {
                kind: Kind::Keyboard,
                title: "Keyboard".into(),
                line,
                chip: if f.keyboard {
                    "Connected"
                } else {
                    "Not connected"
                },
                connected: f.keyboard,
                active,
                info: f.keyboard.then_some(info),
            }
        }
        None => Card {
            kind: Kind::Keyboard,
            title: "Keyboard".into(),
            line: KEYBOARD_ABSENT.into(),
            chip: "Not connected",
            connected: false,
            active: false,
            info: None,
        },
    });
    let pads: Vec<&InputDeviceInfo> = devices
        .iter()
        .filter(|d| {
            !matches!(
                d.identity.backend,
                BackendKind::Keyboard | BackendKind::Touch
            )
        })
        .collect();
    if pads.is_empty() {
        out.push(Card {
            kind: Kind::Pad,
            title: "Controller".into(),
            line: "Plug in or pair, then press a button".into(),
            chip: "Not connected",
            connected: false,
            active: false,
            info: None,
        });
    }
    for info in pads {
        let (line, active) = live(info, Kind::Pad);
        out.push(Card {
            kind: Kind::Pad,
            title: info.identity.name.clone(),
            line,
            chip: "Connected",
            connected: true,
            active,
            info: Some(info),
        });
    }
    let touch = devices
        .iter()
        .find(|d| d.identity.backend == BackendKind::Touch);
    let shown = f.touch && !touch_off;
    out.push(match touch {
        Some(info) if shown => {
            let (line, active) = live(info, Kind::Touch);
            Card {
                kind: Kind::Touch,
                title: "Touch controls".into(),
                line,
                chip: "Connected",
                connected: true,
                active,
                info: Some(info),
            }
        }
        _ => Card {
            kind: Kind::Touch,
            title: "Touch controls".into(),
            line: "Shown on touch screens".into(),
            chip: "Off",
            connected: false,
            active: false,
            info: None,
        },
    });
    out
}

fn port_label(info: &InputDeviceInfo) -> String {
    match info.choice {
        PortChoice::Auto => format!("Auto ({})", info.port.label()),
        PortChoice::Pinned(port) => port.label().to_string(),
    }
}

fn next_choice(choice: PortChoice, step: i32) -> PortChoice {
    let all = PortChoice::ALL;
    let at = all.iter().position(|c| *c == choice).unwrap_or(0) as i32;
    all[(at + step).rem_euclid(all.len() as i32) as usize]
}

/// Analog is on only for the analog pad; the DualShock starts digital and goes analog when
/// the game asks, so it reads as off here and a toggle makes it analog.
fn is_analog(profile: PadProfile) -> bool {
    profile == PadProfile::Analog
}

fn toggled(profile: PadProfile) -> PadProfile {
    if is_analog(profile) {
        PadProfile::Digital
    } else {
        PadProfile::Analog
    }
}

impl HomeState {
    fn push_stop(
        &mut self,
        item: Item,
        row: u32,
        rect: Rect,
        f: &Frame<'_>,
        focus_rect: &mut Option<Rect>,
    ) -> bool {
        let at = self.controls.stops.len();
        self.controls.stops.push(Stop { item, row });
        let focused = self.zone == Zone::Body && self.controls.focus == at;
        if focused {
            *focus_rect = Some(Rect::from_min_max(
                Pos2::new(rect.left(), rect.top() + f.scroll),
                Pos2::new(rect.right(), rect.bottom() + f.scroll),
            ));
        }
        focused
    }

    /// A click area: at least a finger wide and tall, and only the part that is on screen.
    fn hit(&mut self, f: &Frame<'_>, rect: Rect, click: Click) {
        let min = tokens::TOUCH_MIN;
        let grow = Vec2::new(
            ((min - rect.width()) / 2.0).max(0.0),
            ((min - rect.height()) / 2.0).max(0.0),
        );
        let seen = rect
            .expand2(grow)
            .intersect(f.p.clip_rect())
            .intersect(f.view);
        if seen.width() > 1.0 && seen.height() > 1.0 {
            self.hits.push((seen, click));
        }
    }
}

/// Draw the whole page.
pub(super) fn draw(
    h: &mut HomeState,
    ms: &mut MenuState,
    ctx: &egui::Context,
    pc: &mut PageCtx<'_>,
) {
    let lay = pc.lay;
    // A small window keeps the type readable and scrolls instead of shrinking it.
    let u = lay.u.max(0.9);
    let a = pc.appear;
    let desktop = lay.kind == LayoutKind::Desktop;
    let wide = desktop && lay.safe.width() >= 900.0;
    let margin = if desktop {
        tokens::MARGIN_DESKTOP * u
    } else if lay.safe.width() < 380.0 {
        tokens::MARGIN_SMALL_PHONE
    } else {
        tokens::MARGIN_PHONE
    };
    let width = (lay.safe.width() - 2.0 * margin).min(1280.0 * u.max(1.0));
    let left = lay.safe.center().x - width / 2.0;
    let head_top = lay.safe.top()
        + match lay.kind {
            LayoutKind::Desktop => 96.0 * u,
            LayoutKind::Portrait => 64.0 * u,
            LayoutKind::Landscape => 44.0 * u,
        };
    let head = h.reveal(0);
    let from = motion::mark(ctx, pc.painter);
    let title = pc.painter.text(
        Pos2::new(left, head_top),
        Align2::LEFT_TOP,
        "Controls",
        crate::theme::title_font(if desktop {
            tokens::TITLE_DESKTOP * u
        } else {
            tokens::TITLE_PHONE * u
        }),
        fade(tokens::TEXT, a * head),
    );
    let mut top = title.bottom() + 6.0 * u;
    if wide {
        let sub = pc.painter.text(
            Pos2::new(left, top),
            Align2::LEFT_TOP,
            "Everything here is live: plug something in and it appears.",
            FontId::proportional(tokens::BODY * u),
            fade(tokens::MUTED, a * head),
        );
        top = sub.bottom() + 14.0 * u;
    } else {
        top += 12.0 * u;
    }
    motion::shift_since(ctx, pc.painter, from, h.slide(head, u));
    let bottom = lay.safe.bottom()
        - if desktop {
            18.0 * u + h.hint_reserve(lay)
        } else {
            lay.bar_reserve() + 4.0 * u
        };
    h.draw_page_hints(
        pc,
        &[
            (super::Btn::Cross, "Select or rebind"),
            (super::Btn::Circle, "Back"),
        ],
    );
    let full_view = Rect::from_min_max(
        Pos2::new(left - 8.0, top),
        Pos2::new(left + width + 8.0, bottom),
    );

    h.controls.stops.clear();
    h.controls.preview = None;
    let now = ctx.input(|i| i.time);
    let devices = std::mem::take(&mut h.controls.devices);
    for d in &devices {
        if d.live.is_active() {
            h.controls.active_at.insert(d.identity.id.clone(), now);
        }
    }
    let touch_off = pages::visible_rows(ms, Page::Settings)
        .iter()
        .any(|r| matches!(r.action, MenuAction::CycleTouchMode) && r.value == "Off");

    // The segmented bar sits above the scrolling area on a phone.
    let mut focus_rect: Option<Rect> = None;
    let mut view = full_view;
    let (keyboard, touch) = (h.feed.keyboard, h.feed.touch);
    // The content arrives part by part: when the page opens, and again when another segment is
    // picked (sliding in from the side the bar moved toward).
    if h.controls.tab != h.controls.shown_tab {
        h.controls.tab_dir = if h.controls.tab > h.controls.shown_tab {
            1.0
        } else {
            -1.0
        };
        h.controls.shown_tab = h.controls.tab;
        h.controls.tab_age = 0.0;
    } else {
        h.controls.tab_age = (h.controls.tab_age + h.frame_dt).min(10.0);
    }
    let (age, dir) = if h.controls.tab_age < h.page_age {
        (h.controls.tab_age, h.controls.tab_dir)
    } else {
        (h.page_age, h.page_dir)
    };
    let base = FrameBase {
        u,
        a,
        time: now,
        keyboard,
        touch,
        age,
        dir,
        calm: h.calm(),
    };
    if !wide {
        let bar_h = (tokens::TOUCH_MIN * u).max(tokens::TOUCH_MIN);
        // A phone held sideways has no height to spare: the bar shares the title's row.
        let bar = if lay.kind == LayoutKind::Landscape {
            let x = (title.right() + 28.0 * u).min(left + width * 0.4);
            // The shell's close button over a paused game sits at the header's right end.
            let close = if h.pausing(ms) { 46.0 * u } else { 0.0 };
            Rect::from_min_max(
                Pos2::new(x, title.center().y - bar_h / 2.0),
                Pos2::new(left + width - close, title.center().y + bar_h / 2.0),
            )
        } else {
            Rect::from_min_size(Pos2::new(left, top), Vec2::new(width, bar_h))
        };
        let f = base.at(pc.painter, ctx, full_view, 0.0);
        let bar_part = h.reveal(1);
        let from = motion::mark(ctx, pc.painter);
        let shown = Frame {
            a: a * bar_part,
            ..f
        };
        segmented_bar(h, &shown, bar, &mut focus_rect);
        motion::shift_since(ctx, pc.painter, from, h.slide(bar_part, u));
        view = Rect::from_min_max(
            Pos2::new(left - 8.0, bar.bottom() + 10.0 * u),
            full_view.max,
        );
    }
    let clip = pc.painter.with_clip_rect(view);
    let mut content_end = view.top();
    {
        let scroll = h.controls.scroll;
        let f = base.at(&clip, ctx, view, scroll);
        // y of the first line of content, in content space.
        let y0 = view.top() + 4.0;
        let mut row = 1_u32;
        let mut end = y0;
        if wide {
            let gap = 24.0 * u;
            let col_w = (width - 2.0 * gap) / 3.0;
            let xs = [left, left + col_w + gap, left + 2.0 * (col_w + gap)];
            let e0 = devices_column(
                h,
                ms,
                &f,
                &devices,
                xs[0],
                col_w,
                y0,
                &mut row,
                &mut focus_rect,
                touch_off,
            );
            let e1 = keyboard_column(h, ms, &f, xs[1], col_w, y0, &mut row, &mut focus_rect);
            let e2 = touch_column(h, ms, &f, xs[2], col_w, y0, &mut row, &mut focus_rect);
            end = end.max(e0).max(e1).max(e2);
        } else {
            end = match h.controls.tab {
                0 => devices_column(
                    h,
                    ms,
                    &f,
                    &devices,
                    left,
                    width,
                    y0,
                    &mut row,
                    &mut focus_rect,
                    touch_off,
                ),
                1 => keyboard_column(h, ms, &f, left, width, y0, &mut row, &mut focus_rect),
                _ => touch_column(h, ms, &f, left, width, y0, &mut row, &mut focus_rect),
            };
        }
        content_end = content_end.max(end);
    }
    h.controls.devices = devices;

    // Scroll: the wheel, then the focus, then the limits.
    let max_scroll = (content_end - view.bottom() + 12.0 * u).max(0.0);
    // Nothing else on this page scrolls, so the wheel needs no pointer over it.
    h.kin.step(
        &mut h.controls.scroll,
        h.frame_dt,
        max_scroll,
        view.height(),
    );
    let wheel = ctx.input(|i| i.smooth_scroll_delta.y);
    if wheel != 0.0 {
        h.kin.wheel(&mut h.controls.scroll, -wheel, max_scroll);
    }
    if h.controls.seen_focus != Some(h.controls.focus) {
        h.kin.stop();
        if let Some(r) = focus_rect {
            let (top, bottom) = (r.top() - 8.0, r.bottom() + 8.0);
            if top < view.top() + h.controls.scroll {
                h.controls.scroll = top - view.top();
            } else if bottom > view.bottom() + h.controls.scroll {
                h.controls.scroll = bottom - view.bottom();
            }
        }
        h.controls.seen_focus = Some(h.controls.focus);
    }
    h.kin.limit(&mut h.controls.scroll, max_scroll);
    h.controls.max = max_scroll;
    h.controls.view = [view.top(), view.bottom()];
    // A hint that more is below.
    if h.controls.scroll < max_scroll - 1.0 {
        super::triangle(
            pc.painter,
            Pos2::new(view.center().x, view.bottom() - 6.0 * u),
            5.0 * u,
            true,
            fade(tokens::DIM, a),
        );
    }
    // A pad that sets nothing: keep the repaint going for the live cards.
    ctx.request_repaint_after(std::time::Duration::from_millis(60));
}

/// The three segments of the phone's bar: one pill that slides to the segment you pick, the same
/// highlight as the shell's tab bar.
fn segmented_bar(h: &mut HomeState, f: &Frame<'_>, bar: Rect, focus_rect: &mut Option<Rect>) {
    let (p, u, a) = (f.p, f.u, f.a);
    let n = TABS.len();
    let seg_w = bar.width() / n as f32;
    let focused = h.push_stop(Item::Tab(h.controls.tab), 0, bar, f, focus_rect);
    let (tab, dt, calm) = (h.controls.tab, h.frame_dt, h.calm());
    h.controls.seg.step(tab as f32, dt, calm);
    let round = bar.height() / 2.0;
    p.rect_filled(
        bar,
        round,
        fade(Color32::from_rgba_unmultiplied(12, 20, 28, 235), a),
    );
    p.rect_stroke(
        bar,
        round,
        Stroke::new(1.0, fade(Color32::from_rgb(38, 58, 72), a)),
        egui::StrokeKind::Inside,
    );
    let at = h.controls.seg.pos.clamp(0.0, (n - 1) as f32);
    let pill = Rect::from_min_size(
        Pos2::new(bar.left() + seg_w * at, bar.top()),
        Vec2::new(seg_w, bar.height()),
    )
    .shrink2(Vec2::new(3.0 * u, 4.0 * u));
    p.rect_filled(
        pill,
        pill.height() / 2.0,
        fade(Color32::from_rgb(16, 52, 62), a),
    );
    p.rect_stroke(
        pill,
        pill.height() / 2.0,
        Stroke::new(1.0, fade(tokens::ACCENT, a * 0.45)),
        egui::StrokeKind::Inside,
    );
    for (i, name) in TABS.iter().enumerate() {
        let r = Rect::from_min_size(
            Pos2::new(bar.left() + seg_w * i as f32, bar.top()),
            Vec2::new(seg_w, bar.height()),
        );
        let near = (1.0 - (h.controls.seg.pos - i as f32).abs()).clamp(0.0, 1.0);
        p.text(
            r.center(),
            Align2::CENTER_CENTER,
            *name,
            FontId::proportional(tokens::BODY_LARGE * u),
            fade(mix(tokens::TEXT_2, tokens::ACCENT, near), a),
        );
        h.hits.push((r, Click::Ctl(Item::Tab(i))));
    }
    if focused {
        focus_cage(p, bar, a, u);
    }
}

fn section_label(f: &Frame<'_>, x: f32, y: f32, text: &str) -> f32 {
    let r = f.p.text(
        Pos2::new(x, f.sy(y)),
        Align2::LEFT_TOP,
        text,
        crate::theme::title_font(tokens::LABEL * f.u.max(1.0)),
        fade(tokens::DIM, f.a),
    );
    y + r.height() + 12.0 * f.u
}

#[allow(clippy::too_many_arguments)]
fn devices_column(
    h: &mut HomeState,
    ms: &mut MenuState,
    f: &Frame<'_>,
    devices: &[InputDeviceInfo],
    x: f32,
    w: f32,
    y: f32,
    row: &mut u32,
    focus_rect: &mut Option<Rect>,
    touch_off: bool,
) -> f32 {
    let _ = ms;
    let u = f.u;
    let (fl, m, by) = f.part(0);
    let mut y = section_label(&fl, x, y, "Connected now");
    f.settle(m, by);
    let active_at = h.controls.active_at.clone();
    for (i, card) in cards(devices, f, touch_off, &active_at).iter().enumerate() {
        let (fc, m, by) = f.part(i + 1);
        y = draw_card(h, &fc, card, x, w, y, row, focus_rect) + 12.0 * u;
        f.settle(m, by);
    }
    y
}

#[allow(clippy::too_many_arguments)]
fn draw_card(
    h: &mut HomeState,
    f: &Frame<'_>,
    card: &Card<'_>,
    x: f32,
    w: f32,
    y: f32,
    row: &mut u32,
    focus_rect: &mut Option<Rect>,
) -> f32 {
    let (p, u, a) = (f.p, f.u, f.a);
    let lit = card.active;
    // The chip and the words are measured first: the status line wraps to a second row, and the
    // card grows to hold it rather than cutting it.
    let chip_font = FontId::proportional(tokens::CAPTION_LARGE * u.min(1.15));
    let chip_text = if lit { "Active" } else { card.chip };
    let chip_galley = p.layout_no_wrap(chip_text.to_string(), chip_font.clone(), tokens::TEXT);
    let chip_w = chip_galley.size().x + 26.0 * u + if card.connected { 12.0 * u } else { 0.0 };
    let text_x = x + 30.0 * u + 20.0 * u + 14.0 * u;
    let text_w = (x + w - 14.0 * u - chip_w) - 12.0 * u - text_x;
    let title = p.layout_job(fit_job(
        &card.title,
        FontId::proportional(tokens::ROW_LABEL * u),
        fade(tokens::TEXT, a),
        text_w,
    ));
    let line = p.layout_job(wrap_job(
        &card.line,
        FontId::proportional(13.0 * u.max(0.95)),
        fade(if lit { tokens::ACCENT } else { tokens::MUTED }, a),
        text_w,
    ));
    let block = title.size().y + 3.0 * u + line.size().y;
    let top_h = (60.0 * u).max(block + 20.0 * u);
    let bottom_h = if card.info.is_some() { 52.0 * u } else { 0.0 };
    let r = Rect::from_min_size(Pos2::new(x, f.sy(y)), Vec2::new(w, top_h + bottom_h));
    // Grey to cyan over one feedback beat, so a quick tap still shows.
    let id = egui::Id::new(("controls-card", card.kind.icon(), &card.title));
    let k = f
        .ctx
        .animate_bool_with_time(id, card.active, tokens::FEEDBACK);
    let mix = |rest: Color32, on: Color32| rest.lerp_to_gamma(on, k);
    let fill = mix(
        Color32::from_rgba_unmultiplied(40, 90, 110, 26),
        Color32::from_rgba_unmultiplied(34, 200, 229, 34),
    );
    p.add(Shape::convex_polygon(
        chamfer(r, 10.0 * u),
        fade(fill, a),
        Stroke::new(
            1.0 + 0.6 * k,
            fade(mix(tokens::hairline_strong(), tokens::ACCENT), a),
        ),
    ));
    // Icon box, then the words, then the chip.
    let icon_col = mix(tokens::ICON_REST, tokens::ACCENT);
    let icon_c = Pos2::new(r.left() + 30.0 * u, r.top() + top_h / 2.0);
    p.rect_stroke(
        Rect::from_center_size(icon_c, Vec2::splat(40.0 * u)),
        6.0,
        Stroke::new(1.0, fade(mix(tokens::RING, tokens::ACCENT), a)),
        egui::StrokeKind::Inside,
    );
    draw_device_icon(p, icon_c, 11.0 * u, card.kind.icon(), fade(icon_col, a));
    // The chip.
    let chip = Rect::from_center_size(
        Pos2::new(r.right() - 14.0 * u - chip_w / 2.0, r.top() + top_h / 2.0),
        Vec2::new(chip_w, 28.0 * u),
    );
    let chip_col = if lit {
        tokens::ACCENT
    } else if card.connected {
        tokens::TEXT_2
    } else {
        tokens::DIM
    };
    p.rect_stroke(
        chip,
        4.0,
        Stroke::new(1.0, fade(mix(tokens::RING, tokens::ACCENT), a)),
        egui::StrokeKind::Inside,
    );
    let mut tx = chip.left() + 13.0 * u;
    if card.connected {
        p.circle_filled(Pos2::new(tx, chip.center().y), 3.0 * u, fade(tokens::OK, a));
        tx += 12.0 * u;
    }
    p.text(
        Pos2::new(tx, chip.center().y),
        Align2::LEFT_CENTER,
        chip_text,
        chip_font,
        fade(chip_col, a),
    );
    let ty = r.top() + (top_h - block) / 2.0;
    let line_y = ty + title.size().y + 3.0 * u;
    p.galley(Pos2::new(text_x, ty), title, tokens::TEXT);
    p.galley(Pos2::new(text_x, line_y), line, tokens::MUTED);

    if let Some(info) = card.info {
        let by = r.top() + top_h;
        p.line_segment(
            [
                Pos2::new(r.left() + 12.0 * u, by),
                Pos2::new(r.right() - 12.0 * u, by),
            ],
            Stroke::new(1.0, fade(tokens::hairline(), a)),
        );
        *row += 1;
        // Port: a chip that steps through Auto, Port 1, Port 2, Off.
        let label = port_label(info);
        let font = FontId::proportional(tokens::CAPTION_LARGE * u.min(1.15));
        let g = p.layout_no_wrap(label.clone(), font.clone(), tokens::TEXT);
        let pw = (g.size().x + 40.0 * u).max(96.0 * u);
        let port = Rect::from_min_size(
            Pos2::new(r.left() + 14.0 * u, by + (bottom_h - 40.0 * u) / 2.0),
            Vec2::new(pw, 40.0 * u),
        );
        p.rect_stroke(
            port,
            4.0,
            f.edge(port, tokens::RING),
            egui::StrokeKind::Inside,
        );
        p.text(
            Pos2::new(port.left() + 12.0 * u, port.center().y),
            Align2::LEFT_CENTER,
            label,
            font,
            fade(tokens::TEXT_2, a),
        );
        super::triangle(
            p,
            Pos2::new(port.right() - 14.0 * u, port.center().y),
            4.5 * u,
            true,
            fade(tokens::ICON_REST, a),
        );
        let focused = h.push_stop(
            Item::Port(info.identity.id.clone()),
            *row,
            port,
            f,
            focus_rect,
        );
        if focused {
            focus_cage(p, port, a, u);
        }
        h.hit(
            f,
            port.expand2(Vec2::new(2.0, 4.0)),
            Click::Ctl(Item::Port(info.identity.id.clone())),
        );
        // Analog: the label and a toggle. A keyboard has no sticks: it never
        // offers analog mode, and says so.
        if info.identity.backend == crate::input::model::BackendKind::Keyboard {
            p.text(
                Pos2::new(r.right() - 14.0 * u, by + bottom_h / 2.0),
                Align2::RIGHT_CENTER,
                "Digital pad",
                FontId::proportional(tokens::CAPTION_LARGE * u.min(1.15)),
                fade(tokens::MUTED, a),
            );
            *row += 1;
            return y + r.height();
        }
        let analog = is_analog(info.profile);
        let track = pages::draw_toggle(p, r.right() - 14.0 * u, by + bottom_h / 2.0, analog, u, a);
        // The room the port chip leaves; the words give way before they touch it.
        let room = track.left() - 10.0 * u - (port.right() + 12.0 * u);
        let words = if info.profile == PadProfile::DualShock && !analog {
            "Analog if asked"
        } else {
            "Analog mode"
        };
        let g = p.layout_job(fit_job(
            words,
            FontId::proportional(tokens::CAPTION_LARGE * u.min(1.15)),
            fade(tokens::TEXT_2, a),
            room,
        ));
        let lab = Rect::from_min_size(
            Pos2::new(
                track.left() - 10.0 * u - g.size().x,
                track.center().y - g.size().y / 2.0,
            ),
            g.size(),
        );
        p.galley(lab.min, g, tokens::TEXT_2);
        let hit = Rect::from_min_max(
            Pos2::new(lab.left() - 6.0, by + 4.0 * u),
            Pos2::new(track.right() + 8.0, by + bottom_h - 4.0 * u),
        );
        let focused = h.push_stop(
            Item::Mode(info.identity.id.clone()),
            *row,
            hit,
            f,
            focus_rect,
        );
        if focused {
            focus_cage(p, hit, a, u);
        }
        h.hit(f, hit, Click::Ctl(Item::Mode(info.identity.id.clone())));
        *row += 1;
    } else {
        *row += 1;
    }
    y + r.height()
}

/// One row of the map: a label and a few caps.
const MAP: [(&str, &[PadBindTarget]); 5] = [
    (
        "Move",
        &[
            PadBindTarget::Up,
            PadBindTarget::Down,
            PadBindTarget::Left,
            PadBindTarget::Right,
        ],
    ),
    (
        "Face buttons",
        &[
            PadBindTarget::Triangle,
            PadBindTarget::Circle,
            PadBindTarget::Cross,
            PadBindTarget::Square,
        ],
    ),
    (
        "Shoulders",
        &[
            PadBindTarget::L1,
            PadBindTarget::R1,
            PadBindTarget::L2,
            PadBindTarget::R2,
        ],
    ),
    (
        "Start and Select",
        &[PadBindTarget::Start, PadBindTarget::Select],
    ),
    ("Stick clicks", &[PadBindTarget::L3, PadBindTarget::R3]),
];

/// The small name over a cap.
fn cap_name(t: PadBindTarget) -> &'static str {
    match t {
        PadBindTarget::Up | PadBindTarget::LStickUp | PadBindTarget::RStickUp => "Up",
        PadBindTarget::Down | PadBindTarget::LStickDown | PadBindTarget::RStickDown => "Down",
        PadBindTarget::Left | PadBindTarget::LStickLeft | PadBindTarget::RStickLeft => "Left",
        PadBindTarget::Right | PadBindTarget::LStickRight | PadBindTarget::RStickRight => "Right",
        PadBindTarget::Cross => "Cross",
        PadBindTarget::Circle => "Circle",
        PadBindTarget::Square => "Square",
        PadBindTarget::Triangle => "Triangle",
        PadBindTarget::L1 => "L1",
        PadBindTarget::L2 => "L2",
        PadBindTarget::R1 => "R1",
        PadBindTarget::R2 => "R2",
        PadBindTarget::Start => "Start",
        PadBindTarget::Select => "Select",
        PadBindTarget::L3 => "L3",
        PadBindTarget::R3 => "R3",
        PadBindTarget::Analog => "Analog",
    }
}

#[allow(clippy::too_many_arguments)]
fn keyboard_column(
    h: &mut HomeState,
    ms: &mut MenuState,
    f: &Frame<'_>,
    x: f32,
    w: f32,
    y: f32,
    row: &mut u32,
    focus_rect: &mut Option<Rect>,
) -> f32 {
    let (p, u) = (f.p, f.u);
    let (fl, m0, by0) = f.part(0);
    let a = fl.a;
    let mut y = section_label(&fl, x, y, "Keyboard");
    // The hint is also the capture banner.
    let (hint, hint_col) = match ms.controls_capture {
        Some(t) => (
            format!("Press a key for {}. Escape cancels.", t.label()),
            tokens::ACCENT,
        ),
        None => (
            "Select a key, then press the one you want.".to_string(),
            tokens::MUTED,
        ),
    };
    let g = p.layout_job(fit_job(
        &hint,
        FontId::proportional(13.0 * u.max(0.95)),
        fade(hint_col, a),
        w,
    ));
    let gh = g.size().y;
    p.galley(Pos2::new(x, f.sy(y)), g, hint_col);
    y += gh + 10.0 * u;
    f.settle(m0, by0);

    let row_h = 56.0 * u;
    let label_w = (w * 0.30).clamp(92.0 * u, 124.0 * u);
    for (i, (label, targets)) in MAP.into_iter().enumerate() {
        let (f, m, by) = f.part(i + 1);
        let (f, a) = (&f, f.a);
        let r = Rect::from_min_size(Pos2::new(x, f.sy(y)), Vec2::new(w, row_h));
        p.line_segment(
            [r.left_bottom(), r.right_bottom()],
            Stroke::new(1.0, fade(tokens::hairline(), a)),
        );
        p.text(
            Pos2::new(r.left(), r.center().y),
            Align2::LEFT_CENTER,
            label,
            FontId::proportional(tokens::ROW_LABEL * u.min(1.1)),
            fade(tokens::TEXT_2, a),
        );
        let gap = 5.0 * u;
        let avail = w - label_w - 4.0;
        let n = targets.len();
        let cap_w = ((avail - gap * (n as f32 - 1.0)) / n as f32).clamp(40.0 * u, 62.0 * u);
        let mut cx = r.right() - (cap_w * n as f32 + gap * (n as f32 - 1.0));
        for target in targets {
            let cap = Rect::from_min_size(
                Pos2::new(cx, r.center().y - 22.0 * u),
                Vec2::new(cap_w, 44.0 * u),
            );
            cx = cap.right() + gap;
            let focused = h.push_stop(Item::Key(*target), *row, cap, f, focus_rect);
            draw_cap(h, ms, f, cap, *target, focused);
        }
        *row += 1;
        y += row_h;
        f.settle(m, by);
    }
    // The menu key and the reset button arrive with the last row.
    let (f, m, by) = f.part(MAP.len() + 1);
    let (f, a) = (&f, f.a);
    // The menu key is fixed.
    let r = Rect::from_min_size(Pos2::new(x, f.sy(y)), Vec2::new(w, row_h));
    p.line_segment(
        [r.left_bottom(), r.right_bottom()],
        Stroke::new(1.0, fade(tokens::hairline(), a)),
    );
    p.text(
        Pos2::new(r.left(), r.center().y),
        Align2::LEFT_CENTER,
        "Menu",
        FontId::proportional(tokens::ROW_LABEL * u.min(1.1)),
        fade(tokens::TEXT_2, a),
    );
    let cap = Rect::from_min_size(
        Pos2::new(r.right() - 62.0 * u, r.center().y - 22.0 * u),
        Vec2::new(62.0 * u, 44.0 * u),
    );
    p.rect_stroke(
        cap,
        4.0,
        Stroke::new(1.0, fade(tokens::hairline_strong(), a)),
        egui::StrokeKind::Inside,
    );
    p.text(
        Pos2::new(cap.center().x, cap.top() + 15.0 * u),
        Align2::CENTER_CENTER,
        "Fixed",
        FontId::proportional(tokens::CAPTION * u.max(1.0)),
        fade(tokens::DIM, a),
    );
    p.text(
        Pos2::new(cap.center().x, cap.top() + 33.0 * u),
        Align2::CENTER_CENTER,
        "Esc",
        FontId::proportional(14.0 * u),
        fade(tokens::MUTED, a),
    );
    y += row_h + 14.0 * u;
    // Reset.
    let reset = Rect::from_min_size(
        Pos2::new(x, f.sy(y)),
        Vec2::new((w).min(220.0 * u), tokens::TOUCH_MIN * u),
    );
    p.rect_stroke(
        reset,
        4.0,
        f.edge(reset, tokens::RING),
        egui::StrokeKind::Inside,
    );
    p.text(
        reset.center(),
        Align2::CENTER_CENTER,
        "Reset keys to defaults",
        FontId::proportional(tokens::BODY * u),
        fade(tokens::TEXT_2, a),
    );
    let focused = h.push_stop(Item::Reset, *row, reset, f, focus_rect);
    if focused {
        focus_cage(p, reset, a, u);
    }
    h.hit(f, reset, Click::Ctl(Item::Reset));
    *row += 1;
    f.settle(m, by);
    y + reset.height() + 8.0 * u
}

fn draw_cap(
    h: &mut HomeState,
    ms: &MenuState,
    f: &Frame<'_>,
    cap: Rect,
    target: PadBindTarget,
    focused: bool,
) {
    let (p, u, a) = (f.p, f.u, f.a);
    let armed = ms.controls_capture == Some(target);
    let held = ms.controls_live_held.contains(&target);
    let label = ms
        .controls_labels
        .get(&target)
        .cloned()
        .unwrap_or_else(|| "-".into());
    let pulse = ((f.time * std::f64::consts::TAU).sin() * 0.5 + 0.5) as f32;
    let fill = if held {
        Color32::from_rgba_unmultiplied(34, 200, 229, 70)
    } else {
        Color32::from_rgba_unmultiplied(16, 30, 40, 230)
    };
    p.rect_filled(cap, 4.0, fade(fill, a));
    let stroke = if armed {
        Stroke::new(
            2.0,
            fade(tokens::ACCENT.gamma_multiply(0.55 + 0.45 * pulse), a),
        )
    } else if held {
        Stroke::new(1.6, fade(tokens::ACCENT, a))
    } else {
        f.edge(cap, tokens::RING)
    };
    p.rect_stroke(cap, 4.0, stroke, egui::StrokeKind::Inside);
    p.text(
        Pos2::new(cap.center().x, cap.top() + 12.0 * u),
        Align2::CENTER_CENTER,
        cap_name(target),
        FontId::proportional(tokens::CAPTION * u.max(1.0)),
        fade(tokens::DIM, a),
    );
    let key_c = Pos2::new(cap.center().x, cap.top() + 30.0 * u);
    if armed {
        p.text(
            key_c,
            Align2::CENTER_CENTER,
            "...",
            FontId::proportional(15.0 * u),
            fade(tokens::ACCENT, a),
        );
    } else if label == "-" || label.is_empty() {
        p.text(
            key_c,
            Align2::CENTER_CENTER,
            "None",
            FontId::proportional(13.0 * u.max(0.95)),
            fade(tokens::DIM, a),
        );
    } else if let Some(dir) = arrow_of(&label) {
        arrow_glyph(p, key_c, 5.5 * u, dir, fade(tokens::TEXT, a));
    } else {
        let g = p.layout_job(fit_job(
            &short_key(&label),
            FontId::proportional(15.0 * u),
            fade(tokens::TEXT, a),
            cap.width() - 6.0,
        ));
        let pos = key_c - Vec2::new(g.size().x / 2.0, g.size().y / 2.0);
        p.galley(pos, g, tokens::TEXT);
    }
    if focused {
        focus_cage(p, cap, a, u);
    }
    h.hit(f, cap, Click::Ctl(Item::Key(target)));
}

#[allow(clippy::too_many_arguments)]
fn touch_column(
    h: &mut HomeState,
    ms: &mut MenuState,
    f: &Frame<'_>,
    x: f32,
    w: f32,
    y: f32,
    row: &mut u32,
    focus_rect: &mut Option<Rect>,
) -> f32 {
    let (p, u) = (f.p, f.u);
    let (fl, m, by) = f.part(0);
    let mut y = section_label(&fl, x, y, "Touch");
    f.settle(m, by);
    let rows = pages::visible_rows(ms, Page::Settings);
    let row_h = (if u > 0.0 && f.view.width() > 600.0 {
        tokens::ROW_DESKTOP
    } else {
        tokens::ROW_PHONE
    }) * u.max(0.9);
    let mut opacity = 60.0;
    let mut size = 100.0;
    let mut left_handed = false;
    for (i, r) in rows.iter().enumerate() {
        let value = r.value.trim_end_matches('%').parse::<f32>().unwrap_or(0.0);
        match r.action {
            MenuAction::CycleTouchOpacity => opacity = value,
            MenuAction::CycleTouchSize => size = value,
            MenuAction::ToggleTouchLeftHanded => left_handed = r.value == "On",
            _ => {}
        }
        if !matches!(
            r.action,
            MenuAction::CycleTouchMode
                | MenuAction::CycleTouchOpacity
                | MenuAction::CycleTouchSize
                | MenuAction::ToggleTouchHaptics
                | MenuAction::ToggleTouchLeftHanded
        ) {
            continue;
        }
        let rect = Rect::from_min_size(Pos2::new(x, f.sy(y)), Vec2::new(w, row_h - 4.0));
        let focused = h.push_stop(Item::Touch(i), *row, rect, f, focus_rect);
        let (fr, m, by) = f.part(*row as usize);
        pages::draw_row(h, ms, f.ctx, p, rect, r, focused, u, fr.a, i);
        f.settle(m, by);
        *row += 1;
        y += row_h;
    }
    y += 12.0 * u;
    let (fp, m, by) = f.part(*row as usize + 1);
    let (end, rect) = draw_preview(&fp, x, w, y, opacity, size, left_handed);
    y = end;
    if rect.intersects(fp.view) {
        h.controls.preview = Some([rect.left(), rect.right(), x, x + w]);
    }
    f.settle(m, by);
    y + 8.0 * u
}

/// Where the on-screen controller puts its parts, as fractions of the screen: the d-pad
/// centre and the face buttons' centre. Left-handed play swaps them.
fn preview_sides(left_handed: bool) -> (f32, f32) {
    if left_handed {
        (0.83, 0.17)
    } else {
        (0.17, 0.83)
    }
}

/// A phone held sideways with the on-screen controller on it, at the size and strength the
/// sliders say. It is how the page shows what the touch rows do.
fn draw_preview(
    f: &Frame<'_>,
    x: f32,
    w: f32,
    y: f32,
    opacity: f32,
    size: f32,
    left_handed: bool,
) -> (f32, Rect) {
    let (p, u, a) = (f.p, f.u, f.a);
    let pw = w.min(300.0 * u);
    let ph = pw * 0.46;
    // Centred in its column, whatever the column's width.
    let r = Rect::from_min_size(Pos2::new(x + (w - pw) / 2.0, f.sy(y)), Vec2::new(pw, ph));
    p.rect_filled(
        r,
        10.0 * u,
        fade(Color32::from_rgba_unmultiplied(8, 16, 22, 190), a),
    );
    p.rect_stroke(
        r,
        10.0 * u,
        Stroke::new(1.5, fade(tokens::RING, a)),
        egui::StrokeKind::Inside,
    );
    let strength = (0.25 + 0.75 * (opacity / 100.0)).clamp(0.2, 1.0) * a;
    let col = tokens::ACCENT.gamma_multiply(strength);
    let line = Stroke::new(1.6, col);
    let k = ph * 0.17 * (size / 100.0).clamp(0.5, 1.6);
    let at = |fx: f32, fy: f32| Pos2::new(r.left() + r.width() * fx, r.top() + r.height() * fy);
    let (dpad_x, face_x) = preview_sides(left_handed);
    let d = at(dpad_x, 0.56);
    p.rect_stroke(
        Rect::from_center_size(d, Vec2::new(k * 2.4, k * 0.8)),
        2.0,
        line,
        egui::StrokeKind::Inside,
    );
    p.rect_stroke(
        Rect::from_center_size(d, Vec2::new(k * 0.8, k * 2.4)),
        2.0,
        line,
        egui::StrokeKind::Inside,
    );
    let fc = at(face_x, 0.56);
    for off in [
        Vec2::new(0.0, -1.0),
        Vec2::new(1.0, 0.0),
        Vec2::new(0.0, 1.0),
        Vec2::new(-1.0, 0.0),
    ] {
        p.circle_stroke(fc + off * k * 1.0, k * 0.42, line);
    }
    for (fx, fy) in [(dpad_x, 0.1), (face_x, 0.1)] {
        p.rect_stroke(
            Rect::from_center_size(at(fx, fy), Vec2::new(k * 1.7, k * 0.5)),
            3.0,
            line,
            egui::StrokeKind::Inside,
        );
    }
    for fx in [0.44, 0.56] {
        p.rect_stroke(
            Rect::from_center_size(at(fx, 0.9), Vec2::new(k * 0.9, k * 0.34)),
            3.0,
            line,
            egui::StrokeKind::Inside,
        );
    }
    let cap = p.text(
        Pos2::new(r.center().x, r.bottom() + 8.0 * u),
        Align2::CENTER_TOP,
        "Layout preview",
        FontId::proportional(tokens::CAPTION_LARGE * u.min(1.15)),
        fade(tokens::DIM, a),
    );
    (y + ph + 8.0 * u + cap.height(), r)
}

impl HomeState {
    /// A new frame of devices from the app.
    pub(super) fn set_controls_devices(&mut self, devices: Vec<InputDeviceInfo>) {
        self.controls.devices = devices;
    }

    /// What the page wants changed on a device since the last call.
    pub(super) fn take_controls_commands(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.controls.commands)
    }

    /// Whether the page is on screen and wants live devices.
    pub(super) fn wants_devices(&self) -> bool {
        self.page == Page::Controls && !self.closed
    }

    /// Keys, pad and stick on the page: up and down move between rows, left and right move
    /// along a row or change a value, confirm acts, back goes Home.
    pub(super) fn update_controls(
        &mut self,
        ms: &mut MenuState,
        input: &MenuInput,
    ) -> Option<MenuAction> {
        if self.zone == Zone::Nav {
            self.update_nav(ms, input);
            return None;
        }
        // A key is being bound: nothing else moves until it is bound or cancelled.
        if ms.controls_capture.is_some() {
            if input.back {
                ms.controls_capture = None;
            }
            return None;
        }
        if input.back {
            self.page = Page::Home;
            return None;
        }
        let count = self.controls.stops.len();
        if count == 0 {
            if input.up {
                self.zone = Zone::Nav;
            }
            return None;
        }
        let at = self.controls.focus.min(count - 1);
        self.controls.focus = at;
        let stop = self.controls.stops[at].clone();
        if input.up || input.down {
            let want = |s: &Stop| {
                if input.up {
                    s.row < stop.row
                } else {
                    s.row > stop.row
                }
            };
            let target_row = {
                let rows = self
                    .controls
                    .stops
                    .iter()
                    .filter(|s| want(s))
                    .map(|s| s.row);
                if input.up {
                    rows.max()
                } else {
                    rows.min()
                }
            };
            match target_row {
                Some(row) => {
                    // Keep the column when the next row has as many stops, else its first.
                    let col = self.controls.stops[..at]
                        .iter()
                        .rev()
                        .take_while(|s| s.row == stop.row)
                        .count();
                    let first = self
                        .controls
                        .stops
                        .iter()
                        .position(|s| s.row == row)
                        .unwrap_or(at);
                    let len = self.controls.stops.iter().filter(|s| s.row == row).count();
                    self.controls.focus = first + col.min(len - 1);
                }
                None if input.up => {
                    self.zone = Zone::Nav;
                    self.nav = super::nav_items(self.pausing(ms))
                        .iter()
                        .position(|i| i.page() == self.page)
                        .unwrap_or(0);
                }
                None => {}
            }
            return None;
        }
        let step = i32::from(input.right) - i32::from(input.left);
        match &stop.item {
            Item::Key(_) => {
                if input.confirm {
                    return self.activate_item(ms, &stop.item);
                }
                let next = at as i32 + step;
                if step != 0
                    && next >= 0
                    && (next as usize) < count
                    && self.controls.stops[next as usize].row == stop.row
                {
                    self.controls.focus = next as usize;
                }
                None
            }
            Item::Tab(current) => {
                if step != 0 {
                    self.controls.tab =
                        (*current as i32 + step).clamp(0, TABS.len() as i32 - 1) as usize;
                    self.controls.scroll = 0.0;
                }
                None
            }
            Item::Port(_) | Item::Mode(_) => {
                if input.confirm || step != 0 {
                    self.step_device(&stop.item, if step < 0 { -1 } else { 1 });
                }
                None
            }
            Item::Reset => input.confirm.then_some(MenuAction::ResetControls),
            Item::Touch(i) => {
                let rows = pages::visible_rows(ms, Page::Settings);
                let row = rows.get(*i)?;
                ms.item_index = *i;
                pages::step_action(ms, &row.action.clone(), input)
            }
        }
    }

    fn step_device(&mut self, item: &Item, step: i32) {
        match item {
            Item::Port(id) => {
                if let Some(d) = self.controls.devices.iter().find(|d| d.identity.id == *id) {
                    let choice = next_choice(d.choice, step);
                    self.controls
                        .commands
                        .push(Command::Choice(id.clone(), choice));
                }
            }
            Item::Mode(id) => {
                if let Some(d) = self.controls.devices.iter().find(|d| d.identity.id == *id) {
                    self.controls
                        .commands
                        .push(Command::Profile(id.clone(), toggled(d.profile)));
                }
            }
            _ => {}
        }
    }

    /// A click or confirm on an item. Also moves the focus onto it.
    pub(super) fn activate_item(&mut self, ms: &mut MenuState, item: &Item) -> Option<MenuAction> {
        if let Some(at) = self.controls.stops.iter().position(|s| s.item == *item) {
            self.controls.focus = at;
            self.zone = Zone::Body;
        }
        match item {
            Item::Tab(i) => {
                self.controls.tab = (*i).min(TABS.len() - 1);
                self.controls.scroll = 0.0;
            }
            Item::Key(target) => {
                ms.controls_capture = Some(*target);
            }
            Item::Port(_) | Item::Mode(_) => self.step_device(item, 1),
            Item::Reset => return Some(MenuAction::ResetControls),
            Item::Touch(i) => {
                let rows = pages::visible_rows(ms, Page::Settings);
                return rows.get(*i).map(|r| r.action.clone());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn left_handed_play_swaps_the_two_halves() {
        let (dpad, face) = preview_sides(false);
        let (dpad_l, face_l) = preview_sides(true);
        assert!(dpad < face);
        assert_eq!((dpad, face), (face_l, dpad_l));
    }

    #[test]
    fn port_choice_steps_wrap_both_ways() {
        let all = PortChoice::ALL;
        assert_eq!(next_choice(all[all.len() - 1], 1), all[0]);
        assert_eq!(next_choice(all[0], -1), all[all.len() - 1]);
    }

    #[test]
    fn the_dualshock_is_off_and_a_toggle_makes_it_analog() {
        assert!(!is_analog(PadProfile::DualShock));
        assert_eq!(toggled(PadProfile::DualShock), PadProfile::Analog);
        assert_eq!(toggled(PadProfile::Analog), PadProfile::Digital);
    }

    #[test]
    fn long_key_names_are_cut_for_a_cap() {
        assert_eq!(short_key("ArrowUp"), "Up");
        assert_eq!(short_key("Backspace"), "Bksp");
        assert!(short_key("NumpadMultiply").chars().count() <= 6);
    }

    fn page_with_stops() -> (MenuState, HomeState) {
        let mut ms = MenuState::new();
        ms.enable_home(false, false);
        let mut h = HomeState::new(false, false);
        h.page = Page::Controls;
        let key = |t, row| Stop {
            item: Item::Key(t),
            row,
        };
        h.controls.stops = vec![
            key(PadBindTarget::Up, 1),
            key(PadBindTarget::Down, 1),
            key(PadBindTarget::Cross, 2),
            key(PadBindTarget::Circle, 2),
        ];
        (ms, h)
    }

    #[test]
    fn arrows_move_along_a_row_and_between_rows_and_up_leaves_to_the_nav() {
        let (mut ms, mut h) = page_with_stops();
        let press =
            |h: &mut HomeState, ms: &mut MenuState, input: MenuInput| h.update_controls(ms, &input);
        press(
            &mut h,
            &mut ms,
            MenuInput {
                right: true,
                ..MenuInput::default()
            },
        );
        assert_eq!(h.controls.focus, 1);
        // The next row keeps the column.
        press(
            &mut h,
            &mut ms,
            MenuInput {
                down: true,
                ..MenuInput::default()
            },
        );
        assert_eq!(h.controls.focus, 3);
        press(
            &mut h,
            &mut ms,
            MenuInput {
                up: true,
                ..MenuInput::default()
            },
        );
        press(
            &mut h,
            &mut ms,
            MenuInput {
                up: true,
                ..MenuInput::default()
            },
        );
        assert_eq!(h.zone, Zone::Nav, "up from the first row reaches the nav");
    }

    #[test]
    fn confirm_arms_a_key_and_back_cancels_it_before_it_leaves() {
        let (mut ms, mut h) = page_with_stops();
        h.update_controls(
            &mut ms,
            &MenuInput {
                confirm: true,
                ..MenuInput::default()
            },
        );
        assert_eq!(ms.controls_capture, Some(PadBindTarget::Up));
        h.update_controls(
            &mut ms,
            &MenuInput {
                back: true,
                ..MenuInput::default()
            },
        );
        assert_eq!(ms.controls_capture, None);
        assert_eq!(h.page, Page::Controls);
        h.update_controls(
            &mut ms,
            &MenuInput {
                back: true,
                ..MenuInput::default()
            },
        );
        assert_eq!(h.page, Page::Home);
    }
}
