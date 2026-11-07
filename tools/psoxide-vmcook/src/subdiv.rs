//! Crack-free subdivision of held-weapon triangles whose affine texture
//! mapping lands far from the perspective-correct place. The weapon is
//! camera-locked, so the warp of every triangle in every retained pose is
//! known at cook time. Only edges whose two vertices share a bone are split
//! (a bone-local midpoint is exact under that bone's rigid transform), and
//! every triangle touching a split edge is re-triangulated, so no T-junction
//! is left behind. Triangle order is preserved.

use crate::cooked::Cooked;
use crate::runtime as ps1;

pub const VM_TRI_CAP: usize = 1152;
pub const VM_VERT_CAP: usize = 976;

/// Worst on-screen affine warp (px) of every triangle over all key frames.
pub fn tri_warp(c: &Cooked) -> Vec<f64> {
    let md = ps1::load_model(c);
    let mut worst = vec![0f64; c.tris.len()];
    for pose in ps1::key_poses(c) {
        let p = ps1::project(&md, pose, 160);
        let s = ps1::sort(&md, &p);
        for &t in &s.order {
            let tri = md.triangle(t);
            let v: [usize; 3] = tri.idx.map(|x| x as usize);
            let z: [f64; 3] = v.map(|i| p.z[i] as f64);
            let uv: [[f64; 2]; 3] =
                std::array::from_fn(|k| [tri.uv[k].0 as f64, tri.uv[k].1 as f64]);
            let xy: [[f64; 2]; 3] = v.map(|i| [p.xy[i][0] as f64, p.xy[i][1] as f64]);
            let (x1, y1, x2, y2) = (
                xy[1][0] - xy[0][0],
                xy[1][1] - xy[0][1],
                xy[2][0] - xy[0][0],
                xy[2][1] - xy[0][1],
            );
            let (u1, v1, u2, v2) = (
                uv[1][0] - uv[0][0],
                uv[1][1] - uv[0][1],
                uv[2][0] - uv[0][0],
                uv[2][1] - uv[0][1],
            );
            let det = x1 * y2 - x2 * y1;
            if det.abs() < 1e-9 {
                continue;
            }
            let (ja, jb, jc, jd) = (
                (u1 * y2 - u2 * y1) / det,
                (u2 * x1 - u1 * x2) / det,
                (v1 * y2 - v2 * y1) / det,
                (v2 * x1 - v1 * x2) / det,
            );
            let jdet = ja * jd - jb * jc;
            if jdet.abs() < 1e-12 {
                continue;
            }
            let steps = 8;
            for i in 0..=steps {
                for j in 0..=(steps - i) {
                    let (b0, b1) = (i as f64 / steps as f64, j as f64 / steps as f64);
                    let b2 = 1.0 - b0 - b1;
                    let sx0 = b0 * xy[0][0] + b1 * xy[1][0] + b2 * xy[2][0];
                    let sy0 = b0 * xy[0][1] + b1 * xy[1][1] + b2 * xy[2][1];
                    if sx0.abs() > 166.0 || sy0.abs() > 134.0 {
                        continue;
                    }
                    let aff = [
                        b0 * uv[0][0] + b1 * uv[1][0] + b2 * uv[2][0],
                        b0 * uv[0][1] + b1 * uv[1][1] + b2 * uv[2][1],
                    ];
                    let (q0, q1, q2) = (b0 / z[0], b1 / z[1], b2 / z[2]);
                    let qs = q0 + q1 + q2;
                    let per = [
                        (q0 * uv[0][0] + q1 * uv[1][0] + q2 * uv[2][0]) / qs,
                        (q0 * uv[0][1] + q1 * uv[1][1] + q2 * uv[2][1]) / qs,
                    ];
                    let (du, dv) = (aff[0] - per[0], aff[1] - per[1]);
                    let (sx, sy) = ((jd * du - jb * dv) / jdet, (-jc * du + ja * dv) / jdet);
                    worst[t] = worst[t].max((sx * sx + sy * sy).sqrt().min(1000.0));
                }
            }
        }
    }
    worst
}

/// One level of crack-free edge subdivision: every edge of a flagged
/// triangle whose two vertices share a bone gets a bone-local midpoint
/// vertex; every triangle touching a split edge is re-triangulated
/// (1 -> 2, 3 or 4), UVs and the packed normal carried per corner.
pub fn subdivide(
    c: &Cooked,
    flag: &[bool],
    shade: Option<&[[u8; 3]]>,
) -> (Cooked, Option<Vec<[u8; 3]>>) {
    let vb = c.vert_bone();
    let key = |a: u16, b: u16| if a < b { (a, b) } else { (b, a) };
    let mut split: std::collections::BTreeMap<(u16, u16), u16> = std::collections::BTreeMap::new();
    let mut verts = c.verts.clone();
    let mut bones: Vec<(u16, u8, u8)> = vb.clone();
    for (t, tri) in c.tris.iter().enumerate() {
        if !flag[t] {
            continue;
        }
        for k in 0..3 {
            let (a, b) = (tri.idx[k], tri.idx[(k + 1) % 3]);
            if vb[a as usize] != vb[b as usize] {
                continue;
            }
            let e = key(a, b);
            if let std::collections::btree_map::Entry::Vacant(e) = split.entry(e) {
                let (pa, pb) = (verts[a as usize], verts[b as usize]);
                let m = [0, 1, 2].map(|i| ((pa[i] as i32 + pb[i] as i32 + 1).div_euclid(2)) as i16);
                e.insert(verts.len() as u16);
                verts.push(m);
                bones.push(vb[a as usize]);
            }
        }
    }
    type Corner = (u16, (u8, u8), u8);
    let mid = |a: Corner, b: Corner, m: u16| -> Corner {
        (
            m,
            (
                (a.1 .0 as u16 + b.1 .0 as u16).div_ceil(2) as u8,
                (a.1 .1 as u16 + b.1 .1 as u16).div_ceil(2) as u8,
            ),
            (a.2 as u16 + b.2 as u16).div_ceil(2) as u8,
        )
    };
    let mut tris = Vec::new();
    let mut shades: Vec<[u8; 3]> = Vec::new();
    for (ti, tri) in c.tris.iter().enumerate() {
        let v = tri.idx;
        let sh = shade.map(|s| s[ti]).unwrap_or([128; 3]);
        let cn: [Corner; 3] =
            std::array::from_fn(|k| (v[k], (tri.uv[k * 2], tri.uv[k * 2 + 1]), sh[k]));
        let mut push = |p: [Corner; 3]| {
            tris.push(crate::cooked::Tri {
                idx: [p[0].0, p[1].0, p[2].0],
                tex: tri.tex,
                uv: [
                    p[0].1 .0, p[0].1 .1, p[1].1 .0, p[1].1 .1, p[2].1 .0, p[2].1 .1,
                ],
                norm: tri.norm,
            });
            shades.push([p[0].2, p[1].2, p[2].2]);
        };
        let e = |k: usize| {
            split
                .get(&key(v[k], v[(k + 1) % 3]))
                .map(|&m| mid(cn[k], cn[(k + 1) % 3], m))
        };
        let (c0, c1, c2) = (cn[0], cn[1], cn[2]);
        match (e(0), e(1), e(2)) {
            (None, None, None) => push([c0, c1, c2]),
            (Some(m01), None, None) => {
                push([c0, m01, c2]);
                push([m01, c1, c2]);
            }
            (None, Some(m12), None) => {
                push([c0, c1, m12]);
                push([c0, m12, c2]);
            }
            (None, None, Some(m20)) => {
                push([c0, c1, m20]);
                push([m20, c1, c2]);
            }
            (Some(m01), Some(m12), None) => {
                push([m01, c1, m12]);
                push([c0, m01, m12]);
                push([c0, m12, c2]);
            }
            (None, Some(m12), Some(m20)) => {
                push([m12, c2, m20]);
                push([c0, c1, m12]);
                push([c0, m12, m20]);
            }
            (Some(m01), None, Some(m20)) => {
                push([c0, m01, m20]);
                push([m01, c1, c2]);
                push([m01, c2, m20]);
            }
            (Some(m01), Some(m12), Some(m20)) => {
                push([c0, m01, m20]);
                push([m01, c1, m12]);
                push([m20, m12, c2]);
                push([m01, m12, m20]);
            }
        }
    }
    // re-sort vertices by bone so ranges stay contiguous
    let mut order: Vec<usize> = (0..verts.len()).collect();
    order.sort_by_key(|&i| (bones[i].0, bones[i].1, i));
    let mut remap = vec![0u16; verts.len()];
    let mut nv = Vec::new();
    let mut ranges: Vec<crate::cooked::Range> = Vec::new();
    for (n, &o) in order.iter().enumerate() {
        remap[o] = n as u16;
        nv.push(verts[o]);
        let (bone, body, flags) = bones[o];
        match ranges.last_mut() {
            Some(r)
                if r.bone == bone
                    && r.body == body
                    && r.flags == flags
                    && r.first + r.count == n as u16 =>
            {
                r.count += 1
            }
            _ => ranges.push(crate::cooked::Range {
                first: n as u16,
                count: 1,
                bone,
                body,
                flags,
            }),
        }
    }
    for t in &mut tris {
        for i in &mut t.idx {
            *i = remap[*i as usize];
        }
    }
    let mut out = c.clone();
    out.verts = nv;
    out.ranges = ranges;
    out.tris = tris;
    (out, shade.map(|_| shades))
}

/// Subdivide triangles whose on-screen affine warp exceeds `thr` px, up to
/// `levels` times, staying inside the runtime's triangle/vertex caps.
pub fn subdiv_levels(
    c: &Cooked,
    shade: Option<Vec<[u8; 3]>>,
    thr: f64,
    levels: usize,
    tri_cap: usize,
) -> (Cooked, Option<Vec<[u8; 3]>>, usize) {
    let (mut c, mut shade) = (c.clone(), shade);
    let mut done = 0;
    for _ in 0..levels {
        let w = tri_warp(&c);
        let mut cand: Vec<usize> = (0..w.len()).filter(|&t| w[t] > thr).collect();
        if cand.is_empty() {
            break;
        }
        // worst warp first; if the whole level overflows the runtime caps,
        // take the largest prefix that fits
        cand.sort_by(|a, b| w[*b].partial_cmp(&w[*a]).unwrap());
        let try_n = |n: usize| {
            let mut flag = vec![false; w.len()];
            for &t in &cand[..n] {
                flag[t] = true;
            }
            subdivide(&c, &flag, shade.as_deref())
        };
        let fits =
            |x: &Cooked| x.tris.len() <= tri_cap.min(VM_TRI_CAP) && x.verts.len() <= VM_VERT_CAP;
        let full = try_n(cand.len());
        let next = if fits(&full.0) {
            full
        } else {
            let (mut lo, mut hi) = (0usize, cand.len());
            while lo < hi {
                let mid = (lo + hi).div_ceil(2);
                if fits(&try_n(mid).0) {
                    lo = mid
                } else {
                    hi = mid - 1
                }
            }
            if lo == 0 {
                break;
            }
            try_n(lo)
        };
        c = next.0;
        shade = next.1;
        done += 1;
    }
    (c, shade, done)
}
