//! The list pages (Library, Settings, the pause list) and the welcome pages.

use egui::{Align2, Color32, FontId, Pos2, Rect, Shape, Stroke, Vec2};

use super::scene::{fade, CYAN};
use super::{Click, HomeState, LayoutKind, Page, PageCtx};
use crate::ui::menu::{MenuAction, MenuInput, MenuItem, MenuState, TouchSliderKind};

/// What is on the public demo disc. Shown only when the visitor asks.
pub(super) const DISC_CONTENTS: [&str; 8] = [
    "PSoXide Arcade",
    "Celeste Collection",
    "NitroXide",
    "VoXide",
    "Quake (shareware)",
    "Cortex Ignition",
    "PSXcel",
    "Credits and links",
];

/// One welcome page: a title and a few short paragraphs.
pub(super) struct WelcomePage {
    pub title: &'static str,
    pub body: &'static [&'static str],
    pub links: &'static [(&'static str, &'static str)],
}

pub(super) const WELCOME: [WelcomePage; 4] = [
    WelcomePage {
        title: "A PlayStation, in your browser",
        body: &[
            "PSoXide is a free emulator for the original PlayStation. It runs homebrew with no BIOS file.",
            "It is an independent project and is not affiliated with Sony.",
        ],
        links: &[],
    },
    WelcomePage {
        title: "Your games stay on your device",
        body: &[
            "Open a folder of your own disc images or programs and play them here.",
            "Files are read in place and never uploaded.",
        ],
        links: &[],
    },
    WelcomePage {
        title: "Box art",
        body: &[
            "Cover pictures for your own discs are fetched while you browse, from the community collections xlenore/psx-covers and libretro-thumbnails, and kept on this device.",
            "Turn them off in Settings.",
        ],
        links: &[],
    },
    WelcomePage {
        title: "Free to play",
        body: &[
            "The demo disc is homebrew made with the PSoXide tools. The source is on GitHub.",
            "If you want to support the project, there is an optional donation on itch.io.",
        ],
        links: &[
            ("Support PSoXide on itch.io", super::ITCH_URL),
            ("Source code on GitHub", super::SOURCE_URL),
        ],
    },
];

/// A row of a list page.
pub(super) struct Row {
    pub action: MenuAction,
    pub label: String,
    pub value: String,
    pub depth: usize,
}

fn category_name(page: Page, pausing: bool) -> &'static str {
    match (page, pausing) {
        (Page::Library, _) => "Library",
        (Page::Settings, _) => "Settings",
        _ => "Game",
    }
}

/// The rows of the page's category, in order.
pub(super) fn visible_rows(ms: &MenuState, page: Page, pausing: bool) -> Vec<Row> {
    let name = category_name(page, pausing);
    ms.categories
        .iter()
        .find(|c| c.name == name)
        .map(|c| {
            c.items
                .iter()
                .map(|i: &MenuItem| Row {
                    action: i.action.clone(),
                    label: i.label.clone(),
                    value: i.value.clone().unwrap_or_default(),
                    depth: i.depth,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn percent(value: &str) -> Option<u32> {
    value.trim().trim_end_matches('%').parse().ok()
}

/// How a settings row is drawn and edited.
enum Control {
    Plain,
    Toggle(bool),
    Choice(&'static [&'static str], usize),
    Slider(f32, f32, f32),
    Stepper,
}

fn control_for(row: &Row) -> Control {
    let on = row.value == "On";
    match &row.action {
        MenuAction::ToggleMute | MenuAction::ToggleTouchHaptics | MenuAction::ToggleTouchLeftHanded => {
            Control::Toggle(on)
        }
        MenuAction::ToggleSmoothSlowHost => {
            Control::Choice(&["Skip frames", "Slow the game"], usize::from(row.value == "Slow the game"))
        }
        MenuAction::CycleVideoScale => {
            Control::Choice(&["High-res", "Native"], usize::from(row.value == "Native"))
        }
        MenuAction::CycleTouchMode => {
            let options: &'static [&'static str] = &["Auto", "On", "Off"];
            let at = options.iter().position(|o| *o == row.value).unwrap_or(0);
            Control::Choice(options, at)
        }
        MenuAction::CycleVolume => {
            Control::Slider(percent(&row.value).unwrap_or(100) as f32, 0.0, 150.0)
        }
        MenuAction::CycleTouchOpacity => {
            let (lo, hi) = TouchSliderKind::Opacity.range();
            Control::Slider(percent(&row.value).unwrap_or(60) as f32, lo, hi)
        }
        MenuAction::CycleTouchSize => {
            let (lo, hi) = TouchSliderKind::Size.range();
            Control::Slider(percent(&row.value).unwrap_or(100) as f32, lo, hi)
        }
        MenuAction::CycleTextureFilter
        | MenuAction::CycleDeinterlace
        | MenuAction::CycleUiScale
        | MenuAction::CycleMenuOpacity => Control::Stepper,
        _ => Control::Plain,
    }
}

/// Left and Right on a settings row: sliders step, everything else cycles.
/// Confirm activates the row.
pub(super) fn step_action(ms: &mut MenuState, action: &MenuAction, input: &MenuInput) -> Option<MenuAction> {
    let rows = visible_rows(ms, Page::Settings, false);
    let row = rows.get(ms.item_index)?;
    match control_for(row) {
        Control::Slider(value, lo, hi) => {
            let step = if matches!(row.action, MenuAction::CycleVolume) { 10.0 } else { 5.0 };
            let delta = if input.right { step } else if input.left { -step } else { return None };
            let next = (value + delta).clamp(lo, hi);
            apply_slider(ms, &row.action, next as u32)
        }
        _ => (input.confirm || input.left || input.right).then(|| action.clone()),
    }
}

fn apply_slider(ms: &mut MenuState, action: &MenuAction, pct: u32) -> Option<MenuAction> {
    match action {
        MenuAction::CycleVolume => Some(MenuAction::SetVolume(pct.min(150) as u8)),
        other => {
            if let Some(kind) = TouchSliderKind::of(other) {
                ms.touch_slider = Some((kind, pct.min(255) as u8));
            }
            None
        }
    }
}

/// A drag across a slider track: `t` is 0..1 along it.
pub(super) fn slider_set(ms: &mut MenuState, row_index: usize, t: f32) -> Option<MenuAction> {
    let rows = visible_rows(ms, Page::Settings, false);
    let row = rows.get(row_index)?;
    let Control::Slider(_, lo, hi) = control_for(row) else {
        return None;
    };
    let pct = (lo + (hi - lo) * t).round() as u32;
    let action = row.action.clone();
    apply_slider(ms, &action, pct)
}

pub(super) fn draw_list(h: &mut HomeState, ms: &mut MenuState, ctx: &egui::Context, pc: &mut PageCtx<'_>) {
    let pausing = h.pausing(ms);
    let page = if pausing && h.page == Page::Home { Page::Home } else { h.page };
    let rows = visible_rows(ms, page, pausing);
    let lay = pc.lay;
    let p = pc.painter;
    let a = pc.appear;
    let u = lay.u;
    let desktop = lay.kind == LayoutKind::Desktop;
    let touch = ms.touch_mode;
    let row_h = (if touch { 54.0 } else { 46.0 }) * u.max(0.9);
    let width = (if desktop { 780.0 } else { lay.safe.width() - 40.0 * u }).min(lay.safe.width() - 24.0);
    let left = lay.safe.center().x - width / 2.0;
    let head_top = lay.safe.top() + if desktop { 96.0 * u } else { 82.0 * u };
    let title = match page {
        Page::Library => "Library",
        Page::Settings => "Settings",
        _ => "Paused",
    };
    let t = p.text(
        Pos2::new(left, head_top),
        Align2::LEFT_TOP,
        title,
        FontId::proportional(26.0 * u),
        fade(Color32::WHITE, a),
    );
    let top = t.bottom() + 18.0 * u;
    let bottom = lay.safe.bottom() - 18.0 * u;
    let view = Rect::from_min_max(Pos2::new(left, top), Pos2::new(left + width, bottom));
    // Keep the focused row on screen when the focus moved by key.
    if h.last_row != ms.item_index {
        let y0 = ms.item_index as f32 * row_h;
        if y0 < h.list_scroll {
            h.list_scroll = y0;
        } else if y0 + row_h > h.list_scroll + view.height() {
            h.list_scroll = y0 + row_h - view.height();
        }
        h.last_row = ms.item_index;
    }
    let max_scroll = (rows.len() as f32 * row_h - view.height()).max(0.0);
    h.list_scroll = h.list_scroll.clamp(0.0, max_scroll);
    let clip = p.with_clip_rect(view.expand2(Vec2::new(40.0, 0.0)));
    for (i, row) in rows.iter().enumerate() {
        let y = view.top() + i as f32 * row_h - h.list_scroll;
        if y + row_h < view.top() || y > view.bottom() {
            continue;
        }
        let r = Rect::from_min_size(Pos2::new(left, y), Vec2::new(width, row_h - 4.0));
        let focused = h.zone == super::Zone::Body && i == ms.item_index;
        draw_row(h, ms, ctx, &clip, r, row, focused, u, a, i);
    }
    // A scroll hint when more is below.
    if h.list_scroll < max_scroll - 1.0 {
        super::triangle(
            &clip,
            Pos2::new(view.center().x, view.bottom() - 8.0),
            5.0 * u,
            true,
            fade(Color32::from_rgb(111, 129, 146), a),
        );
    }
    if rows.is_empty() {
        p.text(
            view.center_top() + Vec2::new(0.0, 30.0 * u),
            Align2::CENTER_TOP,
            "Nothing here yet.",
            FontId::proportional(15.0 * u),
            fade(Color32::from_rgb(143, 164, 179), a),
        );
    }
    if !desktop || touch {
        back_button(h, p, lay.safe.left() + 20.0 * u, lay.safe.bottom() - 40.0 * u, u, a);
    }
}

fn back_button(h: &mut HomeState, p: &egui::Painter, x: f32, y: f32, u: f32, a: f32) {
    let r = p.text(
        Pos2::new(x, y),
        Align2::LEFT_CENTER,
        "\u{2039} Back",
        FontId::proportional(15.0 * u),
        fade(Color32::from_rgb(160, 180, 195), a),
    );
    h.hits.push((r.expand2(Vec2::new(14.0, 14.0)), Click::Back));
}

#[allow(clippy::too_many_arguments)]
fn draw_row(
    h: &mut HomeState,
    ms: &MenuState,
    ctx: &egui::Context,
    p: &egui::Painter,
    r: Rect,
    row: &Row,
    focused: bool,
    u: f32,
    a: f32,
    index: usize,
) {
    let fill = if focused {
        Color32::from_rgb(14, 40, 50)
    } else {
        Color32::from_rgba_unmultiplied(16, 24, 32, 150)
    };
    p.rect_filled(r, 6.0, fade(fill, a));
    if focused {
        p.rect_filled(
            Rect::from_min_size(r.min + Vec2::new(0.0, 8.0), Vec2::new(3.0, r.height() - 16.0)),
            1.5,
            fade(CYAN, a),
        );
    }
    h.hits.push((r, Click::Row(index)));
    let indent = row.depth as f32 * 22.0 * u;
    let mut x = r.left() + 18.0 * u + indent;
    let cy = r.center().y;
    let is_folder = matches!(row.action, MenuAction::ToggleLibraryFolder(_));
    let launch_id = match &row.action {
        MenuAction::LaunchGame(id) => Some(id.clone()),
        _ => None,
    };
    if is_folder {
        let open = ms.expanded_folders.contains(match &row.action {
            MenuAction::ToggleLibraryFolder(path) => path.as_path(),
            _ => std::path::Path::new(""),
        });
        super::triangle(p, Pos2::new(x + 5.0 * u, cy), 5.0 * u, open, fade(CYAN, a));
        x += 22.0 * u;
    } else if let Some(id) = launch_id {
        // A small case: the cover when there is one, a quiet placeholder if not.
        let size = Vec2::new(r.height() - 14.0, r.height() - 14.0);
        let box_r = Rect::from_min_size(Pos2::new(x, cy - size.y / 2.0), size);
        match h.covers.texture(ctx, &id) {
            Some(tex) => {
                p.image(
                    tex,
                    box_r,
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    fade(Color32::WHITE, a),
                );
            }
            None => {
                p.rect_filled(box_r, 3.0, fade(Color32::from_rgb(17, 36, 46), a));
                p.rect_stroke(
                    box_r,
                    3.0,
                    Stroke::new(1.0, fade(CYAN, a * 0.35)),
                    egui::StrokeKind::Inside,
                );
                p.circle_stroke(box_r.center(), size.x * 0.22, Stroke::new(1.0, fade(CYAN, a * 0.5)));
            }
        }
        x += size.x + 12.0 * u;
    }
    let label_col = if focused { Color32::WHITE } else { Color32::from_rgb(214, 224, 232) };
    let ctrl = control_for(row);
    let right = r.right() - 16.0 * u;
    match ctrl {
        Control::Plain => {
            let lab = p.text(
                Pos2::new(x, cy),
                Align2::LEFT_CENTER,
                &row.label,
                FontId::proportional(16.0 * u),
                fade(label_col, a),
            );
            if !row.value.is_empty() {
                let vx = right;
                let max_w = (vx - lab.right() - 24.0 * u).max(40.0);
                let galley = p.layout(
                    row.value.clone(),
                    FontId::proportional(13.0 * u),
                    fade(Color32::from_rgb(127, 147, 162), a),
                    max_w,
                );
                let gp = Pos2::new(vx - galley.size().x, cy - galley.size().y / 2.0);
                p.add(Shape::galley(gp, galley, Color32::WHITE));
            }
        }
        _ => {
            p.text(
                Pos2::new(x, cy),
                Align2::LEFT_CENTER,
                &row.label,
                FontId::proportional(16.0 * u),
                fade(label_col, a),
            );
            draw_control(h, p, &ctrl, row, r, index, u, a);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_control(
    h: &mut HomeState,
    p: &egui::Painter,
    ctrl: &Control,
    row: &Row,
    r: Rect,
    index: usize,
    u: f32,
    a: f32,
) {
    let cy = r.center().y;
    let right = r.right() - 16.0 * u;
    let dim = fade(Color32::from_rgb(143, 164, 179), a);
    match ctrl {
        Control::Toggle(on) => {
            let w = 44.0 * u;
            let hgt = 24.0 * u;
            let track = Rect::from_min_size(Pos2::new(right - w, cy - hgt / 2.0), Vec2::new(w, hgt));
            p.rect_filled(track, hgt / 2.0, fade(if *on { Color32::from_rgb(19, 156, 176) } else { Color32::from_rgb(48, 59, 74) }, a));
            let knob_x = if *on { track.right() - hgt / 2.0 } else { track.left() + hgt / 2.0 };
            p.circle_filled(Pos2::new(knob_x, cy), hgt / 2.0 - 3.0, fade(if *on { Color32::WHITE } else { Color32::from_rgb(170, 180, 192) }, a));
        }
        Control::Choice(options, at) => {
            let mut x = right;
            for (i, opt) in options.iter().enumerate().rev() {
                let galley = p.layout_no_wrap(
                    (*opt).to_string(),
                    FontId::proportional(13.0 * u),
                    Color32::WHITE,
                );
                let w = galley.size().x + 22.0 * u;
                let pill = Rect::from_min_size(Pos2::new(x - w, cy - 14.0 * u), Vec2::new(w, 28.0 * u));
                if i == *at {
                    p.rect_filled(pill, 14.0 * u, fade(CYAN, a));
                } else {
                    p.rect_stroke(pill, 14.0 * u, Stroke::new(1.0, fade(Color32::from_rgb(48, 59, 74), a)), egui::StrokeKind::Inside);
                }
                let col = if i == *at { Color32::from_rgb(4, 34, 42) } else { Color32::from_rgb(143, 164, 179) };
                p.text(pill.center(), Align2::CENTER_CENTER, *opt, FontId::proportional(13.0 * u), fade(col, a));
                x = pill.left() - 6.0 * u;
            }
        }
        Control::Slider(value, lo, hi) => {
            let label = format!("{}%", *value as u32);
            let vt = p.text(
                Pos2::new(right, cy),
                Align2::RIGHT_CENTER,
                label,
                FontId::proportional(14.0 * u),
                fade(Color32::from_rgb(214, 224, 232), a),
            );
            let tw = (200.0 * u).min(r.width() * 0.4);
            let track = Rect::from_min_max(
                Pos2::new(vt.left() - 16.0 * u - tw, cy - 3.0),
                Pos2::new(vt.left() - 16.0 * u, cy + 3.0),
            );
            let t = ((value - lo) / (hi - lo)).clamp(0.0, 1.0);
            p.rect_filled(track, 3.0, fade(Color32::from_rgb(48, 59, 74), a));
            p.rect_filled(
                Rect::from_min_max(track.min, Pos2::new(track.left() + track.width() * t, track.bottom())),
                3.0,
                fade(CYAN, a),
            );
            p.circle_filled(Pos2::new(track.left() + track.width() * t, cy), 8.0 * u, fade(Color32::WHITE, a));
            h.hits.push((track.expand2(Vec2::new(14.0, 16.0)), Click::Slider(index, track)));
        }
        Control::Stepper => {
            let vt = p.text(
                Pos2::new(right, cy),
                Align2::RIGHT_CENTER,
                format!("{}  \u{203a}", row.value),
                FontId::proportional(14.0 * u),
                dim,
            );
            let _ = vt;
        }
        Control::Plain => {}
    }
}

pub(super) fn draw_welcome(h: &mut HomeState, _ms: &MenuState, pc: &mut PageCtx<'_>) {
    let lay = pc.lay;
    let p = pc.painter;
    let a = pc.appear;
    let u = lay.u;
    let s = lay.safe;
    let page = &WELCOME[h.welcome.min(WELCOME.len() - 1)];
    let width = (560.0 * u).min(s.width() - 40.0 * u);
    let x = s.center().x - width / 2.0;
    let mut y = s.top() + if lay.kind == LayoutKind::Landscape { 64.0 * u } else { 140.0 * u };
    p.text(
        Pos2::new(x, y),
        Align2::LEFT_TOP,
        format!("WELCOME  {} / {}", h.welcome + 1, WELCOME.len()),
        FontId::proportional(11.5 * u),
        fade(CYAN, a),
    );
    y += 28.0 * u;
    let title = p.layout(
        page.title.to_string(),
        FontId::proportional(if lay.kind == LayoutKind::Landscape { 24.0 * u } else { 30.0 * u }),
        fade(Color32::WHITE, a),
        width,
    );
    let th = title.size().y;
    p.add(Shape::galley(Pos2::new(x, y), title, Color32::WHITE));
    y += th + 16.0 * u;
    for para in page.body {
        let g = p.layout(
            (*para).to_string(),
            FontId::proportional(17.0 * u),
            fade(Color32::from_rgb(185, 197, 209), a),
            width,
        );
        let gh = g.size().y;
        p.add(Shape::galley(Pos2::new(x, y), g, Color32::WHITE));
        y += gh + 12.0 * u;
    }
    for (label, url) in page.links {
        let r = p.text(
            Pos2::new(x, y + 4.0 * u),
            Align2::LEFT_TOP,
            *label,
            FontId::proportional(16.0 * u),
            fade(Color32::from_rgb(127, 207, 224), a),
        );
        p.line_segment(
            [r.left_bottom(), r.right_bottom()],
            Stroke::new(1.0, fade(Color32::from_rgb(127, 207, 224), a * 0.5)),
        );
        h.hits.push((r.expand2(Vec2::new(10.0, 10.0)), Click::Link(url)));
        y = r.bottom() + 14.0 * u;
    }
    // Dots and the two steps.
    let foot_y = (y + 36.0 * u).max(s.top() + 120.0 * u).min(s.bottom() - 70.0 * u);
    for i in 0..WELCOME.len() {
        let on = i == h.welcome;
        let c = Pos2::new(x + 6.0 * u + i as f32 * 18.0 * u, foot_y + 18.0 * u);
        p.circle_filled(c, if on { 4.0 * u } else { 3.0 * u }, fade(if on { CYAN } else { Color32::from_rgb(42, 56, 70) }, a));
    }
    let last = h.welcome + 1 >= WELCOME.len();
    let next = Rect::from_min_size(
        Pos2::new(x + width - 120.0 * u, foot_y),
        Vec2::new(120.0 * u, 38.0 * u),
    );
    p.rect_filled(next, 19.0 * u, fade(CYAN, a));
    p.text(
        next.center(),
        Align2::CENTER_CENTER,
        if last { "DONE" } else { "NEXT" },
        FontId::proportional(13.0 * u),
        fade(Color32::from_rgb(4, 34, 42), a),
    );
    h.hits.push((next, Click::WelcomeStep(1)));
    if h.welcome > 0 {
        let prev = Rect::from_min_size(next.min - Vec2::new(110.0 * u, 0.0), Vec2::new(100.0 * u, 38.0 * u));
        p.rect_stroke(prev, 19.0 * u, Stroke::new(1.0, fade(Color32::from_rgb(60, 76, 92), a)), egui::StrokeKind::Inside);
        p.text(prev.center(), Align2::CENTER_CENTER, "BACK", FontId::proportional(13.0 * u), fade(Color32::from_rgb(190, 205, 216), a));
        h.hits.push((prev, Click::WelcomeStep(-1)));
    }
    let close = p.text(
        Pos2::new(s.right() - if lay.kind == LayoutKind::Desktop { 48.0 * u } else { 20.0 * u }, s.top() + 74.0 * u),
        Align2::RIGHT_TOP,
        "Close",
        FontId::proportional(14.0 * u),
        fade(Color32::from_rgb(143, 164, 179), a),
    );
    h.hits.push((close.expand2(Vec2::new(12.0, 12.0)), Click::Dismiss));
}
