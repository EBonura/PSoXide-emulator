//! Where the on-screen controller's controls sit.
//!
//! Pure geometry: window size, device pixel ratio and safe-area insets in,
//! rectangles in window pixels out. Sizes are authored in CSS pixels (the
//! unit phone vendors size thumb targets in: about 7 to 9 mm at 44 to 48
//! px) and multiplied by the device pixel ratio and a fit factor, so a
//! control is the same physical size on a 2x and a 3x screen. Hit areas are
//! larger than the drawn shapes.

use egui::{pos2, vec2, Pos2, Rect, Vec2};
use emulator_core::button;

/// Thumb-reach margin kept clear around the screen edge, in CSS px.
const MARGIN: f32 = 14.0;
/// Shoulder pill size (drawn), CSS px.
const SHOULDER: Vec2 = vec2(68.0, 38.0);
/// Gap between neighbouring pills, CSS px.
const PILL_GAP: f32 = 8.0;
/// Outer span of the D-pad cross, CSS px.
const DPAD_SPAN: f32 = 124.0;
/// Radius of the circle that grabs a touch for the D-pad, CSS px.
const DPAD_ZONE: f32 = 78.0;
/// Face button centre distance from the cluster centre, CSS px.
const FACE_REACH: f32 = 46.0;
/// Face button drawn radius, CSS px.
const FACE_RADIUS: f32 = 26.0;
/// Face button hit radius, CSS px. Larger than half the distance between
/// neighbours, so a thumb between two buttons presses both.
const FACE_HIT: f32 = 37.0;
/// Half the side of a floating stick's capture square, CSS px.
const STICK_ZONE: f32 = 66.0;
/// Drawn ring radius of a stick, CSS px.
const STICK_RING: f32 = 46.0;
/// Start / Select pill size, CSS px.
const SMALL_PILL: Vec2 = vec2(64.0, 34.0);
/// Menu button size, CSS px.
const MENU_BUTTON: Vec2 = vec2(76.0, 36.0);
/// Pause, Save and Debug button size, CSS px (the icons beside Menu).
const ICON_BUTTON: Vec2 = vec2(40.0, 36.0);
/// Width of the whole row of interface buttons, CSS px (without fullscreen).
const UI_ROW: f32 = 3.0 * (40.0 + 6.0) + 76.0;
/// Fullscreen button size, CSS px.
const FULLSCREEN_BUTTON: Vec2 = vec2(40.0, 36.0);
/// Distance of a cluster centre from its screen edge, CSS px.
const CLUSTER_INSET: f32 = 72.0;

/// Safe-area insets in window pixels (notch, rounded corners, home bar).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Insets {
    pub(crate) top: f32,
    pub(crate) right: f32,
    pub(crate) bottom: f32,
    pub(crate) left: f32,
}

/// Everything the layout depends on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LayoutParams {
    /// Window size in pixels.
    pub(crate) size: Vec2,
    /// Pixels per CSS pixel (the device pixel ratio).
    pub(crate) scale: f32,
    /// Safe-area insets in pixels.
    pub(crate) safe: Insets,
    /// Whether the guest's pad is in analog mode (the sticks show).
    pub(crate) analog: bool,
    /// The player's size preference, percent (100 = the fitted size).
    pub(crate) size_pct: u8,
    /// Swap the D-pad / left stick side with the face buttons / right stick.
    pub(crate) left_handed: bool,
    /// Whether the fullscreen button is offered.
    pub(crate) fullscreen_button: bool,
    /// The emulator menu is open: its buttons move to the top-right corner,
    /// clear of the list, and only they stay live.
    pub(crate) menu_open: bool,
}

/// Which way the window is wider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Orientation {
    /// Wider than tall: pads at the sides, game in the middle.
    Landscape,
    /// Taller than wide: game on top, controls in a panel below.
    Portrait,
}

impl Orientation {
    /// Orientation of a window of `size`.
    pub(crate) fn from_size(size: Vec2) -> Self {
        if size.x >= size.y {
            Self::Landscape
        } else {
            Self::Portrait
        }
    }
}

/// A touch-sensitive area.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Region {
    /// A disc.
    Circle { center: Pos2, radius: f32 },
    /// A rectangle.
    Rect(Rect),
}

impl Region {
    /// Whether `point` is inside.
    pub(crate) fn contains(&self, point: Pos2) -> bool {
        match *self {
            Self::Circle { center, radius } => (point - center).length_sq() <= radius * radius,
            Self::Rect(rect) => rect.contains(point),
        }
    }

    /// The centre.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn center(&self) -> Pos2 {
        match *self {
            Self::Circle { center, .. } => center,
            Self::Rect(rect) => rect.center(),
        }
    }

    /// The smallest rectangle around it.
    pub(crate) fn bounds(&self) -> Rect {
        match *self {
            Self::Circle { center, radius } => {
                Rect::from_center_size(center, Vec2::splat(radius * 2.0))
            }
            Self::Rect(rect) => rect,
        }
    }
}

/// What a button's face shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Glyph {
    /// Triangle face button.
    Triangle,
    /// Circle face button.
    Circle,
    /// Cross face button.
    Cross,
    /// Square face button.
    Square,
    /// A text pill (L1, Start, ...).
    Label(&'static str),
}

/// One PS1 button.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PadButton {
    /// The [`emulator_core::button`] bit it presses.
    pub(crate) mask: u16,
    /// What it shows.
    pub(crate) glyph: Glyph,
    /// Where it is drawn.
    pub(crate) draw: Region,
    /// Where a touch counts as pressing it.
    pub(crate) hit: Region,
}

/// The D-pad cross.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DpadPlan {
    /// Centre of the cross.
    pub(crate) center: Pos2,
    /// Half the outer span.
    pub(crate) half_span: f32,
    /// Width of one arm.
    pub(crate) arm: f32,
    /// A touch inside this circle grabs the D-pad.
    pub(crate) zone: f32,
}

/// Left or right analog stick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StickSide {
    /// Left stick.
    Left,
    /// Right stick.
    Right,
}

/// A floating analog stick: a touch anywhere in `zone` plants its centre.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct StickPlan {
    /// Which stick.
    pub(crate) side: StickSide,
    /// Capture area.
    pub(crate) zone: Rect,
    /// Where the ring rests when no finger is on it.
    pub(crate) rest: Pos2,
    /// Ring radius: full travel.
    pub(crate) ring: f32,
}

/// Buttons that belong to the player's interface rather than the pad.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UiId {
    /// Pause or resume the game.
    Pause,
    /// Save the game's state.
    Save,
    /// Open the debug section.
    Debug,
    /// Open or close the emulator menu.
    Menu,
    /// Enter or leave fullscreen.
    Fullscreen,
}

/// A button of the emulator's own interface.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct UiButton {
    /// Which.
    pub(crate) id: UiId,
    /// Drawn rectangle.
    pub(crate) draw: Rect,
    /// Touch rectangle.
    pub(crate) hit: Rect,
}

/// The computed layout.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TouchLayout {
    /// Wider or taller.
    pub(crate) orientation: Orientation,
    /// Pixels per design unit (one CSS px at fit factor 1).
    pub(crate) unit: f32,
    /// The D-pad.
    pub(crate) dpad: DpadPlan,
    /// Face, shoulder, Start and Select buttons.
    pub(crate) buttons: Vec<PadButton>,
    /// The sticks (empty unless the guest pad is analog).
    pub(crate) sticks: Vec<StickPlan>,
    /// Menu and fullscreen buttons.
    pub(crate) ui: Vec<UiButton>,
    /// Height of the panel under the game in portrait (includes the bottom
    /// inset); zero in landscape.
    pub(crate) reserved_bottom: f32,
}

fn pill(rect: Rect, inflate: Vec2, mask: u16, label: &'static str) -> PadButton {
    PadButton {
        mask,
        glyph: Glyph::Label(label),
        draw: Region::Rect(rect),
        hit: Region::Rect(rect.expand2(inflate)),
    }
}

impl TouchLayout {
    /// Lay the controls out for `params`.
    pub(crate) fn compute(params: &LayoutParams) -> Self {
        match Orientation::from_size(params.size) {
            Orientation::Landscape => Self::landscape(params),
            Orientation::Portrait => Self::portrait(params),
        }
    }

    fn landscape(p: &LayoutParams) -> Self {
        let (ox, oy) = (p.safe.left, p.safe.top);
        let w_px = (p.size.x - p.safe.left - p.safe.right).max(1.0);
        let h_px = (p.size.y - p.safe.top - p.safe.bottom).max(1.0);
        let (w_css, h_css) = (w_px / p.scale, h_px / p.scale);
        let fit = (h_css / 340.0).min(w_css / if p.analog { 760.0 } else { 600.0 });
        let cap = (h_css / 290.0).min(w_css / 620.0);
        let k = (fit.clamp(0.62, 1.5) * f32::from(p.size_pct) / 100.0).min(cap.max(0.62));
        let u = p.scale * k;

        let m = MARGIN * u;
        let shoulder_y = oy + m;
        let cx_mid = ox + w_px / 2.0;

        let mut cy = oy + h_px * 0.5 + 8.0 * u;
        // Keep the grab circle of the D-pad below the shoulder row and the
        // menu buttons under it.
        cy = cy.max(shoulder_y + (SHOULDER.y + 14.0 + MENU_BUTTON.y + 9.0 + DPAD_ZONE) * u);
        let (pad_x, face_x) = cluster_x(ox, w_px, u, 1.0, p.left_handed);
        let mut layout = Self::base(
            Orientation::Landscape,
            u,
            1.0,
            pos2(pad_x, cy),
            pos2(face_x, cy),
        );

        layout.shoulders(ox, w_px, shoulder_y, u);
        layout.small_pills(cx_mid, oy + h_px - m - SMALL_PILL.y * u, u, 6.0);
        // Under the left shoulders, out of the game picture; in the top-right
        // corner while the menu list fills the screen.
        let ui_at = if p.menu_open {
            pos2(
                ox + w_px - m - (UI_ROW + 8.0 + FULLSCREEN_BUTTON.x) * u,
                oy + 8.0 * u,
            )
        } else {
            pos2(ox + m, shoulder_y + (SHOULDER.y + 14.0) * u)
        };
        layout.ui_buttons(ui_at, u, p.fullscreen_button);
        if p.analog {
            let zone = STICK_ZONE * u;
            let y = oy + h_px - m - zone;
            let pill_edge = (SMALL_PILL.x + 6.0) * u;
            let (left_pad_x, right_pad_x) = if p.left_handed {
                (face_x, pad_x)
            } else {
                (pad_x, face_x)
            };
            let left_x = (ox + w_px * 0.30)
                .max(left_pad_x + DPAD_ZONE * u + zone + 4.0 * u)
                .min(cx_mid - pill_edge - zone - 4.0 * u);
            let right_x = (ox + w_px * 0.70)
                .min(right_pad_x - (FACE_REACH + FACE_HIT) * u - zone - 4.0 * u)
                .max(cx_mid + pill_edge + zone + 4.0 * u);
            layout.sticks(pos2(left_x, y), pos2(right_x, y), u);
        }
        layout
    }

    fn portrait(p: &LayoutParams) -> Self {
        let (ox, oy) = (p.safe.left, p.safe.top);
        let w_px = (p.size.x - p.safe.left - p.safe.right).max(1.0);
        let w_css = w_px / p.scale;
        let need_css = if p.analog { 378.0 } else { 234.0 };
        let avail_css = (p.size.y - p.safe.top - p.safe.bottom - 0.75 * w_px).max(0.0) / p.scale;
        let k = (w_css / 380.0).clamp(0.7, 1.4) * f32::from(p.size_pct) / 100.0;
        let k = k.min(avail_css / need_css).clamp(0.55, 1.5);
        let u = p.scale * k;
        // On a narrow screen the two clusters shrink so the Start / Select
        // column between them keeps a thumb-sized width.
        let c = ((w_css / k - 2.0 * MARGIN - (SMALL_PILL.x + 6.0))
            / (2.0 * DPAD_ZONE + 2.0 * (FACE_REACH + FACE_HIT)))
            .clamp(0.7, 1.0);

        let panel_h = need_css * u;
        let panel_top = p.size.y - p.safe.bottom - panel_h;
        let m = MARGIN * u;
        let y0 = panel_top + m;
        let cy = y0 + (SHOULDER.y + 5.0 + (FACE_REACH + FACE_HIT) * c + 5.0) * u;
        let cx_mid = ox + w_px / 2.0;
        let (pad_x, face_x) = cluster_x(ox, w_px, u, c, p.left_handed);
        let mut layout = Self::base(
            Orientation::Portrait,
            u,
            c,
            pos2(pad_x, cy),
            pos2(face_x, cy),
        );
        layout.reserved_bottom = panel_h + p.safe.bottom;

        layout.shoulders(ox, w_px, y0, u);
        // Between the D-pad and the face buttons: Select above Start.
        let column = (face_x - (FACE_REACH + FACE_HIT) * u * c) - (pad_x + DPAD_ZONE * u * c);
        let pill_w = (SMALL_PILL.x * u).min(column - 6.0 * u).max(40.0 * u);
        layout.stacked_pills(pos2(cx_mid, cy), pill_w, u);
        layout.ui_buttons(
            pos2(
                ox + w_px - m - (UI_ROW + 8.0 + FULLSCREEN_BUTTON.x) * u,
                oy + 8.0 * u,
            ),
            u,
            p.fullscreen_button,
        );
        if p.analog {
            let zone = STICK_ZONE * u;
            let y = cy + (75.0 + 12.0) * u + zone;
            let left_x = (ox + w_px * 0.27).max(ox + m + zone);
            let right_x = (ox + w_px * 0.73).min(ox + w_px - m - zone);
            layout.sticks(pos2(left_x, y), pos2(right_x, y), u);
        }
        layout
    }

    fn base(orientation: Orientation, u: f32, c: f32, dpad: Pos2, face: Pos2) -> Self {
        let mut layout = Self {
            orientation,
            unit: u,
            dpad: DpadPlan {
                center: dpad,
                half_span: DPAD_SPAN / 2.0 * u * c,
                arm: 42.0 * u * c,
                zone: DPAD_ZONE * u * c,
            },
            buttons: Vec::new(),
            sticks: Vec::new(),
            ui: Vec::new(),
            reserved_bottom: 0.0,
        };
        for (mask, glyph, dx, dy) in [
            (button::TRIANGLE, Glyph::Triangle, 0.0, -FACE_REACH),
            (button::CROSS, Glyph::Cross, 0.0, FACE_REACH),
            (button::SQUARE, Glyph::Square, -FACE_REACH, 0.0),
            (button::CIRCLE, Glyph::Circle, FACE_REACH, 0.0),
        ] {
            let center = face + vec2(dx, dy) * u * c;
            layout.buttons.push(PadButton {
                mask,
                glyph,
                draw: Region::Circle {
                    center,
                    radius: FACE_RADIUS * u * c,
                },
                hit: Region::Circle {
                    center,
                    radius: FACE_HIT * u * c,
                },
            });
        }
        layout
    }

    fn shoulders(&mut self, ox: f32, w_px: f32, y: f32, u: f32) {
        let size = SHOULDER * u;
        let m = MARGIN * u;
        let step = (SHOULDER.x + PILL_GAP) * u;
        let inflate = vec2((PILL_GAP / 2.0 - 0.5) * u, 5.0 * u);
        let left = ox + m;
        let right = ox + w_px - m - size.x;
        for (mask, label, x) in [
            (button::L2, "L2", left),
            (button::L1, "L1", left + step),
            (button::R2, "R2", right),
            (button::R1, "R1", right - step),
        ] {
            self.buttons.push(pill(
                Rect::from_min_size(pos2(x, y), size),
                inflate,
                mask,
                label,
            ));
        }
    }

    /// Select and Start side by side, centred on `cx`, top at `y`.
    fn small_pills(&mut self, cx: f32, y: f32, u: f32, gap: f32) {
        let size = SMALL_PILL * u;
        let inflate = vec2((gap / 2.0 - 0.5) * u, 5.0 * u);
        let half_gap = gap / 2.0 * u;
        self.buttons.push(pill(
            Rect::from_min_size(pos2(cx - half_gap - size.x, y), size),
            inflate,
            button::SELECT,
            "SELECT",
        ));
        self.buttons.push(pill(
            Rect::from_min_size(pos2(cx + half_gap, y), size),
            inflate,
            button::START,
            "START",
        ));
    }

    /// Select above Start, centred on `center`.
    fn stacked_pills(&mut self, center: Pos2, width: f32, u: f32) {
        let size = vec2(width, SMALL_PILL.y * u);
        let inflate = vec2(3.0 * u, 4.0 * u);
        let dy = (SMALL_PILL.y / 2.0 + 5.0) * u;
        self.buttons.push(pill(
            Rect::from_center_size(center - vec2(0.0, dy), size),
            inflate,
            button::SELECT,
            "SELECT",
        ));
        self.buttons.push(pill(
            Rect::from_center_size(center + vec2(0.0, dy), size),
            inflate,
            button::START,
            "START",
        ));
    }

    fn ui_buttons(&mut self, min: Pos2, u: f32, fullscreen: bool) {
        // One pill of four icons: Pause, Save, Debug, Menu.
        let mut x = min.x;
        for id in [UiId::Pause, UiId::Save, UiId::Debug] {
            let r = Rect::from_min_size(pos2(x, min.y), ICON_BUTTON * u);
            self.ui.push(UiButton {
                id,
                draw: r,
                hit: r.expand2(vec2(0.0, 6.0 * u)),
            });
            x += (ICON_BUTTON.x + 6.0) * u;
        }
        let menu = Rect::from_min_size(pos2(x, min.y), MENU_BUTTON * u);
        self.ui.push(UiButton {
            id: UiId::Menu,
            draw: menu,
            hit: menu.expand2(vec2(3.0 * u, 6.0 * u)),
        });
        if fullscreen {
            let fs =
                Rect::from_min_size(pos2(menu.right() + 8.0 * u, min.y), FULLSCREEN_BUTTON * u);
            self.ui.push(UiButton {
                id: UiId::Fullscreen,
                draw: fs,
                hit: fs.expand2(vec2(3.0 * u, 6.0 * u)),
            });
        }
    }

    fn sticks(&mut self, left: Pos2, right: Pos2, u: f32) {
        for (side, rest) in [(StickSide::Left, left), (StickSide::Right, right)] {
            self.sticks.push(StickPlan {
                side,
                zone: Rect::from_center_size(rest, Vec2::splat(STICK_ZONE * 2.0 * u)),
                rest,
                ring: STICK_RING * u,
            });
        }
    }

    /// The stick plan for `side`, when the sticks are shown.
    pub(crate) fn stick(&self, side: StickSide) -> Option<&StickPlan> {
        self.sticks.iter().find(|stick| stick.side == side)
    }
}

/// X of the D-pad cluster and the face cluster centres.
fn cluster_x(ox: f32, w_px: f32, u: f32, c: f32, left_handed: bool) -> (f32, f32) {
    let edge = (MARGIN + CLUSTER_INSET * c) * u;
    let (left, right) = (ox + edge, ox + w_px - edge);
    if left_handed {
        (right, left)
    } else {
        (left, right)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A phone or tablet: name, CSS size, pixel ratio, safe insets in CSS px
    /// for portrait (top, bottom) and landscape (left/right, bottom).
    pub(crate) struct Device {
        pub(crate) name: &'static str,
        pub(crate) css: (f32, f32),
        pub(crate) dpr: f32,
        pub(crate) portrait_inset: (f32, f32),
        pub(crate) landscape_inset: (f32, f32),
    }

    pub(crate) const DEVICES: [Device; 5] = [
        Device {
            name: "iPhone SE",
            css: (375.0, 667.0),
            dpr: 2.0,
            portrait_inset: (20.0, 0.0),
            landscape_inset: (0.0, 0.0),
        },
        Device {
            name: "iPhone 15 Pro",
            css: (393.0, 852.0),
            dpr: 3.0,
            portrait_inset: (59.0, 34.0),
            landscape_inset: (59.0, 21.0),
        },
        Device {
            name: "iPhone 16 Pro Max",
            css: (440.0, 956.0),
            dpr: 3.0,
            portrait_inset: (62.0, 34.0),
            landscape_inset: (62.0, 21.0),
        },
        Device {
            name: "Pixel 7",
            css: (412.0, 915.0),
            dpr: 2.625,
            portrait_inset: (24.0, 0.0),
            landscape_inset: (24.0, 0.0),
        },
        Device {
            name: "iPad",
            css: (820.0, 1180.0),
            dpr: 2.0,
            portrait_inset: (24.0, 20.0),
            landscape_inset: (0.0, 20.0),
        },
    ];

    pub(crate) fn params(device: &Device, landscape: bool, analog: bool) -> LayoutParams {
        let (w, h) = device.css;
        let (size, safe) = if landscape {
            let (side, bottom) = device.landscape_inset;
            (
                vec2(h, w),
                Insets {
                    top: 0.0,
                    right: side,
                    bottom,
                    left: side,
                },
            )
        } else {
            let (top, bottom) = device.portrait_inset;
            (
                vec2(w, h),
                Insets {
                    top,
                    right: 0.0,
                    bottom,
                    left: 0.0,
                },
            )
        };
        let d = device.dpr;
        LayoutParams {
            size: size * d,
            scale: d,
            safe: Insets {
                top: safe.top * d,
                right: safe.right * d,
                bottom: safe.bottom * d,
                left: safe.left * d,
            },
            analog,
            size_pct: 100,
            left_handed: false,
            fullscreen_button: true,
            menu_open: false,
        }
    }

    fn safe_rect(p: &LayoutParams) -> Rect {
        Rect::from_min_max(
            pos2(p.safe.left, p.safe.top),
            pos2(p.size.x - p.safe.right, p.size.y - p.safe.bottom),
        )
    }

    fn hit_rects(layout: &TouchLayout) -> Vec<(String, Rect)> {
        let mut out = Vec::new();
        for b in &layout.buttons {
            out.push((format!("button {:#x}", b.mask), b.hit.bounds()));
        }
        for s in &layout.sticks {
            out.push((format!("stick {:?}", s.side), s.zone));
        }
        out.push((
            "dpad".to_string(),
            Rect::from_center_size(layout.dpad.center, Vec2::splat(layout.dpad.zone * 2.0)),
        ));
        for ui in &layout.ui {
            out.push((format!("ui {:?}", ui.id), ui.hit));
        }
        out
    }

    #[test]
    fn every_control_stays_inside_the_safe_area_on_every_device() {
        for device in &DEVICES {
            for landscape in [false, true] {
                for analog in [false, true] {
                    for left_handed in [false, true] {
                        let mut p = params(device, landscape, analog);
                        p.left_handed = left_handed;
                        let layout = TouchLayout::compute(&p);
                        let safe = safe_rect(&p).expand(1.0);
                        for (name, rect) in hit_rects(&layout) {
                            assert!(
                                safe.contains_rect(rect),
                                "{} {} analog={analog} lh={left_handed}: {name} {rect:?} outside {safe:?}",
                                device.name,
                                if landscape { "landscape" } else { "portrait" },
                            );
                        }
                    }
                }
            }
        }
    }

    /// The text of a pill ("SELECT", "START", "L1") sits inside its outline with room
    /// to spare on every device: it is drawn at `touch_overlay::PILL_TEXT` of the pill's
    /// height, so the pill must be at least as wide as the text plus a margin of
    /// 15% of its width on each side.
    #[test]
    fn pill_labels_fit_inside_their_pills_on_every_device() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        ctx.begin_pass(egui::RawInput::default());
        for device in &DEVICES {
            for landscape in [false, true] {
                for analog in [false, true] {
                    let layout = TouchLayout::compute(&params(device, landscape, analog));
                    for b in &layout.buttons {
                        let Glyph::Label(text) = b.glyph else {
                            continue;
                        };
                        let pill = b.draw.bounds();
                        let width = ctx.fonts(|f| {
                            f.layout_no_wrap(
                                text.to_string(),
                                egui::FontId::proportional(
                                    pill.height() * crate::ui::touch_overlay::PILL_TEXT,
                                ),
                                egui::Color32::WHITE,
                            )
                            .size()
                            .x
                        });
                        assert!(
                            width <= pill.width() * 0.7,
                            "{} {}: {text} is {width:.1} px wide in a {:.1} px pill",
                            device.name,
                            if landscape { "landscape" } else { "portrait" },
                            pill.width(),
                        );
                    }
                }
            }
        }
        let _ = ctx.end_pass();
    }

    /// What is drawn (not just what is touchable) stays inside the safe area, with
    /// the margin the layout promises: nothing sits flush against, or past, an edge.
    #[test]
    fn every_drawn_control_keeps_clear_of_the_safe_area_edges() {
        for device in &DEVICES {
            for landscape in [false, true] {
                for analog in [false, true] {
                    let p = params(device, landscape, analog);
                    let layout = TouchLayout::compute(&p);
                    let inner = safe_rect(&p).shrink(2.0 * device.dpr);
                    let mut drawn: Vec<(String, Rect)> = layout
                        .buttons
                        .iter()
                        .map(|b| (format!("button {:#x}", b.mask), b.draw.bounds()))
                        .collect();
                    drawn.extend(layout.ui.iter().map(|u| (format!("ui {:?}", u.id), u.draw)));
                    for (name, rect) in drawn {
                        assert!(
                            inner.contains_rect(rect),
                            "{} {}: {name} {rect:?} not inside {inner:?}",
                            device.name,
                            if landscape { "landscape" } else { "portrait" },
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn touch_areas_of_different_controls_never_overlap() {
        for device in &DEVICES {
            for landscape in [false, true] {
                for analog in [false, true] {
                    let layout = TouchLayout::compute(&params(device, landscape, analog));
                    // Face buttons overlap each other on purpose.
                    let rects: Vec<(String, Rect, bool)> = hit_rects(&layout)
                        .into_iter()
                        .map(|(n, r)| {
                            let face = n.starts_with("button")
                                && layout.buttons.iter().any(|b| {
                                    format!("button {:#x}", b.mask) == n
                                        && matches!(b.hit, Region::Circle { .. })
                                });
                            (n, r, face)
                        })
                        .collect();
                    for (i, (na, ra, fa)) in rects.iter().enumerate() {
                        for (nb, rb, fb) in &rects[i + 1..] {
                            if *fa && *fb {
                                continue;
                            }
                            assert!(
                                !ra.intersects(*rb),
                                "{} {} analog={analog}: {na} overlaps {nb}",
                                device.name,
                                if landscape { "landscape" } else { "portrait" },
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn hit_areas_are_thumb_sized_and_larger_than_the_drawn_shape() {
        for device in &DEVICES {
            for landscape in [false, true] {
                let layout = TouchLayout::compute(&params(device, landscape, true));
                let min_side = 34.0 * device.dpr;
                for b in &layout.buttons {
                    let (draw, hit) = (b.draw.bounds(), b.hit.bounds());
                    assert!(hit.contains_rect(draw.shrink(0.5)), "{:?}", b.glyph);
                    assert!(
                        hit.width().min(hit.height()) >= min_side,
                        "{} {:?}",
                        device.name,
                        b.glyph
                    );
                }
                for ui in &layout.ui {
                    assert!(ui.hit.height() >= min_side && ui.hit.width() >= min_side);
                }
            }
        }
    }

    #[test]
    fn portrait_keeps_the_game_a_full_width_four_by_three() {
        for device in &DEVICES {
            let p = params(device, false, true);
            let layout = TouchLayout::compute(&p);
            let game_h = p.size.y - p.safe.top - layout.reserved_bottom;
            assert!(
                game_h >= 0.75 * (p.size.x - p.safe.left - p.safe.right) - 1.0,
                "{}: game gets {game_h}",
                device.name
            );
            assert!(layout.reserved_bottom > 0.0);
        }
        let landscape = TouchLayout::compute(&params(&DEVICES[1], true, true));
        assert_eq!(landscape.reserved_bottom, 0.0);
    }

    #[test]
    fn sticks_show_only_for_an_analog_guest() {
        let digital = TouchLayout::compute(&params(&DEVICES[1], true, false));
        assert!(digital.sticks.is_empty());
        let analog = TouchLayout::compute(&params(&DEVICES[1], true, true));
        assert_eq!(analog.sticks.len(), 2);
        assert!(
            analog.stick(StickSide::Left).unwrap().rest.x
                < analog.stick(StickSide::Right).unwrap().rest.x
        );
    }

    #[test]
    fn left_handed_swaps_the_clusters() {
        let mut p = params(&DEVICES[1], true, true);
        let right_handed = TouchLayout::compute(&p);
        p.left_handed = true;
        let left_handed = TouchLayout::compute(&p);
        assert!(right_handed.dpad.center.x < p.size.x / 2.0);
        assert!(left_handed.dpad.center.x > p.size.x / 2.0);
        let cross = |l: &TouchLayout| {
            l.buttons
                .iter()
                .find(|b| b.mask == button::CROSS)
                .unwrap()
                .draw
                .center()
        };
        assert!(cross(&right_handed).x > p.size.x / 2.0);
        assert!(cross(&left_handed).x < p.size.x / 2.0);
    }

    #[test]
    fn the_size_preference_scales_the_controls_but_never_off_screen() {
        let mut p = params(&DEVICES[0], true, true);
        let normal = TouchLayout::compute(&p).unit;
        p.size_pct = 70;
        assert!(TouchLayout::compute(&p).unit < normal);
        p.size_pct = 150;
        let big = TouchLayout::compute(&p);
        let safe = safe_rect(&p).expand(1.0);
        for (name, rect) in hit_rects(&big) {
            assert!(safe.contains_rect(rect), "{name} {rect:?}");
        }
    }
}
