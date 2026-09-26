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

use super::blend::{blend_pixel, dither_rgb, modulate_tint, modulate_tint_dithered, BlendMode};
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
        let mut y = y0;
        while y < y1 {
            if y >= clip.top && y <= clip.bottom {
                let xs = tri_span_x(lx).max(clip.left);
                let xe = tri_span_x(rx).min(clip.right + 1);
                if xs < xe {
                    span(y, xs, xe);
                }
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

/// Draw a set-up textured triangle. `SHADE` picks the texel modulation
/// (for `SHADE_GOURAUD` the colour planes of `setup` supply the tint),
/// `DITHER` the dithered variant of it. `SIMPLE` promises an opaque,
/// untraced primitive without the mask test, whose pixels are plain
/// stores.
#[inline(never)]
pub(super) fn tex_tri<const D: u8, const SHADE: u8, const DITHER: bool, const SIMPLE: bool>(
    vram: &mut [u16; VRAM_LEN],
    clut: &[u16; 256],
    plot: &mut Plotter<'_>,
    setup: &TriRasterSetup,
    clip: Clip,
    tex: TexFetch,
    prim: &TexTri,
) {
    let [pr, pg, pb, pu, pv] = setup.planes;
    let at = |p: (u32, u32, u32), x: i32, y: i32| {
        p.2.wrapping_add((x as u32).wrapping_mul(p.0))
            .wrapping_add((y as u32).wrapping_mul(p.1))
    };
    let (tr, tg, tb) = prim.tint;
    tri_spans(setup, clip, |y, xs, xe| {
        let (mut u, mut v) = (at(pu, xs, y), at(pv, xs, y));
        let (mut r, mut g, mut b) = if SHADE == SHADE_GOURAUD {
            (at(pr, xs, y), at(pg, xs, y), at(pb, xs, y))
        } else {
            (0, 0, 0)
        };
        let row = y as usize * VRAM_WIDTH;
        for x in xs..xe {
            let texel = tex.texel::<D>(vram, clut, u >> 24, v >> 24);
            if texel != 0 {
                let shaded = match SHADE {
                    SHADE_RAW => texel,
                    SHADE_FLAT => {
                        if DITHER {
                            modulate_tint_dithered(texel, tr, tg, tb, x, y)
                        } else {
                            modulate_tint(texel, tr, tg, tb)
                        }
                    }
                    _ => {
                        let (ri, gi, bi) = (r >> 24, g >> 24, b >> 24);
                        if DITHER {
                            modulate_tint_dithered(texel, ri, gi, bi, x, y)
                        } else {
                            modulate_tint(texel, ri, gi, bi)
                        }
                    }
                };
                if SIMPLE {
                    vram[(row + x as usize) & (VRAM_LEN - 1)] = shaded | plot.mask_or;
                } else {
                    let mode = if prim.semi && texel & 0x8000 != 0 {
                        prim.blend
                    } else {
                        BlendMode::Opaque
                    };
                    plot.put(vram, row + x as usize, shaded, mode);
                }
            }
            u = u.wrapping_add(pu.0);
            v = v.wrapping_add(pv.0);
            if SHADE == SHADE_GOURAUD {
                r = r.wrapping_add(pr.0);
                g = g.wrapping_add(pg.0);
                b = b.wrapping_add(pb.0);
            }
        }
    });
}

/// Draw a set-up Gouraud (untextured) triangle.
#[inline(always)]
pub(super) fn shaded_tri<const DITHER: bool>(
    vram: &mut [u16; VRAM_LEN],
    plot: &mut Plotter<'_>,
    setup: &TriRasterSetup,
    clip: Clip,
    mode: BlendMode,
) {
    let [pr, pg, pb, _, _] = setup.planes;
    let at = |p: (u32, u32, u32), x: i32, y: i32| {
        p.2.wrapping_add((x as u32).wrapping_mul(p.0))
            .wrapping_add((y as u32).wrapping_mul(p.1))
    };
    tri_spans(setup, clip, |y, xs, xe| {
        let (mut r, mut g, mut b) = (at(pr, xs, y), at(pg, xs, y), at(pb, xs, y));
        let row = y as usize * VRAM_WIDTH;
        for x in xs..xe {
            let (ri, gi, bi) = (r >> 24, g >> 24, b >> 24);
            let colour = if DITHER {
                dither_rgb(ri as i32, gi as i32, bi as i32, x, y)
            } else {
                ((ri >> 3) | ((gi >> 3) << 5) | ((bi >> 3) << 10)) as u16
            };
            plot.put(vram, row + x as usize, colour, mode);
            r = r.wrapping_add(pr.0);
            g = g.wrapping_add(pg.0);
            b = b.wrapping_add(pb.0);
        }
    });
}

/// Draw a set-up flat (untextured) triangle.
#[inline(always)]
pub(super) fn flat_tri(
    vram: &mut [u16; VRAM_LEN],
    plot: &mut Plotter<'_>,
    setup: &TriRasterSetup,
    clip: Clip,
    colour: u16,
    mode: BlendMode,
) {
    tri_spans(setup, clip, |y, xs, xe| {
        let row = y as usize * VRAM_WIDTH;
        for x in xs..xe {
            plot.put(vram, row + x as usize, colour, mode);
        }
    });
}
