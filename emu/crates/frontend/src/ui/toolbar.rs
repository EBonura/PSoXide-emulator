//! Top-of-window toolbar.
//!
//! Left: a status dot (green running, grey paused) and the game's frame
//! rate. Right, in visual order: Play/Pause, Reset, Save states, Controls,
//! volume/mute, the debug sidebar toggle, and Hide toolbar. Everything else
//! lives in the menu (Settings) or the debug sidebar (Developer, and the
//! detailed performance figures).
//!
//! The left and right halves are laid out into separate clipped
//! rectangles; on a narrow window the frame rate yields before any button.

use egui::{Align, Button, Color32, Context, Label, Layout, Rect, RichText, TopBottomPanel};

use crate::app::AppState;
use crate::icons;
use crate::ui::menu::MenuAction;

/// Icon font size in the button row.
const ICON_SIZE: f32 = 16.0;
/// Minimum clickable size for each icon button. egui will grow the
/// button to fit its content but won't shrink below this.
const BUTTON_SIZE: egui::Vec2 = egui::vec2(30.0, 26.0);
/// Exact height of the toolbar panel.
const BAR_HEIGHT: f32 = 34.0;
/// Horizontal padding inside the toolbar strip.
const TOOLBAR_MARGIN_X: f32 = 8.0;
/// Gap between the metrics lane and the controls lane at wide window sizes.
const TOOLBAR_LANE_GAP: f32 = 8.0;
/// Extra separation between control groups when the complete toolbar fits.
/// Compact mode relies on normal item spacing instead so every action remains
/// reachable before the frame rate is allowed to consume room.
const TOOLBAR_GROUP_GAP: f32 = 8.0;
/// Comfortable width for every toolbar control with the normal group spacing.
const CONTROLS_PREFERRED_WIDTH: f32 = 430.0;
/// Room for the status dot and the frame rate.
const METRICS_MIN_WIDTH: f32 = 90.0;
/// Slider width used in the toolbar.
const AUDIO_SLIDER_WIDTH: f32 = 72.0;

/// Text size for the left-hand frame rate.
const METRIC_TEXT_SIZE: f32 = 12.0;

/// Running / paused status colours.
const STATUS_RUNNING: Color32 = Color32::from_rgb(80, 200, 120);
const STATUS_PAUSED: Color32 = Color32::from_rgb(153, 153, 166);
const METRIC_TEXT: Color32 = Color32::from_rgb(204, 204, 217);
const METRIC_LABEL: Color32 = Color32::from_rgb(102, 102, 115);

/// Paint the top toolbar. Called once per frame before the central
/// panel so the framebuffer clips underneath it.
///
/// The bar slides up when hidden: the panel height animates to 0 while
/// the row stays anchored to the panel's bottom edge, so the whole
/// cluster translates up and clips off under the window top. A small
/// floating tab in the top-right corner brings it back.
pub fn draw(ctx: &Context, state: &mut AppState) {
    // 0.0 = fully shown, 1.0 = fully hidden.
    let t = ctx.animate_bool_with_time(egui::Id::new("toolbar_slide"), state.toolbar_hidden, 0.22);
    let height = (BAR_HEIGHT * (1.0 - t)).max(0.0);

    if height >= 0.5 {
        // Zero-margin frame so `max_rect()` is exactly [0, height]; we paint
        // the fill/separator ourselves and place the row by hand. With the
        // default frame's inner margin the bottom-anchored row math would be
        // off by the margin and the buttons would sit too high.
        let panel_fill = ctx.style().visuals.panel_fill;
        let separator = ctx.style().visuals.widgets.noninteractive.bg_stroke;
        TopBottomPanel::top("toolbar")
            .resizable(false)
            .exact_height(height)
            .frame(egui::Frame::NONE.fill(panel_fill))
            .show(ctx, |ui| {
                let panel_rect = ui.max_rect();
                ui.painter()
                    .hline(panel_rect.x_range(), panel_rect.bottom() - 0.5, separator);
                // Anchor the row to the panel bottom at full height; the
                // shrinking panel clips the overflow above -> slide-up.
                let row_top = panel_rect.bottom() - BAR_HEIGHT;
                let row_left = panel_rect.left() + TOOLBAR_MARGIN_X;
                let row_right = (panel_rect.right() - TOOLBAR_MARGIN_X).max(row_left);
                let row_rect = Rect::from_min_max(
                    egui::pos2(row_left, row_top),
                    egui::pos2(row_right, panel_rect.bottom()),
                );

                let lanes = toolbar_lanes(row_rect.width());
                let controls_left = row_rect.right() - lanes.controls_width;
                let metrics_right = row_rect.left() + lanes.metrics_width;

                let metrics_rect = Rect::from_min_max(
                    egui::pos2(row_rect.left(), row_rect.top()),
                    egui::pos2(metrics_right, row_rect.bottom()),
                );
                let controls_rect = Rect::from_min_max(
                    egui::pos2(controls_left, row_rect.top()),
                    egui::pos2(row_rect.right(), row_rect.bottom()),
                );

                if lanes.metrics_width > 0.5 {
                    ui.scope_builder(
                        egui::UiBuilder::new()
                            .max_rect(metrics_rect)
                            .layout(Layout::left_to_right(Align::Center)),
                        |ui| {
                            ui.set_clip_rect(metrics_rect.intersect(panel_rect));
                            ui.set_width(metrics_rect.width());
                            ui.set_height(metrics_rect.height());
                            draw_metrics(ui, state);
                        },
                    );
                }

                ui.scope_builder(
                    egui::UiBuilder::new()
                        .max_rect(controls_rect)
                        .layout(Layout::right_to_left(Align::Center)),
                    |ui| {
                        ui.set_clip_rect(controls_rect.intersect(panel_rect));
                        ui.set_width(controls_rect.width());
                        ui.set_height(controls_rect.height());
                        draw_toolbar_controls(ui, state, lanes.compact_controls);
                    },
                );
            });
    }

    // Fade the restore tab in only once the bar has mostly cleared, so it
    // never overlaps the sliding toolbar.
    if t > 0.5 {
        draw_restore_tab(ctx, state, ((t - 0.5) / 0.5).clamp(0.0, 1.0));
    }
}

/// Floating tab shown when the toolbar is hidden. Anchored to the
/// top-right so it sits over the framebuffer corner; clicking pulls the
/// bar back down. `opacity` fades it in with the slide.
fn draw_restore_tab(ctx: &Context, state: &mut AppState, opacity: f32) {
    egui::Area::new(egui::Id::new("toolbar_restore"))
        .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-6.0, 6.0))
        .show(ctx, |ui| {
            ui.set_opacity(opacity);
            egui::Frame::new()
                .fill(ui.visuals().panel_fill)
                .corner_radius(6.0)
                .inner_margin(egui::Margin::symmetric(2, 0))
                .show(ui, |ui| {
                    let btn = icon_button(icons::CARET_DOWN);
                    if ui.add(btn).on_hover_text("Show toolbar").clicked() {
                        state.toolbar_hidden = false;
                    }
                });
        });
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ToolbarLanes {
    metrics_width: f32,
    controls_width: f32,
    compact_controls: bool,
}

/// Give actions priority over the frame rate. At wide sizes the controls keep
/// a stable right-aligned lane and the frame rate takes the remainder. Once
/// that would squeeze it below its useful minimum, the frame rate hides and
/// the controls receive the whole row in a denser arrangement.
fn toolbar_lanes(row_width: f32) -> ToolbarLanes {
    let row_width = row_width.max(0.0);
    let wide_minimum = CONTROLS_PREFERRED_WIDTH + TOOLBAR_LANE_GAP + METRICS_MIN_WIDTH;
    if row_width >= wide_minimum {
        ToolbarLanes {
            metrics_width: row_width - CONTROLS_PREFERRED_WIDTH - TOOLBAR_LANE_GAP,
            controls_width: CONTROLS_PREFERRED_WIDTH,
            compact_controls: false,
        }
    } else {
        ToolbarLanes {
            metrics_width: 0.0,
            controls_width: row_width,
            compact_controls: true,
        }
    }
}

fn draw_toolbar_controls(ui: &mut egui::Ui, state: &mut AppState, compact: bool) {
    if compact {
        // The icon buttons retain their hit targets; only whitespace yields.
        ui.spacing_mut().item_spacing.x = 4.0;
    }
    let group_gap = if compact { 0.0 } else { TOOLBAR_GROUP_GAP };
    let mut action = None;

    // Right-to-left layout: first added sits furthest right.
    if ui
        .add(icon_button(icons::CARET_UP))
        .on_hover_text("Hide toolbar")
        .clicked()
    {
        state.toolbar_hidden = true;
    }
    ui.add_space(group_gap);
    let sidebar = toggle_button(icons::BUG, state.panels.debug_sidebar);
    if ui
        .add(sidebar)
        .on_hover_text("Debug sidebar: performance, developer tools, registers, memory, VRAM (F3)")
        .clicked()
    {
        state.panels.debug_sidebar = !state.panels.debug_sidebar;
    }
    ui.add_space(group_gap);
    draw_audio_controls(ui, state);
    ui.add_space(group_gap);
    if ui
        .add(icon_button(icons::GAMEPAD_2))
        .on_hover_text("Controls: controller ports and keyboard keys")
        .clicked()
    {
        action = Some(MenuAction::OpenControls);
    }
    if ui
        .add(icon_button(icons::SAVE))
        .on_hover_text("Save states (F5 save, F7 load)")
        .clicked()
    {
        action = Some(MenuAction::OpenSaveStates);
    }
    ui.add_space(group_gap);
    if ui
        .add(icon_button(icons::ROTATE_CCW))
        .on_hover_text("Reset")
        .clicked()
    {
        action = Some(MenuAction::Reset);
    }
    let (icon, tooltip) = if state.running {
        (icons::PAUSE, "Pause")
    } else {
        (icons::PLAY, "Play")
    };
    if ui.add(icon_button(icon)).on_hover_text(tooltip).clicked() {
        action = Some(MenuAction::ToggleRun);
    }
    if let Some(action) = action {
        let _ = super::apply_menu_action(state, action);
    }
}

/// Volume control: mute button plus gain slider.
fn draw_audio_controls(ui: &mut egui::Ui, state: &mut AppState) {
    let effective = state.effective_audio_volume();
    let icon = if effective <= 0.0 {
        icons::VOLUME_X
    } else if effective < 0.6 {
        icons::VOLUME_1
    } else {
        icons::VOLUME_2
    };
    let btn = toggle_button(icon, !state.audio_muted && state.audio_volume > 0.0);
    if ui.add(btn).on_hover_text("Mute / unmute audio").clicked() {
        state.audio_muted = !state.audio_muted;
        state.status_message_set(if state.audio_muted {
            "Audio muted"
        } else {
            "Audio unmuted"
        });
    }

    let before = state.audio_volume;
    let slider = egui::Slider::new(&mut state.audio_volume, 0.0..=1.5)
        .show_value(false)
        .clamping(egui::SliderClamping::Always);
    let response = ui
        .add_sized([AUDIO_SLIDER_WIDTH, 18.0], slider)
        .on_hover_text("Volume");
    if response.changed() {
        state.audio_volume = state.audio_volume.clamp(0.0, 1.5);
        if state.audio_volume > 0.0 && before <= 0.0 {
            state.audio_muted = false;
        }
    }
}

/// Build a Button at the shared icon size, tinted to indicate active
/// vs. inactive state. Keeps the toggle cluster visually coherent.
fn toggle_button(icon: char, active: bool) -> Button<'static> {
    let color = if active { STATUS_RUNNING } else { METRIC_LABEL };
    // Active toggles use the solid (fill) weight, inactive the outline.
    let icon_font = if active {
        icons::font_fill(ICON_SIZE)
    } else {
        icons::font(ICON_SIZE)
    };
    let label = RichText::new(icon.to_string()).font(icon_font).color(color);
    // `Extend` stops egui truncating/clipping the glyph to its advance width;
    // some Lucide icons (e.g. DISC) have ink that overruns the advance and
    // would otherwise render with the right edge sliced off in the tight button.
    Button::new(label)
        .min_size(BUTTON_SIZE)
        .wrap_mode(egui::TextWrapMode::Extend)
}

/// Plain icon button (no toggle tint) at the shared size. Carries the same
/// `Extend` guard as `toggle_button` so wide Lucide glyphs aren't sliced to
/// their advance width in the tight button.
fn icon_button(icon: char) -> Button<'static> {
    Button::new(icons::text(icon, ICON_SIZE))
        .min_size(BUTTON_SIZE)
        .wrap_mode(egui::TextWrapMode::Extend)
}

/// Left-hand cluster: status dot and the game's frame rate. Everything
/// else (emulation speed, host frame time, audio gaps) is in the debug
/// sidebar's performance panel.
fn draw_metrics(ui: &mut egui::Ui, state: &AppState) {
    ui.add_space(2.0);
    let (dot_color, status) = if state.running {
        (STATUS_RUNNING, "Running")
    } else {
        (STATUS_PAUSED, "Paused")
    };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, dot_color);
    response.on_hover_text(status);
    ui.add_space(10.0);
    let fps = state
        .guest_stats
        .recent_fps()
        .map_or_else(|| "--".to_string(), |fps| format!("{fps:4.1}"));
    ui.add(
        Label::new(
            RichText::new("FPS")
                .color(METRIC_LABEL)
                .size(METRIC_TEXT_SIZE),
        )
        .truncate(),
    );
    ui.add_space(4.0);
    ui.add(
        Label::new(
            RichText::new(fps)
                .color(METRIC_TEXT)
                .monospace()
                .size(METRIC_TEXT_SIZE),
        )
        .truncate(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_toolbar_preserves_metrics_and_full_control_lane() {
        let row_width = 1_000.0;
        let lanes = toolbar_lanes(row_width);

        assert!(!lanes.compact_controls);
        assert_eq!(lanes.controls_width, CONTROLS_PREFERRED_WIDTH);
        assert!(lanes.metrics_width >= METRICS_MIN_WIDTH);
        assert_eq!(
            lanes.metrics_width + TOOLBAR_LANE_GAP + lanes.controls_width,
            row_width
        );
    }

    #[test]
    fn compact_toolbar_gives_actions_the_entire_row_before_metrics() {
        let row_width = CONTROLS_PREFERRED_WIDTH + METRICS_MIN_WIDTH;
        let lanes = toolbar_lanes(row_width);

        assert!(lanes.compact_controls);
        assert_eq!(lanes.metrics_width, 0.0);
        assert_eq!(lanes.controls_width, row_width);
    }

    #[test]
    fn toolbar_lane_widths_remain_valid_for_tiny_windows() {
        let lanes = toolbar_lanes(-10.0);

        assert!(lanes.compact_controls);
        assert_eq!(lanes.metrics_width, 0.0);
        assert_eq!(lanes.controls_width, 0.0);
    }
}
