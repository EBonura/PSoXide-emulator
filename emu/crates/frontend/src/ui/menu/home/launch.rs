//! The launch sequence: the case turns to you and opens, the disc lifts out,
//! the case recedes into the fog, the disc settles flat into the tray, the
//! tray closes and the disc spins up, then the game boots. One continuous
//! eased timeline with overlapping phases, about two and a quarter seconds.
//!
//! The tray and the disc share one frame: the tray lies tilted toward you, and
//! the disc ends in that same plane (see [`TRAY_TILT`]), so it seats exactly
//! in the recess instead of hovering at a different angle.

use egui::epaint::{Mesh, Vertex};
use egui::{Align2, Color32, FontId, Painter, Pos2, Shape, Stroke, TextureId, Vec2};

use super::scene::{fade, mix, Camera, CaseDraw, CoverKind, CYAN, RING_SEGMENTS, V3, VOID};
use super::{tokens, Layout, LayoutKind};
use crate::ui::menu::MenuAction;

/// Seconds from the press to the boot.
pub(super) const DURATION: f32 = tokens::LAUNCH;

/// A launch in progress.
pub(super) struct Launch {
    /// Seconds into the sequence. It runs on past [`DURATION`] while the game
    /// is still loading, so the disc keeps spinning instead of freezing.
    pub t: f32,
    /// An input asked to skip: the sequence plays out at [`SKIP_SPEED`] times
    /// the speed, still on its eased curves, instead of jumping.
    pub skipping: bool,
    pub action: MenuAction,
    pub kind: CoverKind,
    pub title: String,
    pub cover_key: Option<String>,
    /// A preview holds one moment instead of playing.
    pub frozen: bool,
}

fn clamp01(x: f32) -> f32 {
    x.clamp(0.0, 1.0)
}

/// Progress of `t` between `a` and `b`.
fn span(t: f32, a: f32, b: f32) -> f32 {
    clamp01((t - a) / (b - a))
}

fn ease_in_out(x: f32) -> f32 {
    tokens::ease_in_out(x)
}

fn ease_out(x: f32) -> f32 {
    tokens::ease_out_expo(x)
}

// ---- The timeline (seconds) --------------------------------------------------
//
// Phases overlap: each starts before the one before it has finished, so the
// motion never stops between them.

/// The case turns to face you.
const TURN: (f32, f32) = (0.0, 0.36);
/// The cover swings open.
const LID: (f32, f32) = (0.04, 0.50);
/// The disc lifts out of the case.
const LIFT: (f32, f32) = (0.24, 0.72);
/// The case recedes into the fog.
const RECEDE: (f32, f32) = (0.40, 0.98);
/// The tray rises into place under the disc.
const TRAY_RISE: (f32, f32) = (0.46, 0.98);
/// The disc settles into the tray.
const SETTLE: (f32, f32) = (0.74, 1.34);
/// The tray closes (slides in).
const CLOSE: (f32, f32) = (1.10, 1.44);
/// The disc spins up, then holds that speed.
const SPIN_UP: (f32, f32) = (1.16, 1.92);
/// The glow at the tray that hands over to the boot: up, peak, gone by the end.
const FLASH: (f32, f32) = (1.88, 2.1);

/// How far the tray lies back toward the viewer (radians): a flat tray seen
/// from the front would be a sliver, so the tray and the disc in it are tilted
/// together. The disc's final plane is exactly the tray's.
const TRAY_TILT: f32 = 0.66;
/// The disc's radius at design scale, and the recess's.
const DISC_R: f32 = 108.0;
const RECESS_R: f32 = DISC_R * 1.05;
/// Half-width and half-depth of the tray.
const TRAY_HALF: (f32, f32) = (206.0, 138.0);
/// How far above the tray's surface the disc rests (design units).
const DISC_LIFT: f32 = 1.5;

/// Seconds the launch is asked to play at this skip speed.
pub(super) const SKIP_SPEED: f32 = 6.0;

/// Where everything is at time `t`.
struct Pose {
    lid: f32,
    yaw: f32,
    recede: f32,
    /// 0 while the disc floats out of the case, 1 once it lies in the tray.
    settle: f32,
    /// How far the disc has lifted out of the case.
    lift: f32,
    /// How far the tray has risen into place.
    tray_in: f32,
    echo: [f32; 3],
    echo_alpha: f32,
    tray: f32,
    led: f32,
    flash: f32,
    caption: f32,
}

/// The height (design units, relative to the case centre) of the tray's
/// centre and how far it has slid in. Everything that sits in the tray reads
/// this, so nothing can drift from it.
fn tray_y(tray: f32, tray_in: f32) -> f32 {
    236.0 - 70.0 * tray + 60.0 * (1.0 - tray_in)
}

fn lift_of(t: f32) -> f32 {
    ease_in_out(span(t, LIFT.0, LIFT.1))
}

fn settle_of(t: f32) -> f32 {
    ease_in_out(span(t, SETTLE.0, SETTLE.1))
}

/// The disc's height while it floats, before it heads for the tray (for the
/// trailing echoes).
fn float_y(t: f32) -> f32 {
    let tray = ease_in_out(span(t, CLOSE.0, CLOSE.1));
    let tray_in = ease_in_out(span(t, TRAY_RISE.0, TRAY_RISE.1));
    let m = settle_of(t);
    -40.0 * lift_of(t)
        + (tray_y(tray, tray_in) - DISC_LIFT * TRAY_TILT.cos() + 40.0 * lift_of(t)) * m
}

/// The boot flash: a rise to a bit over half strength at the tray, then back to nothing by the
/// hand-off, so the game never starts under a lit screen. A pulse, not a wash (the screen's mean
/// brightness stays under a third).
fn flash_at(t: f32) -> f32 {
    const STRENGTH: f32 = 0.4;
    STRENGTH * span(t, FLASH.0, FLASH.1) * (1.0 - span(t, FLASH.1, DURATION))
}

fn pose(t: f32) -> Pose {
    let lid = 108.0_f32.to_radians() * ease_out(span(t, LID.0, LID.1));
    let turn = ease_out(span(t, TURN.0, TURN.1));
    let yaw = -0.40 + 0.40 * turn;
    let recede = ease_in_out(span(t, RECEDE.0, RECEDE.1));
    let echo = [1.0, 2.0, 3.0].map(|k| float_y(t - 0.04 * k));
    let echo_alpha = 0.42 * span(t, 0.30, 0.46) * (1.0 - span(t, 1.18, 1.36));
    Pose {
        lid,
        yaw,
        recede,
        settle: settle_of(t),
        lift: lift_of(t),
        tray_in: ease_in_out(span(t, TRAY_RISE.0, TRAY_RISE.1)),
        echo,
        echo_alpha,
        tray: ease_in_out(span(t, CLOSE.0, CLOSE.1)),
        led: span(t, CLOSE.0, CLOSE.1 + 0.3),
        flash: flash_at(t),
        caption: span(t, 0.30, 0.62),
    }
}

/// The disc's spin angle at `t` (not clamped to the sequence, so it goes on
/// turning while the game loads): accelerating, then at the speed it reached.
fn spin_at(t: f32) -> f32 {
    let full = std::f32::consts::TAU * 2.0;
    let len = SPIN_UP.1 - SPIN_UP.0;
    if t < SPIN_UP.0 {
        0.0
    } else if t < SPIN_UP.1 {
        let q = (t - SPIN_UP.0) / len;
        full * q * q
    } else {
        full + (t - SPIN_UP.1) * full * 2.0 / len
    }
}

/// A disc as a round plate in space. `tex` is the label or the cover; without
/// one the wedges alternate in teal and the label and hub rings are drawn.
/// `tilt` is the angle about the horizontal axis, `yaw` about the vertical.
#[allow(clippy::too_many_arguments)]
fn disc(
    p: &Painter,
    cam: &Camera,
    center: V3,
    r: f32,
    tilt: f32,
    yaw: f32,
    spin: f32,
    tex: Option<TextureId>,
    alpha: f32,
    ghost: bool,
) {
    const N: usize = RING_SEGMENTS;
    let at = |radius: f32, a: f32| {
        cam.project(
            V3::new(radius * a.cos(), radius * a.sin(), 0.0)
                .rot_x(tilt)
                .rot_y(yaw)
                .add(center),
        )
    };
    let angle = |i: usize| i as f32 / N as f32 * std::f32::consts::TAU;
    let rim: Vec<Pos2> = (0..N).map(|i| at(r, angle(i))).collect();
    let outline = Stroke::new(1.2, fade(CYAN, 0.9 * alpha));
    if ghost {
        p.add(Shape::closed_line(rim, outline));
        return;
    }
    let c = cam.project(center);
    let white = egui::epaint::WHITE_UV;
    let mut mesh = match tex {
        Some(t) => Mesh::with_texture(t),
        None => Mesh::default(),
    };
    if tex.is_some() {
        // One fan: the picture turns with the spin.
        mesh.vertices.push(Vertex {
            pos: c,
            uv: Pos2::new(0.5, 0.5),
            color: fade(Color32::WHITE, alpha),
        });
        for (i, pos) in rim.iter().enumerate() {
            let a = angle(i) + spin;
            mesh.vertices.push(Vertex {
                pos: *pos,
                uv: Pos2::new(0.5 + 0.5 * a.cos(), 0.5 + 0.5 * a.sin()),
                color: fade(Color32::WHITE, alpha),
            });
        }
        for i in 0..N {
            mesh.indices
                .extend([0, 1 + i as u32, 1 + ((i + 1) % N) as u32]);
        }
    } else {
        // Wedges of two teals; the spin turns them in steps of one wedge group.
        const GROUPS: usize = 16;
        let wedge_a = Color32::from_rgb(23, 50, 61);
        let wedge_b = Color32::from_rgb(34, 90, 108);
        let shift = (spin / std::f32::consts::TAU * GROUPS as f32) as usize;
        for i in 0..N {
            let j = (i + 1) % N;
            let group = i * GROUPS / N;
            let color = fade(
                if (group + shift).is_multiple_of(2) {
                    wedge_a
                } else {
                    wedge_b
                },
                alpha,
            );
            let base = mesh.vertices.len() as u32;
            for pos in [c, rim[i], rim[j]] {
                mesh.vertices.push(Vertex {
                    pos,
                    uv: white,
                    color,
                });
            }
            mesh.indices.extend([base, base + 1, base + 2]);
        }
    }
    p.add(Shape::mesh(mesh));
    p.add(Shape::closed_line(rim, outline));
    if tex.is_none() {
        // The label ring.
        let ring: Vec<Pos2> = (0..N).map(|i| at(r * 0.62, angle(i))).collect();
        p.add(Shape::closed_line(
            ring,
            Stroke::new(1.0, fade(CYAN, 0.45 * alpha)),
        ));
    }
    // The hub hole.
    let hole: Vec<Pos2> = (0..N / 2)
        .map(|i| at(r * 0.11, i as f32 / (N / 2) as f32 * std::f32::consts::TAU))
        .collect();
    p.add(Shape::convex_polygon(hole, fade(VOID, alpha), Stroke::NONE));
}

/// A glow over the screen: the design's radial gradient (bright cyan at the tray, teal, then the
/// void colour at the corners), never a flat wash. `k` is the strength, 0 to 1.
fn draw_flash(p: &Painter, full: egui::Rect, k: f32) {
    const SEGMENTS: usize = 48;
    let centre = Pos2::new(full.center().x, full.top() + full.height() * 0.56);
    // Farthest-corner, as the design's gradient.
    let reach = [
        full.left_top(),
        full.right_top(),
        full.left_bottom(),
        full.right_bottom(),
    ]
    .iter()
    .map(|c| (*c - centre).length())
    .fold(0.0_f32, f32::max);
    let k = k.clamp(0.0, 1.0);
    let colour = |rgb: [u8; 3]| {
        let s = |v: u8| (f32::from(v) * k).round() as u8;
        Color32::from_rgba_premultiplied(s(rgb[0]), s(rgb[1]), s(rgb[2]), (255.0 * k).round() as u8)
    };
    let stops = [
        (0.0, colour([201, 244, 255])),
        (0.6, colour([27, 106, 128])),
        (1.01, colour([3, 6, 10])),
    ];
    let white = egui::epaint::WHITE_UV;
    let mut mesh = Mesh::default();
    mesh.vertices.push(Vertex {
        pos: centre,
        uv: white,
        color: stops[0].1,
    });
    for (radius, color) in &stops[1..] {
        for i in 0..SEGMENTS {
            let a = i as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
            mesh.vertices.push(Vertex {
                pos: centre + Vec2::new(a.cos(), a.sin()) * reach * *radius,
                uv: white,
                color: *color,
            });
        }
    }
    let ring = |ring: usize, i: usize| (1 + ring * SEGMENTS + i % SEGMENTS) as u32;
    for i in 0..SEGMENTS {
        mesh.indices.extend([0, ring(0, i), ring(0, i + 1)]);
        mesh.indices
            .extend([ring(0, i), ring(0, i + 1), ring(1, i)]);
        mesh.indices
            .extend([ring(0, i + 1), ring(1, i + 1), ring(1, i)]);
    }
    p.add(Shape::mesh(mesh));
}

/// Draw the whole sequence for `l` over the void.
pub(super) fn draw(
    p: &Painter,
    lay: &Layout,
    l: &Launch,
    art: Option<TextureId>,
    label_tex: Option<TextureId>,
    alpha: f32,
) {
    let raw_t = l.t.max(0.0);
    let t = raw_t.min(DURATION);
    let ps = pose(t);
    let (w, h) = (lay.full.width(), lay.full.height());
    let su = lay.u
        * match lay.kind {
            LayoutKind::Desktop => 1.0,
            LayoutKind::Portrait => 0.66,
            LayoutKind::Landscape => 0.72,
        };
    let cam = Camera {
        origin: Pos2::new(w * 0.5, h * 0.30),
        focal: 1000.0 * lay.u,
    };
    let c0 = V3::new(w * 0.5, h * 0.37, 0.0);
    // The tray's frame: its centre, and the tilt that lays it back toward you.
    // The tray, its recess and the disc in it are all placed through this one
    // transform.
    let tray_origin = V3::new(c0.x, c0.y + tray_y(ps.tray, ps.tray_in) * su, 66.0 * su);
    let to_world = |local: V3| local.rot_x(-TRAY_TILT).add(tray_origin);
    let on_tray = |x: f32, z: f32| cam.project(to_world(V3::new(x * su, 0.0, z * su)));
    let circle_on_tray = |radius: f32, at: (f32, f32)| -> Vec<Pos2> {
        (0..RING_SEGMENTS)
            .map(|i| {
                let a = i as f32 / RING_SEGMENTS as f32 * std::f32::consts::TAU;
                on_tray(at.0 + radius * a.cos(), at.1 + radius * a.sin())
            })
            .collect()
    };
    if ps.tray_in > 0.0 {
        let a = alpha * ps.tray_in;
        let (tw, td) = TRAY_HALF;
        let corners = [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)]
            .map(|(sx, sz)| on_tray(sx * tw, sz * td));
        p.add(Shape::convex_polygon(
            corners.to_vec(),
            fade(Color32::from_rgb(10, 24, 32), a),
            Stroke::new(1.2, fade(CYAN, 0.55 * a)),
        ));
        // The recess the disc sits in, and a dimmer ring inside it.
        p.add(Shape::convex_polygon(
            circle_on_tray(RECESS_R, (0.0, 0.0)),
            fade(Color32::from_rgb(4, 10, 15), a),
            Stroke::new(1.0, fade(CYAN, 0.4 * a)),
        ));
        p.add(Shape::closed_line(
            circle_on_tray(RECESS_R * 0.28, (0.0, 0.0)),
            Stroke::new(1.0, fade(CYAN, 0.25 * a)),
        ));
        // The LED at the front edge.
        p.circle_filled(
            on_tray(tw * 0.8, td * 0.8),
            3.0 * su,
            fade(tokens::OK, a * ps.led),
        );
    }
    // The case: opens, then recedes.
    let case_alpha = alpha * (1.0 - ps.recede);
    if case_alpha > 0.01 {
        let center = V3::new(
            c0.x - 190.0 * su * ps.recede,
            c0.y + 10.0 * su * ps.recede,
            -520.0 * su * ps.recede,
        );
        let draw = CaseDraw {
            kind: l.kind,
            title: &l.title,
            art,
            size: V3::new(210.0 * su, 188.0 * su, 16.0 * su),
            center,
            yaw: ps.yaw,
            pitch: -0.08,
            lid: ps.lid,
            scale: 1.3,
            opacity: case_alpha,
            seed: 0,
            fx: super::scene::CaseFx::default(),
        };
        super::scene::draw_case(p, &cam, &draw);
    }
    // The disc and its echoes. It floats out of the case, then lowers into the
    // tray: its path blends from the float to the seat, and its angle from
    // facing you to the tray's own plane, so it ends exactly in the recess.
    let r = DISC_R * su;
    let tex = label_tex.filter(|_| l.kind == CoverKind::Demo).or(art);
    let flat = std::f32::consts::FRAC_PI_2 - TRAY_TILT;
    let disc_tilt = flat * ps.settle;
    let yaw = ps.yaw * (1.0 - ps.lift) * (1.0 - ps.settle);
    if ps.echo_alpha > 0.01 {
        for (k, y) in ps.echo.iter().enumerate() {
            let a = ps.echo_alpha * (1.0 - (k as f32 + 1.0) / 4.0);
            disc(
                p,
                &cam,
                V3::new(c0.x, c0.y + y * su, 60.0 * su),
                r,
                disc_tilt,
                yaw,
                0.0,
                None,
                alpha * a,
                true,
            );
        }
    }
    // The disc is inside the case until the cover has swung clear.
    if ps.lid > 0.3 || t > 1.0 {
        let floating = V3::new(
            c0.x,
            c0.y - 40.0 * su * ps.lift,
            (-4.0 + 124.0 * ps.lift) * su,
        );
        let seat = to_world(V3::new(0.0, -DISC_LIFT * su, 0.0));
        let m = ps.settle;
        let mut center = V3::new(
            floating.x + (seat.x - floating.x) * m,
            floating.y + (seat.y - floating.y) * m,
            floating.z + (seat.z - floating.z) * m,
        );
        // A little lift toward the viewer on the way down, so it clears the tray's lip.
        center.z += 46.0 * su * (std::f32::consts::PI * m).sin();
        if m > 0.05 {
            // Its shadow on the tray, darker as it comes down.
            p.add(Shape::convex_polygon(
                circle_on_tray(DISC_R * 0.98, (0.0, 0.0)),
                fade(Color32::BLACK, alpha * 0.5 * m * m),
                Stroke::NONE,
            ));
        }
        disc(
            p,
            &cam,
            center,
            r,
            disc_tilt,
            yaw,
            spin_at(raw_t),
            tex,
            alpha,
            false,
        );
    }
    // The title, quietly, once the case has opened.
    let cap = lay.safe.bottom() - 40.0 * lay.u;
    p.text(
        Pos2::new(w * 0.5, cap),
        Align2::CENTER_CENTER,
        &l.title,
        FontId::proportional(13.0 * lay.u),
        fade(mix(tokens::MUTED, CYAN, 0.2), alpha * ps.caption),
    );
    // The flash into the boot: a glow at the tray that fades out again by the hand-off.
    if ps.flash > 0.0 {
        draw_flash(p, lay.full, ps.flash * alpha);
    }
    let _ = Vec2::ZERO;
}

/// Seconds before an input can skip the sequence (so the press that started it
/// does not also skip it).
pub(super) const SKIP_AFTER: f32 = 0.25;
