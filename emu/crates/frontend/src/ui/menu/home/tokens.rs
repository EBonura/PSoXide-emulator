//! Design tokens of the shell: colours, the type scale, spacing, sizes and
//! motion, in one place (the v9 spec). Change a number here, not at the use.

#![allow(dead_code)]

use egui::Color32;

// ---- Colour (on ground #05090d) --------------------------------------------

pub(crate) const GROUND: Color32 = Color32::from_rgb(5, 9, 13);
/// Titles and primary text (18:1).
pub(crate) const TEXT: Color32 = Color32::from_rgb(244, 248, 251);
/// Row labels (13:1).
pub(crate) const TEXT_2: Color32 = Color32::from_rgb(196, 211, 222);
/// Support text and hints (8.9:1).
pub(crate) const MUTED: Color32 = Color32::from_rgb(157, 176, 190);
/// Section labels, inactive nav (6.5:1).
pub(crate) const DIM: Color32 = Color32::from_rgb(129, 150, 166);
/// Inactive icons (6.1:1).
pub(crate) const ICON_REST: Color32 = Color32::from_rgb(125, 144, 160);
/// Glyph rings and borders that carry meaning (4.8:1).
pub(crate) const RING: Color32 = Color32::from_rgb(108, 127, 143);
/// Row separators: rgba(160,225,240,.10).
pub(crate) fn hairline() -> Color32 {
    Color32::from_rgba_unmultiplied(160, 225, 240, 26)
}
/// A border that has to be seen: rgba(160,225,240,.22).
pub(crate) fn hairline_strong() -> Color32 {
    Color32::from_rgba_unmultiplied(160, 225, 240, 56)
}
/// The only accent: active, primary, fills (9.95:1).
pub(crate) const ACCENT: Color32 = Color32::from_rgb(34, 200, 229);
/// Focus outline and corner brackets.
pub(crate) const FOCUS: Color32 = Color32::from_rgb(128, 239, 255);
/// The connected dot, nothing else.
pub(crate) const OK: Color32 = Color32::from_rgb(82, 208, 138);
/// Text on the accent (9:1).
pub(crate) const ON_ACCENT: Color32 = Color32::from_rgb(3, 24, 32);

// ---- Type scale (px) ---------------------------------------------------------

pub(crate) const DISPLAY: f32 = 34.0;
pub(crate) const TITLE_DESKTOP: f32 = 26.0;
pub(crate) const TITLE_PHONE: f32 = 20.0;
pub(crate) const LABEL: f32 = 12.0;
pub(crate) const ROW_LABEL: f32 = 16.0;
pub(crate) const BODY: f32 = 14.0;
pub(crate) const BODY_LARGE: f32 = 16.0;
/// Nothing is smaller than this.
pub(crate) const CAPTION: f32 = 12.0;
pub(crate) const CAPTION_LARGE: f32 = 14.0;
pub(crate) const METRIC: f32 = 36.0;
pub(crate) const SLIDER_VALUE: f32 = 26.0;
pub(crate) const BAR_VALUE: f32 = 22.0;

// ---- Spacing and sizes (px) ---------------------------------------------------

pub(crate) const MARGIN_DESKTOP: f32 = 48.0;
pub(crate) const MARGIN_PHONE: f32 = 20.0;
pub(crate) const MARGIN_SMALL_PHONE: f32 = 14.0;
pub(crate) const ROW_DESKTOP: f32 = 52.0;
pub(crate) const ROW_PHONE: f32 = 56.0;
pub(crate) const TOUCH_MIN: f32 = 44.0;
pub(crate) const SECTION_GAP: f32 = 24.0;
/// The panel's one chamfered corner (top right).
pub(crate) const CHAMFER: f32 = 16.0;
pub(crate) const TAB_BAR_HEIGHT: f32 = 64.0;
pub(crate) const TAB_BAR_ABOVE_EDGE: f32 = 26.0;
pub(crate) const TAB_BAR_SIDE: f32 = 24.0;
pub(crate) const TAB_ITEM_WIDTH: f32 = 70.0;
/// A focused case or card lifts this far.
pub(crate) const FOCUS_LIFT: f32 = 4.0;
pub(crate) const BRACKET: f32 = 9.0;
pub(crate) const FOCUS_STROKE: f32 = 2.0;
pub(crate) const FOCUS_TINT: f32 = 0.12;

// ---- Motion -------------------------------------------------------------------

/// Seconds.
pub(crate) const FEEDBACK: f32 = 0.120;
pub(crate) const ROW_FOCUS_MOVE: f32 = 0.160;
pub(crate) const SHEET: f32 = 0.320;
pub(crate) const NAV_SWITCH: f32 = 0.200;
/// The whole launch sequence.
pub(crate) const LAUNCH: f32 = 2.3;

/// A CSS `cubic-bezier(x1, y1, x2, y2)` easing.
pub(crate) fn bezier(x1: f32, y1: f32, x2: f32, y2: f32, t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    let cx = 3.0 * x1;
    let bx = 3.0 * (x2 - x1) - cx;
    let ax = 1.0 - cx - bx;
    let cy = 3.0 * y1;
    let by = 3.0 * (y2 - y1) - cy;
    let ay = 1.0 - cy - by;
    let x_at = |s: f32| ((ax * s + bx) * s + cx) * s;
    let dx_at = |s: f32| (3.0 * ax * s + 2.0 * bx) * s + cx;
    // Solve x(s) = t, Newton then bisection.
    let mut s = t;
    for _ in 0..6 {
        let err = x_at(s) - t;
        if err.abs() < 1e-4 {
            break;
        }
        let d = dx_at(s);
        if d.abs() < 1e-5 {
            break;
        }
        s -= err / d;
    }
    let (mut lo, mut hi) = (0.0_f32, 1.0_f32);
    if !(0.0..=1.0).contains(&s) || (x_at(s) - t).abs() > 1e-3 {
        s = t;
        for _ in 0..24 {
            if x_at(s) < t {
                lo = s;
            } else {
                hi = s;
            }
            s = (lo + hi) / 2.0;
        }
    }
    ((ay * s + by) * s + cy) * s
}

/// Enter, open, lift: exponential ease-out.
pub(crate) fn ease_out_expo(t: f32) -> f32 {
    bezier(0.16, 1.0, 0.3, 1.0, t)
}

/// Recede, slide.
pub(crate) fn ease_in_out(t: f32) -> f32 {
    bezier(0.65, 0.0, 0.35, 1.0, t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn easings_run_from_zero_to_one_and_expo_is_front_loaded() {
        assert!(ease_out_expo(0.0).abs() < 1e-3);
        assert!((ease_out_expo(1.0) - 1.0).abs() < 1e-3);
        assert!(
            ease_out_expo(0.2) > 0.5,
            "exponential ease-out covers most of the way early"
        );
        assert!((ease_in_out(0.5) - 0.5).abs() < 0.02);
        assert!(ease_in_out(0.25) < 0.25 && ease_in_out(0.75) > 0.75);
    }
}
