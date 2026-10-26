//! The live debug panel: performance, input tape, developer tools, CPU, memory and video, docked
//! beside the running game on a wide window and under it on a tall one, so the game, its
//! controls and its debug readouts share one screen and the game never pauses for them.
//!
//! The individual debug tools still own their content rendering; this module docks them into one
//! panel with a summary on top, a row of tabs and the tab's content scrolling below.

use egui::{
    Align, Color32, FontId, Layout, Rect, RichText, Sense, SidePanel, Stroke, TopBottomPanel,
    UiBuilder, Vec2,
};

use crate::app::AppState;
use crate::theme;
use crate::ui::menu::home::tokens;

use super::{memory, momentum, registers, vram};

const SIDEBAR_MIN_WIDTH: f32 = 320.0;
const SIDEBAR_MAX_WIDTH: f32 = 900.0;
/// A window at least this wide docks the panel at the width the player dragged it to; a narrower
/// one (a phone held sideways, a small window) gives it a share of its width instead.
const WIDE_WINDOW: f32 = 1000.0;
/// Share of a narrow window's width the panel takes beside the game.
const COMPACT_SHARE: f32 = 0.38;
const COMPACT_MIN_WIDTH: f32 = 260.0;
const COMPACT_MAX_WIDTH: f32 = 420.0;
/// Share of the room under the toolbar the panel takes below the game on a tall window, and its
/// limits in points.
const STRIP_SHARE: f32 = 0.42;
const STRIP_MIN_HEIGHT: f32 = 130.0;
const STRIP_MAX_HEIGHT: f32 = 420.0;
/// Inner padding, replacing the default panel frame margin we drop so the
/// slide-in content-width math stays exact.
const SIDEBAR_PAD: f32 = 8.0;
/// Height of the grab bar on a phone's debug sheet, points.
const SHEET_HANDLE: f32 = 18.0;
/// Release speed that throws the sheet to the far side, points per second.
const SHEET_FLING: f32 = 350.0;
/// Height of the phone's one-line summary, points.
const SUMMARY_ROW: f32 = 32.0;

/// Where the panel sits and how much of the window it takes, in points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Dock {
    /// A column on the right of the game: the width.
    Right(f32),
    /// A strip between the game and the controller panel: the height.
    Bottom(f32),
}

/// Where the panel docks in a window of `screen` points, when it has `free_height` points under
/// the toolbar to share and the player last dragged it `remembered_width` wide.
///
/// A window wider than tall puts it on the right, so the game, the pad around it and the readouts
/// all fit one screen; a taller one puts it under the game, above the controller's panel.
pub(crate) fn dock_for(screen: Vec2, free_height: f32, remembered_width: f32) -> Dock {
    if screen.x >= screen.y {
        let width = if screen.x >= WIDE_WINDOW {
            remembered_width
                .clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH)
                .min(screen.x * 0.5)
        } else {
            (screen.x * COMPACT_SHARE).clamp(COMPACT_MIN_WIDTH, COMPACT_MAX_WIDTH)
        };
        Dock::Right(width)
    } else {
        let height = (free_height * STRIP_SHARE).clamp(STRIP_MIN_HEIGHT, STRIP_MAX_HEIGHT);
        Dock::Bottom(height.min((free_height * 0.7).max(0.0)))
    }
}

/// How many points of width the panel takes from the right of a window while it is open, for the
/// on-screen controller to lay itself out in what is left (zero when it docks underneath).
pub(crate) fn right_dock_points(screen: Vec2, remembered_width: f32, open: f32) -> f32 {
    match dock_for(screen, screen.y, remembered_width) {
        Dock::Right(width) => width * open.clamp(0.0, 1.0),
        Dock::Bottom(_) => 0.0,
    }
}

/// Draw the debug panel, sliding in from the right edge or up from the bottom.
///
/// The panel animates its extent to zero while the content stays anchored to the edge it slides
/// from at full size, so the whole panel translates and clips off. A right-hand panel is
/// resizable when fully open; its width is remembered as the animation target.
///
/// `touch_strip` is where the controller's layout left room for the strip under its buttons in
/// portrait; with it the strip fills that room and the game keeps the whole width, without it a tall
/// window takes the strip's height from the game.
pub fn draw(
    ctx: &egui::Context,
    state: &mut AppState,
    vram_tex: egui::TextureId,
    touch_strip: Option<Rect>,
) {
    // 0.0 = closed, 1.0 = open.
    let t = ctx.animate_bool_with_time(
        egui::Id::new("debug_sidebar_slide"),
        state.panels.debug_sidebar,
        0.22,
    );
    state.debug_slide = t;
    if t <= 0.002 {
        return;
    }
    let animating = t < 0.998;
    let screen = ctx.screen_rect().size();
    let free = ctx.available_rect();
    let touch = state.menu.touch_mode();
    if let Some(strip) = touch_strip.filter(|_| screen.x < screen.y) {
        draw_touch_strip(ctx, state, vram_tex, strip, t, touch);
        return;
    }
    match dock_for(screen, free.height(), state.sidebar_width) {
        Dock::Right(width) => draw_right(ctx, state, vram_tex, width, t, animating, touch),
        Dock::Bottom(height) => draw_bottom(ctx, state, vram_tex, height, t, animating, touch),
    }
}

fn panel_colours(ctx: &egui::Context) -> (Color32, Stroke) {
    (
        ctx.style().visuals.panel_fill,
        ctx.style().visuals.widgets.noninteractive.bg_stroke,
    )
}

fn draw_right(
    ctx: &egui::Context,
    state: &mut AppState,
    vram_tex: egui::TextureId,
    target: f32,
    t: f32,
    animating: bool,
    touch: bool,
) {
    let max_w = SIDEBAR_MAX_WIDTH.min(ctx.screen_rect().width());
    let min_w = SIDEBAR_MIN_WIDTH.min(max_w).min(target);
    let (panel_fill, separator) = panel_colours(ctx);

    // Zero-margin fill frame: content padding is handled by hand (SIDEBAR_PAD)
    // so both the animated and open paths share the exact same layout.
    let panel = SidePanel::right("debug-sidebar").frame(egui::Frame::NONE.fill(panel_fill));
    let wide = ctx.screen_rect().width() >= WIDE_WINDOW;
    let panel = if animating {
        panel.resizable(false).exact_width((target * t).max(1.0))
    } else if wide {
        // Only a wide window lets the panel be dragged, and then it remembers the drag.
        panel
            .resizable(true)
            .default_width(target)
            .min_width(min_w)
            .max_width(max_w)
    } else {
        // A narrow window docks it at exactly the width it asked for, so the controller laid
        // out beside it never meets it; a wide row of content scrolls or clips instead.
        panel.resizable(false).exact_width(target)
    };

    let response = panel.show(ctx, |ui| {
        let panel_rect = ui.max_rect();
        // Hairline on the leading (left) edge -- the sidebar edge sliding in.
        ui.painter()
            .vline(panel_rect.left() + 0.5, panel_rect.y_range(), separator);
        // Anchor content at full width so a narrow (animating) panel clips it
        // rather than reflowing; at rest it just fills the panel exactly.
        let content_w = if animating {
            target
        } else {
            panel_rect.width()
        };
        let content_rect =
            Rect::from_min_size(panel_rect.min, Vec2::new(content_w, panel_rect.height()));
        ui.scope_builder(
            UiBuilder::new()
                .max_rect(content_rect.shrink(SIDEBAR_PAD))
                .layout(Layout::top_down(Align::Min)),
            |ui| {
                ui.set_clip_rect(content_rect.intersect(panel_rect));
                draw_contents(ui, state, vram_tex, panel_rect.bottom(), touch);
            },
        );
        // egui persists the panel's width from the frame content's measured
        // rect (`PanelState { rect }`), and the padded scope above measures
        // SIDEBAR_PAD narrower than the panel. Without claiming the full
        // width here, the stored width erodes by the padding every repaint
        // and a dragged sidebar creeps back to min_width.
        ui.expand_to_include_rect(content_rect);
        panel_rect
    });

    let r = response.inner;
    state.debug_rect = [r.left(), r.top(), r.right(), r.bottom()];
    if !animating && wide {
        state.sidebar_width = response.response.rect.width().clamp(min_w, max_w);
    }
}

/// The strip under a phone's controls: drawn in the room the controller's layout kept for it. It
/// wipes down from its top edge as it opens, with its summary on the first frame it is shown.
fn draw_touch_strip(
    ctx: &egui::Context,
    state: &mut AppState,
    vram_tex: egui::TextureId,
    strip: Rect,
    t: f32,
    touch: bool,
) {
    let (panel_fill, separator) = panel_colours(ctx);
    let inset = state.touch_bottom_inset;
    if (state.debug_sheet - state.debug_sheet_goal).abs() > 0.001 {
        ctx.request_repaint();
    }
    let response = TopBottomPanel::bottom("debug-strip")
        .resizable(false)
        .exact_height(strip.height())
        .frame(egui::Frame::NONE.fill(Color32::BLACK))
        .show(ctx, |ui| {
            let panel_rect = ui.max_rect();
            // The part that has opened so far, from the top edge down.
            let shown = Rect::from_min_size(
                panel_rect.min,
                Vec2::new(panel_rect.width(), panel_rect.height() * t),
            );
            ui.painter().rect_filled(shown, 0.0, panel_fill);
            ui.painter()
                .hline(panel_rect.x_range(), panel_rect.top() + 0.5, separator);
            // The contents are laid out at full size from the first frame and clipped to what has
            // opened, so the summary is never missing from a frame.
            sheet_handle(ui, state, panel_rect);
            let content = Rect::from_min_max(
                panel_rect.min + Vec2::new(0.0, SHEET_HANDLE),
                egui::pos2(panel_rect.right(), panel_rect.bottom() - inset),
            );
            ui.scope_builder(
                UiBuilder::new()
                    .max_rect(content.shrink(SIDEBAR_PAD))
                    .layout(Layout::top_down(Align::Min)),
                |ui| {
                    ui.set_clip_rect(shown);
                    draw_contents(ui, state, vram_tex, content.bottom() - SIDEBAR_PAD, touch);
                },
            );
            panel_rect
        });
    let r = response.inner;
    state.debug_rect = [r.left(), r.top(), r.right(), r.bottom()];
}

/// The grab bar on the strip's top edge: drag it up and the debug sheet covers the controller,
/// down and the controller is back with a short strip under it; a tap flips between the two.
fn sheet_handle(ui: &mut egui::Ui, state: &mut AppState, panel: Rect) {
    let area = Rect::from_min_size(panel.min, Vec2::new(panel.width(), SHEET_HANDLE));
    // A finger needs more than the bar: the touch area runs a little below it, across the middle
    // of the panel (the close button at the right stays clear).
    let grab = Rect::from_center_size(
        area.center() + Vec2::new(0.0, 5.0),
        Vec2::new(panel.width() * 0.6, SHEET_HANDLE + 10.0),
    );
    let response = ui.interact(
        grab,
        egui::Id::new("debug-sheet-handle"),
        Sense::click_and_drag(),
    );
    let lit = response.hovered() || response.dragged();
    let ink = if lit {
        tokens::FOCUS
    } else {
        tokens::ICON_REST
    };
    ui.painter().rect_filled(
        Rect::from_center_size(area.center(), Vec2::new(40.0, 4.0)),
        2.0,
        ink,
    );
    let span = (state.debug_sheet_span[0] - state.debug_sheet_span[1]).max(1.0);
    if response.dragged() {
        // The sheet follows the finger: the top edge moves by what the finger moved.
        state.debug_sheet_drag = true;
        let moved = -response.drag_delta().y / span;
        state.debug_sheet = (state.debug_sheet + moved).clamp(0.0, 1.0);
        state.debug_sheet_goal = state.debug_sheet;
    } else if response.drag_stopped() {
        // Let go: settle on the side the sheet was thrown towards, else the nearer one.
        state.debug_sheet_drag = false;
        let speed = ui.input(|i| i.pointer.velocity().y);
        state.debug_sheet_goal = if speed < -SHEET_FLING {
            1.0
        } else if speed > SHEET_FLING {
            0.0
        } else {
            state.debug_sheet.round()
        };
    } else if response.clicked() {
        state.debug_sheet_goal = if state.debug_sheet > 0.5 { 0.0 } else { 1.0 };
    }
}

fn draw_bottom(
    ctx: &egui::Context,
    state: &mut AppState,
    vram_tex: egui::TextureId,
    target: f32,
    t: f32,
    animating: bool,
    touch: bool,
) {
    let (panel_fill, separator) = panel_colours(ctx);
    let height = if animating {
        (target * t).max(1.0)
    } else {
        target
    };
    let response = TopBottomPanel::bottom("debug-strip")
        .resizable(false)
        .exact_height(height)
        .frame(egui::Frame::NONE.fill(panel_fill))
        .show(ctx, |ui| {
            let panel_rect = ui.max_rect();
            ui.painter()
                .hline(panel_rect.x_range(), panel_rect.top() + 0.5, separator);
            // Anchored to the bottom edge at full height: the strip rises into view.
            let content_rect = Rect::from_min_max(
                egui::pos2(panel_rect.left(), panel_rect.bottom() - target),
                panel_rect.max,
            );
            ui.scope_builder(
                UiBuilder::new()
                    .max_rect(content_rect.shrink(SIDEBAR_PAD))
                    .layout(Layout::top_down(Align::Min)),
                |ui| {
                    ui.set_clip_rect(panel_rect);
                    draw_contents(
                        ui,
                        state,
                        vram_tex,
                        content_rect.bottom() - SIDEBAR_PAD,
                        touch,
                    );
                },
            );
            panel_rect
        });
    let r = response.inner;
    state.debug_rect = [r.left(), r.top(), r.right(), r.bottom()];
}

/// The tabs of the debug views.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Performance,
    InputTape,
    Developer,
    Cpu,
    Memory,
    Video,
}

impl Tab {
    const ALL: [Self; 6] = [
        Self::Performance,
        Self::InputTape,
        Self::Developer,
        Self::Cpu,
        Self::Memory,
        Self::Video,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Performance => "Performance",
            Self::InputTape => "Input tape",
            Self::Developer => "Developer",
            Self::Cpu => "CPU",
            Self::Memory => "Memory",
            Self::Video => "Video",
        }
    }
}

fn draw_contents(
    ui: &mut egui::Ui,
    state: &mut AppState,
    vram_tex: egui::TextureId,
    bottom: f32,
    touch: bool,
) {
    if touch {
        // Fingertip-sized controls (36 points) in small type, so the content gets the room.
        let style = ui.style_mut();
        style.spacing.interact_size.y = 36.0;
        style.spacing.button_padding = Vec2::new(9.0, 5.0);
        style.spacing.item_spacing = Vec2::new(6.0, 6.0);
        for (text_style, size) in [
            (egui::TextStyle::Body, 14.0),
            (egui::TextStyle::Button, 14.0),
            (egui::TextStyle::Small, 12.0),
            (egui::TextStyle::Monospace, 13.0),
        ] {
            if let Some(font) = style.text_styles.get_mut(&text_style) {
                font.size = size;
            }
        }
    }
    state.debug_contents_frames = state.debug_contents_frames.wrapping_add(1);
    // A summary that stays put while the tabs change: what is loaded, whether it runs, and how
    // fast, then the close button.
    summary(ui, state, touch);
    ui.add_space(4.0);
    let id = egui::Id::new("debug_sidebar_tab");
    let mut tab = ui.ctx().data(|d| d.get_temp::<usize>(id)).unwrap_or(0);
    egui::ScrollArea::horizontal()
        .id_salt("debug-tabs")
        .auto_shrink([false, true])
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                for (i, candidate) in Tab::ALL.iter().enumerate() {
                    if tab_chip(ui, candidate.label(), tab == i, touch) {
                        tab = i;
                    }
                }
            });
        });
    ui.ctx().data_mut(|d| d.insert_temp(id, tab));
    ui.add_space(2.0);
    ui.separator();

    let height = (bottom - ui.cursor().top()).max(60.0);
    let ((), reading) = momentum::scroll_area(
        ui,
        "debug-content",
        |area| {
            area.max_height(height)
                .min_scrolled_height(height)
                .auto_shrink([false, false])
        },
        |ui| match Tab::ALL[tab.min(Tab::ALL.len() - 1)] {
            Tab::Performance => {
                let mut export = None;
                theme::viz_frame(ui, "", |ui| {
                    export = psoxide_debug_ui::draw(ui, &mut state.guest_stats, Some(vram_tex));
                });
                if let Some(psoxide_debug_ui::PanelAction::ExportCsv { csv, seconds }) = export {
                    state.export_guest_stats_csv(&csv, seconds);
                }
            }
            Tab::InputTape => input_tape(ui, state),
            Tab::Developer => developer(ui, state),
            Tab::Cpu => registers::draw_contents(
                ui,
                &state.cpu,
                &state.exec_history,
                &mut state.breakpoints,
                &mut state.gpr_snapshot,
            ),
            Tab::Memory => memory::draw_contents(
                ui,
                &mut state.memory_view,
                state.bus.as_ref(),
                &state.cpu,
                &mut state.breakpoints,
            ),
            Tab::Video => vram::draw_contents(ui, vram_tex),
        },
    );
    state.debug_scroll = [
        reading.pos,
        reading.max,
        f32::from(u8::from(reading.moving)),
        reading.top,
        reading.bottom,
    ];
}

/// One tab as a chip: a pill that lights in the shell's accent when it is the open one.
fn tab_chip(ui: &mut egui::Ui, label: &str, on: bool, touch: bool) -> bool {
    let font = FontId::proportional(13.0);
    let galley = ui.painter().layout_no_wrap(
        label.to_string(),
        font,
        if on { tokens::FOCUS } else { tokens::DIM },
    );
    let height = if touch { 30.0 } else { 26.0 };
    let size = Vec2::new(galley.size().x + 24.0, height);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let hot = response.hovered();
    let fill = if on {
        Color32::from_rgb(16, 52, 62)
    } else if hot {
        Color32::from_rgb(14, 26, 34)
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, height / 2.0, fill);
    if !on {
        ui.painter().rect_stroke(
            rect,
            height / 2.0,
            Stroke::new(1.0, tokens::hairline()),
            egui::StrokeKind::Inside,
        );
    }
    let colour = if on {
        tokens::FOCUS
    } else if hot {
        tokens::TEXT
    } else {
        tokens::DIM
    };
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley.clone(), colour);
    response.clicked()
}

/// The pinned card above the tabs: what is loaded, whether it runs, and how fast: the frame rate,
/// the time a frame takes and how far the emulation keeps up with real time. The readings come
/// first; a close button ends the row.
fn summary(ui: &mut egui::Ui, state: &mut AppState, touch: bool) {
    let (label, color) = if state.bus.is_none() {
        ("No game", tokens::DIM)
    } else if state.running {
        ("Running", tokens::OK)
    } else {
        ("Paused", Color32::from_rgb(242, 196, 106))
    };
    let fps = state.guest_stats.recent_fps();
    let speed = state.guest_stats.host_speed();
    if touch {
        compact_summary(ui, state, color, fps, speed);
        return;
    }
    let width = ui.available_width();
    ui.horizontal(|ui| {
        ui.label(RichText::new("Debug").color(tokens::ACCENT).size(15.0));
        ui.label(RichText::new(label).color(color).size(13.0));
        // The title takes what the row leaves and is cut, never wrapped.
        let close = if touch { 44.0 } else { 28.0 };
        if let Some(game) = state.current_game.as_ref() {
            let room = (width - ui.cursor().left() + ui.min_rect().left() - close - 12.0).max(0.0);
            ui.add_sized(
                [room, 18.0],
                egui::Label::new(RichText::new(&game.title).color(tokens::TEXT_2).size(13.0))
                    .truncate(),
            );
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let (rect, response) = ui.allocate_exact_size(Vec2::splat(close), Sense::click());
            let ink = if response.hovered() {
                tokens::FOCUS
            } else {
                tokens::ICON_REST
            };
            let c = rect.center();
            let stroke = Stroke::new(1.6, ink);
            ui.painter()
                .line_segment([c + Vec2::new(-5.0, -5.0), c + Vec2::new(5.0, 5.0)], stroke);
            ui.painter()
                .line_segment([c + Vec2::new(-5.0, 5.0), c + Vec2::new(5.0, -5.0)], stroke);
            if response
                .on_hover_text("Close the debug panel (F3)")
                .clicked()
            {
                state.panels.debug_sidebar = false;
            }
        });
    });
    ui.horizontal(|ui| {
        let big = if touch { 30.0 } else { 24.0 };
        let reading = |ui: &mut egui::Ui, value: String, caption: &str| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(
                    RichText::new(value)
                        .font(FontId::proportional(big))
                        .color(Color32::WHITE),
                );
                ui.label(RichText::new(caption).size(12.0).color(tokens::DIM));
            });
            ui.add_space(18.0);
        };
        reading(ui, fps.map_or("--".into(), |f| format!("{f:.0}")), "fps");
        reading(
            ui,
            fps.filter(|f| *f > 0.0)
                .map_or("--".into(), |f| format!("{:.1}", 1000.0 / f)),
            "ms per frame",
        );
        reading(
            ui,
            speed.map_or("--".into(), |(s, _)| format!("{:.0}%", s * 100.0)),
            "of real time",
        );
    });
}

/// The phone's summary: one line of small type, a status dot, the three readings, the title when
/// it fits and the close button, so the tabs and their content get the panel.
fn compact_summary(
    ui: &mut egui::Ui,
    state: &mut AppState,
    status: Color32,
    fps: Option<f64>,
    speed: Option<(f64, f64)>,
) {
    let readings = [
        (fps.map_or("--".into(), |f| format!("{f:.0}")), "fps"),
        (
            fps.filter(|f| *f > 0.0)
                .map_or("--".into(), |f| format!("{:.1}", 1000.0 / f)),
            "ms",
        ),
        (
            speed.map_or("--".into(), |(s, _)| format!("{:.0}", s * 100.0)),
            "% real time",
        ),
    ];
    let row = Rect::from_min_size(
        ui.cursor().min,
        Vec2::new(ui.available_width(), SUMMARY_ROW),
    );
    ui.allocate_rect(row, Sense::hover());
    let painter = ui.painter().clone();
    let mid = row.center().y;
    painter.circle_filled(egui::pos2(row.left() + 6.0, mid), 4.5, status);
    let mut x = row.left() + 18.0;
    for (value, unit) in readings {
        let big = painter.text(
            egui::pos2(x, mid),
            egui::Align2::LEFT_CENTER,
            value,
            FontId::proportional(15.0),
            Color32::WHITE,
        );
        let small = painter.text(
            egui::pos2(big.right() + 3.0, mid + 1.0),
            egui::Align2::LEFT_CENTER,
            unit,
            FontId::proportional(11.0),
            tokens::DIM,
        );
        x = small.right() + 12.0;
    }
    // The close button ends the row; the title takes what is left between, cut, never wrapped.
    let close = Rect::from_min_size(
        egui::pos2(row.right() - SUMMARY_ROW, row.top()),
        Vec2::splat(SUMMARY_ROW),
    );
    if let Some(game) = state.current_game.as_ref() {
        let room = close.left() - x - 8.0;
        if room >= 70.0 {
            let galley = painter.layout(
                game.title.clone(),
                FontId::proportional(12.0),
                tokens::TEXT_2,
                room,
            );
            if galley.rows.len() == 1 {
                let at = egui::pos2(
                    close.left() - 8.0 - galley.size().x,
                    mid - galley.size().y / 2.0,
                );
                painter.galley(at, galley, tokens::TEXT_2);
            }
        }
    }
    let response = ui.interact(close, egui::Id::new("debug-close"), Sense::click());
    let ink = if response.hovered() {
        tokens::FOCUS
    } else {
        tokens::ICON_REST
    };
    let c = close.center();
    let stroke = Stroke::new(1.6, ink);
    painter.line_segment([c + Vec2::new(-5.0, -5.0), c + Vec2::new(5.0, 5.0)], stroke);
    painter.line_segment([c + Vec2::new(-5.0, 5.0), c + Vec2::new(5.0, -5.0)], stroke);
    if response.clicked() {
        state.panels.debug_sidebar = false;
    }
}

/// Record a controller tape (`.pxtape`) of the running game and replay it.
/// The same actions are in the Game menu (F8 records, F4 replays the newest).
fn input_tape(ui: &mut egui::Ui, state: &mut AppState) {
    let (recording, frames) = state.input_recording_status();
    let loaded = state.bus.is_some();
    ui.horizontal_wrapped(|ui| {
        let label = if recording {
            "Stop recording (F8)"
        } else {
            "Record input tape (F8)"
        };
        if ui
            .add_enabled(loaded, egui::Button::new(label))
            .on_hover_text(
                "Restarts the game, then records the controller from boot. \
                 Stopping saves latest.pxtape plus a timestamped copy.",
            )
            .clicked()
        {
            state.toggle_input_recording();
        }
        if ui
            .add_enabled(loaded && !recording, egui::Button::new("Replay last (F4)"))
            .on_hover_text("Restart the game and replay the newest recording")
            .clicked()
        {
            state.replay_last_input();
        }
        if ui
            .add_enabled(loaded && !recording, egui::Button::new("Load replay..."))
            .clicked()
        {
            state.pick_input_replay();
        }
    });
    if recording {
        ui.colored_label(
            egui::Color32::from_rgb(245, 54, 68),
            format!("REC  {frames} frames"),
        );
    } else if !loaded {
        ui.label("Launch a game to record.");
    }
}

/// Stepping, render debugging, freecam, and the SDK examples build.
fn developer(ui: &mut egui::Ui, state: &mut AppState) {
    ui.horizontal_wrapped(|ui| {
        if ui
            .button("Step one instruction")
            .on_hover_text("Pause, then retire one CPU instruction")
            .clicked()
        {
            state.running = false;
            state.menu.sync_run_label(false);
            if let Some(bus) = state.bus.as_mut() {
                if let Ok(record) = state.cpu.step_traced(bus) {
                    crate::app::push_history(&mut state.exec_history, record);
                }
            }
        }
        if ui
            .button("Advance one frame")
            .on_hover_text("Pause, then run one emulated frame")
            .clicked()
        {
            state.running = false;
            state.menu.sync_run_label(false);
            crate::app::step_one_frame(state);
        }
    });
    // Wireframe mode lives on the GPU; there is nothing to flip before a
    // game is loaded.
    let mut wireframe = state.bus.as_ref().is_some_and(|b| b.gpu.wireframe_enabled);
    let response = ui.add_enabled(
        state.bus.is_some(),
        egui::Checkbox::new(&mut wireframe, "Wireframe (edges only)"),
    );
    if response.changed() {
        if let Some(bus) = state.bus.as_mut() {
            bus.gpu.wireframe_enabled = wireframe;
        }
    }
    // Switching freecam KEEPS the camera pose, matching the L3+R3 chord, so
    // glancing back at the game and returning keeps the framing.
    let mut freecam = state.freelook.enabled;
    if ui
        .checkbox(&mut freecam, "Freecam (tap L3+R3)")
        .on_hover_text(
            "Left stick moves, right stick looks, R2 boosts. Game input is paused \
             while it is on. Hold L3+R3 to reset the camera.",
        )
        .changed()
    {
        state.freelook.enabled = freecam;
        state.status_message_set(if freecam {
            "Freecam ON - pad drives the camera, game input paused (hold L3+R3 to reset)"
        } else {
            "Freecam controls off - framing preserved; pad returns to the game"
        });
    }
    #[cfg(not(target_arch = "wasm32"))]
    if ui
        .button("Build SDK examples")
        .on_hover_text("Run `make examples` in the background, then refresh the library")
        .clicked()
    {
        state.start_examples_build();
    }
}
