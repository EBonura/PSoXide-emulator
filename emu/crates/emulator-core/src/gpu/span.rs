// ======================================================================
// Span renderers: the per-row pixel loops behind the triangle rasterisers
// ======================================================================
//
// Each primitive is set up once (coverage from `raster::tri_raster_setup`,
// texture and plot state copied out of the `Gpu`), then drawn row by row
// with a loop specialised by const generics for the texture depth, the
// kind of shading and dithering. The per-pixel arithmetic is exactly that
// of the generic helpers in `blend.rs` (`modulate_tint`,
// `modulate_tint_dithered`, `dither_rgb`, `blend_pixel`), and pixels are
// visited in the same order, so VRAM ends up bit-identical.

use super::blend::{
    blend_pixel, dither_rgb, modulate_tint, modulate_tint_dithered, BlendMode, DITHER_OFFSETS,
};
use super::raster::{tri_span_x, TriRasterSetup};
use crate::vram::{VRAM_HEIGHT, VRAM_WIDTH};

/// Halfwords of VRAM.
pub(super) const VRAM_LEN: usize = VRAM_WIDTH * VRAM_HEIGHT;

/// Texture depth selectors for the span loops.
pub(super) const DEPTH_4: u8 = 0;
pub(super) const DEPTH_8: u8 = 1;
pub(super) const DEPTH_15: u8 = 2;

/// Shading selectors: texel passed through unchanged, modulated by the
/// primitive's flat tint, or by the interpolated vertex colour.
pub(super) const SHADE_RAW: u8 = 0;
pub(super) const SHADE_FLAT: u8 = 1;
pub(super) const SHADE_GOURAUD: u8 = 2;

/// The drawing area, inclusive on all four edges.
#[derive(Clone, Copy)]
pub(super) struct Clip {
    pub top: i32,
    pub bottom: i32,
    pub left: i32,
    pub right: i32,
}

/// Visit the clipped, non-empty spans of a set-up triangle, top to bottom:
/// `span(y, xs, xe)` covers `xs..xe` (right-exclusive).
#[inline(always)]
pub(super) fn tri_spans(setup: &TriRasterSetup, clip: Clip, mut span: impl FnMut(i32, i32, i32)) {
    for &(y0, y1, mut lx, ls, mut rx, rs) in &setup.parts {
        // Rows above the drawing area only step the edges: jump them in
        // one go (exact, the edges are integers), and stop at its bottom.
        let mut y = y0;
        if y < clip.top {
            let skip = i64::from(clip.top.min(y1) - y);
            lx += ls * skip;
            rx += rs * skip;
            y += skip as i32;
        }
        let y1 = y1.min(clip.bottom + 1);
        while y < y1 {
            let xs = tri_span_x(lx).max(clip.left);
            let xe = tri_span_x(rx).min(clip.right + 1);
            if xs < xe {
                span(y, xs, xe);
            }
            lx += ls;
            rx += rs;
            y += 1;
        }
    }
}

/// Texture-page addressing for one primitive, with the texture window
/// folded into an AND/OR pair per axis.
#[derive(Clone, Copy)]
pub(super) struct TexFetch {
    pub and_u: u32,
    pub or_u: u32,
    pub and_v: u32,
    pub or_v: u32,
    pub page_x: u32,
    pub page_y: u32,
}

impl TexFetch {
    /// Whether the texture page (the VRAM a fetch can read, `depth` 0, 1
    /// or 2 for 4, 8 or 15 bpp) intersects the rectangle `x0..=x1`,
    /// `y0..=y1`.
    pub(super) fn page_overlaps(&self, depth: u8, x0: i32, y0: i32, x1: i32, y1: i32) -> bool {
        let width = 64i32 << depth.min(2);
        let (px, py) = (self.page_x as i32, self.page_y as i32);
        let rows = py <= y1 && y0 < py + 256;
        // Columns wrap at the right edge of VRAM.
        let cols = |a: i32, b: i32| a <= x1 && x0 < b;
        let end = px + width;
        rows && (cols(px, end.min(VRAM_WIDTH as i32)) || cols(0, end - VRAM_WIDTH as i32))
    }

    /// The resolved 16-bit texel at 8-bit `(u, v)`; 0 means transparent.
    #[inline(always)]
    pub(super) fn texel<const D: u8>(
        &self,
        vram: &[u16; VRAM_LEN],
        clut: &[u16; 256],
        u: u32,
        v: u32,
    ) -> u16 {
        let u = (u & self.and_u) | self.or_u;
        let v = (v & self.and_v) | self.or_v;
        let row = (((self.page_y + v) as usize) & (VRAM_HEIGHT - 1)) * VRAM_WIDTH;
        let at = |x: u32| vram[(row + ((x as usize) & (VRAM_WIDTH - 1))) & (VRAM_LEN - 1)];
        match D {
            DEPTH_4 => {
                let word = at(self.page_x + (u >> 2));
                clut[((word >> ((u & 3) * 4)) & 0xF) as usize]
            }
            DEPTH_8 => {
                let word = at(self.page_x + (u >> 1));
                clut[((word >> ((u & 1) * 8)) & 0xFF) as usize]
            }
            _ => at(self.page_x + u),
        }
    }
}

/// How pixels land in VRAM: the mask-bit settings and the pixel tracer.
pub(super) struct Plotter<'a> {
    pub mask_check: bool,
    pub mask_or: u16,
    pub owner: Option<&'a mut Vec<u32>>,
    pub cmd_index: u32,
}

impl Plotter<'_> {
    /// The mask-bit settings for the chunked (untraced) loops.
    pub(super) fn merge(&self) -> Merge {
        Merge {
            mask_check: self.mask_check,
            mask_or: self.mask_or,
        }
    }

    /// Same result as `Gpu::plot_pixel_with` for an in-range pixel.
    #[inline(always)]
    pub(super) fn put(&mut self, vram: &mut [u16; VRAM_LEN], idx: usize, fg: u16, mode: BlendMode) {
        let idx = idx & (VRAM_LEN - 1);
        let existing = vram[idx];
        if self.mask_check && existing & 0x8000 != 0 {
            return;
        }
        let pixel = if mode == BlendMode::Opaque {
            fg
        } else {
            blend_pixel(existing, fg, mode)
        };
        vram[idx] = pixel | self.mask_or;
        if let Some(owner) = self.owner.as_deref_mut() {
            owner[idx] = self.cmd_index;
        }
    }
}

/// Per-primitive inputs of a textured triangle.
pub(super) struct TexTri {
    pub tint: (u32, u32, u32),
    pub semi: bool,
    pub blend: BlendMode,
}

/// Pixels per chunk of the chunked span loops. Eight 16-bit pixels are
/// one 128-bit vector; the per-lane arithmetic below is written as plain
/// loops over fixed-size arrays, which the compiler turns into SIMD on
/// every target that has it (NEON, SSE2, wasm simd128) and scalar code
/// elsewhere.
const LANES: usize = 8;

/// `base + i * step` for each lane, in wrapping u32 arithmetic.
#[inline(always)]
fn lanes(base: u32, step: u32) -> [u32; LANES] {
    core::array::from_fn(|i| base.wrapping_add(step.wrapping_mul(i as u32)))
}

#[inline(always)]
fn advance(acc: &mut [u32; LANES], step: u32) {
    let step = step.wrapping_mul(LANES as u32);
    for a in acc.iter_mut() {
        *a = a.wrapping_add(step);
    }
}

/// The dither offsets of `LANES` consecutive pixels of row `y` from `x`.
/// The matrix repeats every four pixels, so this holds for every chunk of
/// a span that starts at `x`.
#[inline(always)]
fn dither_lanes(x: i32, y: i32) -> [i32; LANES] {
    core::array::from_fn(|i| DITHER_OFFSETS[((y & 3) * 4 + ((x + i as i32) & 3)) as usize])
}

/// `modulate_tint` / `modulate_tint_dithered` over a chunk of texels (the
/// texel's mask bit is kept); a raw texel passes through. The tint is the
/// flat `tint`, or for `SHADE_GOURAUD` the top bytes of `r`, `g`, `b`.
#[inline(always)]
fn shade_lanes<const SHADE: u8, const DITHER: bool>(
    texel: &[u16; LANES],
    rgb: &[[u32; LANES]; 3],
    tint: (u32, u32, u32),
    doff: &[i32; LANES],
) -> [u16; LANES] {
    if SHADE == SHADE_RAW {
        return *texel;
    }
    // 16-bit lanes throughout: a tint (<= 255) times an 8-bit texel
    // channel (<= 248) fits in u16, and the dithered sum (-4..=258) in i16.
    let mut k = [[0u16; LANES]; 3];
    for c in 0..3 {
        for i in 0..LANES {
            k[c][i] = if SHADE == SHADE_GOURAUD {
                (rgb[c][i] >> 24) as u16
            } else {
                [tint.0, tint.1, tint.2][c] as u16
            };
        }
    }
    let mut out = [0u16; LANES];
    for i in 0..LANES {
        let t = texel[i];
        let ch = |c: usize, t5: u16| -> u16 {
            if DITHER {
                let m = ((k[c][i] * (t5 << 3)) >> 7).min(0xFF) as i16;
                ((m + doff[i] as i16).clamp(0, 255) >> 3) as u16
            } else {
                ((k[c][i] * t5) >> 7).min(0x1F)
            }
        };
        let c = ch(0, t & 0x1F) | (ch(1, (t >> 5) & 0x1F) << 5) | (ch(2, (t >> 10) & 0x1F) << 10);
        out[i] = c | (t & 0x8000);
    }
    out
}

/// Per-lane `blend_pixel(bg, fg, mode)`: the result keeps `fg`'s mask bit.
#[inline(always)]
fn blend_lanes(bg: &[u16; LANES], fg: &[u16; LANES], mode: BlendMode) -> [u16; LANES] {
    let mut out = *fg;
    let ch = |v: u16, shift: u16| (v >> shift) & 0x1F;
    macro_rules! each {
        ($op:expr) => {
            for i in 0..LANES {
                let (b, f) = (bg[i], fg[i]);
                let op = $op;
                out[i] = op(ch(b, 0), ch(f, 0))
                    | (op(ch(b, 5), ch(f, 5)) << 5)
                    | (op(ch(b, 10), ch(f, 10)) << 10)
                    | (f & 0x8000);
            }
        };
    }
    match mode {
        BlendMode::Opaque => {}
        BlendMode::Average => each!(|b: u16, f: u16| (b + f) >> 1),
        BlendMode::Add => each!(|b: u16, f: u16| (b + f).min(31)),
        BlendMode::Sub => each!(|b: u16, f: u16| b.saturating_sub(f)),
        BlendMode::AddQuarter => each!(|b: u16, f: u16| (b + (f >> 2)).min(31)),
    }
    out
}

/// Run `f` on the `n` (at most `LANES`) pixels of VRAM from `at` as one
/// chunk. Lanes past `n` start as zero and are not stored back.
#[inline(always)]
fn with_chunk(vram: &mut [u16; VRAM_LEN], at: usize, n: usize, f: impl FnOnce(&mut [u16; LANES])) {
    if n == LANES {
        f((&mut vram[at..at + LANES]).try_into().unwrap());
    } else {
        let mut buf = [0u16; LANES];
        for (i, b) in buf.iter_mut().enumerate().take(n) {
            *b = vram[(at + i) & (VRAM_LEN - 1)];
        }
        f(&mut buf);
        for (i, &b) in buf.iter().enumerate().take(n) {
            vram[(at + i) & (VRAM_LEN - 1)] = b;
        }
    }
}

/// How the pixels of an untraced primitive land in VRAM: `plain` is an
/// opaque primitive without the mask test (plain stores), otherwise the
/// mask test and blending apply per pixel as in `Plotter::put`.
#[derive(Clone, Copy)]
pub(super) struct Merge {
    pub mask_check: bool,
    pub mask_or: u16,
}

/// One span of an untraced textured primitive whose texture page does not
/// overlap the pixels it draws, in chunks of `LANES`: fetch the texels,
/// shade them all, then merge the opaque ones into VRAM. With no overlap,
/// fetching a chunk before storing it reads the same texels as the
/// pixel-at-a-time order. `GENERAL` adds semi-transparency and the mask
/// test; without it the primitive is opaque and unmasked.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn tex_span_chunked<const D: u8, const SHADE: u8, const DITHER: bool, const GENERAL: bool>(
    vram: &mut [u16; VRAM_LEN],
    clut: &[u16; 256],
    tex: &TexFetch,
    (y, xs, xe): (i32, i32, i32),
    start: [u32; 5],
    step: [u32; 5],
    prim: &TexTri,
    merge: Merge,
) {
    let mut rgb = if SHADE == SHADE_GOURAUD {
        [
            lanes(start[0], step[0]),
            lanes(start[1], step[1]),
            lanes(start[2], step[2]),
        ]
    } else {
        [[0; LANES]; 3]
    };
    let (mut u, mut v) = (start[3], start[4]);
    let (du, dv) = (step[3], step[4]);
    let doff = if DITHER {
        dither_lanes(xs, y)
    } else {
        [0; LANES]
    };
    let mask_or = merge.mask_or;
    let row = y as usize * VRAM_WIDTH;
    let (mut x, end) = (xs as usize, xe as usize);
    while x < end {
        let n = (end - x).min(LANES);
        let mut texel = [0u16; LANES];
        for t in texel.iter_mut().take(n) {
            *t = tex.texel::<D>(vram, clut, u >> 24, v >> 24);
            u = u.wrapping_add(du);
            v = v.wrapping_add(dv);
        }
        let out = shade_lanes::<SHADE, DITHER>(&texel, &rgb, prim.tint, &doff);
        let at = row + x;
        if GENERAL {
            with_chunk(vram, at, n, |dst| {
                let blended = if prim.semi {
                    blend_lanes(dst, &out, prim.blend)
                } else {
                    out
                };
                for i in 0..LANES {
                    let (d, t) = (dst[i], texel[i]);
                    let skip = t == 0 || (merge.mask_check && d & 0x8000 != 0);
                    let px = if prim.semi && t & 0x8000 != 0 {
                        blended[i]
                    } else {
                        out[i]
                    };
                    dst[i] = if skip { d } else { px | mask_or };
                }
            });
        } else if n == LANES {
            // A whole chunk inside the row: merge the opaque lanes into
            // the destination and store all of it back.
            let dst: &mut [u16; LANES] = (&mut vram[at..at + LANES]).try_into().unwrap();
            for i in 0..LANES {
                dst[i] = if texel[i] != 0 {
                    out[i] | mask_or
                } else {
                    dst[i]
                };
            }
        } else {
            for i in 0..n {
                if texel[i] != 0 {
                    vram[(at + i) & (VRAM_LEN - 1)] = out[i] | mask_or;
                }
            }
        }
        if SHADE == SHADE_GOURAUD {
            for (acc, &s) in rgb.iter_mut().zip(&step[..3]) {
                advance(acc, s);
            }
        }
        x += LANES;
    }
}

/// The attribute planes of a set-up triangle, evaluated at a pixel.
#[inline(always)]
fn plane_at(p: (u32, u32, u32), x: i32, y: i32) -> u32 {
    p.2.wrapping_add((x as u32).wrapping_mul(p.0))
        .wrapping_add((y as u32).wrapping_mul(p.1))
}

/// Draw a set-up textured triangle, untraced, with a texture page clear of
/// the pixels it draws (see `tex_span_chunked`). `SHADE` picks the texel
/// modulation (for `SHADE_GOURAUD` the colour planes of `setup` supply
/// the tint), `DITHER` the dithered variant of it.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(super) fn tex_tri<const D: u8, const SHADE: u8, const DITHER: bool, const GENERAL: bool>(
    vram: &mut [u16; VRAM_LEN],
    clut: &[u16; 256],
    setup: &TriRasterSetup,
    clip: Clip,
    tex: TexFetch,
    prim: &TexTri,
    merge: Merge,
) {
    let [pr, pg, pb, pu, pv] = setup.planes;
    let step = [pr.0, pg.0, pb.0, pu.0, pv.0];
    tri_spans(setup, clip, |y, xs, xe| {
        let start = [
            plane_at(pr, xs, y),
            plane_at(pg, xs, y),
            plane_at(pb, xs, y),
            plane_at(pu, xs, y),
            plane_at(pv, xs, y),
        ];
        tex_span_chunked::<D, SHADE, DITHER, GENERAL>(
            vram,
            clut,
            &tex,
            (y, xs, xe),
            start,
            step,
            prim,
            merge,
        );
    });
}

/// Draw a set-up textured triangle one pixel at a time, in exactly the
/// hardware order. For the rare cases the chunked loop cannot take: the
/// pixel tracer, or a texture page that overlaps the pixels drawn (a
/// pixel may then read a texel an earlier pixel of the span just wrote).
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(super) fn tex_tri_exact(
    vram: &mut [u16; VRAM_LEN],
    clut: &[u16; 256],
    plot: &mut Plotter<'_>,
    setup: &TriRasterSetup,
    clip: Clip,
    tex: TexFetch,
    (depth, shade, dither): (u8, u8, bool),
    prim: &TexTri,
) {
    let [pr, pg, pb, pu, pv] = setup.planes;
    let (tr, tg, tb) = prim.tint;
    tri_spans(setup, clip, |y, xs, xe| {
        let (mut u, mut v) = (plane_at(pu, xs, y), plane_at(pv, xs, y));
        let (mut r, mut g, mut b) = (
            plane_at(pr, xs, y),
            plane_at(pg, xs, y),
            plane_at(pb, xs, y),
        );
        let row = y as usize * VRAM_WIDTH;
        for x in xs..xe {
            let texel = match depth {
                0 => tex.texel::<DEPTH_4>(vram, clut, u >> 24, v >> 24),
                1 => tex.texel::<DEPTH_8>(vram, clut, u >> 24, v >> 24),
                _ => tex.texel::<DEPTH_15>(vram, clut, u >> 24, v >> 24),
            };
            if texel != 0 {
                let (kr, kg, kb) = if shade == SHADE_GOURAUD {
                    (r >> 24, g >> 24, b >> 24)
                } else {
                    (tr, tg, tb)
                };
                let shaded = if shade == SHADE_RAW {
                    texel
                } else if dither {
                    modulate_tint_dithered(texel, kr, kg, kb, x, y)
                } else {
                    modulate_tint(texel, kr, kg, kb)
                };
                let mode = if prim.semi && texel & 0x8000 != 0 {
                    prim.blend
                } else {
                    BlendMode::Opaque
                };
                plot.put(vram, row + x as usize, shaded, mode);
            }
            u = u.wrapping_add(pu.0);
            v = v.wrapping_add(pv.0);
            r = r.wrapping_add(pr.0);
            g = g.wrapping_add(pg.0);
            b = b.wrapping_add(pb.0);
        }
    });
}

/// Draw a set-up Gouraud (untextured) triangle, untraced, in chunks of
/// `LANES`.
#[inline(never)]
pub(super) fn shaded_tri<const DITHER: bool>(
    vram: &mut [u16; VRAM_LEN],
    setup: &TriRasterSetup,
    clip: Clip,
    mode: BlendMode,
    merge: Merge,
) {
    let [pr, pg, pb, _, _] = setup.planes;
    let plain = mode == BlendMode::Opaque && !merge.mask_check;
    tri_spans(setup, clip, |y, xs, xe| {
        let mut rgb = [
            lanes(plane_at(pr, xs, y), pr.0),
            lanes(plane_at(pg, xs, y), pg.0),
            lanes(plane_at(pb, xs, y), pb.0),
        ];
        let doff = if DITHER {
            dither_lanes(xs, y)
        } else {
            [0; LANES]
        };
        let row = y as usize * VRAM_WIDTH;
        let (mut x, end) = (xs as usize, xe as usize);
        while x < end {
            let n = (end - x).min(LANES);
            // `dither_rgb` / the plain 24-to-15-bit truncation per lane.
            let mut fg = [0u16; LANES];
            for (i, f) in fg.iter_mut().enumerate() {
                let ch = |c: usize| {
                    let v = (rgb[c][i] >> 24) as i16;
                    if DITHER {
                        ((v + doff[i] as i16).clamp(0, 255) >> 3) as u16
                    } else {
                        (v >> 3) as u16
                    }
                };
                *f = ch(0) | (ch(1) << 5) | (ch(2) << 10);
            }
            merge_flat_lanes(vram, row + x, n, &fg, plain, mode, merge);
            for (acc, s) in rgb.iter_mut().zip([pr.0, pg.0, pb.0]) {
                advance(acc, s);
            }
            x += LANES;
        }
    });
}

/// Store an untextured chunk: every lane of it is drawn, subject to the
/// mask test, blended by `mode`.
#[inline(always)]
fn merge_flat_lanes(
    vram: &mut [u16; VRAM_LEN],
    at: usize,
    n: usize,
    fg: &[u16; LANES],
    plain: bool,
    mode: BlendMode,
    merge: Merge,
) {
    let mask_or = merge.mask_or;
    if plain {
        if n == LANES {
            let dst: &mut [u16; LANES] = (&mut vram[at..at + LANES]).try_into().unwrap();
            for i in 0..LANES {
                dst[i] = fg[i] | mask_or;
            }
        } else {
            for (i, &f) in fg.iter().enumerate().take(n) {
                vram[(at + i) & (VRAM_LEN - 1)] = f | mask_or;
            }
        }
        return;
    }
    with_chunk(vram, at, n, |dst| {
        let px = blend_lanes(dst, fg, mode);
        for i in 0..LANES {
            let d = dst[i];
            let skip = merge.mask_check && d & 0x8000 != 0;
            dst[i] = if skip { d } else { px[i] | mask_or };
        }
    });
}

/// Draw a set-up flat (untextured) triangle, untraced.
#[inline(never)]
pub(super) fn flat_tri(
    vram: &mut [u16; VRAM_LEN],
    setup: &TriRasterSetup,
    clip: Clip,
    colour: u16,
    mode: BlendMode,
    merge: Merge,
) {
    let plain = mode == BlendMode::Opaque && !merge.mask_check;
    let fg = [colour; LANES];
    tri_spans(setup, clip, |y, xs, xe| {
        let row = y as usize * VRAM_WIDTH;
        if plain {
            vram[row + xs as usize..row + xe as usize].fill(colour | merge.mask_or);
            return;
        }
        let (mut x, end) = (xs as usize, xe as usize);
        while x < end {
            let n = (end - x).min(LANES);
            merge_flat_lanes(vram, row + x, n, &fg, false, mode, merge);
            x += LANES;
        }
    });
}

/// Draw a set-up untextured triangle one pixel at a time through
/// `Plotter::put` (the pixel tracer's path). `shaded` interpolates the
/// colour planes of `setup` (dithered when `dither`), otherwise every
/// pixel is `colour`.
#[inline(never)]
pub(super) fn untextured_tri_exact(
    vram: &mut [u16; VRAM_LEN],
    plot: &mut Plotter<'_>,
    setup: &TriRasterSetup,
    clip: Clip,
    shading: Option<bool>,
    colour: u16,
    mode: BlendMode,
) {
    let [pr, pg, pb, _, _] = setup.planes;
    tri_spans(setup, clip, |y, xs, xe| {
        let (mut r, mut g, mut b) = (
            plane_at(pr, xs, y),
            plane_at(pg, xs, y),
            plane_at(pb, xs, y),
        );
        let row = y as usize * VRAM_WIDTH;
        for x in xs..xe {
            let (ri, gi, bi) = (r >> 24, g >> 24, b >> 24);
            let c = match shading {
                None => colour,
                Some(true) => dither_rgb(ri as i32, gi as i32, bi as i32, x, y),
                Some(false) => ((ri >> 3) | ((gi >> 3) << 5) | ((bi >> 3) << 10)) as u16,
            };
            plot.put(vram, row + x as usize, c, mode);
            r = r.wrapping_add(pr.0);
            g = g.wrapping_add(pg.0);
            b = b.wrapping_add(pb.0);
        }
    });
}
