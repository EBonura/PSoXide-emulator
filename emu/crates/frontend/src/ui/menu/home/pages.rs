//! The list pages (Library, Settings, the pause list) and the welcome pages.

use egui::{Align2, Color32, FontId, Pos2, Rect, Shape, Stroke, Vec2};

use super::scene::{fade, CYAN};
use super::{motion, tokens, Click, HomeState, LayoutKind, Page, PageCtx};
use crate::ui::menu::{MenuAction, MenuInput, MenuItem, MenuState, TouchSliderKind};

/// The games on the public demo disc, with the small picture each shows on the home screen.
/// The count is the one the welcome pages and the home line give ("seven").
pub(super) const DISC_GAMES: [(&str, &[u8]); 7] = [
    (
        "PSoXide Arcade",
        include_bytes!("../../../../../../../assets/home/disc/arcade.png"),
    ),
    (
        "Celeste Collection",
        include_bytes!("../../../../../../../assets/home/disc/celeste.png"),
    ),
    (
        "NitroXide",
        include_bytes!("../../../../../../../assets/home/disc/nitroxide.png"),
    ),
    (
        "VoXide",
        include_bytes!("../../../../../../../assets/home/disc/voxide.png"),
    ),
    (
        "Quake (shareware)",
        include_bytes!("../../../../../../../assets/home/disc/quake.png"),
    ),
    (
        "Cortex Ignition",
        include_bytes!("../../../../../../../assets/home/disc/cortex.png"),
    ),
    (
        "PSXcel",
        include_bytes!("../../../../../../../assets/home/disc/psxcel.png"),
    ),
];

/// One welcome page: a title and a few short paragraphs.
pub(super) struct WelcomePage {
    pub title: &'static str,
    pub body: &'static [&'static str],
    pub links: &'static [(&'static str, &'static str)],
}

pub(super) const WELCOME: [WelcomePage; 3] = [
    WelcomePage {
        title: if cfg!(target_arch = "wasm32") {
            "A PlayStation, in your browser"
        } else {
            "A PlayStation, on your computer"
        },
        body: &[
            "PSoXide is a free PlayStation emulator. It comes with a built-in demo disc: seven homebrew games and demos made for the original PlayStation, from arcade classics to Shareware Quake.",
            "It is independent and not affiliated with Sony.",
        ],
        links: &[],
    },
    WelcomePage {
        title: "Play your own PS1 games",
        body: &[
            "Got PlayStation discs of your own? Open a folder of your PS1 game disc images (CUE/BIN, ISO, CCD/IMG and M3U) and play them here. Files are read in place on your device and never uploaded.",
            "PSoXide fetches cover pictures for your discs from community collections (xlenore/psx-covers and libretro-thumbnails) and names your discs with the Redump list from libretro-database (CC BY-SA 4.0). Both are kept on this device. You can turn this off in Settings.",
        ],
        links: &[],
    },
    WelcomePage {
        title: "Even more games",
        body: &[
            "The PSoXide project also has bigger PlayStation ports, like Half-Life, that you build yourself from your own Steam copy of the original game. Everything is on GitHub.",
            "You can support the project with an optional donation on itch.io.",
            concat!(
                "Version ",
                env!("CARGO_PKG_VERSION"),
                ", GPL-2.0-or-later. PlayStation and PS1 are trademarks of Sony Interactive Entertainment."
            ),
        ],
        links: &[
            ("Support PSoXide on itch.io", super::ITCH_URL),
            ("Source code on GitHub", super::SOURCE_URL),
            ("More PSoXide games you build yourself", super::SITE_URL),
        ],
    },
];

/// A row of a list page.
pub(super) struct Row {
    pub action: MenuAction,
    pub label: String,
    pub value: String,
    pub depth: usize,
    pub burn: Option<MenuAction>,
}

fn category_name(page: Page) -> &'static str {
    match page {
        Page::Settings | Page::Controls => "Settings",
        _ => "Game",
    }
}

/// The rows of the page's category, in order.
pub(super) fn visible_rows(ms: &MenuState, page: Page) -> Vec<Row> {
    let name = category_name(page);
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
                    burn: i.burn_action.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn percent(value: &str) -> Option<u32> {
    value.trim().trim_end_matches('%').parse().ok()
}

/// The face of a slider's number: the same type as every other row's value (UI scale and the rest).
fn value_font(u: f32) -> FontId {
    FontId::proportional(tokens::BODY * u)
}

/// How a settings row is drawn and edited.
enum Control {
    Plain,
    Toggle(bool),
    Choice(&'static [&'static str], usize),
    Slider(f32, f32, f32),
    Stepper,
}

/// Whether a row is a plain action (no switch, slider or stepper to change under the cursor).
pub(super) fn is_plain(row: &Row) -> bool {
    matches!(control_for(row), Control::Plain)
}

fn control_for(row: &Row) -> Control {
    let on = row.value == "On";
    match &row.action {
        MenuAction::ToggleMute
        | MenuAction::ToggleTouchHaptics
        | MenuAction::ToggleTouchLeftHanded
        | MenuAction::ToggleMenuSounds => Control::Toggle(on),
        MenuAction::ToggleSmoothSlowHost => Control::Choice(
            &["Skip frames", "Slow the game"],
            usize::from(row.value == "Slow the game"),
        ),
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
pub(super) fn step_action(
    ms: &mut MenuState,
    action: &MenuAction,
    input: &MenuInput,
) -> Option<MenuAction> {
    let rows = visible_rows(ms, Page::Settings);
    let row = rows.get(ms.item_index)?;
    match control_for(row) {
        Control::Slider(value, lo, hi) => {
            let step = if matches!(row.action, MenuAction::CycleVolume) {
                10.0
            } else {
                5.0
            };
            let delta = if input.right {
                step
            } else if input.left {
                -step
            } else {
                return None;
            };
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
    let rows = visible_rows(ms, Page::Settings);
    let row = rows.get(row_index)?;
    let Control::Slider(_, lo, hi) = control_for(row) else {
        return None;
    };
    let pct = (lo + (hi - lo) * t).round() as u32;
    let action = row.action.clone();
    apply_slider(ms, &action, pct)
}

pub(super) fn draw_list(
    h: &mut HomeState,
    ms: &mut MenuState,
    ctx: &egui::Context,
    pc: &mut PageCtx<'_>,
) {
    let page = h.page;
    let rows = visible_rows(ms, page);
    let lay = pc.lay;
    let p = pc.painter;
    let a = pc.appear;
    let u = lay.u;
    let desktop = lay.kind == LayoutKind::Desktop;
    let touch = ms.touch_mode;
    let row_h = (if desktop {
        tokens::ROW_DESKTOP
    } else {
        tokens::ROW_PHONE
    }) * u.max(0.9);
    let _ = touch;
    let width = (if desktop {
        780.0
    } else {
        lay.safe.width() - 40.0 * u
    })
    .min(lay.safe.width() - 24.0);
    let left = lay.safe.center().x - width / 2.0;
    let head_top = lay.safe.top()
        + match lay.kind {
            LayoutKind::Desktop => 96.0 * u,
            LayoutKind::Portrait => 64.0 * u,
            LayoutKind::Landscape => 44.0 * u,
        };
    let bottom = lay.safe.bottom()
        - if desktop {
            18.0 * u + h.hint_reserve(lay)
        } else {
            lay.bar_reserve() + 4.0 * u
        };
    h.draw_page_hints(
        pc,
        &[
            (super::Btn::Cross, "Select"),
            (
                super::Btn::Circle,
                if page == Page::Game { "Resume" } else { "Back" },
            ),
        ],
    );
    // Over a paused game the list sits on the shell's panel, as Controls and Debug do, so the
    // rows are never read against the game picture.
    let head = h.reveal(0);
    if h.pausing(ms) && h.launch.is_none() {
        let panel = Rect::from_min_max(
            Pos2::new(left - 24.0 * u, head_top - 18.0 * u),
            Pos2::new(left + width + 24.0 * u, bottom + 6.0 * u),
        )
        .intersect(lay.full.shrink(2.0));
        p.add(Shape::convex_polygon(
            super::chamfer(panel, tokens::CHAMFER * u),
            fade(Color32::from_rgba_unmultiplied(8, 16, 22, 242), a * head),
            Stroke::new(1.0, fade(tokens::hairline(), a * head)),
        ));
    }
    let title = match page {
        Page::Settings => "Settings",
        _ => "Game",
    };
    let title_from = motion::mark(ctx, p);
    let t = p.text(
        Pos2::new(left, head_top),
        Align2::LEFT_TOP,
        title,
        crate::theme::title_font(
            if desktop {
                tokens::TITLE_DESKTOP
            } else {
                tokens::TITLE_PHONE
            } * u,
        ),
        fade(tokens::TEXT, a * head),
    );
    motion::shift_since(ctx, p, title_from, h.slide(head, u));
    let top = t.bottom() + 18.0 * u;
    // A thin strip at each end of the list holds its scroll hint, so the arrows never sit on a row.
    let hint = 14.0 * u;
    let view = Rect::from_min_max(
        Pos2::new(left, top + hint),
        Pos2::new(left + width, bottom - hint),
    );
    // Keep the focused row on screen when the focus moved by key.
    if h.last_row != ms.item_index {
        let y0 = ms.item_index as f32 * row_h;
        if y0 < h.list_scroll {
            h.list_scroll = y0;
        } else if y0 + row_h > h.list_scroll + view.height() {
            h.list_scroll = y0 + row_h - view.height();
        }
        h.last_row = ms.item_index;
        h.kin.stop();
    }
    let max_scroll = (rows.len() as f32 * row_h - view.height()).max(0.0);
    // A flick coasts on after the finger lifts, and stretches against either end.
    h.kin
        .step(&mut h.list_scroll, h.frame_dt, max_scroll, view.height());
    // The wheel scrolls a list while the pointer is over it.
    let wheel = ctx.input(|i| i.smooth_scroll_delta.y);
    if wheel != 0.0 && ctx.input(|i| i.pointer.hover_pos().is_some_and(|at| view.contains(at))) {
        h.kin.wheel(&mut h.list_scroll, -wheel, max_scroll);
    }
    h.kin.limit(&mut h.list_scroll, max_scroll);
    let clip = p.with_clip_rect(view.expand2(Vec2::new(40.0, 0.0)));
    for (i, row) in rows.iter().enumerate() {
        let y = view.top() + i as f32 * row_h - h.list_scroll;
        if y + row_h < view.top() || y > view.bottom() {
            continue;
        }
        let r = Rect::from_min_size(Pos2::new(left, y), Vec2::new(width, row_h - 4.0));
        let focused = h.zone == super::Zone::Body && i == ms.item_index;
        // The rows arrive one after another, sliding in along the way the page came.
        let part = h.reveal(i + 1);
        let from = motion::mark(ctx, &clip);
        draw_row_with(h, ms, ctx, &clip, r, row, focused, u, a * part, i, false);
        motion::shift_since(ctx, &clip, from, h.slide(part, u));
    }
    // The focus cage is one frame that glides from row to row (a small spring), not one that
    // switches off on one row and on in the next.
    let dt = h.frame_dt;
    let snap = h.calm() || h.page_age < 0.05;
    let target = ms.item_index.min(rows.len().saturating_sub(1)) as f32 * row_h;
    h.row_cage.step(target, dt, snap);
    if h.zone == super::Zone::Body && !rows.is_empty() {
        let y = view.top() + h.row_cage.pos - h.list_scroll;
        let cage = Rect::from_min_size(Pos2::new(left, y), Vec2::new(width, row_h - 4.0))
            .intersect(clip.clip_rect());
        if cage.height() > 8.0 {
            super::focus_cage(&clip, cage, a * h.reveal(ms.item_index + 1), u);
        }
    }
    // A scroll hint at each end that has rows hidden past it, and none where the list is at rest.
    h.list_view = [h.list_scroll, max_scroll, view.top(), view.bottom()];
    let (more_above, more_below) = scroll_hints(h.list_scroll, max_scroll);
    // Each is drawn in its own strip outside the rows.
    if more_above {
        let c = Pos2::new(view.center().x, view.top() - hint * 0.5);
        super::triangle_up(p, c, 5.0 * u, fade(tokens::MUTED, a));
    }
    if more_below {
        let c = Pos2::new(view.center().x, view.bottom() + hint * 0.5);
        super::triangle(p, c, 5.0 * u, true, fade(tokens::MUTED, a));
    }
    if rows.is_empty() {
        p.text(
            view.center_top() + Vec2::new(0.0, 30.0 * u),
            Align2::CENTER_TOP,
            "Nothing here yet.",
            FontId::proportional(15.0 * u),
            fade(tokens::MUTED, a),
        );
    }
}

/// Whether rows are hidden above and below a list scrolled `scroll` of `max_scroll`.
fn scroll_hints(scroll: f32, max_scroll: f32) -> (bool, bool) {
    (scroll > 1.0, scroll < max_scroll - 1.0)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn draw_row(
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
    draw_row_with(h, ms, ctx, p, r, row, focused, u, a, index, true);
}

/// A row. `cage` draws the focus frame on a focused row; the list pages pass false and glide one
/// shared frame between rows instead.
#[allow(clippy::too_many_arguments)]
fn draw_row_with(
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
    cage: bool,
) {
    // A flat list: a hairline between rows, and the focus cage on the row you are on.
    p.line_segment(
        [r.left_bottom(), r.right_bottom()],
        Stroke::new(1.0, fade(tokens::hairline(), a)),
    );
    if focused && cage {
        super::focus_cage(p, r, a, u);
    }
    h.hits.push((r.intersect(p.clip_rect()), Click::Row(index)));
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
                    fade(tokens::TEXT, a),
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
                p.circle_stroke(
                    box_r.center(),
                    size.x * 0.22,
                    Stroke::new(1.0, fade(CYAN, a * 0.5)),
                );
            }
        }
        x += size.x + 12.0 * u;
    }
    let mut value_right = r.right() - 16.0 * u;
    if row.burn.is_some() {
        let b = Rect::from_center_size(
            Pos2::new(r.right() - 40.0 * u, r.center().y),
            Vec2::new(56.0 * u, 26.0 * u),
        );
        p.rect_stroke(
            b,
            13.0 * u,
            Stroke::new(1.0, fade(CYAN, a * 0.7)),
            egui::StrokeKind::Inside,
        );
        p.text(
            b.center(),
            Align2::CENTER_CENTER,
            "Burn",
            FontId::proportional(12.0 * u),
            fade(tokens::FOCUS, a),
        );
        h.hits.push((b.expand(4.0), Click::RowBurn(index)));
        value_right = b.left() - 12.0 * u;
    }
    let label_col = if focused {
        tokens::TEXT
    } else {
        tokens::TEXT_2
    };
    let ctrl = control_for(row);
    let right = value_right;
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
                    fade(tokens::DIM, a),
                    max_w,
                );
                let gp = Pos2::new(vx - galley.size().x, cy - galley.size().y / 2.0);
                p.add(Shape::galley(gp, galley, tokens::TEXT));
            }
        }
        _ => {
            // A long label wraps in the room the control leaves it.
            let share = if r.width() > 600.0 { 0.55 } else { 0.40 };
            let max_w = ((r.width() - 36.0 * u) * share - (x - r.left())).max(60.0);
            let galley = p.layout(
                row.label.clone(),
                FontId::proportional(if r.width() > 600.0 {
                    16.0 * u
                } else {
                    15.0 * u
                }),
                fade(label_col, a),
                max_w,
            );
            let pos = Pos2::new(x, cy - galley.size().y / 2.0);
            p.add(Shape::galley(pos, galley, tokens::TEXT));
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
    match ctrl {
        Control::Toggle(on) => {
            draw_toggle(p, right, cy, *on, u, a);
        }
        Control::Choice(options, at) => {
            let mut x = right;
            for (i, opt) in options.iter().enumerate().rev() {
                let galley = p.layout_no_wrap(
                    (*opt).to_string(),
                    FontId::proportional(13.0 * u),
                    tokens::TEXT,
                );
                let w = galley.size().x + 22.0 * u;
                let pill =
                    Rect::from_min_size(Pos2::new(x - w, cy - 14.0 * u), Vec2::new(w, 28.0 * u));
                if i == *at {
                    p.rect_filled(pill, 14.0 * u, fade(CYAN, a));
                } else {
                    p.rect_stroke(
                        pill,
                        14.0 * u,
                        Stroke::new(1.0, fade(Color32::from_rgb(48, 59, 74), a)),
                        egui::StrokeKind::Inside,
                    );
                }
                let col = if i == *at {
                    tokens::ON_ACCENT
                } else {
                    tokens::MUTED
                };
                p.text(
                    pill.center(),
                    Align2::CENTER_CENTER,
                    *opt,
                    FontId::proportional(13.0 * u),
                    fade(col, a),
                );
                x = pill.left() - 6.0 * u;
            }
        }
        Control::Slider(value, lo, hi) => {
            let label = format!("{}%", *value as u32);
            p.text(
                Pos2::new(right, cy),
                Align2::RIGHT_CENTER,
                label,
                value_font(u),
                fade(tokens::MUTED, a),
            );
            let tw = (200.0 * u).min(r.width() * 0.4);
            // Every track starts at the same place: room for the widest value, not this one.
            let widest = p
                .layout_no_wrap("150%".to_string(), value_font(u), tokens::TEXT)
                .size()
                .x;
            let track_right = right - widest - 16.0 * u;
            let track = Rect::from_min_max(
                Pos2::new(track_right - tw, cy - 3.0),
                Pos2::new(track_right, cy + 3.0),
            );
            let t = ((value - lo) / (hi - lo)).clamp(0.0, 1.0);
            p.rect_filled(track, 3.0, fade(Color32::from_rgb(48, 59, 74), a));
            p.rect_filled(
                Rect::from_min_max(
                    track.min,
                    Pos2::new(track.left() + track.width() * t, track.bottom()),
                ),
                3.0,
                fade(CYAN, a),
            );
            p.circle_filled(
                Pos2::new(track.left() + track.width() * t, cy),
                8.0 * u,
                fade(tokens::TEXT, a),
            );
            h.hits.push((
                track
                    .expand2(Vec2::new(14.0, 20.0))
                    .intersect(p.clip_rect()),
                Click::Slider(index, track),
            ));
        }
        Control::Stepper => {
            // The value, and a drawn chevron: it cycles on confirm or a tap.
            let c = Pos2::new(right - 4.0 * u, cy);
            let d = 4.5 * u;
            p.add(Shape::line(
                vec![
                    c + Vec2::new(-d * 0.6, -d),
                    c + Vec2::new(d * 0.6, 0.0),
                    c + Vec2::new(-d * 0.6, d),
                ],
                Stroke::new(1.7, fade(tokens::ICON_REST, a)),
            ));
            p.text(
                Pos2::new(right - 18.0 * u, cy),
                Align2::RIGHT_CENTER,
                &row.value,
                FontId::proportional(tokens::BODY * u),
                fade(tokens::MUTED, a),
            );
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
    let top = s.top()
        + if lay.kind == LayoutKind::Landscape {
            64.0 * u
        } else {
            140.0 * u
        };
    let title_px = if lay.kind == LayoutKind::Desktop {
        tokens::TITLE_DESKTOP
    } else {
        tokens::TITLE_PHONE
    };
    // The words get what the footer leaves. A page that does not fit at full size is set smaller
    // (down to 0.74), and a short screen starts it higher, so no line meets the buttons.
    let limit = s.bottom() - lay.bar_reserve() - 48.0 * u - 28.0 * u;
    let height_at = |k: f32, top: f32| -> f32 {
        let uk = u * k;
        let mut y = top + 28.0 * uk;
        let title = p.layout(
            page.title.to_string(),
            crate::theme::title_font(title_px * uk),
            tokens::TEXT,
            width,
        );
        y += title.size().y + 16.0 * uk;
        for para in page.body {
            let g = p.layout(
                (*para).to_string(),
                FontId::proportional(17.0 * uk),
                tokens::TEXT_2,
                width,
            );
            y += g.size().y + 12.0 * uk;
        }
        y += page.links.len() as f32 * (16.0 * uk * 1.25 + 4.0 * uk + 14.0 * uk);
        y
    };
    let mut k = 1.0_f32;
    let mut y = top;
    while height_at(k, y) > limit && k > 0.74 {
        k -= 0.04;
        if k < 0.9 {
            y = y.min(top - (140.0 - 96.0) * u * (1.0 - k) / 0.26);
        }
    }
    let uk = u * k;
    // Each part comes in after the one before it, sliding along the way the page turned.
    let ctx = p.ctx();
    let (head, from) = (h.reveal(0), motion::mark(ctx, p));
    p.text(
        Pos2::new(x, y),
        Align2::LEFT_TOP,
        format!("WELCOME  {} / {}", h.welcome + 1, WELCOME.len()),
        FontId::proportional(tokens::CAPTION * u.max(1.0)),
        fade(CYAN, a * head),
    );
    y += 28.0 * uk;
    let title = p.layout(
        page.title.to_string(),
        crate::theme::title_font(title_px * uk),
        fade(tokens::TEXT, a * head),
        width,
    );
    let th = title.size().y;
    p.add(Shape::galley(Pos2::new(x, y), title, tokens::TEXT));
    y += th + 16.0 * uk;
    motion::shift_since(ctx, p, from, h.slide(head, u));
    for (i, para) in page.body.iter().enumerate() {
        let (part, from) = (h.reveal(i + 1), motion::mark(ctx, p));
        let g = p.layout(
            (*para).to_string(),
            FontId::proportional(17.0 * uk),
            fade(tokens::TEXT_2, a * part),
            width,
        );
        let gh = g.size().y;
        p.add(Shape::galley(Pos2::new(x, y), g, tokens::TEXT));
        y += gh + 12.0 * uk;
        motion::shift_since(ctx, p, from, h.slide(part, u));
    }
    for (label, url) in page.links {
        let (part, from) = (h.reveal(page.body.len() + 1), motion::mark(ctx, p));
        let r = p.text(
            Pos2::new(x, y + 4.0 * uk),
            Align2::LEFT_TOP,
            *label,
            FontId::proportional(16.0 * uk),
            fade(tokens::ACCENT, a * part),
        );
        p.line_segment(
            [r.left_bottom(), r.right_bottom()],
            Stroke::new(1.0, fade(tokens::ACCENT, a * part * 0.5)),
        );
        h.hits
            .push((r.expand2(Vec2::new(10.0, 10.0)), Click::Link(url)));
        y = r.bottom() + 14.0 * uk;
        motion::shift_since(ctx, p, from, h.slide(part, u));
    }
    // Dots and the two steps.
    let foot_y = (y + 36.0 * u)
        .max(s.top() + 120.0 * u)
        .min(s.bottom() - lay.bar_reserve() - 48.0 * u);
    for i in 0..WELCOME.len() {
        let on = i == h.welcome;
        let c = Pos2::new(x + 6.0 * u + i as f32 * 18.0 * u, foot_y + 18.0 * u);
        p.circle_filled(
            c,
            if on { 4.0 * u } else { 3.0 * u },
            fade(
                if on {
                    CYAN
                } else {
                    Color32::from_rgb(42, 56, 70)
                },
                a,
            ),
        );
    }
    h.draw_page_hints(
        pc,
        &[(super::Btn::Cross, "Next"), (super::Btn::Circle, "Close")],
    );
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
        fade(tokens::ON_ACCENT, a),
    );
    h.hits.push((next, Click::WelcomeStep(1)));
    if h.welcome > 0 {
        let prev = Rect::from_min_size(
            next.min - Vec2::new(110.0 * u, 0.0),
            Vec2::new(100.0 * u, 38.0 * u),
        );
        p.rect_stroke(
            prev,
            19.0 * u,
            Stroke::new(1.0, fade(tokens::RING, a)),
            egui::StrokeKind::Inside,
        );
        p.text(
            prev.center(),
            Align2::CENTER_CENTER,
            "BACK",
            FontId::proportional(13.0 * u),
            fade(tokens::TEXT_2, a),
        );
        h.hits.push((prev, Click::WelcomeStep(-1)));
    }
    let close = p.text(
        Pos2::new(
            s.right()
                - if lay.kind == LayoutKind::Desktop {
                    48.0 * u
                } else {
                    20.0 * u
                },
            s.top() + 74.0 * u,
        ),
        Align2::RIGHT_TOP,
        "Skip",
        FontId::proportional(14.0 * u),
        fade(tokens::MUTED, a),
    );
    h.hits
        .push((close.expand2(Vec2::new(12.0, 12.0)), Click::Dismiss));
}

/// A toggle switch whose right edge is at `right`. Returns its track.
pub(super) fn draw_toggle(
    p: &egui::Painter,
    right: f32,
    cy: f32,
    on: bool,
    u: f32,
    a: f32,
) -> Rect {
    let w = 44.0 * u;
    let hgt = 24.0 * u;
    let track = Rect::from_min_size(Pos2::new(right - w, cy - hgt / 2.0), Vec2::new(w, hgt));
    p.rect_filled(
        track,
        hgt / 2.0,
        fade(
            if on {
                Color32::from_rgb(19, 156, 176)
            } else {
                Color32::from_rgb(48, 59, 74)
            },
            a,
        ),
    );
    let knob_x = if on {
        track.right() - hgt / 2.0
    } else {
        track.left() + hgt / 2.0
    };
    p.circle_filled(
        Pos2::new(knob_x, cy),
        hgt / 2.0 - 3.0,
        fade(if on { tokens::TEXT } else { tokens::MUTED }, a),
    );
    track
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_welcome_pages_say_what_the_demo_disc_and_the_project_hold() {
        let words = [
            "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
        ];
        let count = words[DISC_GAMES.len()];
        assert!(
            WELCOME[0].body[0].contains(&format!("{count} homebrew games")),
            "page 1 gives the demo disc's {count} games: {}",
            WELCOME[0].body[0]
        );
        assert!(WELCOME[0].body[0].contains("built-in demo disc"));
        assert!(WELCOME[0].body[0].ends_with("Shareware Quake."));
        assert!(WELCOME[1].body[0].contains("PS1 game disc images"));
        assert!(WELCOME[1].body[0].contains("CUE/BIN"));
        assert!(WELCOME[2].body[0].contains("Half-Life"));
        assert!(WELCOME[2].body[0].contains("your own Steam copy"));
        assert_eq!(WELCOME[2].links.len(), 3, "the links stay");
    }

    #[test]
    fn slider_numbers_use_the_shell_type_not_the_pixel_face() {
        assert_eq!(value_font(1.0).family, egui::FontFamily::Proportional);
        assert!((value_font(1.0).size - tokens::BODY).abs() < 1e-3);
    }

    #[test]
    fn scroll_hints_show_only_where_rows_are_hidden() {
        assert_eq!(scroll_hints(0.0, 300.0), (false, true), "at the top");
        assert_eq!(scroll_hints(120.0, 300.0), (true, true), "in the middle");
        assert_eq!(scroll_hints(300.0, 300.0), (true, false), "at the bottom");
        assert_eq!(scroll_hints(0.0, 0.0), (false, false), "a list that fits");
    }
}
