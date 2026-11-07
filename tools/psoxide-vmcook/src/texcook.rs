//! Texture cooks: an exact port of today's `cook_mdl_tex` (nearest-texel
//! downscale, median cut, unsnapped nearest-colour assignment) and the
//! candidates (area-filtered resample, usage-weighted k-means in the 15-bit
//! colours the GPU displays).

use crate::cooked::Tex;
use crate::mdl;

pub const MAX_TEX: u32 = 64;

/// hl-bsp's final_size: largest power of two <= min(orig, MAX_TEX), in [8, MAX_TEX].
pub fn final_size(o: u32) -> u32 {
    let cap = o.min(MAX_TEX);
    let mut s = 8u32;
    while s * 2 <= cap {
        s *= 2;
    }
    s.clamp(8, MAX_TEX)
}

pub fn expand555(c: u16) -> [u8; 3] {
    let e = |v: u16| ((v << 3) | (v >> 2)) as u8;
    [e(c & 31), e((c >> 5) & 31), e((c >> 10) & 31)]
}

fn pack4(idx: &[u8]) -> Vec<u8> {
    idx.chunks(2)
        .map(|ch| ch[0] | (ch.get(1).copied().unwrap_or(0) << 4))
        .collect()
}

/// Exact area average of the piecewise-constant source over each target
/// texel's footprint (handles both down- and upscaling).
pub fn resample_box(src: &mdl::Tex, fw: usize, fh: usize) -> Vec<[f32; 3]> {
    let (w0, h0) = (src.w, src.h);
    let sx = w0 as f64 / fw as f64;
    let sy = h0 as f64 / fh as f64;
    let mut out = Vec::with_capacity(fw * fh);
    for y in 0..fh {
        let (ya, yb) = (y as f64 * sy, (y + 1) as f64 * sy);
        for x in 0..fw {
            let (xa, xb) = (x as f64 * sx, (x + 1) as f64 * sx);
            let mut acc = [0f64; 3];
            let mut wsum = 0.0;
            let mut yy = ya.floor() as usize;
            while (yy as f64) < yb && yy < h0 {
                let wy = (yb.min(yy as f64 + 1.0) - ya.max(yy as f64)).max(0.0);
                let mut xx = xa.floor() as usize;
                while (xx as f64) < xb && xx < w0 {
                    let wx = (xb.min(xx as f64 + 1.0) - xa.max(xx as f64)).max(0.0);
                    let p = src.pal[src.pix[yy * w0 + xx] as usize];
                    let ww = wx * wy;
                    for k in 0..3 {
                        acc[k] += p[k] as f64 * ww;
                    }
                    wsum += ww;
                    xx += 1;
                }
                yy += 1;
            }
            out.push([
                (acc[0] / wsum) as f32,
                (acc[1] / wsum) as f32,
                (acc[2] / wsum) as f32,
            ]);
        }
    }
    out
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn f(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn d2(a: [f32; 3], b: [f32; 3]) -> f32 {
    (a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)
}

/// Weighted k-means (k-means++ seeding), centroids snapped to the 15-bit
/// colours the GPU displays, final assignment in displayed colour.
pub fn kmeans(px: &[[f32; 3]], wt: &[f32], k: usize, seed: u64) -> (Vec<u16>, Vec<u8>) {
    let n = px.len();
    let mut rng = Rng(seed | 1);
    let mut cents: Vec<[f32; 3]> = Vec::new();
    // seed: weighted pick
    let tot: f64 = wt.iter().map(|&w| w as f64).sum();
    let mut pick = rng.f() * tot;
    let mut first = 0;
    for i in 0..n {
        pick -= wt[i] as f64;
        if pick <= 0.0 {
            first = i;
            break;
        }
    }
    cents.push(px[first]);
    let mut dist: Vec<f32> = px.iter().map(|p| d2(*p, cents[0])).collect();
    while cents.len() < k {
        let tot: f64 = dist.iter().zip(wt).map(|(d, w)| (*d * *w) as f64).sum();
        if tot <= 0.0 {
            break;
        }
        let mut pick = rng.f() * tot;
        let mut ch = n - 1;
        for i in 0..n {
            pick -= (dist[i] * wt[i]) as f64;
            if pick <= 0.0 {
                ch = i;
                break;
            }
        }
        cents.push(px[ch]);
        for i in 0..n {
            dist[i] = dist[i].min(d2(px[i], *cents.last().unwrap()));
        }
    }
    let mut lab = vec![0usize; n];
    for _ in 0..40 {
        let mut changed = false;
        for i in 0..n {
            let mut b = 0;
            let mut bd = f32::INFINITY;
            for (c, ce) in cents.iter().enumerate() {
                let d = d2(px[i], *ce);
                if d < bd {
                    bd = d;
                    b = c;
                }
            }
            if lab[i] != b {
                lab[i] = b;
                changed = true;
            }
        }
        let mut acc = vec![[0f64; 4]; cents.len()];
        for i in 0..n {
            let a = &mut acc[lab[i]];
            for k2 in 0..3 {
                a[k2] += (px[i][k2] * wt[i]) as f64;
            }
            a[3] += wt[i] as f64;
        }
        for (c, a) in acc.iter().enumerate() {
            if a[3] > 0.0 {
                cents[c] = [
                    (a[0] / a[3]) as f32,
                    (a[1] / a[3]) as f32,
                    (a[2] / a[3]) as f32,
                ];
            }
        }
        if !changed {
            break;
        }
    }
    // snap to 15-bit, dedupe, reassign in displayed colour
    let mut words: Vec<u16> = Vec::new();
    for c in &cents {
        let q = |v: f32| ((v.clamp(0.0, 255.0) * 31.0 / 255.0).round() as u16).min(31);
        let w = q(c[0]) | (q(c[1]) << 5) | (q(c[2]) << 10) | 0x8000;
        if !words.contains(&w) {
            words.push(w);
        }
    }
    let disp: Vec<[f32; 3]> = words
        .iter()
        .map(|&w| expand555(w).map(|v| v as f32))
        .collect();
    let idx: Vec<u8> = px
        .iter()
        .map(|p| {
            let mut b = 0;
            let mut bd = f32::INFINITY;
            for (c, d) in disp.iter().enumerate() {
                let e = d2(*p, *d);
                if e < bd {
                    bd = e;
                    b = c;
                }
            }
            b as u8
        })
        .collect();
    while words.len() < 16 {
        words.push(*words.last().unwrap());
    }
    (words, idx)
}

/// Candidate texture: area resample to (fw, fh), usage-weighted k-means.
/// `weight(x, y)` is the importance of target texel (x, y).
pub fn candidate(
    src: &mdl::Tex,
    fw: usize,
    fh: usize,
    weight: &dyn Fn(usize, usize) -> f32,
    seed: u64,
) -> Tex {
    let px = resample_box(src, fw, fh);
    let wt: Vec<f32> = (0..fw * fh).map(|i| weight(i % fw, i / fw)).collect();
    let (words, idx) = kmeans(&px, &wt, 16, seed);
    let mut clut = [0u16; 16];
    clut.copy_from_slice(&words[..16]);
    Tex {
        w: fw as u16,
        h: fh as u16,
        clut,
        pix4: pack4(&idx),
    }
}
