//! The launch sequence: the case turns to you and opens, the disc lifts out,
//! the case recedes into the fog, the disc eases into the tray, the tray
//! closes and the disc spins up, then the game boots. One continuous eased
//! timeline with overlapping phases; the numbers are the design page's.

use egui::epaint::{Mesh, Vertex};
use egui::{Align2, Color32, FontId, Painter, Pos2, Shape, Stroke, TextureId, Vec2};

use super::scene::{fade, mix, Camera, CaseDraw, CoverKind, CYAN, V3, VOID};
use super::{tokens, Layout, LayoutKind};
use crate::ui::menu::MenuAction;

/// Seconds from the press to the boot.
pub(super) const DURATION: f32 = tokens::LAUNCH;

/// A launch in progress.
pub(super) struct Launch {
    pub t: f32,
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

/// Where everything is at time `t`.
struct Pose {
    lid: f32,
    yaw: f32,
    recede: f32,
    disc_y: f32,
    disc_z: f32,
    disc_rx: f32,
    disc_ry: f32,
    echo: [f32; 3],
    echo_alpha: f32,
    tray: f32,
    led: f32,
    spin: f32,
    flash: f32,
    label: &'static str,
}

fn disc_at(t: f32, yaw: f32) -> (f32, f32, f32, f32) {
    let l = ease_in_out(span(t, 0.55, 1.34));
    let m = ease_in_out(span(t, 1.34, 2.32));
    let y = -40.0 * l + 210.0 * m;
    let z = -4.0 + 124.0 * l - 54.0 * m + 46.0 * (std::f32::consts::PI * m).sin();
    let rx = 60.0_f32.to_radians() * m;
    (y, z, rx, yaw * (1.0 - l) * (1.0 - m))
}

fn pose(t: f32) -> Pose {
    let lid = 108.0_f32.to_radians() * ease_out(span(t, 0.08, 0.83));
    let turn = ease_out(span(t, 0.0, 0.71));
    let yaw = -0.40 + 0.40 * turn;
    let recede = ease_in_out(span(t, 0.79, 1.89));
    let (disc_y, disc_z, disc_rx, disc_ry) = disc_at(t, yaw);
    let echo = [1.0, 2.0, 3.0].map(|k| disc_at(t - 0.055 * k, yaw).0);
    let echo_alpha = 0.42 * span(t, 0.6, 0.8) * (1.0 - span(t, 2.3, 2.6));
    let s = ease_in_out(span(t, 2.16, 2.76));
    let led = span(t, 2.16, 2.92);
    // Spin-up: angle 720 q squared, then at the speed it reached.
    let q = span(t, 2.28, 3.94);
    let full = std::f32::consts::TAU * 2.0;
    let spin = if t < 3.94 {
        full * q * q
    } else {
        full + (t - 3.94) * full * 2.0 / (3.94 - 2.28)
    };
    let label = if t < 0.83 {
        "The case opens"
    } else if t < 1.34 {
        "The disc comes out"
    } else if t < 2.32 {
        "The disc goes into the tray"
    } else {
        "Spinning up"
    };
    Pose {
        lid,
        yaw,
        recede,
        disc_y,
        disc_z,
        disc_rx,
        disc_ry,
        echo,
        echo_alpha,
        tray: s,
        led,
        spin,
        flash: span(t, 4.02, 4.49),
        label,
    }
}

/// A disc as a low-poly circle in space. `tex` is the label or the cover;
/// without one the wedges alternate in teal.
#[allow(clippy::too_many_arguments)]
fn disc(
    p: &Painter,
    cam: &Camera,
    center: V3,
    r: f32,
    rx: f32,
    ry: f32,
    spin: f32,
    tex: Option<TextureId>,
    alpha: f32,
    ghost: bool,
) {
    const N: usize = 16;
    let rim: Vec<(Pos2, f32)> = (0..N)
        .map(|i| {
            let a = i as f32 / N as f32 * std::f32::consts::TAU;
            let v = V3::new(r * a.cos(), r * a.sin(), 0.0)
                .rot_x(rx)
                .rot_y(ry)
                .add(center);
            (cam.project(v), a)
        })
        .collect();
    let outline = Stroke::new(1.2, fade(CYAN, 0.9 * alpha));
    if ghost {
        p.add(Shape::closed_line(
            rim.iter().map(|r| r.0).collect(),
            outline,
        ));
        return;
    }
    let mut mesh = match tex {
        Some(t) => Mesh::with_texture(t),
        None => Mesh::default(),
    };
    let white = egui::epaint::WHITE_UV;
    let c = cam.project(center);
    let wedge_a = Color32::from_rgb(23, 50, 61);
    let wedge_b = Color32::from_rgb(34, 90, 108);
    let shift = (spin / std::f32::consts::TAU * N as f32) as usize;
    mesh.vertices.push(Vertex {
        pos: c,
        uv: if tex.is_some() {
            Pos2::new(0.5, 0.5)
        } else {
            white
        },
        color: fade(Color32::WHITE, alpha),
    });
    for (i, (pos, a)) in rim.iter().enumerate() {
        let uv = if tex.is_some() {
            Pos2::new(0.5 + 0.5 * (a + spin).cos(), 0.5 + 0.5 * (a + spin).sin())
        } else {
            white
        };
        let color = if tex.is_some() {
            fade(Color32::WHITE, alpha)
        } else {
            fade(
                if (i + shift).is_multiple_of(2) {
                    wedge_a
                } else {
                    wedge_b
                },
                alpha,
            )
        };
        mesh.vertices.push(Vertex {
            pos: *pos,
            uv,
            color,
        });
    }
    for i in 0..N {
        let j = (i + 1) % N;
        // A wedge per rim edge, so the coloured version shows spin as steps.
        if tex.is_none() {
            let base = mesh.vertices.len() as u32;
            let col = mesh.vertices[1 + i].color;
            mesh.vertices.push(Vertex {
                pos: c,
                uv: white,
                color: col,
            });
            mesh.vertices.push(Vertex {
                pos: rim[i].0,
                uv: white,
                color: col,
            });
            mesh.vertices.push(Vertex {
                pos: rim[j].0,
                uv: white,
                color: col,
            });
            mesh.indices.extend([base, base + 1, base + 2]);
        } else {
            mesh.indices.extend([0, 1 + i as u32, 1 + j as u32]);
        }
    }
    p.add(Shape::mesh(mesh));
    p.add(Shape::closed_line(
        rim.iter().map(|r| r.0).collect(),
        outline,
    ));
    // The hub hole.
    let hole: Vec<Pos2> = (0..N)
        .map(|i| {
            let a = i as f32 / N as f32 * std::f32::consts::TAU;
            cam.project(
                V3::new(r * 0.11 * a.cos(), r * 0.11 * a.sin(), 0.0)
                    .rot_x(rx)
                    .rot_y(ry)
                    .add(center),
            )
        })
        .collect();
    p.add(Shape::convex_polygon(hole, fade(VOID, alpha), Stroke::NONE));
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
    let t = l.t.clamp(0.0, DURATION);
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
    // The tray rises into place under the disc and slides in.
    let tray_in = ease_in_out(span(t, 0.9, 1.9));
    let tray_c = V3::new(
        c0.x,
        c0.y + (236.0 - 70.0 * ps.tray + 60.0 * (1.0 - tray_in)) * su,
        66.0 * su,
    );
    if tray_in > 0.0 {
        let a = alpha * tray_in;
        let (tw, td) = (190.0 * su, 120.0 * su);
        let corners = [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)]
            .map(|(sx, sz)| cam.project(V3::new(tray_c.x + sx * tw, tray_c.y, tray_c.z + sz * td)));
        p.add(Shape::convex_polygon(
            corners.to_vec(),
            fade(Color32::from_rgb(10, 24, 32), a),
            Stroke::new(1.2, fade(CYAN, 0.55 * a)),
        ));
        // The recess the disc sits in.
        let recess: Vec<Pos2> = (0..28)
            .map(|i| {
                let ang = i as f32 / 28.0 * std::f32::consts::TAU;
                cam.project(V3::new(
                    tray_c.x + 84.0 * su * ang.cos(),
                    tray_c.y - 1.0,
                    tray_c.z + 84.0 * su * ang.sin(),
                ))
            })
            .collect();
        p.add(Shape::convex_polygon(
            recess,
            fade(Color32::from_rgb(4, 10, 15), a),
            Stroke::new(1.0, fade(CYAN, 0.4 * a)),
        ));
        // The LED at the front edge.
        let led = cam.project(V3::new(tray_c.x + tw * 0.8, tray_c.y, tray_c.z + td * 0.8));
        p.circle_filled(led, 3.0 * su, fade(tokens::OK, a * ps.led));
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
        };
        super::scene::draw_case(p, &cam, &draw);
    }
    // The disc and its echoes.
    let r = 188.0 * 0.44 * su * 1.3;
    let tex = label_tex.filter(|_| l.kind == CoverKind::Demo).or(art);
    if ps.echo_alpha > 0.01 {
        for (k, y) in ps.echo.iter().enumerate() {
            let a = ps.echo_alpha * (1.0 - (k as f32 + 1.0) / 4.0);
            disc(
                p,
                &cam,
                V3::new(c0.x, c0.y + y * su, 60.0 * su),
                r,
                ps.disc_rx,
                ps.disc_ry,
                0.0,
                None,
                alpha * a,
                true,
            );
        }
    }
    // The disc is inside the case until the cover has swung clear.
    if ps.lid > 0.3 || t > 1.0 {
        disc(
            p,
            &cam,
            V3::new(c0.x, c0.y + ps.disc_y * su, ps.disc_z * su),
            r,
            ps.disc_rx,
            ps.disc_ry,
            ps.spin,
            tex,
            alpha,
            false,
        );
    }
    // The caption.
    let cap = lay.safe.bottom() - 40.0 * lay.u;
    p.text(
        Pos2::new(w * 0.5, cap),
        Align2::CENTER_CENTER,
        ps.label,
        FontId::proportional(13.0 * lay.u),
        fade(mix(tokens::MUTED, CYAN, 0.2), alpha),
    );
    // The flash into the boot.
    if ps.flash > 0.0 {
        p.rect_filled(
            lay.full,
            0.0,
            Color32::from_white_alpha((255.0 * ps.flash * alpha) as u8),
        );
    }
    let _ = Vec2::ZERO;
}

/// Seconds before an input can skip the sequence (so the press that started it
/// does not also skip it).
pub(super) const SKIP_AFTER: f32 = 0.25;
