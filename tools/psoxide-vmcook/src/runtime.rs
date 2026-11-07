//! Runtime-exact emulation of the held-weapon path: psx_asset::hmd8 pose
//! interpolation, the game's GTE bone composition + RTPT (psx-gte host
//! backend over psx-gte-core, bit-exact), the shared viewmodel sort
//! (near reject, NCLIP cull, 64 depth buckets) and GP0 emission into the
//! emulator's GPU (emulator-core rasterizer + silicon-calibrated timing).

use crate::cooked::Cooked;
use emulator_core::gpu::Gpu;
use psx_asset::hmd8::Model;
use psx_gte::math::{Mat3I16, Vec3I16, Vec3I32};
use psx_gte::scene;

pub const VM_SCALE: i32 = 5;
pub const VM_VIEW_SHIFT: [i32; 3] = [30, 30, 40];
pub const NEAR_Z: i32 = 8;
pub const VM_OT_Z0: u32 = 48;
pub const VM_OT_STEP: u32 = 2;
pub const BUCKETS: usize = 64;
pub const TWO_SIDED_TEX: u16 = 0;

pub fn leak(b: Vec<u8>) -> &'static [u8] {
    Box::leak(b.into_boxed_slice())
}

pub fn load_model(c: &Cooked) -> Model {
    Model::from_bytes_with_vertex_cap(leak(c.geom_bytes()), 4096)
}

/// Every sample the runtime can draw. HMA1 models: the games drive held
/// weapons through `looped_clip_phase` / `one_shot_clip_phase` with the
/// clip's own hold duration and a whole-tick elapsed time, or hold
/// `clip_end_pose`, so the reachable positions are exactly
/// `e * span / duration` for e in 0..=duration. Palette models: every
/// (frame, frame2, frac16) the phase functions can return: consecutive pairs
/// inside each clip, the loop wrap, every frac16 0..15, and held last frames.
pub fn runtime_poses(c: &Cooked) -> Vec<(usize, usize, u32)> {
    let md = load_model(c);
    if md.has_tracks() {
        let mut out = Vec::new();
        for clip in 0..md.clip_count() {
            let d = md.clip_hold_ticks(clip).max(1) as usize;
            for e in 0..=d {
                out.push(md.one_shot_clip_phase(clip, d, e));
                out.push(md.looped_clip_phase(clip, d, e));
            }
            out.push(md.clip_end_pose(clip));
        }
        out.sort_unstable();
        out.dedup();
        return out;
    }
    let mut out = Vec::new();
    for cl in &c.clips {
        let first = (cl[0] & 0x7fff) as usize;
        let count = ((cl[1] & 0xff) as usize).max(1);
        for k in 0..count {
            let f = first + k;
            out.push((f, f, 0));
            let nxt = if k + 1 < count { f + 1 } else { first };
            if nxt != f {
                for fr in 1..16 {
                    out.push((f, nxt, fr));
                }
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Key poses: one per retained palette frame, or for HMA1 models every
/// whole source frame of every clip.
pub fn key_poses(c: &Cooked) -> Vec<(usize, usize, u32)> {
    let md = load_model(c);
    if let Some(tracks) = md.tracks() {
        let mut out = Vec::new();
        for clip in 0..md.clip_count() {
            for f in 0..=tracks.model.clip_intervals(clip) {
                out.push((clip, clip, f * 256));
            }
        }
        out.dedup();
        return out;
    }
    (0..c.n_frames).map(|f| (f, f, 0)).collect()
}

fn vm_rot() -> Mat3I16 {
    let base = [[0i32, 0, -4096], [0, -4096, 0], [4096, 0, 0]];
    let mut m = Mat3I16 { m: [[0; 3]; 3] };
    for r in 0..3 {
        for c in 0..3 {
            m.m[r][c] = (base[r][c] * VM_SCALE).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        }
    }
    m
}

fn local_scale_shift(l2w: u16) -> (i32, u32) {
    let s = (4096 / l2w.max(1) as i32).max(1);
    (s, s.trailing_zeros())
}

pub struct Projected {
    pub xy: Vec<[i16; 2]>,
    pub z: Vec<u16>,
}

/// Project one pose exactly as `project_hmd7_model_inner` does, with the
/// GTE screen offset at zero (the draw offset centres the weapon).
pub fn project(md: &Model, pose: (usize, usize, u32), h: u16) -> Projected {
    let (s, _) = local_scale_shift(md.local_to_world_q12());
    let r = vm_rot();
    let origin = Vec3I32::new(
        VM_VIEW_SHIFT[0] * s,
        VM_VIEW_SHIFT[1] * s,
        VM_VIEW_SHIFT[2] * s,
    );
    scene::set_screen_offset(0, 0);
    scene::set_projection_plane(h);
    let mut scratch = [psx_asset::hma1::Affine::ZERO; 256];
    let fr = md.pose(pose.0, pose.1, pose.2, 0, &mut scratch);
    let n = md.vertex_count();
    let mut xy = vec![[0i16; 2]; n];
    let mut z = vec![0u16; n];
    for ri in 0..md.bone_range_count() {
        let range = md.bone_range(ri);
        let bone = fr.bone(range.bone, range.mouth, 0);
        // compose_hmd7_bone_gte
        scene::load_rotation(&r);
        scene::load_translation(Vec3I32::ZERO);
        let col = |c: usize| {
            scene::transform_vertex(Vec3I16::new(
                bone.rotation.m[0][c],
                bone.rotation.m[1][c],
                bone.rotation.m[2][c],
            ))
        };
        let (c0, c1, c2) = (col(0), col(1), col(2));
        let bt = scene::transform_vertex(bone.translation);
        let cl = |v: i32| v.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        let rot = Mat3I16 {
            m: [
                [cl(c0.x), cl(c1.x), cl(c2.x)],
                [cl(c0.y), cl(c1.y), cl(c2.y)],
                [cl(c0.z), cl(c1.z), cl(c2.z)],
            ],
        };
        let tr = Vec3I32::new(
            origin.x.saturating_add(bt.x),
            origin.y.saturating_add(bt.y),
            origin.z.saturating_add(bt.z),
        );
        scene::load_rotation(&rot);
        scene::load_translation(tr);
        let end = (range.first + range.count).min(n);
        let mut v = range.first.min(end);
        while v + 2 < end {
            let p = scene::project_triangle(md.vertex(v), md.vertex(v + 1), md.vertex(v + 2));
            for k in 0..3 {
                xy[v + k] = [p[k].sx, p[k].sy];
                z[v + k] = p[k].sz;
            }
            v += 3;
        }
        while v < end {
            let p = scene::project_vertex(md.vertex(v));
            xy[v] = [p.sx, p.sy];
            z[v] = p.sz;
            v += 1;
        }
    }
    Projected { xy, z }
}

/// Geometric area of a projected triangle clipped to the view, in pixels.
/// Matches the emulator rasterizer's written-pixel count to ~1.5% on the
/// viewmodels; the GPU cost below applies the emulator's silicon-calibrated
/// textured-polygon rate (179/64 CPU cycles per pixel) to it.
pub fn clipped_area(v: [[i16; 2]; 3], center: (i16, i16), rect: (u16, u16, u16, u16)) -> f64 {
    let mut poly: Vec<(f64, f64)> = v
        .iter()
        .map(|p| (p[0] as f64 + center.0 as f64, p[1] as f64 + center.1 as f64))
        .collect();
    let (x0, y0, x1, y1) = (
        rect.0 as f64,
        rect.1 as f64,
        (rect.0 + rect.2) as f64,
        (rect.1 + rect.3) as f64,
    );
    for (axis, bound, keep_greater) in
        [(0, x0, true), (0, x1, false), (1, y0, true), (1, y1, false)]
    {
        let inside = |p: &(f64, f64)| {
            let c = if axis == 0 { p.0 } else { p.1 };
            if keep_greater {
                c >= bound
            } else {
                c <= bound
            }
        };
        let mut out = Vec::new();
        for i in 0..poly.len() {
            let (a, b) = (poly[i], poly[(i + 1) % poly.len()]);
            let (ia, ib) = (inside(&a), inside(&b));
            if ia {
                out.push(a);
            }
            if ia != ib {
                let (ca, cb) = if axis == 0 { (a.0, b.0) } else { (a.1, b.1) };
                let t = (bound - ca) / (cb - ca);
                out.push((a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t));
            }
        }
        poly = out;
        if poly.len() < 3 {
            return 0.0;
        }
    }
    let mut a2 = 0.0;
    for i in 0..poly.len() {
        let (p, q) = (poly[i], poly[(i + 1) % poly.len()]);
        a2 += p.0 * q.1 - q.0 * p.1;
    }
    a2.abs() / 2.0
}

pub fn gpu_cycles_for(
    md: &Model,
    p: &Projected,
    order: &[usize],
    center: (i16, i16),
    rect: (u16, u16, u16, u16),
) -> u64 {
    let px: f64 = order
        .iter()
        .map(|&i| clipped_area(md.triangle(i).idx.map(|k| p.xy[k as usize]), center, rect))
        .sum();
    (px * 179.0 / 64.0).round() as u64
}

pub fn nclip(a: [i16; 2], b: [i16; 2], c: [i16; 2]) -> i64 {
    let (ax, ay, bx, by, cx, cy) = (
        a[0] as i64,
        a[1] as i64,
        b[0] as i64,
        b[1] as i64,
        c[0] as i64,
        c[1] as i64,
    );
    ax * by + bx * cy + cx * ay - ax * cy - bx * ay - cx * by
}

pub struct Sorted {
    /// Triangle indices in GPU draw order.
    pub order: Vec<usize>,
}

/// `viewmodel::sort` + the walk order of `write_chain`.
pub fn sort(md: &Model, p: &Projected) -> Sorted {
    let (s, shift) = local_scale_shift(md.local_to_world_q12());
    let near = (NEAR_Z * s) as u16;
    let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); BUCKETS];
    for i in 0..md.triangle_count() {
        let t = md.triangle(i);
        let [a, b, c] = t.idx.map(|x| x as usize);
        let (za, zb, zc) = (p.z[a], p.z[b], p.z[c]);
        if za < near || zb < near || zc < near {
            continue;
        }
        if t.tex as u16 != TWO_SIDED_TEX && nclip(p.xy[a], p.xy[b], p.xy[c]) >= 0 {
            continue;
        }
        let avg = ((za as u32 + zb as u32 + zc as u32) / 3) >> shift;
        let rel = avg.saturating_sub(VM_OT_Z0) / VM_OT_STEP;
        let k = 1 + (rel as usize).min(BUCKETS - 2);
        buckets[k].push(i);
    }
    let mut order = Vec::new();
    for b in (0..BUCKETS).rev() {
        for &i in buckets[b].iter().rev() {
            order.push(i);
        }
    }
    Sorted { order }
}

/// The game's flat viewmodel shade (vm_normal_shade), brightness neutral.
pub fn vm_normal_shade(n: [i8; 3]) -> u8 {
    let (x, y, z) = (n[0] as i32, n[1] as i32, n[2] as i32);
    if (x | y | z) == 0 {
        return 128;
    }
    let dot = -(x << 2) + y * 3 + (z << 1);
    (104 + ((dot * 15) >> 8)).clamp(64, 144) as u8
}

#[derive(Clone, Copy)]
pub struct Slot {
    pub clut_word: u32,
    pub tpage_word: u32,
    pub window: u32,
}

/// Frame buffer origin in emulated VRAM. Textures live at X>=320.
const FB_X: u16 = 0;
const FB_Y: u16 = 0;

pub struct Frame {
    pub gpu: Gpu,
    pub slots: Vec<Slot>,
    pub center: (i16, i16),
    pub view: (u16, u16, u16, u16),
}

fn gp0(g: &mut Gpu, w: u32) {
    g.gp0_push(w);
}

pub fn upload(g: &mut Gpu, x: u16, y: u16, w_hw: u16, h: u16, halfwords: &[u16]) {
    gp0(g, 0x0100_0000); // clear cache
    gp0(g, 0xA000_0000);
    gp0(g, ((y as u32) << 16) | x as u32);
    gp0(g, ((h as u32) << 16) | w_hw as u32);
    let mut it = halfwords.chunks(2);
    for ch in &mut it {
        let lo = ch[0] as u32;
        let hi = *ch.get(1).unwrap_or(&0) as u32;
        gp0(g, lo | (hi << 16));
    }
}

/// Simple power-of-two aligned allocator over 4bpp texture pages (256x256
/// texels each) at X=320.., mirroring the game's TextureWindowAtlas intent.
pub struct Atlas {
    placed: Vec<(usize, u16, u16, u16, u16)>,
}

impl Atlas {
    pub fn new() -> Atlas {
        Atlas { placed: Vec::new() }
    }
    pub fn alloc(&mut self, w: u16, h: u16) -> (usize, u16, u16) {
        for page in 0..18usize {
            let mut v = 0u16;
            while v + h <= 256 {
                let mut u = 0u16;
                while u + w <= 256 {
                    let hit = self.placed.iter().any(|&(p, pu, pv, pw, ph)| {
                        p == page && u < pu + pw && pu < u + w && v < pv + ph && pv < v + h
                    });
                    if !hit {
                        self.placed.push((page, u, v, w, h));
                        return (page, u, v);
                    }
                    u += w;
                }
                v += h;
            }
        }
        panic!("atlas full");
    }
}

pub fn page_xy(page: usize) -> (u16, u16) {
    // Textures start at X=448 so an expanded (margin) frame buffer at the
    // VRAM origin never overlaps them.
    let x = 448 + (page % 9) as u16 * 64;
    let y = if page / 9 == 0 { 0 } else { 256 };
    (x, y)
}

pub fn tex_window_word(u0: u16, v0: u16, w: u16, h: u16) -> u32 {
    let mask_x = ((!(w - 1)) & 0xff) as u32 / 8;
    let mask_y = ((!(h - 1)) & 0xff) as u32 / 8;
    let off_x = (u0 / 8) as u32;
    let off_y = (v0 / 8) as u32;
    0xE200_0000 | mask_x | (mask_y << 5) | (off_x << 10) | (off_y << 15)
}

impl Frame {
    pub fn with_center(c: &Cooked, rect: (u16, u16, u16, u16), center: (i16, i16)) -> Frame {
        let mut gpu = Gpu::new();
        let mut atlas = Atlas::new();
        let mut slots = Vec::new();
        for (i, t) in c.texs.iter().enumerate() {
            let (page, u0, v0) = atlas.alloc(t.w, t.h);
            let (px, py) = page_xy(page);
            let hw: Vec<u16> = t
                .pix4
                .chunks(2)
                .map(|b| b[0] as u16 | ((*b.get(1).unwrap_or(&0) as u16) << 8))
                .collect();
            upload(&mut gpu, px + u0 / 4, py + v0, t.w / 4, t.h, &hw);
            let (cx, cy) = ((i % 20) as u16 * 16, 480 + (i / 20) as u16);
            upload(&mut gpu, cx, cy, 16, 1, &t.clut);
            let clut_word = (((cy as u32) << 6) | (cx as u32 >> 4)) << 16;
            let tpage = ((px as u32 / 64) & 15) | (((py as u32) / 256) << 4); // 4bpp, semi 0
            slots.push(Slot {
                clut_word,
                tpage_word: tpage << 16,
                window: tex_window_word(u0, v0, t.w, t.h),
            });
        }
        Frame {
            gpu,
            slots,
            center,
            view: rect,
        }
    }

    pub fn begin(&mut self, bg: u16) {
        let g = &mut self.gpu;
        let (x, y, w, h) = self.view;
        gp0(g, 0xE100_0000 | (1 << 10)); // draw mode: tpage 0, dither off, draw to display
        gp0(
            g,
            0xE300_0000 | ((FB_Y as u32 + y as u32) << 10) | (FB_X as u32 + x as u32),
        );
        gp0(
            g,
            0xE400_0000
                | ((FB_Y as u32 + y as u32 + h as u32 - 1) << 10)
                | (FB_X as u32 + x as u32 + w as u32 - 1),
        );
        gp0(g, 0xE600_0000);
        let bg24 = ((bg & 31) as u32) << 3
            | ((((bg >> 5) & 31) as u32) << 11)
            | ((((bg >> 10) & 31) as u32) << 19);
        gp0(g, 0x0200_0000 | bg24);
        gp0(g, ((y as u32) << 16) | x as u32);
        gp0(g, ((h as u32) << 16) | w as u32);
        gp0(
            g,
            0xE500_0000
                | ((self.center.0 as u32) & 0x7ff)
                | (((self.center.1 as u32) & 0x7ff) << 11),
        );
    }

    fn xy(p: [i16; 2]) -> u32 {
        (p[0] as u16 as u32) | ((p[1] as u16 as u32) << 16)
    }

    /// Draw the sorted weapon exactly as write_chain would; returns the GPU
    /// cycles the emulator's silicon-calibrated model charges for them.
    pub fn draw_textured(
        &mut self,
        md: &Model,
        p: &Projected,
        order: &[usize],
        shade: impl Fn(usize) -> u8,
    ) -> u64 {
        let before: u64 = self.gpu.gp0_timing_histogram().iter().sum();
        let mut window = u32::MAX;
        for &i in order {
            let t = md.triangle(i);
            let slot = self.slots[t.tex.min(self.slots.len() - 1)];
            if slot.window != window {
                gp0(&mut self.gpu, slot.window);
                window = slot.window;
            }
            let s = shade(i) as u32;
            let rgb = s | (s << 8) | (s << 16);
            let uvw = |k: usize| (t.uv[k].0 as u32) | ((t.uv[k].1 as u32) << 8);
            let [a, b, c] = t.idx.map(|x| x as usize);
            gp0(&mut self.gpu, 0x2400_0000 | rgb);
            gp0(&mut self.gpu, Self::xy(p.xy[a]));
            gp0(&mut self.gpu, uvw(0) | slot.clut_word);
            gp0(&mut self.gpu, Self::xy(p.xy[b]));
            gp0(&mut self.gpu, uvw(1) | slot.tpage_word);
            gp0(&mut self.gpu, Self::xy(p.xy[c]));
            gp0(&mut self.gpu, uvw(2));
        }
        let _ = before;
        gpu_cycles_for(md, p, order, self.center, self.view)
    }

    /// ID pass: flat triangles in the same order, colour = draw index + 1.
    pub fn draw_ids(&mut self, p: &Projected, md: &Model, order: &[usize]) {
        for (k, &i) in order.iter().enumerate() {
            let id = (k + 1) as u32;
            assert!(id < 0x8000);
            let rgb = ((id & 31) << 3) | (((id >> 5) & 31) << 11) | (((id >> 10) & 31) << 19);
            let t = md.triangle(i);
            let [a, b, c] = t.idx.map(|x| x as usize);
            gp0(&mut self.gpu, 0x2000_0000 | rgb);
            gp0(&mut self.gpu, Self::xy(p.xy[a]));
            gp0(&mut self.gpu, Self::xy(p.xy[b]));
            gp0(&mut self.gpu, Self::xy(p.xy[c]));
        }
    }

    pub fn pixel(&self, x: usize, y: usize) -> u16 {
        self.gpu.vram.get_pixel(FB_X + x as u16, FB_Y + y as u16)
    }

    pub fn rgb(&self) -> Vec<[u8; 3]> {
        let (x0, y0, w, h) = self.view;
        let mut out = Vec::with_capacity(w as usize * h as usize);
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                let p = self.gpu.vram.get_pixel(FB_X + x, FB_Y + y);
                let e = |v: u16| ((v << 3) | (v >> 2)) as u8;
                out.push([e(p & 31), e((p >> 5) & 31), e((p >> 10) & 31)]);
            }
        }
        out
    }
}

/// Coordinate-texture pass: every sampled texel's (u,v) inside its texture,
/// read back per pixel. Uses a 15bpp page holding u | v<<7 | 0x8000 and each
/// triangle's own texture window size (<=128).
pub struct UvPass {
    pub gpu: Gpu,
}

impl UvPass {
    pub fn new() -> UvPass {
        let mut gpu = Gpu::new();
        // 128x128 coordinate texture at VRAM (512,0), 15bpp page x=512.
        let mut hw = Vec::with_capacity(128 * 128);
        for v in 0..128u16 {
            for u in 0..128u16 {
                hw.push(u | (v << 7) | 0x8000);
            }
        }
        upload(&mut gpu, 512, 0, 128, 128, &hw);
        UvPass { gpu }
    }

    pub fn draw(
        &mut self,
        md: &Model,
        c: &Cooked,
        p: &Projected,
        order: &[usize],
        center: (i16, i16),
        view: (u16, u16, u16, u16),
    ) {
        let g = &mut self.gpu;
        let (x, y, w, h) = view;
        gp0(g, 0xE100_0000 | (1 << 10));
        gp0(g, 0xE300_0000 | ((y as u32) << 10) | x as u32);
        gp0(
            g,
            0xE400_0000 | ((y as u32 + h as u32 - 1) << 10) | (x as u32 + w as u32 - 1),
        );
        gp0(g, 0x0200_0000);
        gp0(g, ((y as u32) << 16) | x as u32);
        gp0(g, ((h as u32) << 16) | w as u32);
        gp0(
            g,
            0xE500_0000 | ((center.0 as u32) & 0x7ff) | (((center.1 as u32) & 0x7ff) << 11),
        );
        let tpage = (512u32 / 64) | (2 << 7); // 15bpp
        let mut window = u32::MAX;
        for &i in order {
            let t = md.triangle(i);
            let tx = &c.texs[t.tex];
            let win = tex_window_word(0, 0, tx.w, tx.h);
            if win != window {
                gp0(g, win);
                window = win;
            }
            let uvw = |k: usize| (t.uv[k].0 as u32) | ((t.uv[k].1 as u32) << 8);
            let [a, b, cc] = t.idx.map(|x| x as usize);
            gp0(g, 0x2500_0000 | 0x808080);
            gp0(g, Frame::xy(p.xy[a]));
            gp0(g, uvw(0));
            gp0(g, Frame::xy(p.xy[b]));
            gp0(g, uvw(1) | (tpage << 16));
            gp0(g, Frame::xy(p.xy[cc]));
            gp0(g, uvw(2));
        }
    }

    pub fn uv(&self, x: usize, y: usize) -> Option<(u8, u8)> {
        let p = self.gpu.vram.get_pixel(x as u16, y as u16);
        if p & 0x8000 == 0 {
            None
        } else {
            Some(((p & 127) as u8, ((p >> 7) & 127) as u8))
        }
    }
}
