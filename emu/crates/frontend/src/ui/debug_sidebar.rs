//! The live debug panel: performance, input tape, developer tools, CPU, memory and video, docked
//! beside the running game on a wide window and under it on a tall one, so the game, its
//! controls and its debug readouts share one screen and the game never pauses for them.
//!
//! The individual debug tools still own their content rendering; this module docks them into one
//! panel with a summary on top, a row of tabs and the tab's content scrolling below.

use egui::{
    Align, Color32, FontId, Layout, Rect, Sense, SidePanel, Stroke, TopBottomPanel, UiBuilder, Vec2,
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
/// Share of a phone's width the panel takes on the right, held either way up, and its limits in
/// points: a third of the screen, never narrower than a column of small readable type needs.
const PHONE_SHARE: f32 = 0.33;
const PHONE_MIN_WIDTH: f32 = 112.0;
const PHONE_MAX_WIDTH: f32 = 420.0;
/// A phone's panel at least this wide has room for the summary on one line.
const ROOMY: f32 = 190.0;
/// A panel at least this wide shows the full tab names (a wide desktop sidebar); below it the short ones.
const WIDE_TABS: f32 = 420.0;
/// The panel's type scale on every screen: the smallest text it draws is 9 points.
const PANEL_TYPE_SCALE: f32 = 0.86;

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
/// A phone (`touch`) always docks it as a third of the width on the right, held either way up, so
/// the picture and the controller keep the other two thirds and the whole height. A desktop window
/// wider than tall puts it on the right as well, so the game, the pad around it and the readouts all
/// fit one screen; a taller one puts it under the game.
pub(crate) fn dock_for(screen: Vec2, free_height: f32, remembered_width: f32, touch: bool) -> Dock {
    if touch {
        Dock::Right((screen.x * PHONE_SHARE).clamp(PHONE_MIN_WIDTH, PHONE_MAX_WIDTH))
    } else if screen.x >= screen.y {
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
pub(crate) fn right_dock_points(
    screen: Vec2,
    remembered_width: f32,
    open: f32,
    touch: bool,
) -> f32 {
    match dock_for(screen, screen.y, remembered_width, touch) {
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
pub fn draw(ctx: &egui::Context, state: &mut AppState, vram_tex: egui::TextureId) {
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
    psoxide_debug_ui::set_type_scale(PANEL_TYPE_SCALE);
    match dock_for(screen, free.height(), state.sidebar_width, touch) {
        Dock::Right(width) => draw_right(ctx, state, vram_tex, width, t, animating, touch),
        Dock::Bottom(height) => draw_bottom(ctx, state, vram_tex, height, t, animating),
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
    let inset = state.touch_bottom_inset;

    // Zero-margin fill frame: content padding is handled by hand (SIDEBAR_PAD)
    // so both the animated and open paths share the exact same layout.
    let panel = SidePanel::right("debug-sidebar").frame(egui::Frame::NONE.fill(panel_fill));
    let wide = ctx.screen_rect().width() >= WIDE_WINDOW && !touch;
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
                // A phone keeps the content clear of the home indicator.
                let bottom = panel_rect.bottom() - if touch { inset } else { 0.0 };
                draw_contents(ui, state, vram_tex, bottom);
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

fn draw_bottom(
    ctx: &egui::Context,
    state: &mut AppState,
    vram_tex: egui::TextureId,
    target: f32,
    t: f32,
    animating: bool,
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
                    draw_contents(ui, state, vram_tex, content_rect.bottom() - SIDEBAR_PAD);
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

    /// The label where the panel is a narrow column.
    fn short_label(self) -> &'static str {
        match self {
            Self::Performance => "Perf",
            Self::InputTape => "Tape",
            Self::Developer => "Dev",
            Self::Cpu => "CPU",
            Self::Memory => "Mem",
            Self::Video => "Video",
        }
    }
}

fn draw_contents(ui: &mut egui::Ui, state: &mut AppState, vram_tex: egui::TextureId, bottom: f32) {
    {
        // The panel's type scale, on every screen: small type and 22-point controls, so the
        // content gets the room. The smallest text anywhere in the panel is 9 points (9.5 here,
        // 9.03 in the charts).
        let style = ui.style_mut();
        style.spacing.interact_size.y = 22.0;
        style.spacing.button_padding = Vec2::new(5.0, 2.0);
        style.spacing.item_spacing = Vec2::new(3.0, 3.0);
        for (text_style, size) in [
            (egui::TextStyle::Body, 11.0),
            (egui::TextStyle::Button, 11.0),
            (egui::TextStyle::Small, 9.5),
            (egui::TextStyle::Monospace, 10.5),
        ] {
            if let Some(font) = style.text_styles.get_mut(&text_style) {
                font.size = size;
            }
        }
    }
    state.debug_contents_frames = state.debug_contents_frames.wrapping_add(1);
    // A summary that stays put while the tabs change: what is loaded, whether it runs, and how
    // fast, then the close button.
    summary(ui, state);
    ui.add_space(4.0);
    let id = egui::Id::new("debug_sidebar_tab");
    let mut tab = ui.ctx().data(|d| d.get_temp::<usize>(id)).unwrap_or(0);
    let tabs_top = ui.cursor().top();
    if ui.available_width() < WIDE_TABS {
        // A phone's column: short labels, in one row where they fit (they do from 200 points)
        // and wrapped where they do not, so every tab is in view at once, never scrolled to.
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(3.0, 3.0);
            for (i, candidate) in Tab::ALL.iter().enumerate() {
                if tab_chip(ui, candidate.short_label(), tab == i) {
                    tab = i;
                }
            }
        });
    } else {
        egui::ScrollArea::horizontal()
            .id_salt("debug-tabs")
            .auto_shrink([false, true])
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (i, candidate) in Tab::ALL.iter().enumerate() {
                        if tab_chip(ui, candidate.label(), tab == i) {
                            tab = i;
                        }
                    }
                });
            });
    }
    // How many rows the tabs took (the browser journeys check they fit one on a phone held sideways).
    state.debug_tab_rows = ((ui.cursor().top() - tabs_top + 3.0) / 25.0)
        .round()
        .max(1.0) as u8;
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
        reading.left,
        reading.right,
        reading.content_right,
    ];
}

/// One tab as a chip: a pill that lights in the shell's accent when it is the open one.
fn tab_chip(ui: &mut egui::Ui, label: &str, on: bool) -> bool {
    let font = FontId::proportional(10.0);
    let galley = ui.painter().layout_no_wrap(
        label.to_string(),
        font,
        if on { tokens::FOCUS } else { tokens::DIM },
    );
    let height = 20.0;
    let size = Vec2::new(galley.size().x + 10.0, height);
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
fn summary(ui: &mut egui::Ui, state: &mut AppState) {
    let color = if state.bus.is_none() {
        tokens::DIM
    } else if state.running {
        tokens::OK
    } else {
        Color32::from_rgb(242, 196, 106)
    };
    let fps = state.guest_stats.recent_fps();
    let speed = state.guest_stats.host_speed();
    compact_summary(ui, state, color, fps, speed);
}

/// The phone's summary, in small type: a status dot, the three readings, the disc name when it
/// fits, and the close button. A panel with the room (a tablet, a long phone held sideways) puts it
/// on one line; the usual third of a phone's width takes two: the dot, the frame rate and close,
/// then the frame time and the speed.
fn compact_summary(
    ui: &mut egui::Ui,
    state: &mut AppState,
    status: Color32,
    fps: Option<f64>,
    speed: Option<(f64, f64)>,
) {
    let fps_text = fps.map_or("--".to_string(), |f| format!("{f:.0}"));
    let ms_text = fps
        .filter(|f| *f > 0.0)
        .map_or("--".to_string(), |f| format!("{:.1}", 1000.0 / f));
    let speed_text = speed.map_or("--".to_string(), |(s, _)| format!("{:.0}%", s * 100.0));
    let roomy = ui.available_width() >= ROOMY;
    let row_h = if roomy { 24.0 } else { 36.0 };
    let row = Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), row_h));
    ui.allocate_rect(row, Sense::hover());
    let painter = ui.painter().clone();
    let value_font = FontId::proportional(12.5);
    let unit_font = FontId::proportional(9.5);
    // One reading at `x`: the value, then its unit; returns where the next one may start.
    let reading = |x: f32, y: f32, value: &str, unit: &str| -> f32 {
        let big = painter.text(
            egui::pos2(x, y),
            egui::Align2::LEFT_CENTER,
            value,
            value_font.clone(),
            Color32::WHITE,
        );
        let small = painter.text(
            egui::pos2(big.right() + 2.0, y + 1.0),
            egui::Align2::LEFT_CENTER,
            unit,
            unit_font.clone(),
            tokens::DIM,
        );
        small.right() + 8.0
    };
    let (first, second) = if roomy {
        (row.center().y, row.center().y)
    } else {
        (row.top() + 11.0, row.top() + 29.0)
    };
    painter.circle_filled(egui::pos2(row.left() + 5.0, first), 4.0, status);
    let mut x = reading(row.left() + 15.0, first, &fps_text, "fps");
    if roomy {
        x = reading(x, second, &ms_text, "ms");
        // "real time" where the line has the room, "real" where it has not.
        let word = if ui.available_width() >= 250.0 {
            "real time"
        } else {
            "real"
        };
        x = reading(x, second, &speed_text, word);
    } else {
        let x2 = reading(row.left() + 1.0, second, &ms_text, "ms");
        reading(x2, second, &speed_text, "real");
    }
    // The close button ends the first line; the disc name takes what is left of a roomy one.
    let hit = 28.0;
    let close = Rect::from_min_size(
        egui::pos2(
            row.right() - hit,
            row.top()
                + if roomy {
                    0.0
                } else {
                    first - row.top() - hit / 2.0
                },
        ),
        Vec2::splat(hit),
    );
    if let (true, Some(game)) = (roomy, state.current_game.as_ref()) {
        let room = close.left() - x - 8.0;
        if room >= 70.0 {
            let galley = painter.layout(
                game.title.clone(),
                FontId::proportional(11.0),
                tokens::TEXT_2,
                room,
            );
            if galley.rows.len() == 1 {
                let at = egui::pos2(
                    close.left() - 8.0 - galley.size().x,
                    second - galley.size().y / 2.0,
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
    let stroke = Stroke::new(1.5, ink);
    painter.line_segment([c + Vec2::new(-4.5, -4.5), c + Vec2::new(4.5, 4.5)], stroke);
    painter.line_segment([c + Vec2::new(-4.5, 4.5), c + Vec2::new(4.5, -4.5)], stroke);
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
