//! The polygon void: a dark horizon, a quiet grid floor lost in dithered fog,
//! and low-poly jewel cases standing on it.
//!
//! Everything is painted with egui shapes. A case is a box projected through a
//! simple perspective camera; its front face carries the cover art, drawn
//! either as vector shapes mapped onto the face or as a texture.

use egui::epaint::{Mesh, Vertex};
use egui::{Color32, Painter, Pos2, Rect, Shape, Stroke, TextureId, Vec2};

use crate::theme::MENU_ACCENT;

/// The emulator's near-black blue-grey, deepest tone.
pub(super) const VOID: Color32 = Color32::from_rgb(4, 6, 11);
/// Teal glow at the horizon.
pub(super) const GLOW: Color32 = Color32::from_rgb(10, 37, 48);
/// Cyan accent (the X of the logo).
pub(super) const CYAN: Color32 = Color32::from_rgb(34, 200, 229);

/// A point in scene space: x right, y down, z toward the viewer (pixels).
#[derive(Clone, Copy, Debug)]
pub(super) struct V3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl V3 {
    pub(super) const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    fn rot_y(self, a: f32) -> Self {
        let (s, c) = a.sin_cos();
        Self::new(self.x * c + self.z * s, self.y, -self.x * s + self.z * c)
    }

    fn rot_x(self, a: f32) -> Self {
        let (s, c) = a.sin_cos();
        Self::new(self.x, self.y * c - self.z * s, self.y * s + self.z * c)
    }

    fn add(self, o: Self) -> Self {
        Self::new(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}

/// A perspective camera: the vanishing point and the focal distance.
#[derive(Clone, Copy, Debug)]
pub(super) struct Camera {
    pub origin: Pos2,
    pub focal: f32,
}

impl Camera {
    pub(super) fn project(&self, p: V3) -> Pos2 {
        let k = self.focal / (self.focal - p.z).max(1.0);
        Pos2::new(
            self.origin.x + (p.x - self.origin.x) * k,
            self.origin.y + (p.y - self.origin.y) * k,
        )
    }
}

/// Linear blend of two colours.
pub(super) fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Color32::from_rgba_unmultiplied(
        f(a.r(), b.r()),
        f(a.g(), b.g()),
        f(a.b(), b.b()),
        f(a.a(), b.a()),
    )
}

/// Fade a colour's alpha.
pub(super) fn fade(c: Color32, k: f32) -> Color32 {
    c.gamma_multiply(k.clamp(0.0, 1.0))
}

/// Ease-out cubic.
pub(super) fn ease_out(x: f32) -> f32 {
    1.0 - (1.0 - x.clamp(0.0, 1.0)).powi(3)
}

/// A vertical gradient strip.
fn gradient(painter: &Painter, rect: Rect, top: Color32, bottom: Color32) {
    let mut mesh = Mesh::default();
    let uv = egui::epaint::WHITE_UV;
    for (pos, color) in [
        (rect.left_top(), top),
        (rect.right_top(), top),
        (rect.right_bottom(), bottom),
        (rect.left_bottom(), bottom),
    ] {
        mesh.vertices.push(Vertex { pos, uv, color });
    }
    mesh.indices.extend([0, 1, 2, 0, 2, 3]);
    painter.add(Shape::mesh(mesh));
}

/// Where the horizon sits and how loud the floor is.
#[derive(Clone, Copy, Debug)]
pub(super) struct Backdrop {
    pub rect: Rect,
    /// Horizon height as a fraction of the screen.
    pub horizon: f32,
    /// Overall opacity (0 hides the scene, for cross-fades).
    pub alpha: f32,
    /// Seconds, for the slow floor drift.
    pub time: f32,
}

/// Sky, glow band, grid floor and fog. Quiet on purpose: the case reads first.
pub(super) fn draw_backdrop(painter: &Painter, dither: Option<TextureId>, b: Backdrop) {
    let r = b.rect;
    let hz = r.top() + r.height() * b.horizon;
    // Sky: void at the top, a teal glow settling toward the horizon.
    let split = r.top() + (hz - r.top()) * 0.55;
    gradient(
        painter,
        Rect::from_min_max(r.left_top(), Pos2::new(r.right(), split)),
        fade(VOID, b.alpha),
        fade(Color32::from_rgb(7, 16, 25), b.alpha),
    );
    gradient(
        painter,
        Rect::from_min_max(Pos2::new(r.left(), split), Pos2::new(r.right(), hz)),
        fade(Color32::from_rgb(7, 16, 25), b.alpha),
        fade(GLOW, b.alpha),
    );
    painter.rect_filled(
        Rect::from_min_max(Pos2::new(r.left(), hz), r.right_bottom()),
        0.0,
        fade(VOID, b.alpha),
    );
    // A faint banded glow above the horizon, scanline style.
    let band_top = hz - 70.0 * r.height() / 720.0;
    let mut y = band_top;
    while y < hz {
        let t = (y - band_top) / (hz - band_top);
        painter.rect_filled(
            Rect::from_min_size(Pos2::new(r.left(), y), Vec2::new(r.width(), 6.0)),
            0.0,
            fade(CYAN, b.alpha * 0.07 * t),
        );
        y += 9.0;
    }

    // Floor grid: world lines under a camera 1 unit above the plane.
    let eye = r.height() * 0.62;
    let focal = r.height() * 1.05;
    let cx = r.center().x;
    let floor_bottom = r.bottom();
    let line_alpha = 0.16 * b.alpha;
    // Depth lines (horizontal on screen): z from near to far.
    let drift = (b.time * 0.04).fract();
    for i in 0..22 {
        let z = 1.0 + (i as f32 + drift) * 0.5;
        let y = hz + focal * eye / (focal * z);
        if y > floor_bottom {
            continue;
        }
        let fog = 1.0 - ((y - hz) / (floor_bottom - hz)).clamp(0.0, 1.0).powf(0.5);
        let a = line_alpha * (1.0 - fog * 0.85);
        painter.line_segment(
            [Pos2::new(r.left(), y), Pos2::new(r.right(), y)],
            Stroke::new(1.0, fade(CYAN, a)),
        );
    }
    // Lines that run to the vanishing point.
    let spacing = r.height() * 0.5;
    for k in -14..=14 {
        let x_world = k as f32 * spacing;
        let near = Pos2::new(cx + x_world * 1.6, floor_bottom);
        let far = Pos2::new(cx + x_world * 0.02, hz);
        let steps = 10;
        for s in 0..steps {
            let t0 = s as f32 / steps as f32;
            let t1 = (s + 1) as f32 / steps as f32;
            let p0 = near + (far - near) * t0;
            let p1 = near + (far - near) * t1;
            let fog = (1.0 - t1).powf(1.4);
            painter.line_segment(
                [p0, p1],
                Stroke::new(1.0, fade(CYAN, line_alpha * fog.max(0.04))),
            );
        }
    }

    // Fog: a dithered band that hides where the floor meets the sky.
    if let Some(tex) = dither {
        let h = r.height() * 0.2;
        let band = Rect::from_min_max(
            Pos2::new(r.left(), hz - h * 0.25),
            Pos2::new(r.right(), hz + h),
        );
        let cells = Vec2::new(band.width() / 4.0, band.height() / 4.0);
        let mut mesh = Mesh::with_texture(tex);
        let corners = [
            (band.left_top(), Pos2::new(0.0, 0.0), 0.0),
            (band.right_top(), Pos2::new(cells.x, 0.0), 0.0),
            (band.right_bottom(), Pos2::new(cells.x, cells.y), 1.0),
            (band.left_bottom(), Pos2::new(0.0, cells.y), 1.0),
        ];
        for (pos, uv, _) in corners {
            mesh.vertices.push(Vertex {
                pos,
                uv: Pos2::new(uv.x / 2.0, uv.y / 2.0),
                color: fade(VOID, 0.5 * b.alpha),
            });
        }
        mesh.indices.extend([0, 1, 2, 0, 2, 3]);
        painter.add(Shape::mesh(mesh));
    }
    gradient(
        painter,
        Rect::from_min_max(
            Pos2::new(r.left(), hz),
            Pos2::new(r.right(), hz + r.height() * 0.16),
        ),
        fade(GLOW, b.alpha * 0.55),
        fade(VOID, 0.0),
    );
}

/// The 2x2 checker used for the fog dither (nearest filtering, repeating).
pub(super) fn dither_image() -> egui::ColorImage {
    let mut img = egui::ColorImage::new([2, 2], Color32::TRANSPARENT);
    img.pixels[0] = Color32::WHITE;
    img.pixels[3] = Color32::WHITE;
    img
}

/// What a case's front shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CoverKind {
    /// The PSoXide Demo Disc, drawn in the palette.
    Demo,
    /// "Open a games folder": a dashed ghost case with a plus.
    Ghost,
    /// A disc of the visitor's own, with a generated placeholder front.
    Plain,
    /// A fetched picture.
    Art,
}

/// One jewel case to draw.
pub(super) struct CaseDraw<'a> {
    pub kind: CoverKind,
    pub title: &'a str,
    pub art: Option<TextureId>,
    /// Width, height, depth in pixels at scale 1.
    pub size: V3,
    /// Centre of the case in scene space.
    pub center: V3,
    pub yaw: f32,
    pub pitch: f32,
    pub scale: f32,
    pub opacity: f32,
    /// Placeholder hue seed.
    pub seed: u32,
}

/// A face of the box as four scene-space corners, clockwise from top-left.
struct Face {
    pts: [Pos2; 4],
    z: f32,
}

fn bilerp(f: &Face, u: f32, v: f32) -> Pos2 {
    let top = f.pts[0] + (f.pts[1] - f.pts[0]) * u;
    let bot = f.pts[3] + (f.pts[2] - f.pts[3]) * u;
    top + (bot - top) * v
}

fn area(p: &[Pos2; 4]) -> f32 {
    let mut s = 0.0;
    for i in 0..4 {
        let a = p[i];
        let b = p[(i + 1) % 4];
        s += a.x * b.y - b.x * a.y;
    }
    s * 0.5
}

/// Draw one case: soft shadow, then the visible faces back to front.
pub(super) fn draw_case(painter: &Painter, cam: &Camera, c: &CaseDraw<'_>) {
    let o = c.opacity;
    if o <= 0.01 {
        return;
    }
    let hw = c.size.x * c.scale / 2.0;
    let hh = c.size.y * c.scale / 2.0;
    let hd = c.size.z * c.scale / 2.0;
    let corner = |sx: f32, sy: f32, sz: f32| -> V3 {
        V3::new(sx * hw, sy * hh, sz * hd)
            .rot_y(c.yaw)
            .rot_x(c.pitch)
            .add(c.center)
    };
    let p = |sx: f32, sy: f32, sz: f32| -> (Pos2, f32) {
        let v = corner(sx, sy, sz);
        (cam.project(v), v.z)
    };
    // Shadow on the floor under the case.
    let foot = cam.project(V3::new(c.center.x, c.center.y + hh * 1.02, c.center.z));
    for (k, a) in [(1.35, 0.10), (1.05, 0.14), (0.75, 0.18)] {
        let rx = hw * k;
        let ry = hw * 0.16 * k;
        let pts: Vec<Pos2> = (0..28)
            .map(|i| {
                let t = i as f32 / 28.0 * std::f32::consts::TAU;
                foot + Vec2::new(t.cos() * rx, t.sin() * ry)
            })
            .collect();
        painter.add(Shape::convex_polygon(
            pts,
            fade(Color32::BLACK, a * o),
            Stroke::NONE,
        ));
    }
    let ghost = c.kind == CoverKind::Ghost;
    let (ftl, z0) = p(-1.0, -1.0, 1.0);
    let (ftr, z1) = p(1.0, -1.0, 1.0);
    let (fbr, z2) = p(1.0, 1.0, 1.0);
    let (fbl, z3) = p(-1.0, 1.0, 1.0);
    let front = Face {
        pts: [ftl, ftr, fbr, fbl],
        z: (z0 + z1 + z2 + z3) / 4.0,
    };
    let (btl, y0) = p(-1.0, -1.0, -1.0);
    let (btr, y1) = p(1.0, -1.0, -1.0);
    let (bbr, y2) = p(1.0, 1.0, -1.0);
    let (bbl, y3) = p(-1.0, 1.0, -1.0);
    let back = Face {
        pts: [btl, btr, bbr, bbl],
        z: (y0 + y1 + y2 + y3) / 4.0,
    };
    // Left (spine), right, top, bottom: corners listed so a positive shoelace
    // area means the face looks at the viewer.
    let left = Face {
        pts: [btl, ftl, fbl, bbl],
        z: (y0 + z0 + z3 + y3) / 4.0,
    };
    let right = Face {
        pts: [ftr, btr, bbr, fbr],
        z: (z1 + y1 + y2 + z2) / 4.0,
    };
    let top = Face {
        pts: [btl, btr, ftr, ftl],
        z: (y0 + y1 + z1 + z0) / 4.0,
    };
    let bottom = Face {
        pts: [fbl, fbr, bbr, bbl],
        z: (z3 + z2 + y2 + y3) / 4.0,
    };
    let body = if ghost {
        Color32::from_rgba_unmultiplied(20, 44, 58, 70)
    } else {
        Color32::from_rgb(11, 28, 38)
    };
    let edge = fade(
        if ghost { CYAN } else { Color32::from_rgb(70, 190, 215) },
        0.55 * o,
    );
    let mut faces: Vec<(&Face, Color32, bool)> = vec![
        (&back, mix(body, VOID, 0.4), false),
        (&left, mix(body, CYAN, 0.1), false),
        (&right, mix(body, CYAN, 0.06), false),
        (&top, mix(body, CYAN, 0.16), false),
        (&bottom, mix(body, VOID, 0.5), false),
        (&front, body, true),
    ];
    faces.sort_by(|a, b| a.0.z.total_cmp(&b.0.z));
    for (face, color, is_front) in faces {
        if area(&face.pts) <= 0.0 {
            continue;
        }
        painter.add(Shape::convex_polygon(
            face.pts.to_vec(),
            fade(color, o),
            Stroke::new(1.0, edge),
        ));
        if is_front {
            draw_cover(painter, face, c, o);
        }
    }
}

/// The cover art on the front face, mapped by bilinear interpolation of the
/// face corners (the sway is a few degrees, so the error is invisible).
fn draw_cover(painter: &Painter, face: &Face, c: &CaseDraw<'_>, o: f32) {
    // The cover sits inside the plastic frame.
    let inset = Face {
        pts: [
            bilerp(face, 0.04, 0.05),
            bilerp(face, 0.96, 0.05),
            bilerp(face, 0.96, 0.95),
            bilerp(face, 0.04, 0.95),
        ],
        z: face.z,
    };
    let map = |u: f32, v: f32| bilerp(&inset, u, v);
    let w = (inset.pts[1] - inset.pts[0]).length();
    let unit = w / 100.0;
    let tilt = (inset.pts[1] - inset.pts[0]).angle();
    match c.kind {
        CoverKind::Art => {
            if let Some(tex) = c.art {
                textured_quad(painter, tex, &inset, fade(Color32::WHITE, o));
            }
        }
        CoverKind::Ghost => {
            // Dashed outline and a plus: "add your games here".
            let a = fade(CYAN, 0.7 * o);
            dashed_quad(painter, &inset, a);
            let m = map(0.5, 0.5);
            let r = 11.0 * unit;
            painter.line_segment([m - Vec2::new(r, 0.0), m + Vec2::new(r, 0.0)], Stroke::new(1.5, a));
            painter.line_segment([m - Vec2::new(0.0, r), m + Vec2::new(0.0, r)], Stroke::new(1.5, a));
        }
        CoverKind::Demo => draw_demo_cover(painter, &inset, o, unit, tilt),
        CoverKind::Plain => {
            let hue = c.seed % 4;
            let (a, b) = [
                (Color32::from_rgb(23, 50, 61), Color32::from_rgb(11, 26, 34)),
                (Color32::from_rgb(20, 53, 67), Color32::from_rgb(10, 28, 39)),
                (Color32::from_rgb(23, 58, 68), Color32::from_rgb(11, 30, 38)),
                (Color32::from_rgb(26, 58, 74), Color32::from_rgb(11, 28, 39)),
            ][hue as usize];
            quad_gradient(painter, &inset, fade(a, o), fade(b, o));
            let m = map(0.5, 0.40);
            let r = 20.0 * unit;
            let ring: Vec<Pos2> = (0..32)
                .map(|i| {
                    let t = i as f32 / 32.0 * std::f32::consts::TAU;
                    m + Vec2::new(t.cos() * r, t.sin() * r)
                })
                .collect();
            painter.add(Shape::closed_line(ring, Stroke::new(1.0, fade(CYAN, 0.5 * o))));
            painter.circle_stroke(m, 4.0 * unit, Stroke::new(1.0, fade(CYAN, 0.5 * o)));
            let font = egui::FontId::proportional((w / 100.0 * 7.0).clamp(7.0, 40.0));
            let galley = painter.layout(
                c.title.to_string(),
                font,
                fade(Color32::from_rgb(230, 242, 247), o),
                inset.pts[1].x - inset.pts[0].x - 8.0 * unit,
            );
            let pos = map(0.5, 0.78) - Vec2::new(galley.size().x / 2.0, galley.size().y / 2.0);
            painter.add(Shape::galley(pos, galley, fade(Color32::WHITE, o)));
        }
    }
    // A soft sheen across the upper corner.
    let sheen = Shape::convex_polygon(
        vec![map(0.0, 0.0), map(0.55, 0.0), map(0.0, 0.62)],
        fade(Color32::WHITE, 0.05 * o),
        Stroke::NONE,
    );
    painter.add(sheen);
}

fn quad_gradient(painter: &Painter, f: &Face, top: Color32, bottom: Color32) {
    let mut mesh = Mesh::default();
    let uv = egui::epaint::WHITE_UV;
    for (i, pos) in f.pts.iter().enumerate() {
        mesh.vertices.push(Vertex {
            pos: *pos,
            uv,
            color: if i < 2 { top } else { bottom },
        });
    }
    mesh.indices.extend([0, 1, 2, 0, 2, 3]);
    painter.add(Shape::mesh(mesh));
}

fn textured_quad(painter: &Painter, tex: TextureId, f: &Face, tint: Color32) {
    // Subdivided so the affine interpolation stays close to the true mapping.
    let mut mesh = Mesh::with_texture(tex);
    const N: usize = 6;
    for j in 0..=N {
        for i in 0..=N {
            let u = i as f32 / N as f32;
            let v = j as f32 / N as f32;
            mesh.vertices.push(Vertex {
                pos: bilerp(f, u, v),
                uv: Pos2::new(u, v),
                color: tint,
            });
        }
    }
    for j in 0..N {
        for i in 0..N {
            let a = (j * (N + 1) + i) as u32;
            let b = a + 1;
            let c = a + (N as u32 + 1);
            let d = c + 1;
            mesh.indices.extend([a, b, d, a, d, c]);
        }
    }
    painter.add(Shape::mesh(mesh));
}

fn dashed_quad(painter: &Painter, f: &Face, color: Color32) {
    for i in 0..4 {
        let a = f.pts[i];
        let b = f.pts[(i + 1) % 4];
        painter.extend(Shape::dashed_line(&[a, b], Stroke::new(1.2, color), 7.0, 5.0));
    }
}

/// The Demo Disc cover: a quiet radar of rings, the wordmark and a row of
/// glyphs, in the emulator's palette.
fn draw_demo_cover(painter: &Painter, f: &Face, o: f32, unit: f32, tilt: f32) {
    let map = |u: f32, v: f32| bilerp(f, u, v);
    quad_gradient(
        painter,
        f,
        fade(Color32::from_rgb(11, 26, 36), o),
        fade(Color32::from_rgb(5, 11, 18), o),
    );
    let centre = map(0.5, 0.56);
    for (i, r) in [10.0, 17.0, 24.0, 31.0, 38.0].iter().enumerate() {
        let pts: Vec<Pos2> = (0..40)
            .map(|k| {
                let t = k as f32 / 40.0 * std::f32::consts::TAU;
                centre + Vec2::new(t.cos() * r * unit, t.sin() * r * unit * 0.9)
            })
            .collect();
        painter.add(Shape::closed_line(
            pts,
            Stroke::new(1.0, fade(CYAN, (0.22 - i as f32 * 0.03).max(0.05) * o)),
        ));
    }
    // The disc: a ring around the hub with the cyan X of the logo.
    let r = 17.0 * unit;
    let ring: Vec<Pos2> = (0..40)
        .map(|k| {
            let t = k as f32 / 40.0 * std::f32::consts::TAU;
            centre + Vec2::new(t.cos() * r, t.sin() * r)
        })
        .collect();
    painter.add(Shape::closed_line(ring, Stroke::new(1.4, fade(CYAN, 0.9 * o))));
    painter.circle_stroke(centre, 4.0 * unit, Stroke::new(1.0, fade(CYAN, 0.8 * o)));
    let d = 9.0 * unit;
    for s in [1.0f32, -1.0] {
        painter.line_segment(
            [centre + Vec2::new(-d, -d * s), centre + Vec2::new(d, d * s)],
            Stroke::new(1.6, fade(CYAN, o)),
        );
    }
    // Wordmark, with a cyan X.
    let size = (11.5 * unit).clamp(8.0, 60.0);
    let font = egui::FontId::proportional(size);
    let mark_at = map(0.5, 0.16);
    let mut job = egui::text::LayoutJob::default();
    let white = fade(Color32::WHITE, o);
    for (txt, col) in [("PSo", white), ("X", fade(MENU_ACCENT, o)), ("ide", white)] {
        job.append(
            txt,
            0.0,
            egui::TextFormat {
                font_id: font.clone(),
                color: col,
                ..Default::default()
            },
        );
    }
    let galley = painter.layout_job(job);
    let pos = mark_at - Vec2::new(galley.size().x / 2.0, galley.size().y / 2.0);
    painter.add(Shape::Text(
        egui::epaint::TextShape::new(pos, galley, Color32::WHITE).with_angle(tilt),
    ));
    let small = |painter: &Painter, at: Pos2, text: &str, px: f32, col: Color32| {
        painter.text(
            at,
            egui::Align2::CENTER_CENTER,
            text,
            egui::FontId::proportional(px.clamp(5.0, 40.0)),
            col,
        );
    };
    small(
        painter,
        map(0.5, 0.27),
        "Homebrew for the PlayStation",
        3.9 * unit,
        fade(Color32::from_rgb(159, 181, 196), o),
    );
    small(painter, map(0.5, 0.84), "Demo Disc", 8.0 * unit, fade(Color32::WHITE, o));
    small(
        painter,
        map(0.5, 0.92),
        "PLAYSTATION HOMEBREW",
        3.4 * unit,
        fade(CYAN, 0.9 * o),
    );
}
