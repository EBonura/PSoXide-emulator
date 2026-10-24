//! Unified emulator diagnostics sidebar.
//!
//! The individual debug tools still own their content rendering; this module
//! only docks them into one right-hand sidebar with collapsible sections.

use egui::{Align, Layout, Rect, RichText, SidePanel, UiBuilder};

use crate::app::AppState;
use crate::theme;

use super::{memory, registers, vram};

const SIDEBAR_MIN_WIDTH: f32 = 320.0;
const SIDEBAR_MAX_WIDTH: f32 = 900.0;
/// Inner padding, replacing the default panel frame margin we drop so the
/// slide-in content-width math stays exact.
const SIDEBAR_PAD: f32 = 8.0;

/// Draw the right-hand debug sidebar, sliding in/out from the right edge.
///
/// Mirrors the toolbar slide: the panel width animates to zero while the
/// content stays anchored to the panel's left edge at full width, so the
/// whole sidebar translates right and clips off under the window edge. The
/// panel is resizable only when fully open; its width is remembered as the
/// animation target so a resized sidebar animates to its own width.
pub fn draw(ctx: &egui::Context, state: &mut AppState, vram_tex: egui::TextureId) {
    // 0.0 = closed, 1.0 = open.
    let t = ctx.animate_bool_with_time(
        egui::Id::new("debug_sidebar_slide"),
        state.panels.debug_sidebar,
        0.22,
    );
    if t <= 0.002 {
        return;
    }
    let animating = t < 0.998;
    // On a phone the sidebar is the whole width; it never runs off the edge.
    let screen_w = ctx.screen_rect().width();
    let narrow = screen_w < 700.0;
    let max_w = SIDEBAR_MAX_WIDTH.min(screen_w);
    let min_w = SIDEBAR_MIN_WIDTH.min(max_w);
    let target = if narrow {
        screen_w
    } else {
        state.sidebar_width.clamp(min_w, max_w)
    };
    let panel_fill = ctx.style().visuals.panel_fill;
    let separator = ctx.style().visuals.widgets.noninteractive.bg_stroke;

    // Zero-margin fill frame: content padding is handled by hand (SIDEBAR_PAD)
    // so both the animated and open paths share the exact same layout.
    let panel = SidePanel::right("debug-sidebar").frame(egui::Frame::NONE.fill(panel_fill));
    let panel = if animating {
        panel.resizable(false).exact_width((target * t).max(1.0))
    } else {
        panel
            .resizable(true)
            .default_width(target)
            .min_width(min_w)
            .max_width(max_w)
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
            Rect::from_min_size(panel_rect.min, egui::vec2(content_w, panel_rect.height()));
        ui.scope_builder(
            UiBuilder::new()
                .max_rect(content_rect.shrink(SIDEBAR_PAD))
                .layout(Layout::top_down(Align::Min)),
            |ui| {
                ui.set_clip_rect(content_rect.intersect(panel_rect));
                draw_contents(ui, state, vram_tex, panel_rect.bottom(), false);
            },
        );
        // egui persists the panel's width from the frame content's measured
        // rect (`PanelState { rect }`), and the padded scope above measures
        // SIDEBAR_PAD narrower than the panel. Without claiming the full
        // width here, the stored width erodes by the padding every repaint
        // and a dragged sidebar creeps back to min_width.
        ui.expand_to_include_rect(content_rect);
    });

    if !animating {
        state.sidebar_width = response.response.rect.width().clamp(min_w, max_w);
    }
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

/// The debug section of the shell: the whole page, one column, built for a
/// thumb on a phone (big pinned summary, big tabs, full-width charts) and the
/// same information and functions as the desktop sidebar.
pub fn draw_section(
    ctx: &egui::Context,
    state: &mut AppState,
    vram_tex: egui::TextureId,
    rect: Rect,
) {
    egui::Area::new(egui::Id::new("debug-section"))
        .order(egui::Order::Foreground)
        .fixed_pos(rect.min)
        .show(ctx, |ui| {
            ui.set_clip_rect(rect);
            ui.set_width(rect.width());
            ui.scope(|ui| {
                // Tap targets of at least 44 points and nothing under 13.
                let style = ui.style_mut();
                style.spacing.interact_size.y = 44.0;
                style.spacing.button_padding = egui::vec2(14.0, 10.0);
                style.spacing.item_spacing = egui::vec2(8.0, 10.0);
                style.spacing.slider_width = (rect.width() - 120.0).max(120.0);
                for (text_style, size) in [
                    (egui::TextStyle::Body, 15.0),
                    (egui::TextStyle::Button, 15.0),
                    (egui::TextStyle::Small, 13.0),
                    (egui::TextStyle::Monospace, 14.0),
                ] {
                    if let Some(font) = style.text_styles.get_mut(&text_style) {
                        font.size = size;
                    }
                }
                draw_contents(ui, state, vram_tex, rect.bottom(), true);
            });
        });
}

fn draw_contents(
    ui: &mut egui::Ui,
    state: &mut AppState,
    vram_tex: egui::TextureId,
    bottom: f32,
    big: bool,
) {
    // A summary that stays put while the tabs change: what is loaded, whether
    // it runs, and how fast.
    summary(ui, state, big);
    ui.add_space(4.0);
    let id = egui::Id::new("debug_sidebar_tab");
    let mut tab = ui.ctx().data(|d| d.get_temp::<usize>(id)).unwrap_or(0);
    let tab_button = |ui: &mut egui::Ui, i: usize, width: Option<f32>, tab: &mut usize| {
        let selected = *tab == i;
        let text = RichText::new(Tab::ALL[i].label()).color(if selected {
            egui::Color32::WHITE
        } else {
            theme::TEXT_DIM
        });
        let button = egui::Button::new(text).selected(selected);
        let response = match width {
            Some(w) => ui.add_sized([w, if big { 44.0 } else { 24.0 }], button),
            None => ui.add(button),
        };
        if response.clicked() {
            *tab = i;
        }
    };
    if big {
        // Two rows of three big buttons.
        let w = (ui.available_width() - 2.0 * ui.spacing().item_spacing.x) / 3.0;
        for row in 0..2 {
            ui.horizontal(|ui| {
                for col in 0..3 {
                    tab_button(ui, row * 3 + col, Some(w), &mut tab);
                }
            });
        }
    } else {
        ui.horizontal_wrapped(|ui| {
            for i in 0..Tab::ALL.len() {
                tab_button(ui, i, None, &mut tab);
            }
        });
    }
    ui.ctx().data_mut(|d| d.insert_temp(id, tab));
    ui.separator();

    let height = (bottom - ui.cursor().top()).max(120.0);
    egui::ScrollArea::vertical()
        .max_height(height)
        .min_scrolled_height(height)
        .auto_shrink([false, false])
        .show(ui, |ui| match Tab::ALL[tab.min(Tab::ALL.len() - 1)] {
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
        });
}

/// The pinned card above the tabs: state, frame rate, frame time, how fast
/// the emulation runs against real time and how many frames were late.
fn summary(ui: &mut egui::Ui, state: &AppState, big: bool) {
    let (label, color) = if state.bus.is_none() {
        ("No game", theme::TEXT_DIM)
    } else if state.running {
        ("Running", egui::Color32::from_rgb(82, 208, 138))
    } else {
        ("Paused", egui::Color32::from_rgb(242, 196, 106))
    };
    let fps = state.guest_stats.recent_fps();
    let speed = state.guest_stats.host_speed();
    if big {
        egui::Frame::NONE
            .fill(egui::Color32::from_rgb(14, 24, 32))
            .corner_radius(10)
            .inner_margin(12)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(label).color(color).size(15.0));
                    if let Some(game) = state.current_game.as_ref() {
                        ui.label(RichText::new(&game.title).color(theme::TEXT).size(15.0));
                    }
                });
                ui.horizontal(|ui| {
                    let big_num = |ui: &mut egui::Ui, value: String, caption: &str| {
                        ui.vertical(|ui| {
                            ui.label(
                                RichText::new(value)
                                    .font(egui::FontId::monospace(36.0))
                                    .color(egui::Color32::WHITE),
                            );
                            ui.label(RichText::new(caption).size(13.0).color(theme::TEXT_DIM));
                        });
                        ui.add_space(18.0);
                    };
                    big_num(ui, fps.map_or("--".into(), |f| format!("{f:.0}")), "fps");
                    big_num(
                        ui,
                        fps.filter(|f| *f > 0.0)
                            .map_or("--".into(), |f| format!("{:.1}", 1000.0 / f)),
                        "ms per frame",
                    );
                    big_num(
                        ui,
                        speed.map_or("--".into(), |(s, _)| format!("{:.0}%", s * 100.0)),
                        "of real time",
                    );
                });
            });
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new("Debug")
                .color(theme::ACCENT)
                .size(theme::FONT_SIZE_HEADING),
        );
        ui.label(RichText::new(label).color(color));
        if let Some(game) = state.current_game.as_ref() {
            ui.label(RichText::new(&game.title).color(theme::TEXT));
        }
        if let Some(fps) = fps {
            ui.label(RichText::new(format!("{fps:.0} fps")).monospace());
        }
        if let Some((speed, _)) = speed {
            ui.label(RichText::new(format!("{:.0}% speed", speed * 100.0)).monospace());
        }
    });
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
