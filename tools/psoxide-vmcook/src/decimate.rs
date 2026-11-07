//! Opt-in level-of-detail for held weapons: quadric edge collapse on the
//! cooked HMD8 mesh, run after pruning and texture fitting.
//!
//! Vertices are bone-local and every range is one rigid bone, so a collapse
//! is only allowed between vertices of the same (bone, body mask, flags)
//! group: the merged vertex then moves exactly like both originals in every
//! pose. Each collapse moves one vertex onto a neighbour that already exists
//! (half-edge collapse), so positions stay on the authored i16 lattice and no
//! new vertex is created. Triangles keep their own per-corner UVs, normal and
//! texture, and their order, so the depth buckets draw the survivors in the
//! same relative order. Open edges (the pruned weapon is full of them) carry
//! extra constraint planes so the silhouette is the last thing to move.
//!
//! `silhouette` replays every reachable pose in every view through the same
//! runtime projection, sort and rasterizer the prune check uses and counts
//! pixels covered by exactly one of the two meshes.

use crate::cooked::Cooked;
use crate::runtime;

type Quadric = [f64; 10];

fn plane_quadric(n: [f64; 3], d: f64, w: f64) -> Quadric {
    let (a, b, c) = (n[0], n[1], n[2]);
    [
        w * a * a,
        w * a * b,
        w * a * c,
        w * a * d,
        w * b * b,
        w * b * c,
        w * b * d,
        w * c * c,
        w * c * d,
        w * d * d,
    ]
}

fn add(q: &mut Quadric, r: &Quadric) {
    for i in 0..10 {
        q[i] += r[i];
    }
}

fn eval(q: &Quadric, p: [f64; 3]) -> f64 {
    let (x, y, z) = (p[0], p[1], p[2]);
    q[0] * x * x
        + 2.0 * q[1] * x * y
        + 2.0 * q[2] * x * z
        + 2.0 * q[3] * x
        + q[4] * y * y
        + 2.0 * q[5] * y * z
        + 2.0 * q[6] * y
        + q[7] * z * z
        + 2.0 * q[8] * z
        + q[9]
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn len(a: [f64; 3]) -> f64 {
    dot(a, a).sqrt()
}

/// Weight of an open edge's constraint plane relative to a face plane.
const BOUNDARY_WEIGHT: f64 = 16.0;
/// A collapse may not turn any surviving triangle by more than ~70 degrees.
const MIN_NORMAL_DOT: f64 = 0.35;
/// Small separate parts (a strap, an eye, a sight) are where the detail is:
/// a connected piece of at most this many triangles...
const SMALL_PART_TRIS: usize = 40;
/// ...keeps at least this share of its triangles (and never fewer than 3).
const SMALL_PART_KEEP: f64 = 0.6;

/// Collapse edges, cheapest quadric error first, until at most
/// `target_tris` triangles survive (or no legal collapse remains), then drop
/// unreferenced vertices and rebuild the ranges.
pub fn decimate(c: &Cooked, target_tris: usize) -> Cooked {
    let pos: Vec<[f64; 3]> = c
        .verts
        .iter()
        .map(|v| [v[0] as f64, v[1] as f64, v[2] as f64])
        .collect();
    let group = c.vert_bone();
    let mut tris: Vec<Option<[usize; 3]>> = c
        .tris
        .iter()
        .map(|t| Some(t.idx.map(|i| i as usize)))
        .collect();
    let mut quad = vec![[0.0f64; 10]; pos.len()];
    // Face planes, area weighted.
    // Per edge: the faces using it. An open edge, or one between two
    // textures (a painted detail such as an eye or a dial), is a seam.
    let mut edge_use: std::collections::BTreeMap<(usize, usize), Vec<usize>> =
        std::collections::BTreeMap::new();
    for (ti, t) in tris.iter().enumerate() {
        let [a, b, cc] = t.unwrap();
        let n = cross(sub(pos[b], pos[a]), sub(pos[cc], pos[a]));
        let area = len(n);
        if area < 1e-9 {
            continue;
        }
        let n = [n[0] / area, n[1] / area, n[2] / area];
        let q = plane_quadric(n, -dot(n, pos[a]), area * 0.5);
        for v in [a, b, cc] {
            add(&mut quad[v], &q);
        }
        for (x, y) in [(a, b), (b, cc), (cc, a)] {
            let key = (x.min(y), x.max(y));
            edge_use.entry(key).or_default().push(ti);
        }
    }
    // Seams: a plane through the edge, perpendicular to each face on it.
    for (&(x, y), faces) in &edge_use {
        let seam = faces.len() == 1 || faces.iter().any(|&f| c.tris[f].tex != c.tris[faces[0]].tex);
        if !seam {
            continue;
        }
        for &ti in faces {
            let [a, b, cc] = tris[ti].unwrap();
            let fn_ = cross(sub(pos[b], pos[a]), sub(pos[cc], pos[a]));
            let e = sub(pos[y], pos[x]);
            let n = cross(e, fn_);
            let l = len(n);
            if l < 1e-9 {
                continue;
            }
            let n = [n[0] / l, n[1] / l, n[2] / l];
            let q = plane_quadric(n, -dot(n, pos[x]), BOUNDARY_WEIGHT * dot(e, e));
            add(&mut quad[x], &q);
            add(&mut quad[y], &q);
        }
    }
    // Connected parts (triangles sharing vertices) and the floor each keeps.
    let mut parent: Vec<usize> = (0..pos.len()).collect();
    fn root(p: &mut [usize], mut x: usize) -> usize {
        while p[x] != x {
            p[x] = p[p[x]];
            x = p[x];
        }
        x
    }
    for t in tris.iter().flatten() {
        let r0 = root(&mut parent, t[0]);
        for &v in &t[1..] {
            let r = root(&mut parent, v);
            parent[r] = r0;
        }
    }
    let part: Vec<usize> = tris
        .iter()
        .map(|t| root(&mut parent, t.unwrap()[0]))
        .collect();
    let mut part_alive = vec![0usize; pos.len()];
    for &p in &part {
        part_alive[p] += 1;
    }
    let part_floor: Vec<usize> = part_alive
        .iter()
        .map(|&n| {
            if n == 0 || n > SMALL_PART_TRIS {
                0
            } else {
                ((n as f64 * SMALL_PART_KEEP).ceil() as usize).max(3).min(n)
            }
        })
        .collect();
    let mut alive = tris.iter().filter(|t| t.is_some()).count();
    while alive > target_tris {
        // Every directed edge between same-group vertices, cheapest first.
        let mut cand: Vec<(f64, usize, usize)> = Vec::new();
        for t in tris.iter().flatten() {
            for (x, y) in [(t[0], t[1]), (t[1], t[2]), (t[2], t[0])] {
                for (from, to) in [(x, y), (y, x)] {
                    if group[from] != group[to] {
                        continue;
                    }
                    let mut q = quad[from];
                    add(&mut q, &quad[to]);
                    cand.push((eval(&q, pos[to]).max(0.0), from, to));
                }
            }
        }
        cand.sort_by(|a, b| {
            a.0.partial_cmp(&b.0)
                .unwrap()
                .then((a.1, a.2).cmp(&(b.1, b.2)))
        });
        cand.dedup_by(|a, b| a.1 == b.1 && a.2 == b.2);
        let mut done = false;
        for &(_, from, to) in &cand {
            // Reject a collapse that flips or degenerates a surviving face.
            let ok = tris.iter().flatten().all(|t| {
                if !t.contains(&from) || t.contains(&to) {
                    return true;
                }
                let moved = t.map(|v| if v == from { to } else { v });
                let n0 = cross(sub(pos[t[1]], pos[t[0]]), sub(pos[t[2]], pos[t[0]]));
                let n1 = cross(
                    sub(pos[moved[1]], pos[moved[0]]),
                    sub(pos[moved[2]], pos[moved[0]]),
                );
                let (l0, l1) = (len(n0), len(n1));
                l0 < 1e-9 || (l1 > 1e-9 && dot(n0, n1) > MIN_NORMAL_DOT * l0 * l1)
            });
            // A collapse removes every face holding both ends.
            let mut take = std::collections::BTreeMap::new();
            for (ti, t) in tris.iter().enumerate() {
                if let Some(t) = t {
                    if t.contains(&from) && t.contains(&to) {
                        *take.entry(part[ti]).or_insert(0usize) += 1;
                    }
                }
            }
            if !ok
                || take
                    .iter()
                    .any(|(&p, &n)| part_alive[p] < part_floor[p] + n)
            {
                continue;
            }
            let qf = quad[from];
            add(&mut quad[to], &qf);
            for (ti, t) in tris.iter_mut().enumerate() {
                if let Some(v) = t {
                    if v.contains(&from) {
                        let moved = v.map(|x| if x == from { to } else { x });
                        if moved[0] == moved[1] || moved[1] == moved[2] || moved[0] == moved[2] {
                            *t = None;
                            alive -= 1;
                            part_alive[part[ti]] -= 1;
                        } else {
                            *v = moved;
                        }
                    }
                }
            }
            done = true;
            break;
        }
        if !done {
            break;
        }
    }
    let mut out = c.clone();
    let mut keep = Vec::with_capacity(c.tris.len());
    for (ti, t) in tris.iter().enumerate() {
        keep.push(t.is_some());
        if let Some(v) = t {
            out.tris[ti].idx = v.map(|i| i as u16);
        }
    }
    out.prune(&keep)
}

/// Coverage difference between two meshes over every reachable pose and
/// view: (worst pose's symmetric-difference pixels, that pose's covered
/// pixels in `a`, mean symmetric-difference pixels per pose).
pub fn silhouette(a: &Cooked, b: &Cooked, views: &[(u16, u16, u16)]) -> (usize, usize, f64) {
    let (ma, mb) = (runtime::load_model(a), runtime::load_model(b));
    let poses = runtime::runtime_poses(a);
    let (mut worst, mut worst_cov, mut total, mut n) = (0usize, 0usize, 0usize, 0usize);
    for &(h, w, hh) in views {
        let (rect, center) = crate::expanded(w, hh);
        let mut fa = runtime::Frame::with_center(a, rect, center);
        let mut fb = runtime::Frame::with_center(b, rect, center);
        for &pose in &poses {
            let pa = runtime::project(&ma, pose, h);
            let pb = runtime::project(&mb, pose, h);
            let (sa, sb) = (runtime::sort(&ma, &pa), runtime::sort(&mb, &pb));
            fa.begin(0);
            fa.draw_ids(&pa, &ma, &sa.order);
            fb.begin(0);
            fb.draw_ids(&pb, &mb, &sb.order);
            let (mut diff, mut cov) = (0usize, 0usize);
            for y in rect.1 as usize..(rect.1 + rect.3) as usize {
                for x in rect.0 as usize..(rect.0 + rect.2) as usize {
                    let (ca, cb) = (fa.pixel(x, y) != 0, fb.pixel(x, y) != 0);
                    cov += ca as usize;
                    diff += (ca != cb) as usize;
                }
            }
            if diff > worst {
                worst = diff;
                worst_cov = cov;
            }
            total += diff;
            n += 1;
        }
    }
    (worst, worst_cov, total as f64 / n.max(1) as f64)
}

/// One pose at game size (`view` = projection height, width, height),
/// textured and flat-shaded exactly as the runtime draws it, on grey.
pub fn render(c: &Cooked, pose: (usize, usize, u32), view: (u16, u16, u16)) -> Vec<[u8; 3]> {
    let (h, w, hh) = view;
    let md = runtime::load_model(c);
    let rect = (0u16, 0u16, w, hh);
    let center = ((w / 2) as i16, (hh / 2) as i16);
    let mut fr = runtime::Frame::with_center(c, rect, center);
    let p = runtime::project(&md, pose, h);
    let s = runtime::sort(&md, &p);
    fr.begin(0x3def);
    fr.draw_textured(&md, &p, &s.order, |i| {
        runtime::vm_normal_shade(md.triangle(i).normal)
    });
    fr.rgb()
}
