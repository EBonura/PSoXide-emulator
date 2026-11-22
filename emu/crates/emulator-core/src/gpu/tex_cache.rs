//! The texel law of the GPU timing model: a texture cache that charges for
//! the texels a primitive fetches, not for a change of texture page.
//!
//! Hardware Tests v2.26 (records 0xAC0 to 0xAEF, eight 32 x 32 textured
//! quads a row) showed that on a console a texture page change costs nothing
//! by itself: what costs is reading texels that are not in the 2 KB texture
//! cache, about 1.2 clocks a byte of the lines fetched. The same texels drawn
//! again are free on any page; fresh texels on one page pay as much as a page
//! change does; two copies of a page at different VRAM addresses miss
//! independently. The default law ([`TextureLaw::Pixels`], fitted on v2.1 and
//! v2.4) charges a page change in proportion to the pixels drawn and fresh
//! texels on one page nothing, so it is wrong both ways for those rows.
//!
//! The model is a direct-mapped tag store of 256 lines of 8 bytes (four VRAM
//! halfwords). A line's slot is its place in a cache block of the texture's
//! depth, 16 halfwords x 64 rows at 4 bits (64 x 64 texels) and 32 halfwords x
//! 32 rows at 8 and 15 bits (64 x 32 and 32 x 32 texels), so lines a block
//! apart evict each other. The tag is the line's VRAM address: the same texels
//! on another copy of the page miss. A missed line costs [`MISS_Q8`], less the
//! clocks the primitive's setup leaves its fill idle (a small triangle waits
//! on its setup, and its fetches overlap the wait).
//!
//! Fitted with `gpu/tex_law_tests.rs` (`fit_table`), which replays the rows of
//! v2.1 (0x200 to 0x223), v2.4 (0x8E0 to 0x92A) and v2.26 (0xAC0 to 0xAEE) as
//! the hardware-tests program sends them. Of the geometries tried, a 32 x 64
//! block at 8 bits (as for 4 bits) cannot keep the texels of a 64 x 64 quad
//! drawn twice, which the console does (0xAE8); 64 x 32 can. Two ways, or a
//! flush on a page change, fit worse.
//!
//! The walk visits the texels the rasteriser samples (same coverage rule,
//! same attribute planes, the 480i row rule), but it is costing only: it
//! never touches what is drawn. Selected with `PSOXIDE_TEXTURE_LAW=texels`,
//! read when the GPU is created; anything else keeps the default law, which
//! is unchanged.

use super::raster::{tri_plane_eval, TriRasterSetup};
use super::span::{tri_spans, Clip, TexFetch};

/// Environment variable naming the texture cost law.
pub const ENV_VAR: &str = "PSOXIDE_TEXTURE_LAW";

/// Which texture cost law the GPU timing model charges.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum TextureLaw {
    /// A page or CLUT change refills in proportion to the pixels drawn, and a
    /// footprint past the cache pays a per-pixel cliff (hwtest v2.1, v2.4).
    /// The default.
    #[default]
    Pixels,
    /// A texture cache charges each line of texels fetched that it does not
    /// hold (hwtest v2.26). Opt-in.
    Texels,
}

impl TextureLaw {
    /// The law `PSOXIDE_TEXTURE_LAW` names: `texels` (or `cache`) for
    /// [`TextureLaw::Texels`], anything else or unset for the default.
    pub fn from_env() -> Self {
        Self::parse(std::env::var(ENV_VAR).ok().as_deref())
    }

    /// The law a spec names; `None`, empty or unknown is the default.
    pub fn parse(spec: Option<&str>) -> Self {
        match spec.map(str::trim) {
            Some("texels" | "cache") => TextureLaw::Texels,
            _ => TextureLaw::Pixels,
        }
    }
}

/// Lines in the cache (8 bytes each: 2 KB).
const LINES: usize = 256;
/// A tag no VRAM line has.
const EMPTY: u32 = u32::MAX;

/// Clocks a missed line costs, Q8: 9.4, about 1.2 a byte.
pub(super) const MISS_Q8: u64 = 2400;

/// The tag store, and the misses counted for the packet being costed.
#[derive(Clone)]
pub(super) struct TexCache {
    tags: [u32; LINES],
    misses: u64,
    /// The last line looked up: consecutive texels of a span mostly share a
    /// line, and it is in the cache once looked up.
    last: u32,
}

impl Default for TexCache {
    fn default() -> Self {
        Self {
            tags: [EMPTY; LINES],
            misses: 0,
            last: EMPTY,
        }
    }
}

impl TexCache {
    /// Forget every line (GP0(01h)).
    pub(super) fn clear(&mut self) {
        self.tags = [EMPTY; LINES];
        self.last = EMPTY;
    }

    /// The misses counted since the last call.
    pub(super) fn take_misses(&mut self) -> u64 {
        std::mem::take(&mut self.misses)
    }

    /// Look up the line holding texel `(u, v)` of `fetch` at `depth` (0, 1
    /// or 2 for 4, 8 or 15 bits), fetching it on a miss.
    #[inline]
    fn touch(&mut self, fetch: &TexFetch, depth: u8, u: u32, v: u32) {
        let (u, v) = (
            (u & fetch.and_u) | fetch.or_u,
            (v & fetch.and_v) | fetch.or_v,
        );
        let halfword = match depth {
            0 => u >> 2,
            1 => u >> 1,
            _ => u,
        };
        let x = (fetch.page_x + halfword) & 1023;
        let y = (fetch.page_y + v) & 511;
        // Four halfwords a line.
        let tag = (y << 8) | (x >> 2);
        if tag == self.last {
            return;
        }
        self.last = tag;
        let slot = if depth == 0 {
            ((y & 63) << 2) | ((x >> 2) & 3)
        } else {
            ((y & 31) << 3) | ((x >> 2) & 7)
        } as usize;
        if self.tags[slot] != tag {
            self.tags[slot] = tag;
            self.misses += 1;
        }
    }

    /// Fetch the texels of a set-up triangle, row by row as it is drawn.
    pub(super) fn walk_triangle(
        &mut self,
        setup: &TriRasterSetup,
        clip: Clip,
        fetch: &TexFetch,
        depth: u8,
    ) {
        let (pu, pv) = (setup.planes[3], setup.planes[4]);
        tri_spans(setup, clip, |y, xs, xe| {
            for x in xs..xe {
                let u = u32::from(tri_plane_eval(pu, x, y));
                let v = u32::from(tri_plane_eval(pv, x, y));
                self.touch(fetch, depth, u, v);
            }
        });
    }

    /// Fetch the texels of a textured rectangle: `rows` and `cols` the drawn
    /// (clipped) rows and columns, as offsets from the rectangle's origin.
    pub(super) fn walk_rect(
        &mut self,
        fetch: &TexFetch,
        depth: u8,
        (u0, v0): (u32, u32),
        (flip_x, flip_y): (bool, bool),
        rows: impl Iterator<Item = u32>,
        cols: std::ops::Range<u32>,
    ) {
        for dy in rows {
            let v = if flip_y {
                v0.wrapping_sub(dy)
            } else {
                v0.wrapping_add(dy)
            } & 0xff;
            for dx in cols.clone() {
                let u = if flip_x {
                    u0.wrapping_sub(dx)
                } else {
                    u0.wrapping_add(dx)
                } & 0xff;
                self.touch(fetch, depth, u, v);
            }
        }
    }
}
