//! psoxide-vmcook: cook-time post-pass for camera-locked GoldSrc viewmodels
//! (HMD8 geometry + HLTX textures from hl-bsp `--mdl7-vm`).
//!
//! A held weapon is drawn through one fixed camera transform, so everything
//! it can ever put on screen is decidable at cook time. This tool replays the
//! runtime exactly: every (frame, frame2, frac16) the HMD8 clip phase
//! functions can return, psx_asset's pose interpolation, the games' GTE bone
//! composition and RTPT (psx-gte on psx-gte-core, bit-exact), psx-goldsrc's
//! viewmodel sort (near reject, NCLIP cull, 64 depth buckets) and the
//! emulator's rasterizer, over every projection height the game draws the
//! weapon at, on a frame widened by the procedural bob/recoil draw offset.
//!
//! `prune` keeps only triangles that are the last writer of at least one
//! pixel in some reachable pose, drops the vertices nothing references and
//! rebuilds the per-bone ranges. Triangle order is preserved, so the depth
//! buckets draw the survivors in the same order: the output renders
//! identically, which the tool re-renders every pose to verify (`--check`).
//!
//! `--fit-textures <mdl>` re-sizes every texture to the density the screen
//! samples it at, inside the texels the cook spends today (see fit.rs), and
//! re-cooks it the same way; a simplified cook, which no longer maps onto its
//! source triangles, falls back to `--shrink-textures`.
//!
//! `--shrink-textures <mdl>` additionally lowers each texture's width/height
//! (never raises it) to the power of two nearest the size at which the
//! 5th-percentile visible pixel samples one texel along that axis, re-cooks it
//! from the source MDL with an area-average filter and a 16-colour k-means
//! palette weighted toward the texels the weapon shows, and shifts the UVs.
//!
//! `--decimate <ratio>` (opt-in) reduces the mesh to about `ratio` of its
//! triangles by quadric edge collapse inside each rigid bone (decimate.rs),
//! dropping the vertices nothing references, and reports how many pixels of
//! the weapon's coverage change in the worst reachable pose.
//! `--decimate-report` prints that table for ratios 0.9 down to 0.3 without
//! changing the output. `--render <dir> --render-name <n> --render-poses
//! name:clip:key,...` writes game-size PPMs of the listed key poses before and
//! after decimation (contact-sheet input).
//!
//! `--subdivide <max_tris>` splits triangles whose affine texture warp exceeds
//! `--warp` pixels (default 1) at any key pose (two levels at most, worst
//! first, never past `max_tris` or the runtime caps), then prunes again.

#![allow(clippy::needless_range_loop)]

mod cooked;
mod decimate;
mod fit;
mod mdl;
mod runtime;
mod subdiv;
mod texcook;

use cooked::Cooked;
use std::collections::BTreeMap;

/// Procedural bob/recoil moves the weapon by a GPU draw offset of x -6..+6,
/// y -14..+4 px (HL recoil peaks at 20 -> 10 px plus 2 units of bob), so the
/// screen sees the unshifted image over x in [-6, W+6), y in [-4, H+14).
const MARGIN: (u16, u16, u16, u16) = (6, 4, 6, 14);

pub(crate) fn expanded(w: u16, hh: u16) -> ((u16, u16, u16, u16), (i16, i16)) {
    let (l, t, r, b) = MARGIN;
    (
        (0, 0, w + l + r, hh + t + b),
        ((l + w / 2) as i16, (t + hh / 2) as i16),
    )
}

struct Vis {
    max_px: Vec<u32>,
    used: Vec<Vec<bool>>,
    fu: Vec<Vec<(f32, u32)>>,
    fv: Vec<Vec<(f32, u32)>>,
}

fn tri_rates(xy: [[f64; 2]; 3], uv: [[f64; 2]; 3]) -> Option<(f64, f64)> {
    let (x1, y1, x2, y2) = (
        xy[1][0] - xy[0][0],
        xy[1][1] - xy[0][1],
        xy[2][0] - xy[0][0],
        xy[2][1] - xy[0][1],
    );
    let det = x1 * y2 - x2 * y1;
    if det.abs() < 1e-9 {
        return None;
    }
    let (u1, v1, u2, v2) = (
        uv[1][0] - uv[0][0],
        uv[1][1] - uv[0][1],
        uv[2][0] - uv[0][0],
        uv[2][1] - uv[0][1],
    );
    let a = (u1 * y2 - u2 * y1) / det;
    let b = (u2 * x1 - u1 * x2) / det;
    let c = (v1 * y2 - v2 * y1) / det;
    let d = (v2 * x1 - v1 * x2) / det;
    Some(((a * a + b * b).sqrt(), (c * c + d * d).sqrt()))
}

fn wpct(v: &[(f32, u32)], q: f64) -> f32 {
    if v.is_empty() {
        return f32::NAN;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let tot: f64 = s.iter().map(|x| x.1 as f64).sum();
    let mut acc = 0.0;
    for (x, w) in &s {
        acc += *w as f64;
        if acc >= q * tot {
            return *x;
        }
    }
    s.last().unwrap().0
}

fn visibility(c: &Cooked, views: &[(u16, u16, u16)]) -> Vis {
    let md = runtime::load_model(c);
    let poses = runtime::runtime_poses(c);
    let mut max_px = vec![0u32; c.tris.len()];
    let mut used: Vec<Vec<bool>> = c
        .texs
        .iter()
        .map(|t| vec![false; t.w as usize * t.h as usize])
        .collect();
    let mut fu: Vec<Vec<(f32, u32)>> = vec![Vec::new(); c.texs.len()];
    let mut fv: Vec<Vec<(f32, u32)>> = vec![Vec::new(); c.texs.len()];
    for &(h, w, hh) in views {
        let (rect, center) = expanded(w, hh);
        let mut fr = runtime::Frame::with_center(c, rect, center);
        let mut uvp = runtime::UvPass::new();
        for &pose in &poses {
            let p = runtime::project(&md, pose, h);
            let s = runtime::sort(&md, &p);
            fr.begin(0);
            fr.draw_ids(&p, &md, &s.order);
            uvp.draw(&md, c, &p, &s.order, fr.center, rect);
            let mut count: BTreeMap<usize, u32> = BTreeMap::new();
            for y in rect.1 as usize..(rect.1 + rect.3) as usize {
                for x in rect.0 as usize..(rect.0 + rect.2) as usize {
                    let id = fr.pixel(x, y) & 0x7fff;
                    if id == 0 {
                        continue;
                    }
                    let tri = s.order[id as usize - 1];
                    *count.entry(tri).or_default() += 1;
                    let tex = md.triangle(tri).tex;
                    if let Some((u, v)) = uvp.uv(x, y) {
                        let tx = &c.texs[tex];
                        let (u, v) = (u as usize % tx.w as usize, v as usize % tx.h as usize);
                        used[tex][v * tx.w as usize + u] = true;
                    }
                }
            }
            for (&t, &n) in &count {
                max_px[t] = max_px[t].max(n);
                let tri = md.triangle(t);
                let xy: [[f64; 2]; 3] = std::array::from_fn(|k| {
                    let q = p.xy[tri.idx[k] as usize];
                    [q[0] as f64, q[1] as f64]
                });
                let uv: [[f64; 2]; 3] =
                    std::array::from_fn(|k| [tri.uv[k].0 as f64, tri.uv[k].1 as f64]);
                if let Some((ru, rv)) = tri_rates(xy, uv) {
                    if ru > 1e-6 {
                        fu[tri.tex].push((ru as f32, n));
                    }
                    if rv > 1e-6 {
                        fv[tri.tex].push((rv as f32, n));
                    }
                }
            }
        }
    }
    Vis {
        max_px,
        used,
        fu,
        fv,
    }
}

/// Render every reachable pose of both models in every view and count
/// frames and pixels that differ.
fn compare_all(a: &Cooked, b: &Cooked, views: &[(u16, u16, u16)]) -> (usize, usize) {
    let (ma, mb) = (runtime::load_model(a), runtime::load_model(b));
    let poses = runtime::runtime_poses(a);
    let (mut bad_frames, mut bad_px) = (0, 0);
    let shade =
        |m: &psx_asset::hmd8::Model, i: usize| runtime::vm_normal_shade(m.triangle(i).normal);
    for &(h, w, hh) in views {
        let (rect, center) = expanded(w, hh);
        let mut fa = runtime::Frame::with_center(a, rect, center);
        let mut fb = runtime::Frame::with_center(b, rect, center);
        for &pose in &poses {
            let pa = runtime::project(&ma, pose, h);
            let sa = runtime::sort(&ma, &pa);
            let pb = runtime::project(&mb, pose, h);
            let sb = runtime::sort(&mb, &pb);
            fa.begin(0x3def);
            fa.draw_textured(&ma, &pa, &sa.order, |i| shade(&ma, i));
            fb.begin(0x3def);
            fb.draw_textured(&mb, &pb, &sb.order, |i| shade(&mb, i));
            let d = fa
                .rgb()
                .iter()
                .zip(fb.rgb().iter())
                .filter(|(x, y)| x != y)
                .count();
            if d > 0 {
                bad_frames += 1;
                bad_px += d;
            }
        }
    }
    (bad_frames, bad_px)
}

fn parse_views(s: &str) -> Vec<(u16, u16, u16)> {
    s.split(',')
        .map(|v| {
            let p: Vec<u16> = v
                .split('x')
                .map(|x| x.parse().expect("view HxWxH"))
                .collect();
            (p[0], p[1], p[2])
        })
        .collect()
}

/// Shrink minified textures by powers of two (to the size nearest one texel
/// per pixel at the 5th-percentile visible pixel), re-cook them from the MDL
/// and shift
/// the UVs to match: floor(floor(x) / 2^k) == floor(x / 2^k).
fn shrink_textures(c: &mut Cooked, vis: &Vis, mdl_path: &str) {
    let m = mdl::Mdl::load(mdl_path, 0).expect("source mdl");
    let mut slot_tex: Vec<usize> = Vec::new();
    for t in &m.tris {
        if !slot_tex.contains(&t.tex) {
            slot_tex.push(t.tex);
        }
    }
    for slot in 0..c.texs.len() {
        let (bw, bh) = (c.texs[slot].w as usize, c.texs[slot].h as usize);
        let shrink = |cur: usize, f: f32| -> usize {
            if !f.is_finite() || f <= 0.0 {
                return cur;
            }
            let want = (cur as f64 / f as f64).max(1.0).log2().round();
            (1usize << want.clamp(0.0, 12.0) as u32).min(cur).max(8)
        };
        let (w, h) = (
            shrink(bw, wpct(&vis.fu[slot], 0.05)),
            shrink(bh, wpct(&vis.fv[slot], 0.05)),
        );
        if (w, h) == (bw, bh) || slot >= slot_tex.len() {
            continue;
        }
        let used = &vis.used[slot];
        let weight = |x: usize, y: usize| {
            if used[(y * bh / h) * bw + x * bw / w] {
                1.0
            } else {
                0.1
            }
        };
        c.texs[slot] = texcook::candidate(
            &m.texs[slot_tex[slot]],
            w,
            h,
            &weight,
            0x9e37_79b9_7f4a_7c15 ^ slot as u64,
        );
        let (sx, sy) = ((bw / w).trailing_zeros(), (bh / h).trailing_zeros());
        for t in c.tris.iter_mut().filter(|t| t.tex as usize == slot) {
            for k in 0..3 {
                t.uv[k * 2] >>= sx;
                t.uv[k * 2 + 1] >>= sy;
            }
        }
    }
}

/// Worst key-pose affine warp and how many triangles exceed `thr` pixels.
fn warp_summary(c: &Cooked, thr: f64) -> (f64, usize) {
    let w = subdiv::tri_warp(c);
    (
        w.iter().cloned().fold(0.0, f64::max),
        w.iter().filter(|&&x| x > thr).count(),
    )
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let usage = "usage: psoxide-vmcook prune <in.hmd8> <in.hltx> <out.hmd8> <out.hltx> \
                 [--views HxWxH,...] [--check] [--fit-textures <src.mdl> | --shrink-textures <src.mdl>] \
                 [--subdivide <max_tris>] [--subdivide-extra <tris>] [--warp <px>]";
    if a.get(1).map(String::as_str) != Some("prune") || a.len() < 6 {
        eprintln!("{usage}");
        std::process::exit(2);
    }
    let flag = |name: &str| {
        a.iter()
            .position(|x| x == name)
            .and_then(|i| a.get(i + 1))
            .cloned()
    };
    let views = parse_views(&flag("--views").unwrap_or_else(|| "160x320x240".into()));
    let g = std::fs::read(&a[2]).expect("read geometry");
    let t = std::fs::read(&a[3]).expect("read textures");
    let c = Cooked::parse(&g, &t);
    let vis = visibility(&c, &views);
    let keep: Vec<bool> = vis.max_px.iter().map(|&n| n > 0).collect();
    let mut out = c.prune(&keep);
    if a.iter().any(|x| x == "--check") {
        let (bad_frames, bad_px) = compare_all(&c, &out, &views);
        if bad_frames > 0 {
            eprintln!("prune changed {bad_px} pixels in {bad_frames} frames");
            std::process::exit(1);
        }
    }
    let mut shrink_from = flag("--shrink-textures");
    if let Some(mdl_path) = flag("--fit-textures") {
        let m = mdl::Mdl::load(&mdl_path, 0).expect("source mdl");
        let slot_tex = fit::slot_textures(&m);
        match fit::source_tri_map(&m, &slot_tex, &c) {
            Some(map) => {
                let kept: Vec<usize> = map
                    .iter()
                    .zip(&keep)
                    .filter(|(_, &k)| k)
                    .map(|(&si, _)| si)
                    .collect();
                let rate_u: Vec<f32> = vis.fu.iter().map(|v| wpct(v, 0.05)).collect();
                let rate_v: Vec<f32> = vis.fv.iter().map(|v| wpct(v, 0.05)).collect();
                let st = fit::SlotStats {
                    rate_u: &rate_u,
                    rate_v: &rate_v,
                    used: &vis.used,
                };
                let sizes = fit::budget_sizes(&c, &m, &slot_tex, &st);
                fit::apply(&mut out, &c, &m, &slot_tex, &kept, &sizes, &st);
            }
            None => {
                eprintln!("vmcook: {} is not a plain reordering of its source mesh; shrinking textures only", a[2]);
                shrink_from = Some(mdl_path);
            }
        }
    }
    let mut vis = vis;
    let cap_abs = flag("--subdivide").map(|c| c.parse::<usize>().expect("--subdivide <max_tris>"));
    let cap_extra =
        flag("--subdivide-extra").map(|c| c.parse::<usize>().expect("--subdivide-extra <tris>"));
    if cap_abs.is_some() || cap_extra.is_some() {
        let cap = cap_abs
            .unwrap_or(usize::MAX)
            .min(cap_extra.map_or(usize::MAX, |e| out.tris.len() + e));
        let warp: f64 = flag("--warp").map_or(1.0, |w| w.parse().expect("--warp <px>"));
        let before = warp_summary(&out, warp);
        let (sub, _, levels) = subdiv::subdiv_levels(&out, None, warp, 2, cap);
        if levels > 0 {
            let v2 = visibility(&sub, &views);
            let keep: Vec<bool> = v2.max_px.iter().map(|&n| n > 0).collect();
            out = sub.prune(&keep);
            vis = visibility(&out, &views);
        }
        let after = warp_summary(&out, warp);
        println!(
            "vmcook: affine warp max {:.1} -> {:.1} px, triangles over {warp} px {} -> {} (cap {cap})",
            before.0, after.0, before.1, after.1
        );
    }
    if let Some(mdl_path) = shrink_from {
        shrink_textures(&mut out, &vis, &mdl_path);
    }
    if a.iter().any(|x| x == "--decimate-report") {
        for step in (3..=9).rev() {
            let r = step as f64 / 10.0;
            let d = decimate::decimate(&out, (out.tris.len() as f64 * r).ceil() as usize);
            let (worst, cov, mean) = decimate::silhouette(&out, &d, &views);
            println!(
                "vmcook: decimate {r:.1}: {} -> {} tris, {} -> {} verts, silhouette worst {worst} px of {cov}, mean {mean:.1} px",
                out.tris.len(), d.tris.len(), out.verts.len(), d.verts.len()
            );
        }
    }
    if let Some(r) = flag("--decimate") {
        let r: f64 = r.parse().expect("--decimate <ratio>");
        let d = decimate::decimate(&out, (out.tris.len() as f64 * r).ceil() as usize);
        let (worst, cov, mean) = decimate::silhouette(&out, &d, &views);
        println!(
            "vmcook: decimate {r}: {} -> {} tris, {} -> {} verts, silhouette worst {worst} px of {cov}, mean {mean:.1} px",
            out.tris.len(), d.tris.len(), out.verts.len(), d.verts.len()
        );
        if let Some(dir) = flag("--render") {
            let name = flag("--render-name").unwrap_or_else(|| "weapon".into());
            let spec = flag("--render-poses").unwrap_or_else(|| "idle:0:0".into());
            for item in spec.split(',') {
                let f: Vec<&str> = item.split(':').collect();
                let (clip, key): (usize, u32) = (f[1].parse().unwrap(), f[2].parse().unwrap());
                for (tag, mesh) in [("current", &out), ("decimated", &d)] {
                    let rgb = decimate::render(mesh, (clip, clip, key * 256), views[0]);
                    let (w, h) = (views[0].1 as usize, views[0].2 as usize);
                    let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
                    for px in rgb {
                        ppm.extend_from_slice(&px);
                    }
                    std::fs::write(format!("{dir}/{name}-{}-{tag}.ppm", f[0]), ppm)
                        .expect("write render");
                }
            }
        }
        out = d;
    }
    let (s0, s1) = (c.sizes(), out.sizes());
    println!(
        "vmcook: {} -> {} tris, {} -> {} verts, geometry {} -> {} B, textures {} -> {} B ({})",
        c.tris.len(),
        out.tris.len(),
        c.verts.len(),
        out.verts.len(),
        s0.geom(),
        s1.geom(),
        s0.tex(),
        s1.tex(),
        out.texs
            .iter()
            .map(|t| format!("{}x{}", t.w, t.h))
            .collect::<Vec<_>>()
            .join(" ")
    );
    std::fs::write(&a[4], out.geom_bytes()).expect("write geometry");
    std::fs::write(&a[5], out.tex_bytes()).expect("write textures");
}
