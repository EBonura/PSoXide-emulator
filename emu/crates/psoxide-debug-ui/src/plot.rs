//! Painter-level chart helpers: palette, time axis, chart frames.

use egui::{Align2, Color32, FontId, Pos2, Rect, Response, Sense, Shape, Stroke, Ui};

/// Chart surface (matches the frontend's darkest panel fill).
pub const SURFACE: Color32 = Color32::from_rgb(20, 20, 24);
/// Grid and axis hairlines.
pub const GRID: Color32 = Color32::from_rgb(42, 42, 50);
/// Primary text.
pub const TEXT: Color32 = Color32::from_rgb(214, 214, 226);
/// Secondary text.
pub const TEXT_DIM: Color32 = Color32::from_rgb(138, 138, 152);
/// Muted text (axis ticks).
pub const TEXT_MUTED: Color32 = Color32::from_rgb(98, 98, 112);
/// Hover cursor.
pub const CURSOR: Color32 = Color32::from_rgb(230, 230, 240);

/// Categorical series colours (dark-surface steps), fixed order.
pub const BLUE: Color32 = Color32::from_rgb(0x39, 0x87, 0xe5);
/// Slot 2.
pub const ORANGE: Color32 = Color32::from_rgb(0xd9, 0x59, 0x26);
/// Slot 3.
pub const AQUA: Color32 = Color32::from_rgb(0x19, 0x9e, 0x70);
/// Slot 4.
pub const YELLOW: Color32 = Color32::from_rgb(0xc9, 0x85, 0x00);
/// Slot 5.
pub const MAGENTA: Color32 = Color32::from_rgb(0xd5, 0x51, 0x81);
/// Slot 7.
pub const VIOLET: Color32 = Color32::from_rgb(0x90, 0x85, 0xe9);
/// Neutral series (other / unattributed).
pub const NEUTRAL: Color32 = Color32::from_rgb(0x6b, 0x6b, 0x78);

/// Status: late frame / over budget.
pub const CRITICAL: Color32 = Color32::from_rgb(0xd0, 0x3b, 0x3b);

/// Small UI font.
pub fn small() -> FontId {
    FontId::proportional(10.5)
}

/// Regular UI font.
pub fn body() -> FontId {
    FontId::proportional(12.0)
}

/// Monospace digits for values.
pub fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}

/// Mix `color` toward black (`t` < 0) or white (`t` > 0).
pub fn shade(color: Color32, t: f32) -> Color32 {
    let target = if t < 0.0 { 0.0 } else { 255.0 };
    let t = t.abs().clamp(0.0, 1.0);
    let mix = |c: u8| (f32::from(c) + (target - f32::from(c)) * t).round() as u8;
    Color32::from_rgb(mix(color.r()), mix(color.g()), mix(color.b()))
}

/// Visible time range in vblank units (inclusive of both ends).
#[derive(Clone, Copy, Debug)]
pub struct TimeAxis {
    /// Plot area.
    pub rect: Rect,
    /// Vblank index at the left edge.
    pub first: u64,
    /// Vblank index at the right edge.
    pub last: u64,
}

impl TimeAxis {
    /// Vblanks shown.
    pub fn span(&self) -> u64 {
        self.last - self.first + 1
    }

    /// X of the left edge of a vblank's slot.
    pub fn x(&self, vblank: f64) -> f32 {
        let t = (vblank - self.first as f64) / self.span() as f64;
        self.rect.left() + t as f32 * self.rect.width()
    }

    /// Vblank under screen X.
    pub fn vblank_at(&self, x: f32) -> u64 {
        let t = ((x - self.rect.left()) / self.rect.width()).clamp(0.0, 0.9999);
        self.first + (t as f64 * self.span() as f64) as u64
    }

    /// Vblanks per pixel column, at least one.
    pub fn bin(&self) -> u64 {
        (self.span() as f32 / self.rect.width().max(1.0))
            .ceil()
            .max(1.0) as u64
    }
}

/// Allocate a chart area with hover/drag/click sensing and paint its
/// surface. Returns the full rect and the response.
pub fn chart_area(ui: &mut Ui, height: f32) -> (Rect, Response) {
    let width = ui.available_width().max(60.0);
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, height), Sense::click_and_drag());
    ui.painter().rect_filled(rect, 3.0, SURFACE);
    (rect, response)
}

/// Horizontal gridline with a right-aligned label inside the plot.
pub fn hline(ui: &Ui, rect: Rect, y: f32, label: &str, dashed: bool, color: Color32) {
    let painter = ui.painter_at(rect);
    let stroke = Stroke::new(1.0, color);
    if dashed {
        painter.extend(Shape::dashed_line(
            &[Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
            stroke,
            4.0,
            3.0,
        ));
    } else {
        painter.line_segment(
            [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
            stroke,
        );
    }
    if !label.is_empty() {
        painter.text(
            Pos2::new(rect.right() - 3.0, y - 1.0),
            Align2::RIGHT_BOTTOM,
            label,
            small(),
            TEXT_MUTED,
        );
    }
}

/// Time labels along the bottom edge: seconds relative to `now`.
pub fn time_ticks(ui: &Ui, axis: &TimeAxis, now: u64, refresh_hz: f64) {
    let painter = ui.painter_at(axis.rect.expand2(egui::vec2(0.0, 12.0)));
    let span_s = axis.span() as f64 / refresh_hz;
    let step_s = [0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 30.0, 60.0]
        .into_iter()
        .find(|step| span_s / step <= 6.0)
        .unwrap_or(60.0);
    let first_s = (axis.first as f64 - now as f64) / refresh_hz;
    let last_s = (axis.last as f64 - now as f64) / refresh_hz;
    let mut t = (first_s / step_s).ceil() * step_s;
    while t <= last_s + 1e-6 {
        let x = axis.x(now as f64 + t * refresh_hz);
        painter.line_segment(
            [
                Pos2::new(x, axis.rect.top()),
                Pos2::new(x, axis.rect.bottom()),
            ],
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 8)),
        );
        let label = if t.abs() < 1e-6 {
            "now".to_string()
        } else if step_s < 1.0 {
            format!("{t:.1}s")
        } else {
            format!("{t:.0}s")
        };
        painter.text(
            Pos2::new(x, axis.rect.bottom() + 1.0),
            Align2::CENTER_TOP,
            label,
            small(),
            TEXT_MUTED,
        );
        t += step_s;
    }
}

/// Vertical hover cursor.
pub fn cursor(ui: &Ui, axis: &TimeAxis, vblank: u64) {
    if vblank < axis.first || vblank > axis.last {
        return;
    }
    let x = axis.x(vblank as f64 + 0.5);
    ui.painter_at(axis.rect).line_segment(
        [
            Pos2::new(x, axis.rect.top()),
            Pos2::new(x, axis.rect.bottom()),
        ],
        Stroke::new(1.0, CURSOR.gamma_multiply(0.55)),
    );
}

/// Compact count: 950, 12.3k, 4.1M.
pub fn count(value: f64) -> String {
    let value = value.max(0.0);
    if value >= 9_999_500.0 {
        format!("{:.1}M", value / 1_000_000.0)
    } else if value >= 1_000_000.0 {
        format!("{:.2}M", value / 1_000_000.0)
    } else if value >= 9_999.5 {
        format!("{:.0}k", value / 1_000.0)
    } else if value >= 1_000.0 {
        format!("{:.1}k", value / 1_000.0)
    } else if value >= 10.0 || value == value.round() {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}
