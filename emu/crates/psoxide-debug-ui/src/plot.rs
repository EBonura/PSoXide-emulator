//! Painter-level chart helpers: palette, time axis, chart frames.

use std::sync::atomic::{AtomicU32, Ordering};

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

/// The panel's type scale as `f32` bits: 1 on a desktop, smaller on a phone.
static TYPE_SCALE: AtomicU32 = AtomicU32::new(0x3f80_0000);

/// Set the scale every font of the panel is drawn at (1.0 is the desktop size).
pub fn set_type_scale(scale: f32) {
    TYPE_SCALE.store(scale.clamp(0.5, 2.0).to_bits(), Ordering::Relaxed);
}

/// Where the stat cards ended when last drawn, in points down the screen (f32 bits): the browser
/// journeys check they are in view without scrolling.
pub static LAYOUT: [AtomicU32; 8] = [const { AtomicU32::new(0) }; 8];

/// Where the phone panel's section headers were last drawn: the y of each (f32 bits), negative
/// while the section is open, and how many there are. The browser journeys tap them.
pub static SECTIONS: [AtomicU32; 8] = [const { AtomicU32::new(0) }; 8];
pub static SECTION_COUNT: AtomicU32 = AtomicU32::new(0);

/// The panel is laid out for a phone: one density tier below the desktop's (see [`tiny`]).
static TINY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Lay the panel out for a phone: 8-point text, 10-point values, no card chrome, 50-point charts,
/// sections folded to a line. A desktop keeps the normal dense layout.
pub fn set_tiny(on: bool) {
    TINY.store(on, Ordering::Relaxed);
}

/// Whether the panel is laid out for a phone.
pub fn tiny() -> bool {
    TINY.load(Ordering::Relaxed)
}

/// Where the stat cards ended when last drawn (see [`STATS_BOTTOM`]).
pub static STATS_BOTTOM: AtomicU32 = AtomicU32::new(0);

/// The panel's type scale itself: 1 on a desktop, below 1 where the panel is set small.
pub fn type_scale() -> f32 {
    f32::from_bits(TYPE_SCALE.load(Ordering::Relaxed))
}

/// A font size in points at the panel's type scale.
pub fn px(size: f32) -> f32 {
    size * f32::from_bits(TYPE_SCALE.load(Ordering::Relaxed))
}

/// Small UI font.
pub fn small() -> FontId {
    FontId::proportional(px(10.5))
}

/// Axis and gridline labels: 9 points when the panel is set small, else the small font.
pub fn axis_font() -> FontId {
    if tiny() {
        FontId::proportional(7.5)
    } else if type_scale() < 0.95 {
        FontId::proportional(9.0)
    } else {
        small()
    }
}

/// Regular UI font.
pub fn body() -> FontId {
    FontId::proportional(px(12.0))
}

/// Digits for values, in the same face as the rest of the interface. (The pixel face the
/// monospace family starts with reads badly at these sizes, and no value here needs columns.)
pub fn figures(size: f32) -> FontId {
    FontId::proportional(px(size + 1.5))
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
    // A phone's charts are sparklines: half the height, never under 40 points.
    let height = if tiny() {
        (height * 0.5).max(40.0)
    } else {
        height
    };
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
            axis_font(),
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
        // Set for a phone the labels sit inside the plot, along its bottom edge.
        let (y, anchor) = if tiny() {
            (axis.rect.bottom() - 1.0, Align2::CENTER_BOTTOM)
        } else {
            (axis.rect.bottom() + 1.0, Align2::CENTER_TOP)
        };
        painter.text(Pos2::new(x, y), anchor, label, axis_font(), TEXT_MUTED);
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
