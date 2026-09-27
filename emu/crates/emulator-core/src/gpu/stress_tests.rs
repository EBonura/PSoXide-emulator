//! Randomised rasteriser stress test.
//!
//! Drives a standalone [`Gpu`] with a long seeded stream of GP0/GP1 words
//! covering every primitive type and mode (flat, Gouraud, textured and
//! textured-Gouraud triangles and quads, rectangles of every size, lines and
//! polylines, fills, VRAM copies, CPU uploads and downloads) under random
//! draw state (texture pages and depths, CLUTs, texture windows, blend
//! modes, dither, mask bits, draw areas and offsets, rectangle flips), with
//! random vertices that include off-screen, oversized, degenerate and
//! axis-aligned cases. VRAM starts as random noise, so texture and CLUT
//! fetches read random texels with random mask bits.
//!
//! The digest folds VRAM, the CLUT cache, the draw-timing histograms and the
//! work counters at regular checkpoints, plus every word read back through
//! GPUREAD. The expected digests were recorded from the rasteriser as of
//! 8f6091c, so any change to a pixel, a timing charge or the CLUT cache
//! behaviour fails the test.
//!
//! On a mismatch, set `PSOXIDE_RASTER_STRESS_TRACE=<dir>` to write every
//! checkpoint digest to `<dir>/<name>.txt`; comparing that file against one
//! produced by a known-good build locates the first diverging checkpoint.
//! `PSOXIDE_RASTER_STRESS_EVERY=1` makes every command a checkpoint.

use super::*;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn word(&mut self) -> u32 {
        (self.next() >> 32) as u32
    }

    fn below(&mut self, n: u32) -> u32 {
        ((self.next() >> 32) % u64::from(n.max(1))) as u32
    }

    fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + self.below((hi - lo + 1) as u32) as i32
    }

    fn chance(&mut self, percent: u32) -> bool {
        self.below(100) < percent
    }
}

fn fnv_fold(h: u64, v: u64) -> u64 {
    (h ^ v).wrapping_mul(0x0000_0100_0000_01B3)
}

fn words_digest(words: &[u16]) -> u64 {
    let mut h = 0xCBF2_9CE4_8422_2325u64;
    for chunk in words.chunks_exact(4) {
        let v = u64::from(chunk[0])
            | (u64::from(chunk[1]) << 16)
            | (u64::from(chunk[2]) << 32)
            | (u64::from(chunk[3]) << 48);
        h = fnv_fold(h, v);
    }
    h
}

/// Corners and texture coordinates of an axis-aligned quad.
type SpriteQuad = ([(i32, i32); 4], [(u32, u32); 4]);

struct Stress {
    gpu: Gpu,
    rng: Rng,
    digest: u64,
    trace: Vec<u64>,
}

impl Stress {
    fn new(seed: u64) -> Self {
        let mut rng = Rng(seed | 1);
        let mut gpu = Gpu::new();
        for w in gpu.vram.words_mut() {
            *w = rng.word() as u16;
        }
        Self {
            gpu,
            rng,
            digest: 0xCBF2_9CE4_8422_2325,
            trace: Vec::new(),
        }
    }

    fn fold(&mut self, v: u64) {
        self.digest = fnv_fold(self.digest, v);
    }

    fn gp0(&mut self, word: u32) {
        self.gpu.write32(GP0_ADDR, word);
    }

    fn gp1(&mut self, word: u32) {
        self.gpu.write32(GP1_ADDR, word);
    }

    fn checkpoint(&mut self) {
        let mut h = words_digest(self.gpu.vram.words());
        h = fnv_fold(h, words_digest(&self.gpu.clut_cache));
        h = fnv_fold(h, u64::from(self.gpu.clut_line_a_reg));
        h = fnv_fold(h, u64::from(self.gpu.clut_line_b_reg));
        for i in 0..256 {
            h = fnv_fold(h, self.gpu.gp0_timing_hist[i]);
            h = fnv_fold(h, self.gpu.gp0_dma_timing_hist[i]);
            h = fnv_fold(h, u64::from(self.gpu.gp0_opcode_hist[i]));
        }
        let w = *self.gpu.work_counters();
        h = fnv_fold(h, w.pixels);
        h = fnv_fold(h, w.texture_pixels);
        h = fnv_fold(h, w.vram_upload_pixels);
        for p in w.texpage_pixels {
            h = fnv_fold(h, p);
        }
        h = fnv_fold(h, self.gpu.busy_credit);
        h = fnv_fold(h, u64::from(self.gpu.status.raw));
        if let Some(owner) = &self.gpu.pixel_owner {
            let mut o = 0xCBF2_9CE4_8422_2325u64;
            for &v in owner {
                o = fnv_fold(o, u64::from(v));
            }
            h = fnv_fold(h, o);
        }
        self.fold(h);
        self.trace.push(self.digest);
    }

    /// A vertex word with random junk in the ignored bits 11..15 / 27..31.
    fn vertex(&mut self, x: i32, y: i32) -> u32 {
        let junk = if self.rng.chance(20) {
            self.rng.word() & 0xF800_F800
        } else {
            0
        };
        (x as u32 & 0x7FF) | ((y as u32 & 0x7FF) << 16) | junk
    }

    fn colour(&mut self) -> u32 {
        match self.rng.below(8) {
            0 => 0x80_8080,
            1 => 0xFF_FFFF,
            2 => 0,
            3 => {
                let c = self.rng.below(256);
                c | (c << 8) | (c << 16)
            }
            _ => self.rng.word() & 0xFF_FFFF,
        }
    }

    /// `n` vertices in one of several shapes.
    fn shape(&mut self, n: usize) -> Vec<(i32, i32)> {
        let kind = self.rng.below(100);
        let (cx, cy) = (self.rng.range(-80, 1100), self.rng.range(-80, 590));
        let spread = match kind {
            0..=39 => 24,
            40..=69 => 160,
            70..=79 => 600,
            _ => 0,
        };
        let mut out: Vec<(i32, i32)> = (0..n)
            .map(|_| {
                if (80..88).contains(&kind) {
                    // Full signed 11-bit range, often oversized.
                    (self.rng.range(-1024, 1023), self.rng.range(-1024, 1023))
                } else {
                    (
                        cx + self.rng.range(-spread, spread),
                        cy + self.rng.range(-spread, spread),
                    )
                }
            })
            .collect();
        match kind {
            88..=91 => {
                // Degenerate: repeated vertices.
                let v = out[0];
                let k = 1 + self.rng.below(n as u32 - 1) as usize;
                out[k] = v;
            }
            92..=95 => {
                // Collinear, or a flat horizontal/vertical edge.
                let (dx, dy) = (self.rng.range(-40, 40), self.rng.range(-40, 40));
                let (dx, dy) = match self.rng.below(3) {
                    0 => (dx, 0),
                    1 => (0, dy),
                    _ => (dx, dy),
                };
                for (i, v) in out.iter_mut().enumerate() {
                    *v = (cx + dx * i as i32, cy + dy * i as i32);
                }
            }
            96..=99 => {
                // Exactly at the 1023 / 511 extent limits.
                let w = if self.rng.chance(50) { 1023 } else { 1024 };
                let h = if self.rng.chance(50) { 511 } else { 512 };
                out[0] = (cx - w / 2, cy - h / 2);
                if n > 1 {
                    out[1] = (cx - w / 2 + w, cy - h / 2);
                }
                if n > 2 {
                    out[2] = (cx - w / 2, cy - h / 2 + h);
                }
            }
            _ => {}
        }
        out
    }

    /// Axis-aligned quad corners in submission order (TL, TR, BL, BR),
    /// sometimes mirrored, with matching sprite UVs.
    fn sprite_quad(&mut self) -> SpriteQuad {
        let (x, y) = (self.rng.range(-40, 1060), self.rng.range(-40, 540));
        let big = self.rng.chance(15);
        let w = if big {
            self.rng.range(-300, 300)
        } else {
            self.rng.range(-8, 64)
        };
        let h = if big {
            self.rng.range(-300, 300)
        } else {
            self.rng.range(-8, 64)
        };
        let v = [(x, y), (x + w, y), (x, y + h), (x + w, y + h)];
        let (u0, v0) = (self.rng.below(256), self.rng.below(256));
        let uv = match self.rng.below(4) {
            // Half-open 1:1 texels.
            0 | 1 => {
                let (uw, vh) = (w.unsigned_abs(), h.unsigned_abs());
                [
                    (u0, v0),
                    ((u0 + uw).min(255), v0),
                    (u0, (v0 + vh).min(255)),
                    ((u0 + uw).min(255), (v0 + vh).min(255)),
                ]
            }
            // Flipped.
            2 => {
                let (uw, vh) = (w.unsigned_abs().min(u0), h.unsigned_abs().min(v0));
                [(u0, v0), (u0 - uw, v0), (u0, v0 - vh), (u0 - uw, v0 - vh)]
            }
            _ => [
                (self.rng.below(256), self.rng.below(256)),
                (self.rng.below(256), self.rng.below(256)),
                (self.rng.below(256), self.rng.below(256)),
                (self.rng.below(256), self.rng.below(256)),
            ],
        };
        (v, uv)
    }

    fn uv_word(&mut self, u: u32, v: u32, high: u32) -> u32 {
        (u & 0xFF) | ((v & 0xFF) << 8) | (high << 16)
    }

    fn clut_word(&mut self) -> u32 {
        match self.rng.below(4) {
            // A small palette of CLUT addresses so the cache hits often.
            0 | 1 => [0x7FC0, 0x7F80, 0x4000, 0x0010][self.rng.below(4) as usize],
            _ => self.rng.word() & 0xFFFF,
        }
    }

    fn tpage_word(&mut self) -> u32 {
        let mut t = self.rng.word() & 0xFFFF;
        if self.rng.chance(50) {
            // Keep the upper-bank bit (11) rare.
            t &= !0x0800;
        }
        t
    }

    fn polygon(&mut self) {
        let op = 0x20 + self.rng.below(0x20);
        let quad = op & 0x08 != 0;
        let gouraud = op & 0x10 != 0;
        let textured = op & 0x04 != 0;
        let n = if quad { 4 } else { 3 };
        let (verts, uvs) = if quad && self.rng.chance(40) {
            let (v, uv) = self.sprite_quad();
            (v.to_vec(), uv.to_vec())
        } else {
            let v = self.shape(n);
            let uv = (0..n)
                .map(|_| (self.rng.below(256), self.rng.below(256)))
                .collect();
            (v, uv)
        };
        let mut words = Vec::with_capacity(12);
        let c0 = self.colour();
        words.push((op << 24) | c0);
        let clut = self.clut_word();
        let tpage = self.tpage_word();
        for i in 0..n {
            if i > 0 && gouraud {
                // The top byte of a colour word is ignored; sometimes fill it.
                let junk = if self.rng.chance(10) {
                    self.rng.word() & 0xFF00_0000
                } else {
                    0
                };
                let c = self.colour() | junk;
                words.push(c);
            }
            let vw = self.vertex(verts[i].0, verts[i].1);
            words.push(vw);
            if textured {
                let high = match i {
                    0 => clut,
                    1 => tpage,
                    _ => self.rng.word() & 0xFFFF,
                };
                let w = self.uv_word(uvs[i].0, uvs[i].1, high);
                words.push(w);
            }
        }
        for w in words {
            self.gp0(w);
        }
    }

    fn rectangle(&mut self) {
        let op = 0x60 + self.rng.below(0x20);
        let textured = op & 0x04 != 0;
        let c = self.colour();
        self.gp0((op << 24) | c);
        let (x, y) = (self.rng.range(-80, 1100), self.rng.range(-80, 590));
        let v = self.vertex(x, y);
        self.gp0(v);
        if textured {
            let clut = self.clut_word();
            let (u, v) = (self.rng.below(256), self.rng.below(256));
            let w = self.uv_word(u, v, clut);
            self.gp0(w);
        }
        if (op >> 3) & 3 == 0 {
            let (w, h) = match self.rng.below(10) {
                0 => (self.rng.below(1100), self.rng.below(600)),
                1 => (self.rng.word() & 0xFFFF, self.rng.word() & 0xFFFF),
                2 => (0, self.rng.below(20)),
                _ => (self.rng.below(70), self.rng.below(70)),
            };
            self.gp0(w | (h << 16));
        }
    }

    fn line(&mut self) {
        let op = 0x40 + self.rng.below(0x20);
        let gouraud = op & 0x10 != 0;
        let poly = op & 0x08 != 0;
        let n = if poly {
            2 + self.rng.below(6) as usize
        } else {
            2
        };
        let verts = self.shape(n.max(3));
        let c = self.colour();
        self.gp0((op << 24) | c);
        for (i, &(x, y)) in verts.iter().take(n).enumerate() {
            if i > 0 && gouraud {
                let c = self.colour();
                self.gp0(c);
            }
            let v = self.vertex(x, y);
            // A data word that happens to look like a terminator ends the
            // polyline early; keep the stream deterministic either way.
            self.gp0(v);
        }
        if poly {
            let t = if self.rng.chance(50) {
                0x5555_5555
            } else {
                0x5000_5000
            };
            self.gp0(t);
        }
    }

    fn fill(&mut self) {
        let c = self.colour();
        self.gp0(0x0200_0000 | c);
        let xy = self.rng.word() & 0x01FF_03FF;
        self.gp0(xy);
        let wh = if self.rng.chance(20) {
            self.rng.word() & 0x01FF_03FF
        } else {
            self.rng.below(96) | (self.rng.below(96) << 16)
        };
        self.gp0(wh);
    }

    fn copy(&mut self) {
        let low = self.rng.word() & 0x1F_FFFF;
        self.gp0(0x8000_0000 | low);
        let (a, b) = (self.rng.word() & 0x01FF_03FF, self.rng.word() & 0x01FF_03FF);
        // Overlapping copies are common in scrolling code.
        let b = if self.rng.chance(30) {
            (a & 0x01FF_03FF).wrapping_add(self.rng.below(8) | (self.rng.below(8) << 16))
                & 0x01FF_03FF
        } else {
            b
        };
        self.gp0(a);
        self.gp0(b);
        let wh = if self.rng.chance(5) {
            self.rng.word() & 0x01FF_03FF
        } else {
            (1 + self.rng.below(64)) | ((1 + self.rng.below(64)) << 16)
        };
        self.gp0(wh);
    }

    fn upload(&mut self) {
        let low = self.rng.word() & 0x1F_FFFF;
        self.gp0(0xA000_0000 | low);
        let xy = if self.rng.chance(20) {
            // Near the right/bottom edge so the payload wraps.
            (1000 + self.rng.below(24)) | ((490 + self.rng.below(22)) << 16)
        } else {
            self.rng.word() & 0x01FF_03FF
        };
        let (w, h) = (1 + self.rng.below(40), 1 + self.rng.below(40));
        self.gp0(xy);
        self.gp0(w | (h << 16));
        for _ in 0..(w * h).div_ceil(2) {
            let d = self.rng.word();
            self.gp0(d);
        }
    }

    fn download(&mut self) {
        let low = self.rng.word() & 0x1F_FFFF;
        self.gp0(0xC000_0000 | low);
        let xy = if self.rng.chance(20) {
            (1000 + self.rng.below(24)) | ((490 + self.rng.below(22)) << 16)
        } else {
            self.rng.word() & 0x01FF_03FF
        };
        let (w, h) = (1 + self.rng.below(40), 1 + self.rng.below(40));
        self.gp0(xy);
        self.gp0(w | (h << 16));
        for _ in 0..(w * h).div_ceil(2) {
            let d = self.gpu.read32(GP0_ADDR).unwrap_or(0);
            self.fold(u64::from(d));
        }
    }

    fn state(&mut self) {
        match self.rng.below(12) {
            0..=2 => {
                let w = self.rng.word() & 0x3FFF;
                self.gp0(0xE100_0000 | w);
            }
            3 => {
                let w = if self.rng.chance(50) {
                    0
                } else {
                    self.rng.word() & 0xF_FFFF
                };
                self.gp0(0xE200_0000 | w);
            }
            4 | 5 => {
                let (l, t, r, b) = if self.rng.chance(60) {
                    (0, 0, 1023, 511)
                } else {
                    let l = self.rng.below(1024);
                    let t = self.rng.below(512);
                    let r = if self.rng.chance(10) {
                        self.rng.below(1024)
                    } else {
                        (l + self.rng.below(400)).min(1023)
                    };
                    let b = if self.rng.chance(10) {
                        self.rng.below(512)
                    } else {
                        (t + self.rng.below(300)).min(511)
                    };
                    (l, t, r, b)
                };
                self.gp0(0xE300_0000 | l | (t << 10));
                self.gp0(0xE400_0000 | r | (b << 10));
            }
            6 => {
                let (x, y) = if self.rng.chance(70) {
                    (self.rng.range(-64, 400), self.rng.range(-64, 300))
                } else {
                    (self.rng.range(-1024, 1023), self.rng.range(-1024, 1023))
                };
                self.gp0(0xE500_0000 | (x as u32 & 0x7FF) | ((y as u32 & 0x7FF) << 11));
            }
            7 | 8 => {
                // Mask bits: mostly off so blending paths dominate.
                let m = if self.rng.chance(60) {
                    0
                } else {
                    self.rng.below(4)
                };
                self.gp0(0xE600_0000 | m);
            }
            9 => self.gp0(0x0100_0000),
            10 => {
                let bit = u32::from(self.rng.chance(20));
                self.gp1(0x0900_0000 | bit);
            }
            _ => {
                // E1 with dither and a common tpage, to weight dithered draws.
                let w = (self.rng.word() & 0x1FF) | 0x200;
                self.gp0(0xE100_0000 | w);
            }
        }
    }

    fn command(&mut self) {
        match self.rng.below(100) {
            0..=44 => self.polygon(),
            45..=59 => self.rectangle(),
            60..=69 => self.line(),
            70..=72 => self.fill(),
            73..=75 => self.copy(),
            76..=77 => self.upload(),
            78 => self.download(),
            _ => self.state(),
        }
    }

    fn run(&mut self, commands: usize, every: usize) -> u64 {
        for i in 0..commands {
            self.command();
            if (i + 1) % every == 0 {
                self.checkpoint();
            }
        }
        self.checkpoint();
        self.digest
    }
}

fn run_case(name: &str, seed: u64, commands: usize, setup: impl FnOnce(&mut Gpu)) -> u64 {
    let every = std::env::var("PSOXIDE_RASTER_STRESS_EVERY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256usize);
    let mut s = Stress::new(seed);
    setup(&mut s.gpu);
    let digest = s.run(commands, every);
    if let Ok(dir) = std::env::var("PSOXIDE_RASTER_STRESS_TRACE") {
        let text: String = s.trace.iter().map(|d| format!("{d:016x}\n")).collect();
        std::fs::write(std::path::Path::new(&dir).join(format!("{name}.txt")), text)
            .expect("write stress trace");
    }
    let w = s.gpu.work_counters();
    let hist = s.gpu.gp0_opcode_histogram();
    let polys: u32 = hist[0x20..0x40].iter().sum();
    let rects: u32 = hist[0x60..0x80].iter().sum();
    let lines: u32 = hist[0x40..0x60].iter().sum();
    println!(
        "raster stress {name}: {digest:016x} (pixels {} textured {} polys {polys} rects {rects} lines {lines})",
        w.pixels, w.texture_pixels
    );
    digest
}

#[test]
fn raster_stress_random_primitives_a() {
    let d = run_case("a", 0x5EED_0001, 150_000, |_| {});
    assert_eq!(d, 0x8298_df67_24ad_ac19, "raster stress a digest changed");
}

#[test]
fn raster_stress_random_primitives_b() {
    let d = run_case("b", 0x5EED_0002, 150_000, |_| {});
    assert_eq!(d, 0x4354_d706_6ea4_9b57, "raster stress b digest changed");
}

#[test]
fn raster_stress_pixel_tracer() {
    let d = run_case("tracer", 0x5EED_0003, 6_000, |gpu| {
        gpu.enable_pixel_tracer()
    });
    assert_eq!(
        d, 0x3d5d_0680_ce40_0e52,
        "raster stress tracer digest changed"
    );
}

#[test]
fn raster_stress_wireframe() {
    let d = run_case("wire", 0x5EED_0004, 4_000, |gpu| {
        gpu.wireframe_enabled = true
    });
    assert_eq!(
        d, 0x6a6a_c676_b348_14e2,
        "raster stress wireframe digest changed"
    );
}

/// One 24-bit display pixel the way the display readout used to fetch
/// it: pixel `px` of a line starting at VRAM halfword `start_x` covers
/// bytes `3*px..3*px+2` from there, which may straddle two halfwords.
fn reference_rgb24(gpu: &Gpu, start_x: u16, px: u16, y: u16) -> (u8, u8, u8) {
    let byte_x = (start_x as u32) * 2 + (px as u32) * 3;
    let word_x = (byte_x / 2) as u16;
    let w0 = gpu.vram.get_pixel(word_x, y);
    let w1 = gpu.vram.get_pixel(word_x.wrapping_add(1), y);
    if byte_x & 1 == 0 {
        ((w0 & 0xFF) as u8, (w0 >> 8) as u8, (w1 & 0xFF) as u8)
    } else {
        ((w0 >> 8) as u8, (w1 & 0xFF) as u8, (w1 >> 8) as u8)
    }
}

/// `Gpu::display_hash` output.
type DisplayHash = (u64, u32, u32, usize);
/// `Gpu::display_rgba8` output.
type DisplayRgba = (Vec<u8>, u32, u32);

/// The display hash and RGBA conversion before they worked a row at a
/// time: a pixel at a time through `get_pixel` / `reference_rgb24`.
fn reference_display(gpu: &Gpu) -> (DisplayHash, DisplayRgba) {
    let hash = if !gpu.display_configured {
        (psx_hw::hash::Fnv1a64::new().finish(), 0, 0, 0)
    } else {
        let da = gpu.display_area();
        let mut h = psx_hw::hash::Fnv1a64::new();
        let mut byte_len = 0usize;
        let effective_h = da.height.min((VRAM_HEIGHT as u16).saturating_sub(da.y));
        let effective_w = if da.bpp24 {
            da.width.min(rgb24_pixels_left(da.x))
        } else {
            da.width.min((VRAM_WIDTH as u16).saturating_sub(da.x))
        };
        for dy in 0..effective_h {
            for dx in 0..effective_w {
                if da.bpp24 {
                    let (r, g, b) = reference_rgb24(gpu, da.x, dx, da.y + dy);
                    h.update(&[r, g, b]);
                    byte_len += 3;
                } else {
                    h.update(&gpu.vram.get_pixel(da.x + dx, da.y + dy).to_le_bytes());
                    byte_len += 2;
                }
            }
        }
        (h.finish(), effective_w as u32, effective_h as u32, byte_len)
    };
    let da = gpu.display_area();
    let eff_h = da.height.min((VRAM_HEIGHT as u16).saturating_sub(da.y));
    let eff_w = if da.bpp24 {
        da.width.min(rgb24_pixels_left(da.x))
    } else {
        da.width.min((VRAM_WIDTH as u16).saturating_sub(da.x))
    };
    let off_x = gpu
        .horizontal_display_offset_px()
        .clamp(-(eff_w as i32), eff_w as i32);
    let off_y = gpu
        .vertical_display_offset_px()
        .clamp(-(eff_h as i32), eff_h as i32);
    let mut out = Vec::new();
    for dy in 0..eff_h {
        let src_y = dy as i32 - off_y;
        for dx in 0..eff_w {
            let src_x = dx as i32 - off_x;
            if src_x < 0 || src_x >= eff_w as i32 || src_y < 0 || src_y >= eff_h as i32 {
                out.extend_from_slice(&[0, 0, 0, 0xFF]);
                continue;
            }
            let sy = da.y + src_y as u16;
            if da.bpp24 {
                let (r, g, b) = reference_rgb24(gpu, da.x, src_x as u16, sy);
                out.extend_from_slice(&[r, g, b, 0xFF]);
            } else {
                let pixel = gpu.vram.get_pixel(da.x + src_x as u16, sy);
                let r = ((pixel & 0x1F) as u8) << 3;
                let g = (((pixel >> 5) & 0x1F) as u8) << 3;
                let b = (((pixel >> 10) & 0x1F) as u8) << 3;
                out.extend_from_slice(&[r | (r >> 5), g | (g >> 5), b | (b >> 5), 0xFF]);
            }
        }
    }
    (hash, (out, eff_w as u32, eff_h as u32))
}

#[test]
fn display_hash_and_rgba_match_the_per_pixel_reference() {
    let mut s = Stress::new(0x5EED_0005);
    // Unconfigured display first.
    assert_eq!(s.gpu.display_hash(), reference_display(&s.gpu).0);
    for _ in 0..600 {
        let rng = &mut s.rng;
        let start = if rng.chance(30) {
            // Near the right / bottom edges.
            (1024 - rng.below(700)) & 0x3FF | ((512 - rng.below(300)) & 0x1FF) << 10
        } else {
            rng.word() & 0x7FFFF
        };
        let x1 = (0x260 + rng.range(-400, 400)) as u32;
        let hrange = (x1 & 0xFFF) | ((x1 + 2560) & 0xFFF) << 12;
        let y1 = if rng.chance(50) {
            0x10
        } else {
            rng.below(0x40)
        };
        let y2 = y1 + rng.below(520);
        let vrange = (y1 & 0x3FF) | (y2 & 0x3FF) << 10;
        let mode = rng.word() & 0x7F;
        s.gp1(0x0500_0000 | start);
        s.gp1(0x0600_0000 | hrange);
        s.gp1(0x0700_0000 | vrange);
        s.gp1(0x0800_0000 | mode);
        let (hash, rgba) = reference_display(&s.gpu);
        assert_eq!(s.gpu.display_hash(), hash, "display_hash");
        assert!(s.gpu.display_rgba8() == rgba, "display_rgba8");
    }
}
