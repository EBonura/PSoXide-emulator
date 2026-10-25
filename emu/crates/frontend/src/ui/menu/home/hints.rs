//! Button hints: the drawn PlayStation glyphs (cross, circle, triangle, square),
//! the shoulder pills (L1, R1, L2, R2) and, for a keyboard, the key caps that
//! do the same job, in the shell's palette.
//!
//! A hint is shown beside the thing the button does, so a control is never a
//! secret. Which kind shows depends on what the player is using: glyphs when a
//! controller is connected, key caps for a keyboard, nothing on a touch screen
//! (the controls are right there to be tapped).

use egui::{Align2, Color32, FontId, Painter, Pos2, Rect, Shape, Stroke, Vec2};

use super::scene::{fade, CYAN};
use super::tokens;

/// A button of the pad.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Btn {
    Cross,
    Circle,
    Triangle,
    Square,
    L1,
    R1,
    L2,
    R2,
}

impl Btn {
    /// The keyboard key that does the same in the default map.
    pub(super) const fn key(self) -> &'static str {
        match self {
            Self::Cross => "Enter",
            Self::Circle => "Esc",
            Self::Triangle => "Tab",
            Self::Square => "H",
            Self::L1 => "Q",
            Self::R1 => "E",
            Self::L2 => "1",
            Self::R2 => "3",
        }
    }

    /// The pill label of a shoulder button.
    const fn shoulder(self) -> Option<&'static str> {
        match self {
            Self::L1 => Some("L1"),
            Self::R1 => Some("R1"),
            Self::L2 => Some("L2"),
            Self::R2 => Some("R2"),
            _ => None,
        }
    }
}

/// What kind of hint the player sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HintStyle {
    /// Drawn glyphs, for a controller.
    Pad,
    /// Key caps, for a keyboard.
    Keys,
}

/// Radius of a face-button glyph at `u`.
pub(super) fn face_radius(u: f32) -> f32 {
    9.0 * u
}

/// A face button: a ring with its symbol. `press` (0 to 1) lights it.
pub(super) fn draw_face(p: &Painter, c: Pos2, r: f32, btn: Btn, a: f32, press: f32) {
    let ring = lerp_color(tokens::RING, tokens::FOCUS, press);
    let ink = lerp_color(tokens::DIM, tokens::FOCUS, press);
    draw_face_in(p, c, r, btn, a, press, ring, ink);
}

/// A face button on the accent colour (the primary button): dark ink.
pub(super) fn draw_face_on_accent(p: &Painter, c: Pos2, r: f32, btn: Btn, a: f32) {
    draw_face_in(p, c, r, btn, a, 0.0, tokens::ON_ACCENT, tokens::ON_ACCENT);
}

#[allow(clippy::too_many_arguments)]
fn draw_face_in(
    p: &Painter,
    c: Pos2,
    r: f32,
    btn: Btn,
    a: f32,
    press: f32,
    ring: Color32,
    ink: Color32,
) {
    if press > 0.01 {
        p.circle_filled(c, r, fade(CYAN, 0.22 * press * a));
    }
    p.circle_stroke(c, r, Stroke::new(1.2, fade(ring, a)));
    let g = Stroke::new(1.3, fade(ink, a));
    let d = r * 0.38;
    match btn {
        Btn::Cross => {
            p.line_segment([c + Vec2::new(-d, -d), c + Vec2::new(d, d)], g);
            p.line_segment([c + Vec2::new(-d, d), c + Vec2::new(d, -d)], g);
        }
        Btn::Circle => {
            p.circle_stroke(c, d * 1.05, g);
        }
        Btn::Triangle => {
            let top = c + Vec2::new(0.0, -d * 1.1);
            let bl = c + Vec2::new(-d * 1.05, d * 0.8);
            let br = c + Vec2::new(d * 1.05, d * 0.8);
            p.add(Shape::closed_line(vec![top, br, bl], g));
        }
        Btn::Square => {
            p.rect_stroke(
                Rect::from_center_size(c, Vec2::splat(d * 2.0)),
                0.0,
                g,
                egui::StrokeKind::Middle,
            );
        }
        _ => {}
    }
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    super::scene::mix(a, b, t)
}

/// A shoulder button or a key cap: a small rounded pill with a label.
/// Returns its rect. `press` (0 to 1) lights it.
pub(super) fn draw_pill(p: &Painter, center: Pos2, text: &str, u: f32, a: f32, press: f32) -> Rect {
    let font = FontId::proportional(11.5 * u.max(0.9));
    let g = p.layout_no_wrap(text.to_string(), font, tokens::DIM);
    let w = (g.size().x + 14.0 * u).max(26.0 * u);
    let h = 20.0 * u;
    let r = Rect::from_center_size(center, Vec2::new(w, h));
    let border = lerp_color(tokens::RING, tokens::FOCUS, press);
    let ink = lerp_color(tokens::DIM, tokens::FOCUS, press);
    p.rect_filled(
        r,
        h / 2.0,
        fade(
            lerp_color(Color32::from_rgb(10, 20, 28), CYAN, 0.28 * press),
            a * 0.9,
        ),
    );
    p.rect_stroke(
        r,
        h / 2.0,
        Stroke::new(1.2, fade(border, a)),
        egui::StrokeKind::Inside,
    );
    p.text(
        r.center(),
        Align2::CENTER_CENTER,
        text,
        FontId::proportional(11.5 * u.max(0.9)),
        fade(ink, a),
    );
    r
}

/// The glyph or cap of `btn` centred at `c`; returns its rect.
pub(super) fn draw_btn(
    p: &Painter,
    c: Pos2,
    u: f32,
    a: f32,
    style: HintStyle,
    btn: Btn,
    press: f32,
) -> Rect {
    match (style, btn.shoulder()) {
        (HintStyle::Pad, None) => {
            let r = face_radius(u);
            draw_face(p, c, r, btn, a, press);
            Rect::from_center_size(c, Vec2::splat(r * 2.0))
        }
        (HintStyle::Pad, Some(label)) => draw_pill(p, c, label, u, a, press),
        (HintStyle::Keys, _) => draw_pill(p, c, btn.key(), u, a, press),
    }
}

/// The width a hint of `btn` takes, for laying a row out before drawing it.
pub(super) fn btn_width(p: &Painter, u: f32, style: HintStyle, btn: Btn) -> f32 {
    match (style, btn.shoulder()) {
        (HintStyle::Pad, None) => face_radius(u) * 2.0,
        (HintStyle::Pad, Some(label)) => pill_width(p, label, u),
        (HintStyle::Keys, _) => pill_width(p, btn.key(), u),
    }
}

fn pill_width(p: &Painter, text: &str, u: f32) -> f32 {
    let g = p.layout_no_wrap(
        text.to_string(),
        FontId::proportional(11.5 * u.max(0.9)),
        tokens::DIM,
    );
    (g.size().x + 14.0 * u).max(26.0 * u)
}

/// A hint: the glyph, then what it does. `x` is its left edge, `y` its middle.
/// Returns the rect it covers.
#[allow(clippy::too_many_arguments)]
pub(super) fn hint(
    p: &Painter,
    x: f32,
    y: f32,
    u: f32,
    a: f32,
    style: HintStyle,
    btn: Btn,
    label: &str,
    size: f32,
) -> Rect {
    let w = btn_width(p, u, style, btn);
    let g = draw_btn(p, Pos2::new(x + w / 2.0, y), u, a, style, btn, 0.0);
    let t = p.text(
        Pos2::new(g.right() + 8.0 * u, y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(size),
        fade(tokens::DIM, a),
    );
    g.union(t)
}

/// A row of hints starting at `x`, ending where the last label ends.
pub(super) fn hint_row(
    p: &Painter,
    mut x: f32,
    y: f32,
    u: f32,
    a: f32,
    style: HintStyle,
    items: &[(Btn, &str)],
    size: f32,
) -> f32 {
    for (btn, label) in items {
        let r = hint(p, x, y, u, a, style, *btn, label, size);
        x = r.right() + 22.0 * u;
    }
    x
}
