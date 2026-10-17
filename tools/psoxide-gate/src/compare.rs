//! Presentation comparison with explicit visible polygon coverage.
use crate::img::{Img, Rect};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    /// Declared 2D bounds are conservative: transparent holes stay exact too.
    #[serde(default)]
    pub two_d: Vec<Rect>,
    /// Each record describes a final visible contribution, not a bounding box
    /// of every primitive ever submitted. Hidden polygon edges grant no tolerance.
    #[serde(default)]
    pub polygons: Vec<Polygon>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Polygon {
    /// Observed GP0 polygon opcode, with quad/textured bits retained.
    pub opcode: u8,
    /// Native display coordinates in canonical perimeter order, after draw
    /// offset/display origin. GP0 quad packet order must be converted 0,1,3,2.
    pub vertices: Vec<[i32; 2]>,
    /// Non-overlapping native visible pixel runs after clipping/occlusion.
    /// These must come from observed ownership or an explicit region declaration.
    pub visible: Vec<Rect>,
    /// Capture-bound command/state or declared-region provenance.
    pub evidence: String,
    /// Maximum display-code delta allowed only on this polygon's actual boundary.
    /// This never applies to an interior pixel or any declared 2D pixel.
    pub edge_channel: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Exact,
    TwoD,
    Interior,
    Edge(u8),
}

pub struct Comparison {
    pub raw: u64,
    pub normalized_exact: u64,
    pub two_d: u64,
    pub unclassified: u64,
    pub interior: u64,
    pub edge: u64,
    pub accepted_edges: u64,
    pub classes: Vec<Class>,
    /// Real surviving differences in hardware coordinates. 3D native errors
    /// occupy their complete scale-by-scale box, 2D errors retain exact pixels.
    pub surviving: Vec<bool>,
    pub reference: Img,
    pub hardware: Img,
}
impl Comparison {
    pub fn coverage(&self) -> [u64; 3] {
        let mut counts = [0; 3];
        for c in &self.classes {
            counts[match c {
                Class::TwoD => 0,
                Class::Interior | Class::Edge(_) => 1,
                Class::Exact => 2,
            }] += 1;
        }
        counts
    }
    pub fn projected(&self) -> u64 {
        self.surviving.iter().filter(|v| **v).count() as u64
    }
    pub fn regions(&self) -> Vec<Rect> {
        let mut out = Vec::new();
        for y in (0..self.hardware.h).step_by(64) {
            for x in (0..self.hardware.w).step_by(64) {
                let r = Rect {
                    x,
                    y,
                    w: 64.min(self.hardware.w - x),
                    h: 64.min(self.hardware.h - y),
                };
                if self.in_rect(r) > 0 {
                    out.push(r);
                }
            }
        }
        out
    }
    pub fn in_rect(&self, r: Rect) -> u64 {
        (r.y..r.y + r.h)
            .map(|y| {
                (r.x..r.x + r.w)
                    .filter(|x| self.surviving[(y * self.hardware.w + *x) as usize])
                    .count() as u64
            })
            .sum()
    }
    pub fn has_differences(&self) -> bool {
        self.two_d != 0 || self.unclassified != 0 || self.interior != 0 || self.edge != 0
    }
    pub fn description(&self) -> String {
        let coverage = self.coverage();
        format!(
            "{}raw {}; normalized exact {}; surviving 2D {} HW pixels; unresolved unclassified {} HW pixels; 3D interior {} native pixels; 3D edge {} native pixels; accepted boundary {} native pixels; native coverage 2D={} 3D={} unclassified={}",
            if self.has_differences() {
                ""
            } else {
                "identical after comparison rules; "
            },
            self.raw,
            self.normalized_exact,
            self.two_d,
            self.unclassified,
            self.interior,
            self.edge,
            self.accepted_edges,
            coverage[0],
            coverage[1],
            coverage[2]
        )
    }
    pub fn overlay(&self) -> Img {
        let mut out = self.reference.clone();
        for (p, &different) in out.rgba.chunks_exact_mut(4).zip(&self.surviving) {
            if different {
                p[..3].copy_from_slice(&[255, 0, 255]);
            } else {
                for v in &mut p[..3] {
                    *v /= 4;
                }
            }
        }
        out
    }
}

/// Strip expanded low bits on both paths, then use the same display expansion.
/// No channel/radius/fraction tolerance is part of color normalization.
pub fn normalize(img: &Img) -> Img {
    let mut out = img.clone();
    for p in out.rgba.chunks_exact_mut(4) {
        for c in &mut p[..3] {
            let n = *c >> 3;
            *c = (n << 3) | (n >> 2);
        }
    }
    out
}

fn valid_rect(r: Rect, w: u32, h: u32) -> bool {
    r.w > 0
        && r.h > 0
        && r.x.checked_add(r.w).is_some_and(|v| v <= w)
        && r.y.checked_add(r.h).is_some_and(|v| v <= h)
}
/// Distance to the finite polygon segment, in doubled coordinates. One half
/// native pixel is the maximum boundary band; vertices cannot create a huge mask.
fn segment_distance2(x: f64, y: f64, a: [i32; 2], b: [i32; 2]) -> f64 {
    let (ax, ay, bx, by) = (a[0] as f64, a[1] as f64, b[0] as f64, b[1] as f64);
    let (dx, dy) = (bx - ax, by - ay);
    let length = dx * dx + dy * dy;
    let t = if length == 0.0 {
        0.0
    } else {
        ((x - ax) * dx + (y - ay) * dy) / length
    }
    .clamp(0.0, 1.0);
    (x - ax - t * dx).powi(2) + (y - ay - t * dy).powi(2)
}
fn boundary(p: &Polygon, x: u32, y: u32) -> bool {
    (0..p.vertices.len()).any(|i| {
        segment_distance2(
            x as f64 + 0.5,
            y as f64 + 0.5,
            p.vertices[i],
            p.vertices[(i + 1) % p.vertices.len()],
        ) <= 0.25
    })
}
fn in_polygon(p: &Polygon, x: u32, y: u32) -> bool {
    let (x, y) = (x as f64 + 0.5, y as f64 + 0.5);
    let mut inside = false;
    for i in 0..p.vertices.len() {
        let a = p.vertices[i];
        let b = p.vertices[(i + 1) % p.vertices.len()];
        if (a[1] as f64 > y) != (b[1] as f64 > y)
            && x < ((b[0] - a[0]) as f64 * (y - a[1] as f64) / (b[1] - a[1]) as f64 + a[0] as f64)
        {
            inside = !inside;
        }
    }
    inside
}
/// GP0 quads submit a triangle strip, not a perimeter. Preserve the two
/// submitted triangles by walking packet vertices 0,1,3,2 around the outside.
#[cfg(test)]
pub fn packet_perimeter(vertices: &[[i32; 2]]) -> Result<Vec<[i32; 2]>, String> {
    match vertices.len() {
        3 => Ok(vertices.to_vec()),
        4 => Ok(vec![vertices[0], vertices[1], vertices[3], vertices[2]]),
        _ => Err("GP0 polygon requires three or four packet vertices".into()),
    }
}
pub fn classify(layout: &Layout, w: u32, h: u32) -> Result<Vec<Class>, String> {
    let mut out = vec![Class::Exact; (w * h) as usize];
    for p in &layout.polygons {
        let expected = if p.opcode & 8 != 0 { 4 } else { 3 };
        if !(0x20..=0x3f).contains(&p.opcode)
            || p.vertices.len() != expected
            || p.evidence.trim().is_empty()
            || p.visible.is_empty()
            || p.edge_channel > 24
        {
            return Err("polygon needs 3/4 vertices, visible coverage, provenance and bounded edge_channel <=24".into());
        }
        if p.vertices
            .iter()
            .any(|v| v[0] < 0 || v[1] < 0 || v[0] > w as i32 || v[1] > h as i32)
        {
            return Err("polygon vertex outside display".into());
        }
        let area: i64 = (0..p.vertices.len())
            .map(|i| {
                let a = p.vertices[i];
                let b = p.vertices[(i + 1) % p.vertices.len()];
                a[0] as i64 * b[1] as i64 - b[0] as i64 * a[1] as i64
            })
            .sum();
        if area == 0 {
            return Err("degenerate polygon".into());
        }
        // A bowtie, repeated edge or concave perimeter is not safe evidence for
        // this boundary classifier. Keep unsupported shapes explicitly blocked.
        let mut winding = 0;
        for i in 0..p.vertices.len() {
            let a = p.vertices[i];
            let b = p.vertices[(i + 1) % p.vertices.len()];
            let c = p.vertices[(i + 2) % p.vertices.len()];
            let cross = (b[0] - a[0]) as i64 * (c[1] - b[1]) as i64
                - (b[1] - a[1]) as i64 * (c[0] - b[0]) as i64;
            if cross == 0 || (winding != 0 && cross.signum() != winding) {
                return Err("polygon requires a simple convex perimeter".into());
            }
            winding = cross.signum();
        }
        let axis_textured_quad = p.opcode & 4 != 0
            && expected == 4
            && p.vertices.iter().enumerate().all(|(i, a)| {
                let b = p.vertices[(i + 1) % 4];
                a[0] == b[0] || a[1] == b[1]
            });
        for r in &p.visible {
            if !valid_rect(*r, w, h) {
                return Err("visible polygon coverage outside display".into());
            }
            for y in r.y..r.y + r.h {
                for x in r.x..r.x + r.w {
                    let edge = boundary(p, x, y);
                    if !in_polygon(p, x, y) && !edge {
                        return Err("visible coverage outside polygon".into());
                    }
                    let c = &mut out[(y * w + x) as usize];
                    if *c != Class::Exact {
                        return Err("overlapping final polygon ownership".into());
                    }
                    *c = if axis_textured_quad {
                        Class::TwoD
                    } else if edge {
                        Class::Edge(p.edge_channel)
                    } else {
                        Class::Interior
                    };
                }
            }
        }
    }
    for r in &layout.two_d {
        if !valid_rect(*r, w, h) {
            return Err("2D region outside display".into());
        }
        for y in r.y..r.y + r.h {
            for x in r.x..r.x + r.w {
                out[(y * w + x) as usize] = Class::TwoD;
            }
        }
    }
    Ok(out)
}

pub fn compare(cpu: &Img, hw: &Img, scale: u32, layout: &Layout) -> Result<Comparison, String> {
    if ![1, 3].contains(&scale) || (hw.w, hw.h) != (cpu.w * scale, cpu.h * scale) {
        return Err("comparison requires native HW1 or exact 3x dimensions".into());
    }
    let classes = classify(layout, cpu.w, cpu.h)?;
    let cpu_n = normalize(cpu);
    let hardware = normalize(hw);
    let reference = cpu_n.enlarge(scale);
    let raw = hw.diff(&cpu.enlarge(scale), 0, 1, &[], None).differing;
    let normalized_exact = hardware.diff(&reference, 0, 1, &[], None).differing;
    let mut result = Comparison {
        raw,
        normalized_exact,
        two_d: 0,
        unclassified: 0,
        interior: 0,
        edge: 0,
        accepted_edges: 0,
        classes,
        surviving: vec![false; (hw.w * hw.h) as usize],
        reference,
        hardware,
    };
    for y in 0..cpu.h {
        for x in 0..cpu.w {
            let class = result.classes[(y * cpu.w + x) as usize];
            let ref_px = cpu_n.px(x, y);
            if matches!(class, Class::Exact | Class::TwoD) {
                for yy in y * scale..(y + 1) * scale {
                    for xx in x * scale..(x + 1) * scale {
                        if result.hardware.px(xx, yy) != ref_px {
                            result.surviving[(yy * hw.w + xx) as usize] = true;
                            if class == Class::TwoD {
                                result.two_d += 1;
                            } else {
                                result.unclassified += 1;
                            }
                        }
                    }
                }
            } else {
                // Average canonical 8-bit channels, then round once at 1x.
                // Requantizing the average to 5-bit would hide interior errors.
                let mut sum = [0u32; 3];
                for yy in y * scale..(y + 1) * scale {
                    for xx in x * scale..(x + 1) * scale {
                        let p = result.hardware.px(xx, yy);
                        for c in 0..3 {
                            sum[c] += u32::from(p[c]);
                        }
                    }
                }
                let area = scale * scale;
                let mut delta = 0u8;
                for c in 0..3 {
                    let box_c = ((sum[c] + area / 2) / area) as u8;
                    delta = delta.max(box_c.abs_diff(ref_px[c]));
                }
                if delta == 0 {
                    continue;
                }
                if scale == 3 && matches!(class,Class::Edge(t) if delta<=t) {
                    result.accepted_edges += 1;
                    continue;
                }
                if class == Class::Interior {
                    result.interior += 1;
                } else {
                    result.edge += 1;
                }
                for yy in y * scale..(y + 1) * scale {
                    for xx in x * scale..(x + 1) * scale {
                        result.surviving[(yy * hw.w + xx) as usize] = true;
                    }
                }
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn image(w: u32, h: u32, c: u8) -> Img {
        Img::from_rgba(w, h, vec![c; (w * h * 4) as usize])
    }
    fn set(img: &mut Img, x: u32, y: u32, c: u8) {
        let i = ((y * img.w + x) * 4) as usize;
        img.rgba[i..i + 3].fill(c);
    }
    fn polygon() -> Polygon {
        Polygon {
            opcode: 0x20,
            vertices: vec![[0, 0], [8, 0], [0, 8]],
            visible: vec![
                Rect {
                    x: 0,
                    y: 0,
                    w: 1,
                    h: 1,
                },
                Rect {
                    x: 2,
                    y: 2,
                    w: 1,
                    h: 1,
                },
                Rect {
                    x: 3,
                    y: 4,
                    w: 1,
                    h: 1,
                },
            ],
            evidence: "capture-bound triangle7 final owner runs".into(),
            edge_channel: 16,
        }
    }
    #[test]
    fn normalization_is_not_a_color_tolerance() {
        let cpu = image(8, 8, 255);
        let hw = image(24, 24, 248);
        let same = compare(&cpu, &hw, 3, &Layout::default()).unwrap();
        assert!(!same.has_differences());
        assert!(same
            .description()
            .starts_with("identical after comparison rules;"));
        let wrong = image(24, 24, 240);
        assert_eq!(
            compare(&cpu, &wrong, 3, &Layout::default())
                .unwrap()
                .unclassified,
            576
        );
    }
    #[test]
    fn shifted_and_missing_glyph_pixels_survive() {
        let mut cpu = image(8, 8, 0);
        for y in 1..6 {
            set(&mut cpu, 2, y, 248);
        }
        let mut hw = cpu.enlarge(3);
        set(&mut hw, 6, 6, 0);
        set(&mut hw, 7, 6, 0);
        set(&mut hw, 9, 6, 248);
        let layout = Layout {
            two_d: vec![Rect {
                x: 0,
                y: 0,
                w: 8,
                h: 8,
            }],
            polygons: vec![],
        };
        let d = compare(&cpu, &hw, 3, &layout).unwrap();
        assert_eq!(d.two_d, 3);
        assert!(d.has_differences());
    }
    #[test]
    fn only_observed_polygon_edges_receive_tolerance() {
        let cpu = image(8, 8, 80);
        let mut hw = cpu.enlarge(3);
        for y in 12..15 {
            for x in 9..12 {
                set(&mut hw, x, y, 88);
            }
        }
        let layout = Layout {
            two_d: vec![],
            polygons: vec![polygon()],
        };
        let d = compare(&cpu, &hw, 3, &layout).unwrap();
        assert!(!d.has_differences());
        assert_eq!(d.accepted_edges, 1);
        for y in 6..9 {
            for x in 6..9 {
                set(&mut hw, x, y, 88);
            }
        }
        let d = compare(&cpu, &hw, 3, &layout).unwrap();
        assert_eq!(d.interior, 1);
        assert_eq!(d.accepted_edges, 1);
    }
    #[test]
    fn hw1_polygon_edges_remain_exact() {
        let cpu = image(8, 8, 80);
        let mut hw = cpu.clone();
        set(&mut hw, 3, 4, 88);
        let d = compare(
            &cpu,
            &hw,
            1,
            &Layout {
                two_d: vec![],
                polygons: vec![polygon()],
            },
        )
        .unwrap();
        assert_eq!(d.edge, 1);
        assert_eq!(d.accepted_edges, 0);
        assert!(d.has_differences());
    }
    #[test]
    fn single_subpixel_interior_error_survives_box_average() {
        let cpu = image(8, 8, 80);
        let mut hw = cpu.enlarge(3);
        set(&mut hw, 6, 6, 88);
        let d = compare(
            &cpu,
            &hw,
            3,
            &Layout {
                two_d: vec![],
                polygons: vec![polygon()],
            },
        )
        .unwrap();
        assert_eq!(d.interior, 1);
        assert_eq!(d.projected(), 9);
    }
    #[test]
    fn packet_quad_order_converts_and_bowties_fail() {
        let packet = [[0, 0], [8, 0], [0, 8], [8, 8]];
        let mut p = Polygon {
            opcode: 0x2c,
            vertices: packet_perimeter(&packet).unwrap(),
            visible: vec![Rect {
                x: 0,
                y: 0,
                w: 8,
                h: 8,
            }],
            evidence: "observed canonical quad".into(),
            edge_channel: 24,
        };
        assert!(classify(
            &Layout {
                two_d: vec![],
                polygons: vec![p.clone()]
            },
            8,
            8
        )
        .unwrap()
        .iter()
        .all(|c| *c == Class::TwoD));
        p.vertices = packet.to_vec();
        assert!(classify(
            &Layout {
                two_d: vec![],
                polygons: vec![p]
            },
            8,
            8
        )
        .is_err());
        assert!(packet_perimeter(&packet[..2]).is_err());
    }
    #[test]
    fn box_average_does_not_blur_2d_or_forgive_interior_color() {
        let cpu = image(8, 8, 80);
        let mut hw = cpu.enlarge(3);
        set(&mut hw, 6, 6, 88);
        set(&mut hw, 7, 6, 72);
        let layout = Layout {
            two_d: vec![],
            polygons: vec![polygon()],
        };
        assert!(!compare(&cpu, &hw, 3, &layout).unwrap().has_differences());
        let protected = Layout {
            two_d: vec![Rect {
                x: 2,
                y: 2,
                w: 1,
                h: 1,
            }],
            ..layout
        };
        assert_eq!(compare(&cpu, &hw, 3, &protected).unwrap().two_d, 2);
    }
    #[test]
    fn hud_and_transparent_holes_override_polygon_edges() {
        let cpu = image(8, 8, 80);
        let mut hw = cpu.enlarge(3);
        set(&mut hw, 9, 12, 88);
        let layout = Layout {
            two_d: vec![Rect {
                x: 3,
                y: 4,
                w: 1,
                h: 1,
            }],
            polygons: vec![polygon()],
        };
        let d = compare(&cpu, &hw, 3, &layout).unwrap();
        assert_eq!(d.two_d, 1);
        assert_eq!(d.accepted_edges, 0);
    }
    #[test]
    fn hidden_edges_and_unclassified_pixels_are_not_forgiven() {
        let cpu = image(8, 8, 80);
        let mut hw = cpu.enlarge(3);
        set(&mut hw, 18, 18, 88);
        assert_eq!(
            compare(
                &cpu,
                &hw,
                3,
                &Layout {
                    two_d: vec![],
                    polygons: vec![polygon()]
                }
            )
            .unwrap()
            .unclassified,
            1
        );
    }
    #[test]
    fn axis_aligned_textured_quad_cannot_receive_3d_tolerance() {
        let p = Polygon {
            opcode: 0x2c,
            vertices: vec![[0, 0], [8, 0], [8, 8], [0, 8]],
            visible: vec![Rect {
                x: 0,
                y: 0,
                w: 8,
                h: 8,
            }],
            evidence: "observed GP0 textured quad".into(),
            edge_channel: 24,
        };
        let cpu = image(8, 8, 80);
        let mut hw = cpu.enlarge(3);
        set(&mut hw, 0, 0, 88);
        let d = compare(
            &cpu,
            &hw,
            3,
            &Layout {
                two_d: vec![],
                polygons: vec![p],
            },
        )
        .unwrap();
        assert_eq!(d.two_d, 1);
        assert_eq!(d.accepted_edges, 0);
        assert_eq!(d.coverage(), [64, 0, 0]);
    }
    #[test]
    fn unsafe_metadata_fails_closed() {
        let mut p = polygon();
        p.evidence.clear();
        assert!(classify(
            &Layout {
                two_d: vec![],
                polygons: vec![p]
            },
            8,
            8
        )
        .is_err());
        let mut p = polygon();
        p.visible.push(Rect {
            x: 6,
            y: 6,
            w: 1,
            h: 1,
        });
        assert!(classify(
            &Layout {
                two_d: vec![],
                polygons: vec![p]
            },
            8,
            8
        )
        .is_err());
        let mut p = polygon();
        p.edge_channel = 255;
        assert!(classify(
            &Layout {
                two_d: vec![],
                polygons: vec![p]
            },
            8,
            8
        )
        .is_err());
        assert!(classify(
            &Layout {
                two_d: vec![Rect {
                    x: 8,
                    y: 0,
                    w: 1,
                    h: 1
                }],
                polygons: vec![]
            },
            8,
            8
        )
        .is_err());
        let p = polygon();
        assert!(classify(
            &Layout {
                two_d: vec![],
                polygons: vec![p.clone(), p]
            },
            8,
            8
        )
        .is_err());
    }
}
