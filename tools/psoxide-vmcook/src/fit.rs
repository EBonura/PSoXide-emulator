//! `--fit-textures`: resize every held-weapon texture to the density the
//! screen actually samples it at, inside today's texel budget, and re-cook it
//! from the source MDL (area filter, usage-weighted k-means).
//!
//! Sizes start from the shrink-only rule (the power of two nearest one texel
//! per pixel at the 5th-percentile visible pixel, never above the cooked
//! size), then the most magnified axis anywhere on the weapon doubles, up to
//! the source size and 128, while the total stays within the texels the
//! weapon's cook spends today. Textures no surviving triangle uses are
//! dropped (slot 0, the two-sided sleeve, keeps its index as an 8x8 stub).
//! UVs are rebuilt from each triangle's source triangle with the cook's own
//! rules, which needs the unsimplified cook: [`source_tri_map`] returns
//! `None` for anything else, and the caller falls back to shrink-only.

use crate::cooked::{Cooked, Tex};
use crate::mdl::Mdl;
use crate::texcook;
use std::collections::HashMap;

pub const STUDIO_NF_CHROME: i32 = 0x0002;
const MAX_FIT: usize = 128;

/// Source texture of each cooked slot: slots are numbered in order of first
/// use by the MDL's triangles (hl-bsp `cook_mdl`).
pub fn slot_textures(m: &Mdl) -> Vec<usize> {
    let mut slot_tex: Vec<usize> = Vec::new();
    for t in &m.tris {
        if !slot_tex.contains(&t.tex) {
            slot_tex.push(t.tex);
        }
    }
    slot_tex
}

fn chrome_uv(n: [f64; 3], fw: f32, fh: f32) -> (u8, u8) {
    let u = (0.5 + (n[0] as f32).clamp(-1.0, 1.0) * 0.5) * (fw - 1.0).max(1.0);
    let v = (0.5 - (n[2] as f32).clamp(-1.0, 1.0) * 0.5) * (fh - 1.0).max(1.0);
    (
        u.round().clamp(0.0, 255.0) as u8,
        v.round().clamp(0.0, 255.0) as u8,
    )
}

/// One cooked triangle's UVs rebuilt from source triangle `si` for a
/// texture of size (fw, fh), with the cook's rules (corners reversed).
fn tri_uvs(m: &Mdl, si: usize, fw: u16, fh: u16) -> [u8; 6] {
    let st = &m.tris[si];
    let src = &m.texs[st.tex];
    let mut uv = [0u8; 6];
    for k in 0..3 {
        let corner = 2 - k;
        if src.flags & STUDIO_NF_CHROME != 0 {
            let (u, v) = chrome_uv(m.norms[st.n[corner]], fw as f32, fh as f32);
            uv[k * 2] = u;
            uv[k * 2 + 1] = v;
        } else {
            let (ss, tt) = (st.st[corner][0], st.st[corner][1]);
            uv[k * 2] = (ss * fw as f32 / src.w.max(1) as f32).clamp(0.0, 255.0) as u8;
            uv[k * 2 + 1] = (tt * fh as f32 / src.h.max(1) as f32).clamp(0.0, 255.0) as u8;
        }
    }
    uv
}

/// For every cooked triangle, the source triangle it came from. The cook
/// keeps every source vertex, ordered by (bone, index), and writes corners
/// (c, b, a). `None` when the cook is not a plain reordering of the source
/// (a simplified mesh) or its texture sizes / UVs do not reproduce.
pub fn source_tri_map(m: &Mdl, slot_tex: &[usize], c: &Cooked) -> Option<Vec<usize>> {
    if c.verts.len() != m.verts.len() || c.texs.len() != slot_tex.len() {
        return None;
    }
    for (slot, &t) in slot_tex.iter().enumerate() {
        let src = &m.texs[t];
        let (fw, fh) = (
            texcook::final_size(src.w as u32) as u16,
            texcook::final_size(src.h as u32) as u16,
        );
        if (c.texs[slot].w, c.texs[slot].h) != (fw, fh) {
            return None;
        }
    }
    let mut order: Vec<usize> = (0..m.verts.len()).collect();
    order.sort_by_key(|&v| (m.vbone[v], v));
    let mut vremap = vec![0u16; m.verts.len()];
    for (n, &o) in order.iter().enumerate() {
        vremap[o] = n as u16;
    }
    let mut by_key: HashMap<([u16; 3], u16), Vec<usize>> = HashMap::new();
    for (i, t) in m.tris.iter().enumerate() {
        let slot = slot_tex.iter().position(|&x| x == t.tex)? as u16;
        let k = ([vremap[t.v[2]], vremap[t.v[1]], vremap[t.v[0]]], slot);
        by_key.entry(k).or_default().push(i);
    }
    let mut used = vec![false; m.tris.len()];
    let mut out = Vec::with_capacity(c.tris.len());
    for t in &c.tris {
        let cands = by_key.get(&(t.idx, t.tex))?;
        let si = *cands.iter().find(|&&i| !used[i]).or(cands.first())?;
        used[si] = true;
        let tex = &c.texs[t.tex as usize];
        if tri_uvs(m, si, tex.w, tex.h) != t.uv {
            return None;
        }
        out.push(si);
    }
    Some(out)
}

/// Per-slot screen statistics from the visibility pass (see main.rs).
pub struct SlotStats<'a> {
    /// 5th-percentile texels per pixel along u / v at the cooked size.
    pub rate_u: &'a [f32],
    pub rate_v: &'a [f32],
    /// Texels (at the cooked size) any visible pixel samples.
    pub used: &'a [Vec<bool>],
}

fn pow2_round(x: f64) -> usize {
    1usize << (x.max(1.0).log2().round() as i32).clamp(0, 12)
}

/// Fitted size of every slot, inside the cooked texel total.
pub fn budget_sizes(base: &Cooked, m: &Mdl, slot_tex: &[usize], st: &SlotStats) -> Vec<(u16, u16)> {
    let mut sizes: Vec<(u16, u16)> = base.texs.iter().map(|t| (t.w, t.h)).collect();
    for (slot, size) in sizes.iter_mut().enumerate() {
        let (bw, bh) = (base.texs[slot].w as f64, base.texs[slot].h as f64);
        let want = |cur: f64, f: f32| {
            if f.is_finite() && f > 0.0 {
                pow2_round(cur / f as f64)
            } else {
                cur as usize
            }
        };
        let w = want(bw, st.rate_u[slot]).min(bw as usize).max(8);
        let h = want(bh, st.rate_v[slot]).min(bh as usize).max(8);
        *size = (w as u16, h as u16);
    }
    let budget: usize = base.texs.iter().map(|t| t.w as usize * t.h as usize).sum();
    loop {
        let cur: usize = sizes.iter().map(|&(w, h)| w as usize * h as usize).sum();
        let mut best: Option<(f64, usize, bool)> = None;
        for (slot, &(w, h)) in sizes.iter().enumerate() {
            let src = &m.texs[slot_tex[slot]];
            let (bw, bh) = (base.texs[slot].w as f64, base.texs[slot].h as f64);
            for (axis, rate, cur_sz, cap) in [
                (
                    false,
                    st.rate_u[slot] as f64 * w as f64 / bw,
                    w,
                    src.w.max(1).next_power_of_two().min(MAX_FIT),
                ),
                (
                    true,
                    st.rate_v[slot] as f64 * h as f64 / bh,
                    h,
                    src.h.max(1).next_power_of_two().min(MAX_FIT),
                ),
            ] {
                if rate.is_finite()
                    && rate < 0.75
                    && (cur_sz as usize) < cap
                    && best.is_none_or(|b| rate < b.0)
                {
                    best = Some((rate, slot, axis));
                }
            }
        }
        let Some((_, slot, axis)) = best else { break };
        let (w, h) = sizes[slot];
        if cur + w as usize * h as usize > budget {
            break;
        }
        sizes[slot] = if axis { (w, h * 2) } else { (w * 2, h) };
    }
    sizes
}

/// Re-cook `c`'s textures at `sizes` and rebuild its UVs. `tri_src[t]` is
/// the source triangle of `c.tris[t]`; `st.used` refers to `base`'s sizes.
pub fn apply(
    c: &mut Cooked,
    base: &Cooked,
    m: &Mdl,
    slot_tex: &[usize],
    tri_src: &[usize],
    sizes: &[(u16, u16)],
    st: &SlotStats,
) {
    let mut used_slot = vec![false; c.texs.len()];
    for t in &c.tris {
        used_slot[t.tex as usize] = true;
    }
    let mut remap = vec![u16::MAX; c.texs.len()];
    let mut texs: Vec<Tex> = Vec::new();
    for slot in 0..c.texs.len() {
        if !used_slot[slot] && slot != 0 {
            continue;
        }
        remap[slot] = texs.len() as u16;
        let src = &m.texs[slot_tex[slot]];
        texs.push(if !used_slot[slot] {
            texcook::candidate(src, 8, 8, &|_, _| 1.0, 1)
        } else {
            let (w, h) = (sizes[slot].0 as usize, sizes[slot].1 as usize);
            let (bw, bh) = (base.texs[slot].w as usize, base.texs[slot].h as usize);
            let used = &st.used[slot];
            let weight = move |x: usize, y: usize| -> f32 {
                if used[(y * bh / h) * bw + x * bw / w] {
                    1.0
                } else {
                    0.1
                }
            };
            texcook::candidate(src, w, h, &weight, 0x9e37_79b9_7f4a_7c15 ^ slot as u64)
        });
    }
    for (t, &si) in c.tris.iter_mut().zip(tri_src) {
        let slot = t.tex as usize;
        t.uv = tri_uvs(m, si, sizes[slot].0, sizes[slot].1);
        t.tex = remap[slot];
    }
    c.texs = texs;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cooked::{Cooked, Tex as CookedTex};
    use crate::mdl::{Mdl, Tex as SrcTex};

    fn cooked(sizes: &[(u16, u16)]) -> Cooked {
        Cooked {
            n_frames: 1,
            tex_hit: 0,
            l2w: 4096,
            flags: 0,
            n_bones: 1,
            clips: vec![],
            frame_times: vec![],
            ranges: vec![],
            verts: vec![],
            poses: vec![],
            tail: vec![],
            tris: vec![],
            texs: sizes
                .iter()
                .map(|&(w, h)| CookedTex {
                    w,
                    h,
                    clut: [0; 16],
                    pix4: vec![0; w as usize * h as usize / 2],
                })
                .collect(),
        }
    }

    fn source(sizes: &[(usize, usize)]) -> Mdl {
        Mdl {
            verts: vec![],
            vbone: vec![],
            norms: vec![],
            tris: vec![],
            texs: sizes
                .iter()
                .map(|&(w, h)| SrcTex {
                    flags: 0,
                    w,
                    h,
                    pix: vec![0; w * h],
                    pal: vec![[0; 3]; 256],
                })
                .collect(),
        }
    }

    #[test]
    fn budget_grows_the_most_magnified_texture_without_spending_more_texels() {
        // Slot 0 is magnified 4x on screen, slot 1 (four times the texels)
        // is minified 4x: its savings pay for slot 0's growth.
        let base = cooked(&[(64, 64), (128, 128)]);
        let m = source(&[(256, 256), (256, 256)]);
        let used = vec![vec![true; 128 * 128]; 2];
        let st = SlotStats {
            rate_u: &[0.25, 4.0],
            rate_v: &[0.25, 4.0],
            used: &used,
        };
        let sizes = budget_sizes(&base, &m, &[0, 1], &st);
        assert_eq!(sizes[1], (32, 32), "minified slot shrinks to its density");
        assert!(
            sizes[0].0 > 64 || sizes[0].1 > 64,
            "magnified slot grows: {sizes:?}"
        );
        let texels: usize = sizes.iter().map(|&(w, h)| w as usize * h as usize).sum();
        assert!(texels <= 64 * 64 + 128 * 128, "{texels} texels");
        assert!(sizes[0].0 <= 128 && sizes[0].1 <= 128);
    }

    #[test]
    fn budget_never_exceeds_the_source_resolution() {
        let base = cooked(&[(64, 32)]);
        let m = source(&[(80, 40)]);
        let used = vec![vec![true; 64 * 32]];
        let st = SlotStats {
            rate_u: &[0.1],
            rate_v: &[0.1],
            used: &used,
        };
        // Growing would need 128x64 (> the 80x40 source's power of two), and
        // the budget is the slot's own texels: nothing changes.
        assert_eq!(budget_sizes(&base, &m, &[0], &st), vec![(64, 32)]);
    }
}
