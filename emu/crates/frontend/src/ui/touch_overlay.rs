//! The on-screen PlayStation controller, drawn as vector shapes so it stays
//! crisp at any pixel density. Layout and touch handling are in
//! `input::touch`; this only paints their state.

use egui::epaint::{CornerRadius, PathShape, PathStroke};
use egui::{
    pos2, vec2, Color32, Context, FontId, LayerId, Order, Painter, Pos2, Rect, Stroke, Vec2,
};

use crate::icons;
use crate::input::backend::touch::TouchDevice;
use crate::input::touch::layout::{Glyph, Region, StickSide, TouchLayout, UiId};
use emulator_core::button;

/// Menu and focus accent, shared with the rest of the interface.
const ACCENT: Color32 = Color32::from_rgb(34, 200, 229);
const BODY: Color32 = Color32::from_rgb(22, 26, 34);
const INK: Color32 = Color32::from_rgb(226, 232, 240);
const TRIANGLE: Color32 = Color32::from_rgb(70, 196, 150);
const CIRCLE: Color32 = Color32::from_rgb(232, 86, 96);
const CROSS: Color32 = Color32::from_rgb(96, 140, 232);
const SQUARE: Color32 = Color32::from_rgb(222, 140, 190);

fn with_alpha(color: Color32, alpha: f32) -> Color32 {
    let [r, g, b, _] = color.to_array();
    Color32::from_rgba_unmultiplied(r, g, b, (alpha.clamp(0.0, 1.0) * 255.0).round() as u8)
}

/// Points per window pixel for the layer being painted.
struct Space {
    inv: f32,
}

impl Space {
    fn p(&self, p: Pos2) -> Pos2 {
        pos2(p.x * self.inv, p.y * self.inv)
    }
    fn len(&self, v: f32) -> f32 {
        v * self.inv
    }
    fn rect(&self, r: Rect) -> Rect {
        Rect::from_min_max(self.p(r.min), self.p(r.max))
    }
}

/// Height (in points) of the black panel under the game in portrait, and the
/// insets (in points) the game keeps clear of, when the controller shows.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Reserved {
    pub(crate) top: f32,
    pub(crate) bottom: f32,
    pub(crate) left: f32,
    pub(crate) right: f32,
}

/// What the controller takes from the game's area, in points.
pub(crate) fn reserved(ctx: &Context, touch: &TouchDevice) -> Reserved {
    let Some(layout) = touch.layout() else {
        return Reserved::default();
    };
    let safe_px = touch.safe_px();
    let inv = 1.0 / ctx.pixels_per_point();
    Reserved {
        top: safe_px.top * inv,
        bottom: layout.reserved_bottom * inv,
        left: safe_px.left * inv,
        right: safe_px.right * inv,
    }
}

/// Paint the controller above everything else.
pub(crate) fn draw(ctx: &Context, touch: &TouchDevice, menu_open: bool) {
    let show_pad = !menu_open;
    let Some(layout) = touch.layout() else {
        return;
    };
    let alpha = touch.alpha();
    if alpha <= 0.01 {
        return;
    }
    let painter = ctx.layer_painter(LayerId::new(
        Order::Foreground,
        egui::Id::new("touch-controller"),
    ));
    let space = Space {
        inv: 1.0 / ctx.pixels_per_point(),
    };
    let pad = touch.pad();
    if show_pad {
        draw_dpad(&painter, &space, layout, pad.held(), alpha);
        for b in &layout.buttons {
            let down = pad.held() & b.mask != 0;
            match b.glyph {
                Glyph::Label(text) => {
                    draw_pill(&painter, &space, b.draw.bounds(), text, down, alpha)
                }
                glyph => draw_face(&painter, &space, b.draw, glyph, down, alpha),
            }
        }
        for stick in &layout.sticks {
            draw_stick(&painter, &space, touch, stick.side, alpha);
        }
    }
    for ui in &layout.ui {
        let down = pad.ui_pressed() == Some(ui.id);
        match ui.id {
            UiId::Pause => draw_icon_button(&painter, &space, ui.draw, icons::PAUSE, down, alpha),
            UiId::Save => draw_icon_button(&painter, &space, ui.draw, icons::SAVE, down, alpha),
            UiId::Debug => draw_icon_button(&painter, &space, ui.draw, icons::BUG, down, alpha),
            UiId::Menu => draw_menu_button(&painter, &space, ui.draw, down, menu_open, alpha),
            UiId::Fullscreen => draw_fullscreen_button(&painter, &space, ui.draw, down, alpha),
        }
    }
}

/// Pill label size as a fraction of the pill's height. 0.34 leaves a clear margin
/// either side of SELECT, the widest label, on every phone (see the layout tests).
pub(crate) const PILL_TEXT: f32 = 0.34;

fn draw_pill(painter: &Painter, s: &Space, rect: Rect, text: &str, down: bool, alpha: f32) {
    let r = s.rect(rect);
    let round = CornerRadius::same((r.height() * 0.5).round().min(255.0) as u8);
    let fill = if down { ACCENT } else { BODY };
    painter.rect_filled(
        r,
        round,
        with_alpha(fill, alpha * if down { 0.9 } else { 0.62 }),
    );
    painter.rect_stroke(
        r,
        round,
        Stroke::new(
            s.len(rect.height() * 0.045).max(1.0),
            with_alpha(INK, alpha * 0.75),
        ),
        egui::StrokeKind::Inside,
    );
    painter.text(
        r.center(),
        egui::Align2::CENTER_CENTER,
        text,
        FontId::proportional(r.height() * PILL_TEXT),
        with_alpha(if down { BODY } else { INK }, (alpha + 0.2).min(1.0)),
    );
}

fn draw_face(painter: &Painter, s: &Space, region: Region, glyph: Glyph, down: bool, alpha: f32) {
    let Region::Circle { center, radius } = region else {
        return;
    };
    let (c, r) = (s.p(center), s.len(radius));
    let color = match glyph {
        Glyph::Triangle => TRIANGLE,
        Glyph::Circle => CIRCLE,
        Glyph::Cross => CROSS,
        Glyph::Square => SQUARE,
        Glyph::Label(_) => INK,
    };
    if down {
        // A halo and a filled face read instantly, even under a thumb.
        painter.circle_filled(c, r * 1.18, with_alpha(color, alpha * 0.28));
        painter.circle_filled(c, r, with_alpha(color, alpha * 0.85));
        painter.circle_stroke(c, r, Stroke::new(r * 0.07, with_alpha(INK, alpha)));
    } else {
        painter.circle_filled(c, r, with_alpha(BODY, alpha * 0.62));
        painter.circle_stroke(c, r, Stroke::new(r * 0.07, with_alpha(INK, alpha * 0.7)));
    }
    let ink = if down { BODY } else { color };
    let stroke = Stroke::new(r * 0.12, with_alpha(ink, (alpha + 0.25).min(1.0)));
    let g = r * 0.42;
    match glyph {
        Glyph::Triangle => {
            let top = c + vec2(0.0, -g * 1.05);
            let bl = c + vec2(-g * 1.0, g * 0.8);
            let br = c + vec2(g * 1.0, g * 0.8);
            painter.add(PathShape::closed_line(vec![top, br, bl], stroke));
        }
        Glyph::Circle => {
            painter.circle_stroke(c, g * 1.05, stroke);
        }
        Glyph::Cross => {
            painter.line_segment([c + vec2(-g, -g), c + vec2(g, g)], stroke);
            painter.line_segment([c + vec2(-g, g), c + vec2(g, -g)], stroke);
        }
        Glyph::Square => {
            painter.rect_stroke(
                Rect::from_center_size(c, Vec2::splat(g * 2.0)),
                CornerRadius::ZERO,
                stroke,
                egui::StrokeKind::Middle,
            );
        }
        Glyph::Label(_) => {}
    }
}

fn draw_dpad(painter: &Painter, s: &Space, layout: &TouchLayout, held: u16, alpha: f32) {
    let d = &layout.dpad;
    let c = s.p(d.center);
    let (half, arm) = (s.len(d.half_span), s.len(d.arm));
    let hub = arm * 0.5;
    let round = (arm * 0.28).round().min(255.0) as u8;
    let outline = Stroke::new(
        s.len(layout.unit * 1.6).max(1.0),
        with_alpha(INK, alpha * 0.7),
    );
    // (direction bit, rectangle, rounded corners)
    let arms = [
        (
            button::UP,
            Rect::from_min_max(pos2(c.x - hub, c.y - half), pos2(c.x + hub, c.y - hub)),
            CornerRadius {
                nw: round,
                ne: round,
                sw: 0,
                se: 0,
            },
        ),
        (
            button::DOWN,
            Rect::from_min_max(pos2(c.x - hub, c.y + hub), pos2(c.x + hub, c.y + half)),
            CornerRadius {
                nw: 0,
                ne: 0,
                sw: round,
                se: round,
            },
        ),
        (
            button::LEFT,
            Rect::from_min_max(pos2(c.x - half, c.y - hub), pos2(c.x - hub, c.y + hub)),
            CornerRadius {
                nw: round,
                ne: 0,
                sw: round,
                se: 0,
            },
        ),
        (
            button::RIGHT,
            Rect::from_min_max(pos2(c.x + hub, c.y - hub), pos2(c.x + half, c.y + hub)),
            CornerRadius {
                nw: 0,
                ne: round,
                sw: 0,
                se: round,
            },
        ),
    ];
    painter.rect_filled(
        Rect::from_center_size(c, Vec2::splat(arm)),
        CornerRadius::ZERO,
        with_alpha(BODY, alpha * 0.62),
    );
    for (bit, rect, corners) in arms {
        let down = held & bit != 0;
        let fill = if down { ACCENT } else { BODY };
        painter.rect_filled(
            rect,
            corners,
            with_alpha(fill, alpha * if down { 0.9 } else { 0.62 }),
        );
        painter.rect_stroke(rect, corners, outline, egui::StrokeKind::Inside);
        // Chevron marking the direction.
        let toward = match bit {
            button::UP => vec2(0.0, -1.0),
            button::DOWN => vec2(0.0, 1.0),
            button::LEFT => vec2(-1.0, 0.0),
            _ => vec2(1.0, 0.0),
        };
        let side = vec2(-toward.y, toward.x);
        let tip = rect.center() + toward * arm * 0.12;
        let back = rect.center() - toward * arm * 0.10;
        let w = arm * 0.2;
        painter.add(PathShape::convex_polygon(
            vec![tip, back + side * w, back - side * w],
            with_alpha(if down { BODY } else { INK }, (alpha + 0.15).min(1.0) * 0.8),
            PathStroke::NONE,
        ));
    }
}

fn draw_stick(painter: &Painter, s: &Space, touch: &TouchDevice, side: StickSide, alpha: f32) {
    let Some(layout) = touch.layout() else {
        return;
    };
    let Some(plan) = layout.stick(side) else {
        return;
    };
    let ring = s.len(plan.ring);
    let held = touch.pad().stick_center(side);
    let center = s.p(held.unwrap_or(plan.rest));
    let knob_at = touch.pad().stick_knob(side).map_or(center, |p| s.p(p));
    let edge = Stroke::new(
        s.len(layout.unit * 1.8).max(1.0),
        with_alpha(INK, alpha * if held.is_some() { 0.85 } else { 0.4 }),
    );
    painter.circle_filled(
        center,
        ring,
        with_alpha(BODY, alpha * if held.is_some() { 0.5 } else { 0.28 }),
    );
    painter.circle_stroke(center, ring, edge);
    if held.is_some() {
        painter.circle_stroke(
            center,
            ring * 0.45,
            Stroke::new(edge.width * 0.6, with_alpha(INK, alpha * 0.18)),
        );
    }
    let knob = ring * 0.46;
    if held.is_some() {
        painter.circle_filled(knob_at, knob, with_alpha(ACCENT, alpha * 0.85));
        painter.circle_stroke(
            knob_at,
            knob,
            Stroke::new(edge.width, with_alpha(INK, alpha)),
        );
    } else {
        // Idle: the cheapest form that still shows where the thumb goes.
        painter.circle_filled(knob_at, knob, with_alpha(INK, alpha * 0.2));
    }
    let _ = side;
}

/// One icon of the interface pill: Pause, Save or Debug.
fn draw_icon_button(painter: &Painter, s: &Space, rect: Rect, glyph: char, down: bool, alpha: f32) {
    let r = s.rect(rect);
    let round = CornerRadius::same((r.height() * 0.5).round().min(255.0) as u8);
    painter.rect_filled(
        r,
        round,
        with_alpha(
            if down { ACCENT } else { BODY },
            alpha * if down { 0.9 } else { 0.62 },
        ),
    );
    painter.rect_stroke(
        r,
        round,
        Stroke::new(1.2, with_alpha(INK, alpha * 0.75)),
        egui::StrokeKind::Inside,
    );
    painter.text(
        r.center(),
        egui::Align2::CENTER_CENTER,
        glyph,
        egui::FontId::proportional(r.height() * 0.5),
        with_alpha(if down { BODY } else { INK }, (alpha + 0.2).min(1.0)),
    );
}

fn draw_menu_button(painter: &Painter, s: &Space, rect: Rect, down: bool, close: bool, alpha: f32) {
    let r = s.rect(rect);
    let round = CornerRadius::same((r.height() * 0.5).round().min(255.0) as u8);
    painter.rect_filled(
        r,
        round,
        with_alpha(
            if down { ACCENT } else { BODY },
            alpha * if down { 0.9 } else { 0.62 },
        ),
    );
    painter.rect_stroke(
        r,
        round,
        Stroke::new(1.2, with_alpha(INK, alpha * 0.75)),
        egui::StrokeKind::Inside,
    );
    let c = r.center();
    let ink = with_alpha(if down { BODY } else { INK }, (alpha + 0.2).min(1.0));
    let w = r.height() * 0.5;
    if close {
        let h = r.height() * 0.17;
        let stroke = Stroke::new(r.height() * 0.08, ink);
        painter.line_segment([c + vec2(-h, -h), c + vec2(h, h)], stroke);
        painter.line_segment([c + vec2(-h, h), c + vec2(h, -h)], stroke);
        return;
    }
    for dy in [-0.2, 0.0, 0.2] {
        let y = c.y + r.height() * dy;
        painter.line_segment(
            [pos2(c.x - w * 0.5, y), pos2(c.x + w * 0.5, y)],
            Stroke::new(r.height() * 0.07, ink),
        );
    }
}

fn draw_fullscreen_button(painter: &Painter, s: &Space, rect: Rect, down: bool, alpha: f32) {
    let r = s.rect(rect);
    let round = CornerRadius::same((r.height() * 0.3).round().min(255.0) as u8);
    painter.rect_filled(
        r,
        round,
        with_alpha(
            if down { ACCENT } else { BODY },
            alpha * if down { 0.9 } else { 0.62 },
        ),
    );
    painter.rect_stroke(
        r,
        round,
        Stroke::new(1.2, with_alpha(INK, alpha * 0.75)),
        egui::StrokeKind::Inside,
    );
    let ink = with_alpha(if down { BODY } else { INK }, (alpha + 0.2).min(1.0));
    let stroke = Stroke::new(r.height() * 0.07, ink);
    let c = r.center();
    let h = r.height() * 0.24;
    let k = h * 0.55;
    for (sx, sy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
        let corner = c + vec2(sx * h, sy * h);
        painter.line_segment([corner, corner - vec2(sx * k, 0.0)], stroke);
        painter.line_segment([corner, corner - vec2(0.0, sy * k)], stroke);
    }
}
