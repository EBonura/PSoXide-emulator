// SPDX-License-Identifier: GPL-2.0-or-later
//! PSX hardware renderer -- wgpu render pipeline that draws each
//! GP0 primitive at the internal-resolution multiple of native PSX
//! VRAM, producing fractional upscaling for free.
//!
//! ## Architecture
//!
//! The HW target is **VRAM-shaped**: a `(1024 · S) × (512 · S)`
//! texture (S = internal-resolution multiplier). PSX vertex coords
//! map directly into this VRAM space; the display shown to the user
//! is just a sub-rect read of the texture (the PSX `display_area`,
//! scaled by S). This mirrors real hardware -- the GPU draws into
//! persistent VRAM and the CRT scans out a window of it.
//!
//! Consequences:
//! - **VRAM persistence works for free.** A demo that draws once at
//!   boot and then just present-flips keeps its pixels because we
//!   never clear the target between frames.
//! - **Native↔Window is a single knob (S).** S=1 → 1024×512 texture
//!   → tiny PSX-native rasterisation; the display sub-rect (e.g.
//!   320×240) gets scaled up by Nearest at egui paint = "big crisp
//!   PSX pixels". S=N → N× rasterisation density; the display
//!   sub-rect is N× larger and paints at ~1:1 = "sharp host edges".
//! - **One pipeline for everything.** The vertex shader divides by
//!   constant `(1024, 512)` regardless of S. The wgpu viewport
//!   tracks the texture's pixel dims, so density follows S
//!   automatically -- no shader maths changes when S does.
//!
//! ## Two backends, one crate
//!
//! This crate owns BOTH host-GPU implementations of the PSX GPU:
//!
//! - **Enhanced** ([`HwRenderer`], `pipeline`/`target`/`translator`):
//!   the user-facing render pipeline described above. Upscales, but
//!   is not bit-exact (f32 interpolation, 8-bit rather than 5-bit
//!   blending, host line and rectangle coverage). Polygons are drawn
//!   half a pixel down-right so their coverage and texels follow the
//!   CPU's corner sampling.
//! - **Accurate** ([`ComputeBackend`], `rasterizer`/`scanline`/
//!   `vram`/`replay`): a compute-shader rasterizer that reproduces
//!   the silicon-matched CPU rasterizer pixel-for-pixel at native
//!   resolution. Serves as the parity oracle for the shared decode
//!   path and as the seed for any future accurate display mode.
//!
//! Both consume the same `GpuCmdLogEntry` stream (recorded by
//! `emulator-core::Gpu`, or synthesized from an engine OT by
//! [`from_ot::build_cmd_log`]) through the shared `decode` helpers.
//!
//! ## Sibling crate
//!
//! - `emulator-core` -- owns `Gpu` (CPU rasterizer + VRAM + cmd_log)
//!   and `Bus`. Its CPU rasterizer is the source of truth.

// Internals: two renderer implementations over one decode layer. The
// crate's public surface is deliberately tiny (see the re-exports
// below) -- everything else is plumbing the frontend never touches.
pub(crate) mod decode;
mod from_ot;
pub(crate) mod interpreter;
pub(crate) mod pipeline;
pub(crate) mod primitive;
pub(crate) mod rasterizer;
mod replay;
pub(crate) mod scanline;
pub(crate) mod target;
/// Raw-byte format of the views the HW draw and blit passes render through.
pub use target::RENDER_FORMAT;
mod translator;
pub(crate) mod vram;

// The whole external API: the Enhanced renderer (HwRenderer +
// ScaleMode below), the Accurate/oracle backend, the OT-to-cmd_log
// bridge the editor preview uses, and the VRAM dimensions.
pub use from_ot::{build_cmd_log, build_cmd_log_into};
pub use replay::ComputeBackend;
pub use target::{VRAM_HEIGHT, VRAM_WIDTH};
pub use translator::Translator;

use pipeline::HwPipeline;
use target::{RenderTarget, MAX_SCALE};
use translator::DrawRun;

use emulator_core::gpu::GpuCmdLogEntry;
use emulator_core::Gpu;

/// Top-level HW renderer. Owns the wgpu pipeline + the VRAM-shaped
/// target. The frontend creates one, calls [`HwRenderer::render_frame`]
/// each frame, and the egui central panel reads
/// [`HwRenderer::texture_id`] (paired with the display-area UV
/// sub-rect -- the renderer doesn't care about display size, only
/// the PSX-VRAM space the primitives live in).
///
/// `wgpu::Device` and `wgpu::Queue` are internally reference-counted
/// in wgpu 24, so cloning them into the renderer is cheap.
pub struct HwRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: HwPipeline,
    target: RenderTarget,
    translator: Translator,
    texture_words: Vec<u16>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct VramCopyRect {
    sx: u32,
    sy: u32,
    dx: u32,
    dy: u32,
    w: u32,
    h: u32,
}

/// Toolbar Native↔Window selector. Mirrors the frontend's
/// `app::ScaleMode` so this crate doesn't depend on `frontend`.
/// The renderer translates this + a panel-size hint into an
/// internal-resolution multiplier S in [`HwRenderer::scale_for`].
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ScaleMode {
    /// Internal scale = 1. Display sub-rect rendered at PSX-native
    /// pixel density; egui Nearest scales up at paint = chunky
    /// retro pixels.
    Native,
    /// Internal scale chosen from the current presentation/display
    /// pixel budget, clamped to `[1, MAX_SCALE]`. Display sub-rect
    /// rasterised near host density = sharp edges at any window size.
    Window,
}

impl HwRenderer {
    /// Live constructor -- registers the target with `egui_renderer`
    /// so the central panel can paint it. Initial scale = 1; bump
    /// it via [`HwRenderer::set_internal_scale`] on toggle/resize.
    pub fn new(
        device: wgpu::Device,
        queue: wgpu::Queue,
        egui_renderer: &mut egui_wgpu::Renderer,
    ) -> Self {
        let pipeline = HwPipeline::new(&device);
        let target = RenderTarget::new(&device, &queue, egui_renderer);
        Self {
            device,
            queue,
            pipeline,
            target,
            translator: Translator::new(),
            texture_words: vec![0; (VRAM_WIDTH * VRAM_HEIGHT) as usize],
        }
    }

    /// Headless constructor -- no surface, no egui registration.
    /// Used by the parity harness and any CLI dump path.
    pub fn new_headless(device: wgpu::Device, queue: wgpu::Queue) -> Self {
        let pipeline = HwPipeline::new(&device);
        let target = RenderTarget::new_headless(&device, &queue);
        Self {
            device,
            queue,
            pipeline,
            target,
            translator: Translator::new(),
            texture_words: vec![0; (VRAM_WIDTH * VRAM_HEIGHT) as usize],
        }
    }

    /// Stable `egui::TextureId` of the VRAM-shaped target. The
    /// frontend's central panel reads this every frame, paired with
    /// a UV sub-rect = `display_area / VRAM_DIMS`.
    pub fn texture_id(&self) -> egui::TextureId {
        self.target.texture_id()
    }

    /// Current internal-resolution multiplier in effect.
    pub fn internal_scale(&self) -> u32 {
        self.target.scale()
    }

    /// Map [`ScaleMode`] + presentation/display dims to an internal
    /// scale. `Native` → 1; `Window` → the smallest integer scale
    /// that covers the current framebuffer presentation budget,
    /// capped at [`MAX_SCALE`]. Pure function so the frontend can
    /// test scaling decisions without wgpu state.
    pub fn scale_for(mode: ScaleMode, panel_size_px: (u32, u32), display_size: (u32, u32)) -> u32 {
        match mode {
            ScaleMode::Native => 1,
            ScaleMode::Window => {
                let sx = panel_size_px.0.max(1) as f32 / display_size.0.max(1) as f32;
                let sy = panel_size_px.1.max(1) as f32 / display_size.1.max(1) as f32;
                (sx.max(sy).ceil() as u32).clamp(1, MAX_SCALE)
            }
        }
    }

    /// Reallocate the target to the requested internal scale. Cheap
    /// when unchanged. Reallocation clears the new texture to opaque
    /// black; callers should resync the target from CPU VRAM when this
    /// returns `true`.
    pub fn set_internal_scale(
        &mut self,
        scale: u32,
        egui_renderer: Option<&mut egui_wgpu::Renderer>,
    ) -> bool {
        self.target
            .ensure_scale(&self.device, &self.queue, egui_renderer, scale)
    }

    /// Set the sample-time texture filter mode (0 nearest, 1 edge). Cheap
    /// uniform write; safe to call every frame.
    pub fn set_texture_filter(&self, mode: u32) {
        self.pipeline.set_filter_mode(&self.queue, mode);
    }

    /// Rebuild the persistent HW target from the CPU VRAM mirror.
    /// Use after internal-scale reallocations, which necessarily
    /// clear the target texture while CPU VRAM still contains the
    /// authoritative persistent PSX pixels.
    pub fn sync_target_from_vram(&mut self, vram_words: &[u16]) {
        self.sync_texture_from_vram(vram_words);
        self.write_scaled_vram_rect_wrapped(0, 0, VRAM_WIDTH, VRAM_HEIGHT, |col, row| {
            vram_words[(row * VRAM_WIDTH + col) as usize]
        });
    }

    /// Translator draw-env state for incremental-replay diagnostics.
    pub fn debug_env(&self) -> (i32, i32, i32, i32, i32, i32) {
        self.translator.debug_env()
    }

    /// Render one frame's `cmd_log` into the persistent VRAM target.
    /// Always loads the existing texture -- never clears -- so PSX
    /// VRAM-style persistence holds across frames. `vram_words` is
    /// the CPU rasterizer's VRAM snapshot from the start of this
    /// `cmd_log`; CPU→VRAM uploads and VRAM copies update the shader
    /// source texture as the command stream is replayed.
    pub fn render_frame(&mut self, gpu: &Gpu, cmd_log: &[GpuCmdLogEntry], vram_words: &[u16]) {
        self.sync_texture_from_vram(vram_words);
        // The dither path maps fragments back to PSX-native pixels, so
        // the shader needs the current internal scale.
        self.pipeline
            .set_internal_scale(&self.queue, self.target.scale());

        // Wireframe: edge strips draw with transparent interiors, so stale
        // edges would accumulate in this persistent target. Rebuild it from
        // CPU VRAM (kept clean per frame by the rasterizer's edge journal,
        // Gpu::wireframe_frame_boundary) before replaying, via a fullscreen
        // GPU blit -- works at any internal scale, so wireframe is correct
        // in both native and hi-res modes. Normal mode never blits: PSX
        // VRAM persistence semantics stay untouched.
        if gpu.wireframe_enabled {
            self.blit_vram_to_target();
        }
        let mut segment_start = 0;
        for (i, entry) in cmd_log.iter().enumerate() {
            if is_vram_image_op(entry) {
                self.render_draw_segment(&cmd_log[segment_start..i], gpu.wireframe_enabled);
                self.mirror_vram_image_op(entry, vram_words);
                segment_start = i + 1;
            }
        }
        self.render_draw_segment(&cmd_log[segment_start..], gpu.wireframe_enabled);
    }

    fn render_draw_segment(&mut self, cmd_log: &[GpuCmdLogEntry], wireframe: bool) {
        // Headless full-run replays can hand this a single giant segment
        // (millions of logged draws); translating it in one piece would
        // size the vertex buffer past wgpu's max-buffer limit. Segments
        // draw sequentially into the persistent target, so splitting
        // preserves order.
        const MAX_SEGMENT_ENTRIES: usize = 1 << 18;
        if cmd_log.len() > MAX_SEGMENT_ENTRIES {
            for chunk in cmd_log.chunks(MAX_SEGMENT_ENTRIES) {
                self.render_draw_segment(chunk, wireframe);
            }
            return;
        }
        if cmd_log.is_empty() {
            return;
        }

        let frame = self.translator.translate_with_wireframe(cmd_log, wireframe);
        if frame.total() > 0 {
            self.pipeline.upload_vertices(
                &self.device,
                &self.queue,
                bytemuck::cast_slice(frame.vertices),
            );
            Self::draw_runs(
                &self.device,
                &self.queue,
                &self.pipeline,
                &self.target,
                frame.runs,
            );
        }
    }

    /// Overwrite the whole scaled target with the current contents of the
    /// VRAM sampler texture (one fullscreen triangle). Wireframe-mode
    /// helper; see the call site in [`HwRenderer::render_frame`].
    fn blit_vram_to_target(&mut self) {
        self.blit_vram_to_view(self.target.render_view());
    }

    /// Expand the R16Uint VRAM texture into any `RENDER_FORMAT` color
    /// attachment (one fullscreen triangle, `fs_blit`'s BGR15 -> RGB8
    /// decode). `view` must be the raw-byte (`Rgba8Unorm`) view of the
    /// texture, like the target's render view: through an sRGB
    /// view the display codes would be encoded a second time. Public so the frontend's VRAM debug view can be filled
    /// GPU-side instead of re-decoding half a million pixels on the CPU
    /// every frame the way `Vram::to_rgba8` does.
    pub fn blit_vram_to_view(&self, view: &wgpu::TextureView) {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("psx-hw-blit-encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("psx-hw-blit-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_bind_group(0, self.pipeline.bind_group(), &[]);
            pass.set_pipeline(self.pipeline.blit_pipeline());
            pass.draw(0..3, 0..1);
        }
        self.queue.submit(Some(encoder.finish()));
    }

    fn draw_runs(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pipeline: &HwPipeline,
        target: &RenderTarget,
        runs: &[DrawRun],
    ) {
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("psx-hw-renderer-encoder"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("psx-hw-renderer-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target.render_view(),
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // PSX VRAM is persistent -- never clear.
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_bind_group(0, pipeline.bind_group(), &[]);
            pass.set_vertex_buffer(0, pipeline.vertex_buffer().slice(..));

            let scale = target.scale();
            let (target_w, target_h) = target.size();
            for run in runs {
                if run.count == 0 || run.clip[0] > run.clip[2] || run.clip[1] > run.clip[3] {
                    continue;
                }
                let x = run.clip[0] as u32 * scale;
                let y = run.clip[1] as u32 * scale;
                let right = ((run.clip[2] as u32 + 1) * scale).min(target_w);
                let bottom = ((run.clip[3] as u32 + 1) * scale).min(target_h);
                if right <= x || bottom <= y {
                    continue;
                }
                pass.set_scissor_rect(x, y, right - x, bottom - y);
                pass.set_pipeline(pipeline.pipeline(run.kind));
                pass.set_blend_constant(pipeline.blend_constant(run.kind));
                pass.draw(run.start..(run.start + run.count), 0..1);
            }
        }
        queue.submit(Some(encoder.finish()));
    }

    fn mirror_vram_image_op(&mut self, entry: &GpuCmdLogEntry, vram_words: &[u16]) {
        match entry.opcode {
            0x80..=0x9F => self.mirror_vram_copy(entry),
            0xA0..=0xBF => self.mirror_vram_upload(entry, vram_words),
            _ => {}
        }
    }

    fn mirror_vram_copy(&mut self, entry: &GpuCmdLogEntry) {
        let Some(rect) = decode_vram_copy_packet(&entry.fifo) else {
            return;
        };
        let VramCopyRect {
            sx,
            sy,
            dx,
            dy,
            w,
            h,
        } = rect;
        if w == 0 || h == 0 {
            return;
        }

        self.copy_texture_words_wrapped(sx, sy, dx, dy, w, h);
        self.upload_texture_words_rect(dx, dy, w, h);

        let scale = self.target.scale();
        let out_w = w * scale;
        let temp = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("psx-hw-vram-copy-temp"),
            size: wgpu::Extent3d {
                width: out_w,
                height: scale,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: crate::target::TARGET_FORMAT,
            usage: wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("psx-hw-vram-copy-encoder"),
            });

        // Match the CPU rasterizer's row-buffer copy semantics:
        // read one complete wrapped source row, then write that row
        // to the wrapped destination. This keeps overlapping copies
        // and edge-wrapping image ops in command order.
        for row in 0..h {
            let src_y = (sy + row) & (VRAM_HEIGHT - 1);
            let dst_y = (dy + row) & (VRAM_HEIGHT - 1);

            let mut copied = 0;
            while copied < w {
                let src_x = (sx + copied) & (VRAM_WIDTH - 1);
                let chunk_w = (w - copied).min(VRAM_WIDTH - src_x);
                encoder.copy_texture_to_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: self.target.texture(),
                        mip_level: 0,
                        origin: wgpu::Origin3d {
                            x: src_x * scale,
                            y: src_y * scale,
                            z: 0,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::TexelCopyTextureInfo {
                        texture: &temp,
                        mip_level: 0,
                        origin: wgpu::Origin3d {
                            x: copied * scale,
                            y: 0,
                            z: 0,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::Extent3d {
                        width: chunk_w * scale,
                        height: scale,
                        depth_or_array_layers: 1,
                    },
                );
                copied += chunk_w;
            }

            copied = 0;
            while copied < w {
                let dst_x = (dx + copied) & (VRAM_WIDTH - 1);
                let chunk_w = (w - copied).min(VRAM_WIDTH - dst_x);
                encoder.copy_texture_to_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &temp,
                        mip_level: 0,
                        origin: wgpu::Origin3d {
                            x: copied * scale,
                            y: 0,
                            z: 0,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::TexelCopyTextureInfo {
                        texture: self.target.texture(),
                        mip_level: 0,
                        origin: wgpu::Origin3d {
                            x: dst_x * scale,
                            y: dst_y * scale,
                            z: 0,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::Extent3d {
                        width: chunk_w * scale,
                        height: scale,
                        depth_or_array_layers: 1,
                    },
                );
                copied += chunk_w;
            }
        }
        self.queue.submit(Some(encoder.finish()));
    }

    fn mirror_vram_upload(&mut self, entry: &GpuCmdLogEntry, vram_words: &[u16]) {
        if entry.fifo.len() < 3 {
            return;
        }
        let xy = entry.fifo[1];
        let wh = entry.fifo[2];
        let x = xy & 0x3FF;
        let y = (xy >> 16) & 0x1FF;
        let raw_w = wh & 0x3FF;
        let raw_h = (wh >> 16) & 0x1FF;
        let w = if raw_w == 0 { VRAM_WIDTH } else { raw_w };
        let h = if raw_h == 0 { VRAM_HEIGHT } else { raw_h };
        let payload = entry.fifo.get(3..).unwrap_or(&[]);
        if payload.is_empty() {
            self.write_texture_words_rect_wrapped(x, y, w, h, |col, row| {
                let xx = (x + col) & (VRAM_WIDTH - 1);
                let yy = (y + row) & (VRAM_HEIGHT - 1);
                vram_words[(yy * VRAM_WIDTH + xx) as usize]
            });
            self.upload_texture_words_rect(x, y, w, h);
            self.write_scaled_vram_rect_wrapped(x, y, w, h, |col, row| {
                let xx = (x + col) & (VRAM_WIDTH - 1);
                let yy = (y + row) & (VRAM_HEIGHT - 1);
                vram_words[(yy * VRAM_WIDTH + xx) as usize]
            });
            return;
        }

        self.write_texture_words_rect_wrapped(x, y, w, h, |col, row| {
            let pixel_index = row * w + col;
            let Some(&word) = payload.get((pixel_index / 2) as usize) else {
                return 0;
            };
            if pixel_index & 1 == 0 {
                word as u16
            } else {
                (word >> 16) as u16
            }
        });
        self.upload_texture_words_rect(x, y, w, h);
        self.write_scaled_vram_rect_wrapped(x, y, w, h, |col, row| {
            let pixel_index = row * w + col;
            let Some(&word) = payload.get((pixel_index / 2) as usize) else {
                return 0;
            };
            if pixel_index & 1 == 0 {
                word as u16
            } else {
                (word >> 16) as u16
            }
        });
    }

    /// Bring the sampler texture up to `vram_words`, sending the GPU only
    /// what changed. `texture_words` always holds exactly what the GPU
    /// texture holds (every write to it goes through here or
    /// `upload_texture_words_rect`), so comparing against it finds the
    /// changed rows; each run of them uploads as one rectangle spanning its
    /// changed columns. A game redraws its framebuffer and little else, so
    /// this is a fraction of the megabyte a full upload moved every frame,
    /// with the same result.
    fn sync_texture_from_vram(&mut self, vram_words: &[u16]) {
        if vram_words.len() != self.texture_words.len() {
            return;
        }
        let width = VRAM_WIDTH as usize;
        let rows = VRAM_HEIGHT as usize;
        let mut y = 0;
        while y < rows {
            let row = y * width..(y + 1) * width;
            if self.texture_words[row.clone()] == vram_words[row] {
                y += 1;
                continue;
            }
            let first = y;
            let (mut x0, mut x1) = (width, 0);
            while y < rows {
                let row = y * width..(y + 1) * width;
                let (old, new) = (&self.texture_words[row.clone()], &vram_words[row]);
                let Some(lo) = old.iter().zip(new).position(|(a, b)| a != b) else {
                    break;
                };
                let hi = width
                    - old
                        .iter()
                        .rev()
                        .zip(new.iter().rev())
                        .position(|(a, b)| a != b)
                        .unwrap_or(0);
                x0 = x0.min(lo);
                x1 = x1.max(hi);
                y += 1;
            }
            for row in first..y {
                let span = row * width + x0..row * width + x1;
                self.texture_words[span.clone()].copy_from_slice(&vram_words[span]);
            }
            self.upload_texture_words_rect(
                x0 as u32,
                first as u32,
                (x1 - x0) as u32,
                (y - first) as u32,
            );
        }
    }

    fn copy_texture_words_wrapped(&mut self, sx: u32, sy: u32, dx: u32, dy: u32, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        let mut row_buf = vec![0u16; w as usize];
        for row in 0..h {
            let src_y = (sy + row) & (VRAM_HEIGHT - 1);
            let dst_y = (dy + row) & (VRAM_HEIGHT - 1);
            for col in 0..w {
                let src_x = (sx + col) & (VRAM_WIDTH - 1);
                row_buf[col as usize] = self.texture_words[(src_y * VRAM_WIDTH + src_x) as usize];
            }
            for col in 0..w {
                let dst_x = (dx + col) & (VRAM_WIDTH - 1);
                self.texture_words[(dst_y * VRAM_WIDTH + dst_x) as usize] = row_buf[col as usize];
            }
        }
    }

    fn write_texture_words_rect_wrapped(
        &mut self,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        pixel_at: impl Fn(u32, u32) -> u16,
    ) {
        for row in 0..h {
            let yy = (y + row) & (VRAM_HEIGHT - 1);
            for col in 0..w {
                let xx = (x + col) & (VRAM_WIDTH - 1);
                self.texture_words[(yy * VRAM_WIDTH + xx) as usize] = pixel_at(col, row);
            }
        }
    }

    fn upload_texture_words_rect(&self, x: u32, y: u32, w: u32, h: u32) {
        let words = &self.texture_words;
        self.pipeline
            .upload_vram_rect_wrapped(&self.queue, x, y, w, h, |col, row| {
                let xx = (x + col) & (VRAM_WIDTH - 1);
                let yy = (y + row) & (VRAM_HEIGHT - 1);
                words[(yy * VRAM_WIDTH + xx) as usize]
            });
    }

    fn write_scaled_vram_rect_wrapped(
        &self,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        pixel_at: impl Fn(u32, u32) -> u16,
    ) {
        if w == 0 || h == 0 {
            return;
        }

        let scale = self.target.scale();
        let mut row = 0;
        while row < h {
            let dst_y = (y + row) & (VRAM_HEIGHT - 1);
            let chunk_h = (h - row).min(VRAM_HEIGHT - dst_y);
            let mut col = 0;
            while col < w {
                let dst_x = (x + col) & (VRAM_WIDTH - 1);
                let chunk_w = (w - col).min(VRAM_WIDTH - dst_x);

                let out_w = chunk_w * scale;
                let out_h = chunk_h * scale;
                let row_bytes = out_w * 4;
                let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
                let padded_row_bytes = row_bytes.div_ceil(align) * align;
                let mut rgba = vec![0u8; (padded_row_bytes * out_h) as usize];

                for src_y in 0..chunk_h {
                    for src_x in 0..chunk_w {
                        let color = bgr15_to_rgba8(pixel_at(col + src_x, row + src_y));
                        for sy in 0..scale {
                            let out_y = src_y * scale + sy;
                            let row_start = (out_y * padded_row_bytes) as usize;
                            for sx in 0..scale {
                                let out_x = src_x * scale + sx;
                                let off = row_start + (out_x * 4) as usize;
                                rgba[off..off + 4].copy_from_slice(&color);
                            }
                        }
                    }
                }

                self.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: self.target.texture(),
                        mip_level: 0,
                        origin: wgpu::Origin3d {
                            x: dst_x * scale,
                            y: dst_y * scale,
                            z: 0,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    &rgba,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(padded_row_bytes),
                        rows_per_image: Some(out_h),
                    },
                    wgpu::Extent3d {
                        width: out_w,
                        height: out_h,
                        depth_or_array_layers: 1,
                    },
                );

                col += chunk_w;
            }
            row += chunk_h;
        }
    }

    /// Synchronously read back the entire VRAM-shaped target as
    /// tightly-packed RGBA8 (sRGB color space, since `TARGET_FORMAT`
    /// is `Rgba8UnormSrgb`). Issues a blocking `device.poll(Wait)`
    /// -- for headless/parity use, never the per-frame loop.
    pub fn read_pixels_rgba8(&self) -> (u32, u32, Vec<u8>) {
        let (w, h) = self.target.size();
        self.read_subrect_rgba8(0, 0, w, h)
    }

    /// Read back a `(w × h)` sub-rect of the target starting at
    /// `(x, y)` in target-pixel coordinates (i.e. PSX VRAM coords ×
    /// internal scale). Designed for parity-style display-sub-rect
    /// extraction: pass `(display.x * S, display.y * S, display.w *
    /// S, display.h * S)` to grab exactly the user-visible region.
    pub fn read_subrect_rgba8(&self, x: u32, y: u32, w: u32, h: u32) -> (u32, u32, Vec<u8>) {
        let (tw, th) = self.target.size();
        let x = x.min(tw);
        let y = y.min(th);
        let w = w.min(tw - x);
        let h = h.min(th - y);
        if w == 0 || h == 0 {
            return (0, 0, Vec::new());
        }
        let unpadded_bpr = w * 4;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_bpr = unpadded_bpr.div_ceil(align) * align;
        let buffer_size = (padded_bpr * h) as wgpu::BufferAddress;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("psx-hw-readback"),
            size: buffer_size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("psx-hw-readback-encoder"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: self.target.texture(),
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bpr),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| r.expect("map readback"));
        self.device.poll(wgpu::Maintain::Wait);
        let data = slice.get_mapped_range();
        let mut out = Vec::with_capacity((unpadded_bpr * h) as usize);
        for row in 0..h {
            let start = (row * padded_bpr) as usize;
            let end = start + unpadded_bpr as usize;
            out.extend_from_slice(&data[start..end]);
        }
        drop(data);
        buffer.unmap();
        (w, h, out)
    }
}

fn is_vram_image_op(entry: &GpuCmdLogEntry) -> bool {
    matches!(entry.opcode, 0x80..=0xBF)
}

fn decode_vram_copy_packet(fifo: &[u32]) -> Option<VramCopyRect> {
    if fifo.len() < 4 {
        return None;
    }
    let src = fifo[1];
    let dst = fifo[2];
    let wh = fifo[3];
    let raw_w = wh & (VRAM_WIDTH - 1);
    let raw_h = (wh >> 16) & (VRAM_HEIGHT - 1);
    Some(VramCopyRect {
        sx: src & (VRAM_WIDTH - 1),
        sy: (src >> 16) & (VRAM_HEIGHT - 1),
        dx: dst & (VRAM_WIDTH - 1),
        dy: (dst >> 16) & (VRAM_HEIGHT - 1),
        w: if raw_w == 0 { VRAM_WIDTH } else { raw_w },
        h: if raw_h == 0 { VRAM_HEIGHT } else { raw_h },
    })
}

fn bgr15_to_rgba8(pixel: u16) -> [u8; 4] {
    let r5 = (pixel & 0x1F) as u8;
    let g5 = ((pixel >> 5) & 0x1F) as u8;
    let b5 = ((pixel >> 10) & 0x1F) as u8;
    [
        (r5 << 3) | (r5 >> 2),
        (g5 << 3) | (g5 >> 2),
        (b5 << 3) | (b5 >> 2),
        0xFF,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headless_renderer() -> Option<HwRenderer> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))?;
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("psx-hw-sync-test-device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))
        .ok()?;
        Some(HwRenderer::new_headless(device, queue))
    }

    fn pixel_block(renderer: &HwRenderer, psx_x: u32, psx_y: u32) -> Vec<[u8; 4]> {
        let scale = renderer.internal_scale();
        let (_, _, rgba) = renderer.read_subrect_rgba8(psx_x * scale, psx_y * scale, scale, scale);
        rgba.chunks_exact(4)
            .map(|px| [px[0], px[1], px[2], px[3]])
            .collect()
    }

    #[test]
    fn vram_copy_packet_decode_masks_psx_fields() {
        let rect = decode_vram_copy_packet(&[0x80_00_00_00, 0x0203_0402, 0x0206_0405, 0x0201_0402])
            .unwrap();

        assert_eq!(
            rect,
            VramCopyRect {
                sx: 2,
                sy: 3,
                dx: 5,
                dy: 6,
                w: 2,
                h: 1,
            }
        );
    }

    /// The fullscreen blit (wireframe rebuild, VRAM debug view) must leave
    /// the stored byte the VRAM display code, as the CPU decode does: a grey
    /// ramp of all 32 levels, through the blit, reads back as the codes.
    /// Rendering the blit through the sRGB view stored encode(code) instead,
    /// so the ramp came out brighter than the same VRAM drawn or uploaded.
    #[test]
    fn the_fullscreen_blit_stores_the_vram_display_codes() {
        let Some(mut renderer) = headless_renderer() else {
            eprintln!("skipping HW blit test: no headless wgpu adapter");
            return;
        };
        let mut vram_words = vec![0u16; (VRAM_WIDTH * VRAM_HEIGHT) as usize];
        for level in 0..32u16 {
            let word = level | (level << 5) | (level << 10);
            vram_words[(40 * VRAM_WIDTH) as usize + level as usize] = word;
        }
        renderer.sync_texture_from_vram(&vram_words);
        renderer.blit_vram_to_target();
        for level in 0..32u16 {
            let word = level | (level << 5) | (level << 10);
            assert_eq!(
                pixel_block(&renderer, u32::from(level), 40),
                vec![bgr15_to_rgba8(word)],
                "level {level}"
            );
        }
    }

    #[test]
    fn sync_target_from_vram_restores_pixels_after_scale_reallocation() {
        let Some(mut renderer) = headless_renderer() else {
            eprintln!("skipping HW sync test: no headless wgpu adapter");
            return;
        };

        let psx_x = 37;
        let psx_y = 29;
        let color = 0x001F;
        let mut vram_words = vec![0; (VRAM_WIDTH * VRAM_HEIGHT) as usize];
        vram_words[(psx_y * VRAM_WIDTH + psx_x) as usize] = color;

        renderer.sync_target_from_vram(&vram_words);
        assert_eq!(
            pixel_block(&renderer, psx_x, psx_y),
            vec![bgr15_to_rgba8(color)]
        );

        assert!(!renderer.set_internal_scale(1, None));
        assert!(renderer.set_internal_scale(2, None));
        assert_eq!(
            pixel_block(&renderer, psx_x, psx_y),
            vec![[0, 0, 0, 255]; 4]
        );

        renderer.sync_target_from_vram(&vram_words);
        assert_eq!(
            pixel_block(&renderer, psx_x, psx_y),
            vec![bgr15_to_rgba8(color); 4]
        );
    }

    /// Run one GP0 word stream through the CPU rasterizer with the
    /// command log armed, then replay that same log through the HW
    /// renderer at internal scale 1. Returns `(cpu_vram, renderer)`
    /// so callers can compare the two backends pixel for pixel.
    fn run_both_backends(
        words: &[u32],
        renderer: &mut HwRenderer,
        seed: &[(u16, u16, u16)],
    ) -> Vec<u16> {
        let mut cpu = Gpu::new();
        for &(x, y, v) in seed {
            cpu.vram.set_pixel(x, y, v);
        }
        // render_frame wants VRAM as of the *start* of the log.
        let start_vram = cpu.vram.words().to_vec();
        cpu.enable_cmd_log();
        for &w in words {
            cpu.gp0_push(w);
        }
        renderer.render_frame(&cpu, &cpu.cmd_log, &start_vram);
        cpu.vram.words().to_vec()
    }

    #[test]
    fn oversized_textured_quad_preserves_background() {
        let Some(mut renderer) = headless_renderer() else {
            eprintln!("skipping HW extent test: no headless wgpu adapter");
            return;
        };
        let mut words = line_env();
        words.extend_from_slice(&[
            0x0200_FF00,
            0,
            xyw(320, 240),
            // A black 4bpp foreground quad whose height exceeds the GPU's
            // 511-pixel triangle limit must not erase the valid background.
            0x2E80_8080,
            pack_xy((-209, -1023)),
            (480 * 64) << 16,
            pack_xy((431, -1023)),
            (6 + 32) << 16,
            pack_xy((-209, 1023)),
            0,
            pack_xy((431, 1023)),
            0,
        ]);
        let cpu = run_both_backends(&words, &mut renderer, &[(384, 0, 0x1111), (1, 480, 1)]);
        let (_, _, rgba) = renderer.read_subrect_rgba8(0, 0, 320, 240);
        for (i, got) in rgba.chunks_exact(4).enumerate() {
            let y = i / 320;
            let x = i % 320;
            let want = bgr15_to_rgba8(cpu[y * VRAM_WIDTH as usize + x]);
            assert_eq!(got, want, "background at ({x},{y})");
            assert_eq!(got, [0, 255, 0, 255]);
        }
    }

    fn pack_xy(p: (i32, i32)) -> u32 {
        ((p.0 as u32) & 0xFFFF) | (((p.1 as u32) & 0xFFFF) << 16)
    }

    /// GP0 word stream for one axis-aligned Gouraud quad over
    /// `(8,8)..(40,40)`. Axis-aligned so both backends cover exactly
    /// the same pixels -- a diagonal edge diverges on the fill rule,
    /// which would drown out the colour comparison this exists to make.
    fn gouraud_quad(colors: [u32; 4], dither: bool) -> Vec<u32> {
        vec![
            0xE300_0000,                                  // draw area top-left (0,0)
            0xE400_0000 | 1023 | (511 << 10),             // draw area bottom-right
            0xE500_0000,                                  // draw offset (0,0)
            0xE100_0000 | if dither { 0x200 } else { 0 }, // draw mode
            0x3800_0000 | colors[0],                      // Gouraud quad + vertex 0 colour
            pack_xy((8, 8)),
            colors[1],
            pack_xy((40, 8)),
            colors[2],
            pack_xy((8, 40)),
            colors[3],
            pack_xy((40, 40)),
        ]
    }

    /// Read the 32x32 quad interior from both backends as
    /// `(cpu_rgba, hw_rgba)` pairs, in row-major order.
    fn quad_interior(cpu_vram: &[u16], renderer: &HwRenderer) -> Vec<([u8; 4], [u8; 4])> {
        let s = renderer.internal_scale();
        let (_, _, rgba) = renderer.read_subrect_rgba8(8 * s, 8 * s, 32 * s, 32 * s);
        (0..32u32)
            .flat_map(|y| (0..32u32).map(move |x| (x, y)))
            .map(|(x, y)| {
                let want = cpu_vram[((y + 8) * VRAM_WIDTH + x + 8) as usize];
                // Top-left sample of this PSX pixel's SxS target block.
                let o = ((y * s * 32 * s) + x * s) * 4;
                let got = &rgba[o as usize..][..4];
                (bgr15_to_rgba8(want), [got[0], got[1], got[2], got[3]])
            })
            .collect()
    }

    fn at(i: usize) -> (usize, usize) {
        (i % 32 + 8, i / 32 + 8)
    }

    /// GP0(E1) bit 9 dither must reach the HW fragment shader and
    /// produce byte-identical output to the CPU rasterizer. Regression
    /// guard: the flag used to stop at the translator, so `--dump-hw`
    /// rendered flat 8-bit colour while `--dump-display` dithered.
    ///
    /// Flat vertex colours, so barycentric interpolation is exact and
    /// every remaining difference is the dither itself. The pure black
    /// and pure white cases pin the clamp: `dither_rgb` saturates at
    /// 0 and 255 rather than wrapping, so those must come back flat.
    #[test]
    fn flat_gouraud_dither_matches_cpu_backend_exactly() {
        let Some(mut renderer) = headless_renderer() else {
            eprintln!("skipping HW dither test: no headless wgpu adapter");
            return;
        };
        assert_eq!(renderer.internal_scale(), 1, "compares PSX-native pixels");

        // (colour word, must the dither break it into >1 colour?)
        // 0x00C08040 = R 0x40, G 0x80, B 0xC0 -- each channel sits off a
        // 5-bit boundary, so every channel has dithering to do.
        let cases = [
            (0x00C0_8040u32, true),
            (0x0008_0808, true),  // near black, still straddling a 5-bit step
            (0x00FF_FFFF, false), // white: +3 clamps back to 255
            (0x0000_0000, false), // black: -4 clamps back to 0
        ];
        for (color, expect_pattern) in cases {
            let cpu_vram = run_both_backends(&gouraud_quad([color; 4], true), &mut renderer, &[]);
            let pixels = quad_interior(&cpu_vram, &renderer);
            for (i, (want, got)) in pixels.iter().enumerate() {
                let (x, y) = at(i);
                assert_eq!(
                    want, got,
                    "colour {color:#08x}: CPU/HW divergence at ({x}, {y})"
                );
            }
            let distinct: std::collections::HashSet<_> = pixels.iter().map(|(w, _)| *w).collect();
            assert_eq!(
                distinct.len() > 1,
                expect_pattern,
                "colour {color:#08x}: unexpected dither spread {distinct:?}"
            );
        }
    }

    /// The control the equality tests can't provide on their own: with
    /// GP0(E1) bit 9 clear the quad must come back as one flat colour.
    /// A shader that dithers unconditionally passes every other test
    /// here and fails this one.
    #[test]
    fn dither_off_leaves_the_quad_flat() {
        let Some(mut renderer) = headless_renderer() else {
            eprintln!("skipping HW dither test: no headless wgpu adapter");
            return;
        };

        let color = 0x00C0_8040;
        let cpu_vram = run_both_backends(&gouraud_quad([color; 4], false), &mut renderer, &[]);
        let pixels = quad_interior(&cpu_vram, &renderer);
        let hw: std::collections::HashSet<_> = pixels.iter().map(|(_, g)| *g).collect();
        let cpu: std::collections::HashSet<_> = pixels.iter().map(|(w, _)| *w).collect();
        assert_eq!(cpu.len(), 1, "CPU reference is not flat: {cpu:?}");
        assert_eq!(hw.len(), 1, "HW dithered with GP0(E1) bit 9 clear: {hw:?}");
    }

    /// The 4x4 matrix is indexed by PSX pixel, not by target pixel, so
    /// at internal scale S every PSX pixel must still be one uniform
    /// SxS block matching the CPU. Fails if the scale never reaches the
    /// shader: the pattern would then vary inside each block.
    ///
    /// Polygons are drawn half a PSX pixel down-right (see the shader's
    /// `vs_main`), which at scale 2 is one host pixel, so the quad's first
    /// block row and column are half covered; the check starts at the
    /// first fully covered block.
    #[test]
    fn dither_pattern_follows_psx_pixels_at_internal_scale_2() {
        let Some(mut renderer) = headless_renderer() else {
            eprintln!("skipping HW dither test: no headless wgpu adapter");
            return;
        };
        assert!(renderer.set_internal_scale(2, None));

        let cpu_vram = run_both_backends(&gouraud_quad([0x00C0_8040; 4], true), &mut renderer, &[]);
        let (_, _, rgba) = renderer.read_subrect_rgba8(16, 16, 64, 64);
        for y in 1..32usize {
            for x in 1..32usize {
                let want = bgr15_to_rgba8(cpu_vram[(y + 8) * VRAM_WIDTH as usize + x + 8]);
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let o = ((y * 2 + dy) * 64 + x * 2 + dx) * 4;
                    let got = [rgba[o], rgba[o + 1], rgba[o + 2], rgba[o + 3]];
                    assert_eq!(
                        want,
                        got,
                        "scale-2 block ({}, {}) sub-pixel ({dx}, {dy}) diverges",
                        x + 8,
                        y + 8
                    );
                }
            }
        }
    }

    /// Same path with a real gradient. The HW backend interpolates in
    /// f32 where the CPU walks a fixed-point plane, and dither makes
    /// that tolerance visible: a sub-LSB interpolation difference near a
    /// 5-bit boundary flips the truncated channel. So this pins the
    /// bound (one 5-bit step) rather than byte equality -- exactness on
    /// a gradient needs the compute backend, not this rasterizer.
    #[test]
    fn gouraud_gradient_dither_stays_within_one_step_of_cpu() {
        let Some(mut renderer) = headless_renderer() else {
            eprintln!("skipping HW dither test: no headless wgpu adapter");
            return;
        };

        let colors = [0x00C0_8040, 0x0080_40C0, 0x0040_C080, 0x00A0_60E0];
        let cpu_vram = run_both_backends(&gouraud_quad(colors, true), &mut renderer, &[]);
        let pixels = quad_interior(&cpu_vram, &renderer);

        for (i, (want, got)) in pixels.iter().enumerate() {
            let (x, y) = at(i);
            for ch in 0..3 {
                let steps = (want[ch] as i32 - got[ch] as i32).abs() / 8;
                assert!(
                    steps <= 1,
                    "({x}, {y}) channel {ch} off by {steps} 5-bit steps: cpu {want:?} hw {got:?}"
                );
                // The one-step bound alone would also hold for an
                // undithered image, so pin what only this path
                // produces: a 15bpp display code. Skipping dither
                // leaves the HW backend's full 8-bit gradient, which
                // is not one.
                let c5 = got[ch] >> 3;
                assert_eq!(
                    got[ch],
                    (c5 << 3) | (c5 >> 2),
                    "({x}, {y}) channel {ch} was not truncated to 15bpp: {got:?}"
                );
            }
        }
    }

    /// VRAM seed + GP0 stream for a row of 8x8 tint-modulated textured
    /// quads (GP0 0x2C), one per `(texel, tint)` combo, at `y = 8` and
    /// `x = 8 + k * 8`. Texture is 15bpp direct at tpage (512, 0), one
    /// texel per combo.
    ///
    /// UV is constant across each quad, so both backends sample exactly
    /// the same texel at every pixel and no UV-interpolation difference
    /// can leak into what this measures: `modulate` plus the dither.
    /// 8x8 walks the whole 4x4 dither matrix four times.
    fn tinted_textured_quads(combos: &[(u16, u32)]) -> (Vec<(u16, u16, u16)>, Vec<u32>) {
        const TEX_X: u16 = 512; // tpage x unit 8 * 64
        const TPAGE: u32 = 8 | (2 << 7); // x unit 8, y 0, 15bpp direct
        let seed = combos
            .iter()
            .enumerate()
            .map(|(k, &(texel, _))| (TEX_X + k as u16, 0, texel))
            .collect();
        let mut words = vec![
            0xE300_0000,                      // draw area top-left (0,0)
            0xE400_0000 | 1023 | (511 << 10), // draw area bottom-right
            0xE500_0000,                      // draw offset (0,0)
            0xE100_0200,                      // draw mode: dither on
        ];
        for (k, &(_, tint)) in combos.iter().enumerate() {
            let x0 = 8 + k as i32 * 8;
            let uv = k as u32; // u = k, v = 0
            words.extend_from_slice(&[
                0x2C00_0000 | tint,
                pack_xy((x0, 8)),
                uv, // clut (unused at 15bpp) in the high half
                pack_xy((x0 + 8, 8)),
                uv | (TPAGE << 16),
                pack_xy((x0, 16)),
                uv,
                pack_xy((x0 + 8, 16)),
                uv,
            ]);
        }
        (seed, words)
    }

    /// Tint-modulated textured prims with GP0(E1) dither on must match
    /// the CPU rasterizer byte for byte.
    ///
    /// This is the case the shader's old float modulate could not hold:
    /// it computed `tex * tint * 2/255` where the CPU computes
    /// `tint * (t5 << 3) / 0x80`, i.e. `/128`. 2/255 != 1/128, so the HW
    /// ran up to 2/255 bright -- invisible on its own, but the dither
    /// adds -4..+3 before truncating to 5 bits, so a combo sitting near
    /// a bucket edge flips the truncated channel and the pixel lands a
    /// whole 5-bit step off.
    #[test]
    fn tinted_textured_quad_dither_matches_cpu_backend_exactly() {
        let Some(mut renderer) = headless_renderer() else {
            eprintln!("skipping HW modulate test: no headless wgpu adapter");
            return;
        };
        assert_eq!(renderer.internal_scale(), 1, "compares PSX-native pixels");

        let bgr15 = |r: u16, g: u16, b: u16| r | (g << 5) | (b << 10);
        // Tint word packs `R | G << 8 | B << 16`. No combo may use the
        // 0x808080 identity tint: the CPU treats it as the raw-texture
        // sentinel and stops dithering.
        let combos = [
            (bgr15(31, 17, 9), 0x00C0_8040),
            (bgr15(16, 16, 16), 0x00E3_9137),
            (bgr15(5, 21, 30), 0x007F_01FF),
            (bgr15(31, 31, 31), 0x00FF_FFFF), // clamps at 255 pre-dither
            (bgr15(1, 2, 3), 0x0080_817F),
            (bgr15(24, 12, 6), 0x0033_2211),
            (bgr15(9, 18, 27), 0x00A5_5AC3),
            (bgr15(30, 15, 7), 0x0040_7E81),
        ];

        let (seed, words) = tinted_textured_quads(&combos);
        let cpu_vram = run_both_backends(&words, &mut renderer, &seed);
        let w = combos.len() as u32 * 8;
        let (_, _, rgba) = renderer.read_subrect_rgba8(8, 8, w, 8);

        let mut spread = 0usize;
        for (k, (texel, tint)) in combos.iter().enumerate() {
            let mut distinct = std::collections::HashSet::new();
            for y in 0..8u32 {
                for x in 0..8u32 {
                    let px = 8 + k as u32 * 8 + x;
                    let py = 8 + y;
                    let want = bgr15_to_rgba8(cpu_vram[(py * VRAM_WIDTH + px) as usize]);
                    let o = ((y * w + px - 8) * 4) as usize;
                    let got = [rgba[o], rgba[o + 1], rgba[o + 2], rgba[o + 3]];
                    assert_eq!(
                        want, got,
                        "texel {texel:#06x} x tint {tint:#08x}: CPU/HW divergence at ({px}, {py})"
                    );
                    distinct.insert(want);
                }
            }
            assert_ne!(
                distinct.iter().copied().collect::<Vec<_>>(),
                vec![[0, 0, 0, 255]],
                "texel {texel:#06x} x tint {tint:#08x}: quad never drew"
            );
            spread += usize::from(distinct.len() > 1);
        }
        // Without a real dither spread the equality above would also
        // hold for a shader that never dithers at all. Only the
        // saturating white x white combo is legitimately flat.
        assert!(
            spread >= combos.len() - 1,
            "dither spread only {spread}/{} combos",
            combos.len()
        );
    }

    fn load_batch_tape(path: &str) -> Vec<Vec<GpuCmdLogEntry>> {
        let data = std::fs::read(path).expect("batch tape");
        let mut batches = Vec::new();
        let mut i = 0usize;
        while i + 4 <= data.len() {
            let n = u32::from_le_bytes(data[i..i + 4].try_into().unwrap()) as usize;
            i += 4;
            let mut batch = Vec::with_capacity(n);
            for _ in 0..n {
                let opcode = data[i];
                i += 1;
                let wc = u32::from_le_bytes(data[i..i + 4].try_into().unwrap()) as usize;
                i += 4;
                let mut fifo = Vec::with_capacity(wc);
                for _ in 0..wc {
                    fifo.push(u32::from_le_bytes(data[i..i + 4].try_into().unwrap()));
                    i += 4;
                }
                batch.push(GpuCmdLogEntry {
                    index: 0,
                    opcode,
                    fifo: fifo.into(),
                });
            }
            batches.push(batch);
        }
        batches
    }

    /// Probe: after replaying a prefix of the recorded batch tape, does a
    /// page-0 quad still land? Binary-searches the first poisoned prefix.
    /// Run with PSX_BATCH_TAPE=<batches.bin> -- --ignored --nocapture
    #[test]
    #[ignore]
    fn bisect_poison_batch() {
        let Ok(tape_path) = std::env::var("PSX_BATCH_TAPE") else {
            eprintln!("set PSX_BATCH_TAPE");
            return;
        };
        let batches = load_batch_tape(&tape_path);
        eprintln!("tape: {} batches", batches.len());
        let vram_words = vec![0u16; (VRAM_WIDTH * VRAM_HEIGHT) as usize];
        let gpu = Gpu::new();
        let probe_fails = |prefix: usize| -> bool {
            let Some(mut renderer) = headless_renderer() else {
                panic!("no adapter");
            };
            let _ = renderer.set_internal_scale(1, None);
            for batch in &batches[..prefix] {
                renderer.render_frame(&gpu, batch, &vram_words);
            }
            let env_and_quad = [
                GpuCmdLogEntry {
                    index: 0,
                    opcode: 0xE3,
                    fifo: vec![0xE300_0000].into(),
                },
                GpuCmdLogEntry {
                    index: 1,
                    opcode: 0xE4,
                    fifo: vec![0xE400_0000 | (239 << 10) | 319].into(),
                },
                GpuCmdLogEntry {
                    index: 2,
                    opcode: 0xE5,
                    fifo: vec![0xE500_0000].into(),
                },
                GpuCmdLogEntry {
                    index: 3,
                    opcode: 0x28,
                    fifo: vec![
                        0x2800_3050,
                        (200 << 16) | 8,
                        (200 << 16) | 24,
                        (216 << 16) | 8,
                        (216 << 16) | 24,
                    ]
                    .into(),
                },
            ];
            renderer.render_frame(&gpu, &env_and_quad, &vram_words);
            let (_, _, rgba) = renderer.read_subrect_rgba8(10, 202, 1, 1);
            rgba[..3] != [0x50, 0x30, 0x00]
        };
        let total = batches.len();
        if !probe_fails(total) {
            eprintln!("probe PASSES after full tape -- no poison found");
            return;
        }
        let mut lo = 0usize; // passes
        let mut hi = total; // fails
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            let f = probe_fails(mid);
            eprintln!("prefix {mid}: {}", if f { "FAIL" } else { "pass" });
            if f {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        eprintln!(
            "first poisoned prefix: {hi} (poison batch index {})",
            hi - 1
        );
        let poison = &batches[hi - 1];
        eprintln!("poison batch: {} entries", poison.len());
        for e in poison.iter().take(40) {
            eprintln!(
                "  op {:02x} words {:x?}",
                e.opcode,
                &e.fifo[..e.fifo.len().min(8)]
            );
        }
    }

    /// Replay the recorded batch tape and dump both framebuffer pages
    /// plus per-page brightness, to confirm the in-test reproduction.
    /// PSX_BATCH_TAPE=<batches.bin> -- --ignored --nocapture
    #[test]
    #[ignore]
    fn replay_tape_and_dump_pages() {
        let Ok(tape_path) = std::env::var("PSX_BATCH_TAPE") else {
            eprintln!("set PSX_BATCH_TAPE");
            return;
        };
        let batches = load_batch_tape(&tape_path);
        let Some(mut renderer) = headless_renderer() else {
            panic!("no adapter");
        };
        let vram_words = vec![0u16; (VRAM_WIDTH * VRAM_HEIGHT) as usize];
        let gpu = Gpu::new();
        for batch in &batches {
            renderer.render_frame(&gpu, batch, &vram_words);
        }
        let (w, h, rgba) = renderer.read_subrect_rgba8(0, 0, 320, 480);
        let mut bright0 = 0u32;
        let mut bright240 = 0u32;
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                let s = rgba[i] as u32 + rgba[i + 1] as u32 + rgba[i + 2] as u32;
                if s > 30 {
                    if y < 240 {
                        bright0 += 1;
                    } else {
                        bright240 += 1;
                    }
                }
            }
        }
        eprintln!("page0 bright={bright0} page240 bright={bright240}");
        let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
        for px in rgba.chunks_exact(4) {
            ppm.extend_from_slice(&px[..3]);
        }
        std::fs::write("/tmp/tape-replay-pages.ppm", ppm).unwrap();
        eprintln!("wrote /tmp/tape-replay-pages.ppm");
    }

    /// Bisect with the REAL page0 cycle as probe: after a prefix of the
    /// tape, replay the recorded page0 env+draw batches and check page0
    /// receives non-fill content. PSX_BATCH_TAPE + PSX_PROBE_ENV +
    /// PSX_PROBE_DRAW select the tape and probe batch indices.
    #[test]
    #[ignore]
    fn bisect_poison_real_probe() {
        let Ok(tape_path) = std::env::var("PSX_BATCH_TAPE") else {
            return;
        };
        let env_idx: usize = std::env::var("PSX_PROBE_ENV").unwrap().parse().unwrap();
        let draw_idx: usize = std::env::var("PSX_PROBE_DRAW").unwrap().parse().unwrap();
        let batches = load_batch_tape(&tape_path);
        let vram_words = vec![0u16; (VRAM_WIDTH * VRAM_HEIGHT) as usize];
        let gpu = Gpu::new();
        let probe_fails = |prefix: usize| -> bool {
            let Some(mut renderer) = headless_renderer() else {
                panic!("no adapter");
            };
            for batch in &batches[..prefix] {
                renderer.render_frame(&gpu, batch, &vram_words);
            }
            renderer.render_frame(&gpu, &batches[env_idx], &vram_words);
            renderer.render_frame(&gpu, &batches[draw_idx], &vram_words);
            let (w, h, rgba) = renderer.read_subrect_rgba8(0, 0, 320, 240);
            let mut content = 0u32;
            for i in (0..(w * h * 4) as usize).step_by(4) {
                let px = (rgba[i], rgba[i + 1], rgba[i + 2]);
                if px != (5, 7, 12) && px != (0, 0, 0) {
                    content += 1;
                }
            }
            content < 100
        };
        eprintln!(
            "standalone (prefix 0): {}",
            if probe_fails(0) { "FAIL" } else { "pass" }
        );
        let total = batches.len().min(env_idx);
        if !probe_fails(total) {
            eprintln!("probe passes after full prefix -- not state-armed");
            return;
        }
        let mut lo = 0usize;
        let mut hi = total;
        if probe_fails(0) {
            eprintln!("fails standalone -- batch content alone is broken");
            return;
        }
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            let f = probe_fails(mid);
            eprintln!("prefix {mid}: {}", if f { "FAIL" } else { "pass" });
            if f {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        eprintln!("first arming prefix: {hi} (batch index {})", hi - 1);
        let poison = &batches[hi - 1];
        eprintln!("arming batch: {} entries", poison.len());
        for e in poison.iter().take(40) {
            eprintln!(
                "  op {:02x} words {:x?}",
                e.opcode,
                &e.fifo[..e.fifo.len().min(8)]
            );
        }
    }

    /// A quad with one vertex ABOVE the screen (negative Y after the
    /// draw offset) must still rasterize its on-screen part -- the
    /// page-0 case (offset 0) where room walls cross the top edge.
    #[test]
    fn negative_y_vertex_quad_rasterizes_on_page0() {
        let Some(mut renderer) = headless_renderer() else {
            eprintln!("skipping: no headless wgpu adapter");
            return;
        };
        let vram_words = vec![0; (VRAM_WIDTH * VRAM_HEIGHT) as usize];
        let gpu = Gpu::new();
        for (e5, base_y, probe_y) in [
            (0xE500_0000u32, 0u32, 5u32),
            (0xE500_0000 | (240 << 11), 240, 245),
        ] {
            let neg40 = 0x7FFu32 & (-40i32 as u32 & 0x7FF);
            let log = [
                GpuCmdLogEntry {
                    index: 0,
                    opcode: 0xE3,
                    fifo: vec![0xE300_0000 | (base_y << 10)].into(),
                },
                GpuCmdLogEntry {
                    index: 1,
                    opcode: 0xE4,
                    fifo: vec![0xE400_0000 | ((base_y + 239) << 10) | 319].into(),
                },
                GpuCmdLogEntry {
                    index: 2,
                    opcode: 0xE5,
                    fifo: vec![e5].into(),
                },
                // Quad spanning y=-40..60, x=0..100: top edge off-screen.
                GpuCmdLogEntry {
                    index: 3,
                    opcode: 0x28,
                    fifo: vec![
                        0x2800_3050,
                        (neg40 << 16),
                        (neg40 << 16) | 100,
                        (60 << 16),
                        (60 << 16) | 100,
                    ]
                    .into(),
                },
            ];
            renderer.render_frame(&gpu, &log, &vram_words);
            let (_, _, rgba) = renderer.read_subrect_rgba8(50, base_y + probe_y - base_y + 5, 1, 1);
            assert_eq!(
                &rgba[..3],
                &[0x50, 0x30, 0x00],
                "negative-Y quad missing at offset {base_y}"
            );
        }
    }

    /// Standalone replay of the recorded page0 cycle WITH real VRAM
    /// textures: env+fill batch then draw batch, then dump page0 and
    /// the wall probe pixel. PSX_BATCH_TAPE, PSX_VRAM_BIN, PSX_PROBE_ENV,
    /// PSX_PROBE_DRAW.
    #[test]
    #[ignore]
    fn standalone_page0_cycle_with_real_vram() {
        let Ok(tape_path) = std::env::var("PSX_BATCH_TAPE") else {
            return;
        };
        let vram_path = std::env::var("PSX_VRAM_BIN").unwrap();
        let env_idx: usize = std::env::var("PSX_PROBE_ENV").unwrap().parse().unwrap();
        let draw_idx: usize = std::env::var("PSX_PROBE_DRAW").unwrap().parse().unwrap();
        let batches = load_batch_tape(&tape_path);
        let raw = std::fs::read(&vram_path).unwrap();
        let vram_words: Vec<u16> = raw
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let Some(mut renderer) = headless_renderer() else {
            panic!("no adapter");
        };
        let gpu = Gpu::new();
        renderer.render_frame(&gpu, &batches[env_idx], &vram_words);
        renderer.render_frame(&gpu, &batches[draw_idx], &vram_words);
        let (w, h, rgba) = renderer.read_subrect_rgba8(0, 0, 320, 240);
        let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
        for px in rgba.chunks_exact(4) {
            ppm.extend_from_slice(&px[..3]);
        }
        std::fs::write("/tmp/standalone-page0.ppm", ppm).unwrap();
        let wall = |x: u32, y: u32| {
            let i = ((y * w + x) * 4) as usize;
            (rgba[i], rgba[i + 1], rgba[i + 2])
        };
        eprintln!(
            "wall(290,80)={:?} floor(160,215)={:?} crate(100,80)={:?}",
            wall(290, 80),
            wall(160, 215),
            wall(100, 80)
        );
        eprintln!("wrote /tmp/standalone-page0.ppm");
    }

    /// Bisect the first tape prefix after which the page0 wall pixel
    /// stops rendering. Wall-pixel predicate with real VRAM textures.
    #[test]
    #[ignore]
    fn bisect_wall_killer() {
        let Ok(tape_path) = std::env::var("PSX_BATCH_TAPE") else {
            return;
        };
        let vram_path = std::env::var("PSX_VRAM_BIN").unwrap();
        let env_idx: usize = std::env::var("PSX_PROBE_ENV").unwrap().parse().unwrap();
        let draw_idx: usize = std::env::var("PSX_PROBE_DRAW").unwrap().parse().unwrap();
        let batches = load_batch_tape(&tape_path);
        let raw = std::fs::read(&vram_path).unwrap();
        let vram_words: Vec<u16> = raw
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let gpu = Gpu::new();
        let wall_dead = |prefix: usize| -> bool {
            let Some(mut renderer) = headless_renderer() else {
                panic!("no adapter");
            };
            for batch in &batches[..prefix] {
                renderer.render_frame(&gpu, batch, &vram_words);
            }
            renderer.render_frame(&gpu, &batches[env_idx], &vram_words);
            renderer.render_frame(&gpu, &batches[draw_idx], &vram_words);
            let (_, _, rgba) = renderer.read_subrect_rgba8(290, 80, 1, 1);
            rgba[0] as u32 + rgba[1] as u32 + rgba[2] as u32 <= 6
        };
        let total = env_idx;
        eprintln!("prefix 0: {}", if wall_dead(0) { "DEAD" } else { "alive" });
        eprintln!(
            "prefix {total}: {}",
            if wall_dead(total) { "DEAD" } else { "alive" }
        );
        if !wall_dead(total) {
            eprintln!("wall alive after full prefix -- cannot bisect");
            return;
        }
        let mut lo = 0usize;
        let mut hi = total;
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            let d = wall_dead(mid);
            eprintln!("prefix {mid}: {}", if d { "DEAD" } else { "alive" });
            if d {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        eprintln!("first killing prefix: {hi} (batch {})", hi - 1);
        let killer = &batches[hi - 1];
        eprintln!("killer batch: {} entries", killer.len());
        for e in killer.iter().take(50) {
            eprintln!(
                "  op {:02x} {:x?}",
                e.opcode,
                &e.fifo[..e.fifo.len().min(10)]
            );
        }
    }

    /// Control: env + quad in ONE render_frame call must land.
    #[test]
    fn same_batch_env_and_draw_lands() {
        let Some(mut renderer) = headless_renderer() else {
            eprintln!("skipping: no headless wgpu adapter");
            return;
        };
        assert!(renderer.set_internal_scale(2, None));
        let vram_words = vec![0; (VRAM_WIDTH * VRAM_HEIGHT) as usize];
        let gpu = Gpu::new();
        let log = [
            GpuCmdLogEntry {
                index: 0,
                opcode: 0xE3,
                fifo: vec![0xE300_0000].into(),
            },
            GpuCmdLogEntry {
                index: 1,
                opcode: 0xE4,
                fifo: vec![0xE400_0000 | (239 << 10) | 319].into(),
            },
            GpuCmdLogEntry {
                index: 2,
                opcode: 0xE5,
                fifo: vec![0xE500_0000].into(),
            },
            GpuCmdLogEntry {
                index: 3,
                opcode: 0x28,
                fifo: vec![
                    0x2800_3050,
                    (8 << 16) | 8,
                    (8 << 16) | 24,
                    (24 << 16) | 8,
                    (24 << 16) | 24,
                ]
                .into(),
            },
        ];
        renderer.render_frame(&gpu, &log, &vram_words);
        assert_eq!(
            pixel_block(&renderer, 10, 10),
            vec![[0x50, 0x30, 0x00, 0xFF]; 4],
            "same-batch draw missing"
        );
    }

    /// A draw-only batch with NO env in it at all (fresh default state).
    #[test]
    fn draw_only_batch_lands_with_default_env() {
        let Some(mut renderer) = headless_renderer() else {
            eprintln!("skipping: no headless wgpu adapter");
            return;
        };
        assert!(renderer.set_internal_scale(2, None));
        let vram_words = vec![0; (VRAM_WIDTH * VRAM_HEIGHT) as usize];
        let gpu = Gpu::new();
        let quad = [GpuCmdLogEntry {
            index: 0,
            opcode: 0x28,
            fifo: vec![
                0x2800_3050,
                (8 << 16) | 8,
                (8 << 16) | 24,
                (24 << 16) | 8,
                (24 << 16) | 24,
            ]
            .into(),
        }];
        renderer.render_frame(&gpu, &quad, &vram_words);
        assert_eq!(
            pixel_block(&renderer, 10, 10),
            vec![[0x50, 0x30, 0x00, 0xFF]; 4],
            "draw-only batch missing"
        );
    }

    /// Split-batch incremental replay: the draw env (E3/E4/E5) arrives
    /// in one render_frame call, the primitives in a LATER call, exactly
    /// like the GUI's per-host-frame drains. Draws must land for both
    /// framebuffer pages.
    #[test]
    fn split_batch_env_then_draw_lands_on_both_pages() {
        let Some(mut renderer) = headless_renderer() else {
            eprintln!("skipping HW split-batch test: no headless wgpu adapter");
            return;
        };
        assert!(renderer.set_internal_scale(2, None));
        let vram_words = vec![0; (VRAM_WIDTH * VRAM_HEIGHT) as usize];
        let gpu = Gpu::new();
        for page_y in [0u32, 240u32] {
            // Batch 1: draw area + draw offset for this page, alone.
            let env = [
                GpuCmdLogEntry {
                    index: 0,
                    opcode: 0xE3,
                    fifo: vec![0xE300_0000 | (page_y << 10)].into(),
                },
                GpuCmdLogEntry {
                    index: 1,
                    opcode: 0xE4,
                    fifo: vec![0xE400_0000 | ((page_y + 239) << 10) | 319].into(),
                },
                GpuCmdLogEntry {
                    index: 2,
                    opcode: 0xE5,
                    fifo: vec![0xE500_0000 | (page_y << 11)].into(),
                },
            ];
            renderer.render_frame(&gpu, &env, &vram_words);
            // Batch 2: one opaque mono quad at (8,8)-(24,24), page-relative.
            let quad = [GpuCmdLogEntry {
                index: 0,
                opcode: 0x28,
                fifo: vec![
                    0x2800_3050,
                    (8 << 16) | 8,
                    (8 << 16) | 24,
                    (24 << 16) | 8,
                    (24 << 16) | 24,
                ]
                .into(),
            }];
            renderer.render_frame(&gpu, &quad, &vram_words);
            let px = pixel_block(&renderer, 10, page_y + 10);
            assert_eq!(
                px,
                vec![[0x50, 0x30, 0x00, 0xFF]; 4],
                "split-batch draw missing on page y={page_y}"
            );
        }
    }

    #[test]
    fn render_frame_fill_rect_writes_target() {
        let Some(mut renderer) = headless_renderer() else {
            eprintln!("skipping HW fill test: no headless wgpu adapter");
            return;
        };
        assert!(renderer.set_internal_scale(2, None));
        let log = [GpuCmdLogEntry {
            index: 0,
            opcode: 0x02,
            fifo: vec![0x0210_2030, 0, (8 << 16) | 8].into(),
        }];
        let vram_words = vec![0; (VRAM_WIDTH * VRAM_HEIGHT) as usize];
        renderer.render_frame(&Gpu::new(), &log, &vram_words);

        assert_eq!(
            pixel_block(&renderer, 4, 4),
            vec![[0x30, 0x20, 0x10, 0xFF]; 4]
        );
    }

    // ---- GP0 line parity vs the CPU rasterizer ----
    //
    // These run the FULL pipeline the frontend uses: raw GP0 words
    // into `emulator_core::Gpu` (which rasterizes them AND captures
    // the cmd_log, including polyline continuation words), then the
    // drained log through `HwRenderer::render_frame`, then pixel-set
    // comparison of both outputs. The CPU rasterizer is the
    // semantics oracle; tolerances are stated per test.

    use std::collections::BTreeSet;

    /// Draw env used by the line tests: draw area (0,0)-(319,239),
    /// zero draw offset.
    fn line_env() -> Vec<u32> {
        vec![0xE300_0000, 0xE400_0000 | (239 << 10) | 319, 0xE500_0000]
    }

    fn xyw(x: u16, y: u16) -> u32 {
        u32::from(x) | (u32::from(y) << 16)
    }

    /// Feed raw GP0 words to a fresh CPU Gpu with the cmd_log armed,
    /// optionally pre-filling VRAM (both rasterizers must start from
    /// the same background for blend tests). Returns the rasterizing
    /// Gpu, the drained log and the pre-draw VRAM snapshot to hand to
    /// `render_frame`.
    fn cpu_run_gp0(words: &[u32], prefill: u16) -> (Gpu, Vec<GpuCmdLogEntry>, Vec<u16>) {
        let mut gpu = Gpu::new();
        if prefill != 0 {
            for y in 0..VRAM_HEIGHT as u16 {
                for x in 0..VRAM_WIDTH as u16 {
                    gpu.vram.set_pixel(x, y, prefill);
                }
            }
        }
        let vram_before: Vec<u16> = gpu.vram.words().to_vec();
        gpu.enable_cmd_log();
        for &w in words {
            gpu.gp0_push(w);
        }
        let log = gpu.drain_completed_cmd_log();
        (gpu, log, vram_before)
    }

    fn cpu_changed_pixels(gpu: &Gpu, background: u16) -> BTreeSet<(u16, u16)> {
        let mut set = BTreeSet::new();
        for y in 0..240u16 {
            for x in 0..320u16 {
                if gpu.vram.get_pixel(x, y) != background {
                    set.insert((x, y));
                }
            }
        }
        set
    }

    fn hw_changed_pixels(renderer: &HwRenderer, background: [u8; 4]) -> BTreeSet<(u16, u16)> {
        let (w, h, rgba) = renderer.read_subrect_rgba8(0, 0, 320, 240);
        let mut set = BTreeSet::new();
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                if [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]] != background {
                    set.insert((x as u16, y as u16));
                }
            }
        }
        set
    }

    /// Run one GP0 stream through both rasterizers at internal scale
    /// 1 and return `(cpu gpu, cpu lit, hw lit, renderer)` over the
    /// 320x240 draw area. `None` = no wgpu adapter (test self-skips).
    #[allow(clippy::type_complexity)]
    fn line_case(
        words: &[u32],
        prefill: u16,
    ) -> Option<(Gpu, BTreeSet<(u16, u16)>, BTreeSet<(u16, u16)>, HwRenderer)> {
        let mut renderer = headless_renderer()?;
        let (gpu, log, vram_before) = cpu_run_gp0(words, prefill);
        if prefill != 0 {
            // Seed the persistent target with the background so the
            // HW blend sees the same destination the CPU had.
            renderer.sync_target_from_vram(&vram_before);
        }
        renderer.render_frame(&gpu, &log, &vram_before);
        let cpu = cpu_changed_pixels(&gpu, prefill);
        let hw = hw_changed_pixels(&renderer, bgr15_to_rgba8(prefill));
        Some((gpu, cpu, hw, renderer))
    }

    /// Horizontal / vertical / 45-degree flat lines light the exact
    /// pixel set the CPU Bresenham walker lights (both directions),
    /// endpoint-inclusive, at the exact flat colour.
    #[test]
    fn gp0_flat_lines_match_cpu_pixel_sets_exactly() {
        let cases: [(&str, u32, u32); 4] = [
            ("horizontal", xyw(10, 10), xyw(60, 10)),
            ("reversed horizontal", xyw(60, 30), xyw(10, 30)),
            ("vertical", xyw(20, 20), xyw(20, 80)),
            ("45 degree diagonal", xyw(10, 100), xyw(40, 130)),
        ];
        for (name, v0, v1) in cases {
            let mut words = line_env();
            words.extend([0x40FF_FFFF, v0, v1]);
            let Some((_gpu, cpu, hw, renderer)) = line_case(&words, 0) else {
                eprintln!("skipping: no headless wgpu adapter");
                return;
            };
            assert!(!cpu.is_empty(), "{name}: CPU drew nothing");
            assert_eq!(cpu, hw, "{name}: pixel sets diverge");
            let &(px, py) = cpu.iter().next().unwrap();
            assert_eq!(
                pixel_block(&renderer, px as u32, py as u32),
                vec![[0xFF, 0xFF, 0xFF, 0xFF]],
                "{name}: flat white must land exactly"
            );
        }
    }

    /// Arbitrary-slope lines: the HW band samples pixel centers
    /// while the CPU walks integer steps, so individual columns may
    /// round to a neighbouring row. Contract: exactly one pixel per
    /// column (x-major), each within one row of the CPU's choice,
    /// endpoints exact.
    #[test]
    fn gp0_arbitrary_slope_line_stays_within_one_pixel_of_cpu() {
        let mut words = line_env();
        words.extend([0x40FF_FFFF, xyw(10, 20), xyw(50, 35)]);
        let Some((_gpu, cpu, hw, _renderer)) = line_case(&words, 0) else {
            eprintln!("skipping: no headless wgpu adapter");
            return;
        };
        for x in 10..=50u16 {
            let cpu_rows: Vec<u16> = cpu.iter().filter(|p| p.0 == x).map(|p| p.1).collect();
            let hw_rows: Vec<u16> = hw.iter().filter(|p| p.0 == x).map(|p| p.1).collect();
            assert_eq!(cpu_rows.len(), 1, "CPU column {x}");
            assert_eq!(hw_rows.len(), 1, "HW column {x}");
            let diff = (i32::from(cpu_rows[0]) - i32::from(hw_rows[0])).abs();
            assert!(
                diff <= 1,
                "column {x}: CPU row {} vs HW row {}",
                cpu_rows[0],
                hw_rows[0]
            );
        }
        assert!(hw.contains(&(10, 20)), "start endpoint missing");
        assert!(hw.contains(&(50, 35)), "end endpoint missing");
        assert!(
            hw.iter().all(|p| (10..=50).contains(&p.0)),
            "column overrun"
        );
    }

    /// Gouraud line: coverage matches the CPU exactly on an
    /// axis-aligned segment; colours interpolate within a small
    /// tolerance of the CPU's per-step integer walk (host f32
    /// interpolation + the CPU's 5-bit VRAM quantization).
    #[test]
    fn gp0_gouraud_line_matches_cpu_coverage_and_colors() {
        let mut words = line_env();
        words.extend([0x5000_00FF, xyw(10, 10), 0x0000_FF00, xyw(60, 10)]);
        let Some((gpu, cpu, hw, renderer)) = line_case(&words, 0) else {
            eprintln!("skipping: no headless wgpu adapter");
            return;
        };
        assert_eq!(cpu, hw, "gouraud coverage diverges");

        for x in [10u16, 35, 60] {
            let cpu_px = bgr15_to_rgba8(gpu.vram.get_pixel(x, 10));
            let hw_px = pixel_block(&renderer, x as u32, 10)[0];
            for c in 0..3 {
                let diff = (i32::from(cpu_px[c]) - i32::from(hw_px[c])).abs();
                assert!(
                    diff <= 8,
                    "x={x} channel {c}: CPU {:?} vs HW {:?}",
                    cpu_px,
                    hw_px
                );
            }
        }
    }

    /// Mono polyline: all segments must draw, which exercises the
    /// cmd_log continuation-word capture end-to-end (without it the
    /// HW renderer only sees the first segment). Axis-aligned
    /// segments so the pixel sets match exactly.
    #[test]
    fn gp0_mono_polyline_draws_every_segment() {
        let mut words = line_env();
        words.extend([
            0x48FF_FFFF, // polyline start
            xyw(20, 20),
            xyw(80, 20),
            xyw(80, 60), // continuation vertex
            xyw(20, 60), // continuation vertex
            0x5555_5555, // terminator
        ]);
        let Some((_gpu, cpu, hw, _renderer)) = line_case(&words, 0) else {
            eprintln!("skipping: no headless wgpu adapter");
            return;
        };
        // Sanity: the CPU drew all three segments.
        assert!(cpu.contains(&(50, 20)) && cpu.contains(&(80, 40)) && cpu.contains(&(50, 60)));
        assert_eq!(cpu, hw, "polyline pixel sets diverge");
    }

    /// Shaded polyline continuation pair (colour + vertex words)
    /// draws with matching coverage.
    #[test]
    fn gp0_shaded_polyline_draws_every_segment() {
        let mut words = line_env();
        words.extend([
            0x5800_00FF, // polyline start, c0 red
            xyw(20, 100),
            0x0000_FF00, // c1 green
            xyw(80, 100),
            0x00FF_0000,  // continuation colour (blue)
            xyw(80, 140), // continuation vertex
            0x5000_5000,  // terminator
        ]);
        let Some((_gpu, cpu, hw, _renderer)) = line_case(&words, 0) else {
            eprintln!("skipping: no headless wgpu adapter");
            return;
        };
        assert!(cpu.contains(&(50, 100)) && cpu.contains(&(80, 120)));
        assert_eq!(cpu, hw, "shaded polyline pixel sets diverge");
    }

    /// Semi-transparent line: coverage (which pixels changed against
    /// the prefilled background) matches the CPU exactly; every
    /// covered pixel is actually blended, not replaced. Blend VALUES
    /// are compared in `semi_trans_modes_match_cpu_blend_values`.
    #[test]
    fn gp0_semi_trans_line_blends_with_background_coverage_parity() {
        let prefill = 0x4210; // mid grey (r=g=b=16 in 5-bit)
        let mut words = line_env();
        words.push(0xE100_0000); // blend mode 0 (Average)
        words.extend([0x42FF_FFFF, xyw(10, 50), xyw(60, 50)]);
        let Some((_gpu, cpu, hw, renderer)) = line_case(&words, prefill) else {
            eprintln!("skipping: no headless wgpu adapter");
            return;
        };
        assert!(!cpu.is_empty());
        assert_eq!(cpu, hw, "semi-trans coverage diverges");
        let blended = pixel_block(&renderer, 30, 50)[0];
        assert_ne!(blended, bgr15_to_rgba8(prefill), "pixel not touched");
        assert_ne!(
            blended,
            [0xFF, 0xFF, 0xFF, 0xFF],
            "pixel replaced instead of blended"
        );
    }

    /// All four semi-transparency modes combine the destination and the
    /// source on the gamma-coded 5-bit values, so the HW result must land
    /// within one 5-bit step of the CPU rasterizer. Blending through the
    /// sRGB view instead (decode, add in linear light, re-encode) left an
    /// additive +24/255 rectangle at about +3/255.
    #[test]
    fn semi_trans_modes_match_cpu_blend_values() {
        let prefill = 0x4210; // r=g=b=16 in 5-bit, 132 in display codes
                              // (E1 blend-mode bits, source grey, name)
        let cases = [
            (0x0000u32, 0xF8u32, "average (B+F)/2"),
            (0x0020, 0x18, "add B+F"),
            (0x0040, 0x40, "subtract B-F"),
            (0x0060, 0x80, "add quarter B+F/4"),
        ];
        for (mode, grey, name) in cases {
            let mut words = line_env();
            words.push(0xE100_0000 | mode);
            let src = grey * 0x0001_0101;
            words.extend([0x6200_0000 | src, xyw(40, 40), (8 << 16) | 8]);
            let Some((gpu, _, _, renderer)) = line_case(&words, prefill) else {
                eprintln!("skipping: no headless wgpu adapter");
                return;
            };
            let want = bgr15_to_rgba8(gpu.vram.words()[(44 * VRAM_WIDTH + 44) as usize]);
            let got = pixel_block(&renderer, 44, 44)[0];
            assert_ne!(
                want,
                bgr15_to_rgba8(prefill),
                "{name}: CPU blend was a no-op"
            );
            for c in 0..3 {
                assert!(
                    want[c].abs_diff(got[c]) <= 8,
                    "{name}: channel {c} cpu {want:?} hw {got:?}"
                );
            }
        }
    }

    /// Textured polygons, slivers included, light the same pixels with the
    /// same texels as the CPU rasterizer. The CPU decides coverage by testing
    /// each pixel's top-left corner against the triangle and reads the texel
    /// at that corner; the host samples pixel centres, so the polygon is
    /// drawn half a pixel down-right to land on the same points. Without
    /// that, a thin triangle on the Quake shotgun lit pixels the CPU skips and
    /// sampled the texture's border column there, a row of blue dots.
    ///
    /// Raw-texture triangles (no tint, no dither) so the comparison is exact.
    #[test]
    fn textured_polygons_match_cpu_coverage_and_texels() {
        let Some(mut renderer) = headless_renderer() else {
            eprintln!("skipping: no headless wgpu adapter");
            return;
        };
        assert_eq!(renderer.internal_scale(), 1, "compares PSX-native pixels");
        let mut state = 0x2545_F491u32;
        let mut next = move |n: u32| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 8) % n
        };
        // 64x64 texture, every texel opaque and different from its neighbours.
        let mut seed = Vec::new();
        for y in 0..64u16 {
            for x in 0..64u16 {
                let c = 1 + ((x * 7 + y * 13 + (x ^ y) * 3) % 0x7FFE);
                seed.push((512 + x, y, c));
            }
        }
        let mut words = line_env();
        words.push(0xE100_0000 | 8 | (2 << 7)); // tpage x 512, 15bpp
        for _ in 0..300 {
            let v: Vec<(i32, i32)> = (0..3)
                .map(|_| (8 + next(48) as i32, 8 + next(48) as i32))
                .collect();
            let uv: Vec<(u32, u32)> = (0..3).map(|_| (next(64), next(64))).collect();
            words.extend([
                0x2500_0000,
                pack_xy(v[0]),
                uv[0].0 | (uv[0].1 << 8),
                pack_xy(v[1]),
                (uv[1].0 | (uv[1].1 << 8)) | (((8 | (2 << 7)) as u32) << 16),
                pack_xy(v[2]),
                uv[2].0 | (uv[2].1 << 8),
            ]);
        }
        let cpu = run_both_backends(&words, &mut renderer, &seed);
        let (_, _, rgba) = renderer.read_subrect_rgba8(0, 0, 64, 64);
        let black = [0u8, 0, 0, 255];
        let mut bad_coverage = Vec::new();
        let mut bad_texel = Vec::new();
        for y in 0..64usize {
            for x in 0..64usize {
                let want = bgr15_to_rgba8(cpu[y * VRAM_WIDTH as usize + x]);
                let got = &rgba[(y * 64 + x) * 4..][..4];
                let got = [got[0], got[1], got[2], got[3]];
                if (want == black) != (got == black) {
                    bad_coverage.push((x, y, want, got));
                } else if want != got {
                    bad_texel.push((x, y, want, got));
                }
            }
        }
        assert!(
            bad_coverage.is_empty(),
            "{} pixels covered by only one rasterizer, first: {:?}",
            bad_coverage.len(),
            &bad_coverage[..bad_coverage.len().min(4)]
        );
        // The CPU truncates each gradient to 1/4096 of a texel per pixel and
        // the host interpolates in f32, so a pixel whose corner lands within
        // that error of a texel boundary can pick the neighbour. A fraction of
        // a percent of pixels, never more than one texel off.
        assert!(
            bad_texel.len() <= 41,
            "{} of 4096 pixels sample another texel than the CPU, first: {:?}",
            bad_texel.len(),
            &bad_texel[..bad_texel.len().min(4)]
        );
    }

    /// Both mono and shaded zero-length segments plot one pixel,
    /// matching the CPU's silicon-coordinate DDA.
    #[test]
    fn gp0_zero_length_lines_match_cpu_walkers() {
        let mut words = line_env();
        words.extend([0x40FF_FFFF, xyw(15, 15), xyw(15, 15)]);
        let Some((_gpu, cpu, hw, _renderer)) = line_case(&words, 0) else {
            eprintln!("skipping: no headless wgpu adapter");
            return;
        };
        let expected: BTreeSet<(u16, u16)> = [(15u16, 15u16)].into_iter().collect();
        assert_eq!(cpu, expected, "CPU mono zero-length plots one pixel");
        assert_eq!(hw, expected, "HW mono zero-length plots one pixel");

        let mut words = line_env();
        words.extend([0x5000_00FF, xyw(15, 15), 0x0000_FF00, xyw(15, 15)]);
        let Some((_gpu, cpu, hw, _renderer)) = line_case(&words, 0) else {
            return;
        };
        assert_eq!(cpu, expected, "CPU shaded zero-length plots one pixel");
        assert_eq!(hw, expected, "HW shaded zero-length plots one pixel");
    }

    /// At internal scale 2 a line's band covers the full S x S block
    /// of every PSX pixel it lights -- lines upscale like every
    /// other primitive, endpoint block included.
    #[test]
    fn gp0_line_scales_with_internal_resolution() {
        let Some(mut renderer) = headless_renderer() else {
            eprintln!("skipping: no headless wgpu adapter");
            return;
        };
        assert!(renderer.set_internal_scale(2, None));
        let mut words = line_env();
        words.extend([0x40FF_FFFF, xyw(10, 10), xyw(20, 10)]);
        let (gpu, log, vram_before) = cpu_run_gp0(&words, 0);
        renderer.render_frame(&gpu, &log, &vram_before);

        // HW flat colours keep full 8-bit precision (the 5-bit
        // clamp is a CPU-side property).
        let white = [0xFF, 0xFF, 0xFF, 0xFF];
        let black = [0x00, 0x00, 0x00, 0xFF];
        for x in [10u32, 15, 20] {
            assert_eq!(
                pixel_block(&renderer, x, 10),
                vec![white; 4],
                "PSX pixel ({x}, 10) must be fully lit at scale 2"
            );
        }
        assert_eq!(pixel_block(&renderer, 21, 10), vec![black; 4]);
        assert_eq!(pixel_block(&renderer, 15, 9), vec![black; 4]);
        assert_eq!(pixel_block(&renderer, 15, 11), vec![black; 4]);
    }

    /// Visual showcase, not an assertion test: renders a line scene
    /// (star burst, gouraud polyline, mono polyline outline,
    /// semi-trans lines over a filled patch) through BOTH
    /// rasterizers and writes a side-by-side PPM (CPU left, HW
    /// right) for eyeballing. Run manually:
    /// `cargo test -p psx-gpu-render --lib dump_gp0_line_showcase -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn dump_gp0_line_showcase() {
        let mut words = line_env();
        // Dark blue backdrop + mid-grey patch for the blend lines.
        words.extend([0x0230_1810, 0, (240 << 16) | 320]);
        words.extend([
            0x2860_6060,
            xyw(200, 90),
            xyw(310, 90),
            xyw(200, 190),
            xyw(310, 190),
        ]);
        // White star burst (mono singles at assorted slopes).
        let (cx, cy) = (80i32, 150i32);
        for (dx, dy) in [
            (70, 0),
            (-70, 0),
            (0, 60),
            (0, -60),
            (60, 30),
            (-60, 30),
            (60, -30),
            (-60, -30),
            (30, 55),
            (-30, 55),
            (30, -55),
            (-30, -55),
        ] {
            words.extend([
                0x40FF_FFFF,
                xyw(cx as u16, cy as u16),
                xyw((cx + dx) as u16, (cy + dy) as u16),
            ]);
        }
        // Gouraud polyline zigzag across the top.
        words.extend([
            0x5800_00FF, // red
            xyw(10, 20),
            0x0000_FF00, // green
            xyw(60, 50),
            0x00FF_0000, // blue
            xyw(110, 20),
            0x0000_FFFF, // yellow
            xyw(160, 50),
            0x00FF_00FF, // magenta
            xyw(210, 20),
            0x5555_5555,
        ]);
        // Mono polyline outline (closed rectangle).
        words.extend([
            0x4800_FFFF, // yellow
            xyw(20, 200),
            xyw(170, 200),
            xyw(170, 230),
            xyw(20, 230),
            xyw(20, 200),
            0x5555_5555,
        ]);
        // Semi-trans (Average) white lines crossing the grey patch.
        words.push(0xE100_0000);
        words.extend([0x42FF_FFFF, xyw(190, 80), xyw(310, 200)]);
        words.extend([0x42FF_FFFF, xyw(310, 80), xyw(190, 200)]);
        words.extend([0x42FF_FFFF, xyw(190, 140), xyw(310, 140)]);

        let Some((gpu, _cpu, _hw, renderer)) = line_case(&words, 0) else {
            eprintln!("skipping: no headless wgpu adapter");
            return;
        };
        let (w, h) = (320u32, 240u32);
        let (_, _, hw_rgba) = renderer.read_subrect_rgba8(0, 0, w, h);
        let mut ppm = format!("P6\n{} {h}\n255\n", w * 2).into_bytes();
        for y in 0..h {
            for x in 0..w {
                let px = bgr15_to_rgba8(gpu.vram.get_pixel(x as u16, y as u16));
                ppm.extend_from_slice(&px[..3]);
            }
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                ppm.extend_from_slice(&hw_rgba[i..i + 3]);
            }
        }
        std::fs::write("/tmp/gp0-lines-cpu-vs-hw.ppm", ppm).unwrap();
        eprintln!("wrote /tmp/gp0-lines-cpu-vs-hw.ppm (left CPU, right HW)");
    }

    // ---- Edge texture filter (docs/texture-filters-spec.md) ----

    const FILTER_NONE: u32 = 0;
    const FILTER_EDGE: u32 = 1;

    /// Texture page x unit 8 (pixel x 512), y 0, 15bpp direct.
    const TPAGE_15BPP: u32 = 8 | (2 << 7);

    /// Render a `tw x th` 15bpp texture magnified `mag` times with a raw
    /// textured quad (GP0 0x2D) at `(8, 8)` under filter `mode`, and return
    /// the `(tw * mag) x (th * mag)` RGBA block. `pre` words (for example a
    /// texture window) go in front of the quad. Pixels outside the quad's
    /// opaque texels stay black.
    ///
    /// The quad's UVs run `0..tw` and `0..th`. A PS1 polygon samples a pixel's
    /// top-left corner and rounds to the nearest texel, so its last pixels
    /// read one texel past the UV extent; the texture gets an edge-replicated
    /// extra row and column, the padding PS1 art carries for the same reason
    /// (left transparent when `pre` is non-empty, so a texture-window test
    /// sees only what it put there).
    fn render_texture(
        renderer: &mut HwRenderer,
        texels: &[u16],
        tw: u32,
        th: u32,
        mag: u32,
        mode: u32,
        pre: &[u32],
    ) -> Vec<[u8; 4]> {
        assert_eq!(texels.len() as u32, tw * th);
        let mut seed: Vec<(u16, u16, u16)> = Vec::new();
        for y in 0..=th {
            for x in 0..=tw {
                let t = if pre.is_empty() || (x < tw && y < th) {
                    texels[(y.min(th - 1) * tw + x.min(tw - 1)) as usize]
                } else {
                    0
                };
                seed.push((512 + x as u16, y as u16, t));
            }
        }
        let mut words = line_env();
        words.extend_from_slice(pre);
        let (x0, y0) = (8i32, 8i32);
        let (x1, y1) = (x0 + (tw * mag) as i32, y0 + (th * mag) as i32);
        words.extend_from_slice(&[
            0x2D00_0000,
            pack_xy((x0, y0)),
            0,
            pack_xy((x1, y0)),
            tw | (TPAGE_15BPP << 16),
            pack_xy((x0, y1)),
            th << 8,
            pack_xy((x1, y1)),
            tw | (th << 8),
        ]);
        renderer.set_texture_filter(mode);
        run_both_backends(&words, renderer, &seed);
        let (_, _, rgba) = renderer.read_subrect_rgba8(8, 8, tw * mag, th * mag);
        rgba.chunks_exact(4)
            .map(|p| [p[0], p[1], p[2], p[3]])
            .collect()
    }

    fn filter_renderer() -> Option<HwRenderer> {
        let r = headless_renderer();
        if r.is_none() {
            eprintln!("skipping HW texture-filter test: no headless wgpu adapter");
        }
        r
    }

    #[test]
    fn edge_leaves_a_flat_texture_alone() {
        let Some(mut r) = filter_renderer() else {
            return;
        };
        let texel = 0x3DEF;
        let want = bgr15_to_rgba8(texel);
        {
            let mode = FILTER_EDGE;
            let out = render_texture(&mut r, &[texel; 64], 8, 8, 4, mode, &[]);
            assert!(out.iter().all(|&p| p == want), "mode {mode}");
        }
    }

    #[test]
    fn transparent_texels_never_bleed_colour_into_the_silhouette() {
        let Some(mut r) = filter_renderer() else {
            return;
        };
        let red = 0x001F;
        let texels: Vec<u16> = (0..64).map(|i| if i % 8 < 4 { 0 } else { red }).collect();
        // The nearest-texel render decides the silhouette; the filter must not
        // move it or tint the transparent side.
        let reds_of = |out: &[[u8; 4]]| out.iter().filter(|p| p[0] == 255).count();
        let nearest = render_texture(&mut r, &texels, 8, 8, 4, FILTER_NONE, &[]);
        let want = reds_of(&nearest);
        assert!(want > 0, "nearest render drew no opaque texels");
        {
            let mode = FILTER_EDGE;
            let out = render_texture(&mut r, &texels, 8, 8, 4, mode, &[]);
            for p in &out {
                assert!(
                    *p == [0, 0, 0, 255] || *p == [255, 0, 0, 255],
                    "mode {mode}: {p:?}"
                );
            }
            assert_eq!(reds_of(&out), want, "silhouette area changed, mode {mode}");
        }
    }

    #[test]
    fn edge_keeps_an_orthogonal_step_orthogonal_and_narrow() {
        let Some(mut r) = filter_renderer() else {
            return;
        };
        let (a, b) = (0x0C63, 0x7E10);
        let texels: Vec<u16> = (0..16 * 8)
            .map(|i| if i % 16 < 8 { a } else { b })
            .collect();
        let out = render_texture(&mut r, &texels, 16, 8, 4, FILTER_EDGE, &[]);
        let (wa, wb) = (bgr15_to_rgba8(a), bgr15_to_rgba8(b));
        let rows: Vec<&[[u8; 4]]> = out.chunks_exact(64).collect();
        for row in &rows {
            assert_eq!(*row, rows[0], "rows differ: the edge leaked a slope");
        }
        let blended = rows[0].iter().filter(|&&p| p != wa && p != wb).count();
        assert!(blended <= 2, "transition {blended} pixels wide");
    }

    /// Count the 45 degree staircase's per-diagonal colour spread. A straight
    /// diagonal edge has a constant colour along each `x + y = k` line away
    /// from the border; a staircase does not.
    fn diagonal_spread(out: &[[u8; 4]], size: usize, margin: usize) -> u32 {
        let mut total = 0;
        for k in (2 * margin)..(2 * (size - margin)) {
            let mut lo = 255u8;
            let mut hi = 0u8;
            for y in margin..size - margin {
                let Some(x) = k.checked_sub(y) else { continue };
                if x < margin || x >= size - margin {
                    continue;
                }
                let v = out[y * size + x][0];
                lo = lo.min(v);
                hi = hi.max(v);
            }
            total += u32::from(hi.saturating_sub(lo));
        }
        total
    }

    #[test]
    fn edge_straightens_a_diagonal_staircase() {
        let Some(mut r) = filter_renderer() else {
            return;
        };
        let (light, dark) = (0x7FFF, 0x0421);
        let n = 16u32;
        let texels: Vec<u16> = (0..n * n)
            .map(|i| if i % n + i / n < n { light } else { dark })
            .collect();
        let size = (n * 4) as usize;
        let near = render_texture(&mut r, &texels, n, n, 4, FILTER_NONE, &[]);
        let edge = render_texture(&mut r, &texels, n, n, 4, FILTER_EDGE, &[]);
        let (sn, se) = (
            diagonal_spread(&near, size, 8),
            diagonal_spread(&edge, size, 8),
        );
        eprintln!("diagonal spread: nearest {sn}, edge {se}");
        assert!(sn > 0 && se * 3 < sn, "nearest {sn}, edge {se}");
        // Flat areas far from the edge stay exactly as they were.
        assert_eq!(near[10 * size + 10], edge[10 * size + 10]);
        assert_eq!(
            near[(size - 10) * size + size - 10],
            edge[(size - 10) * size + size - 10]
        );
    }

    #[test]
    fn edge_does_not_erode_an_isolated_texel() {
        let Some(mut r) = filter_renderer() else {
            return;
        };
        let mut texels = vec![0x0421u16; 64];
        texels[3 * 8 + 3] = 0x7FFF;
        let bright = |out: &[[u8; 4]]| out.iter().filter(|p| p[0] >= 128).count();
        let near = bright(&render_texture(&mut r, &texels, 8, 8, 4, FILTER_NONE, &[]));
        let edge = bright(&render_texture(&mut r, &texels, 8, 8, 4, FILTER_EDGE, &[]));
        eprintln!("isolated texel area: nearest {near}, edge {edge}");
        assert_eq!(near, 16);
        assert!(edge * 10 >= near * 7, "texel shrank from {near} to {edge}");
    }

    #[test]
    fn filters_reproduce_a_clut_texture_at_texel_centres() {
        let Some(mut r) = filter_renderer() else {
            return;
        };
        // 16x16 4bpp texture, 1:1 on screen: every fragment samples a texel
        // centre, where the Edge filter must return the nearest texel (its
        // blend collapses to the corner it is at).
        let mut seed = Vec::new();
        for i in 0..16u16 {
            // Non-zero CLUT entries so no texel is transparent.
            seed.push((i, 480, 0x0421 + i * 0x0735));
        }
        let mut state = 0x1234_5678u32;
        let mut nibbles = [[0u16; 16]; 16];
        for row in nibbles.iter_mut() {
            for n in row.iter_mut() {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *n = ((state >> 24) & 0xF) as u16;
            }
        }
        for (y, row) in nibbles.iter().enumerate() {
            for wx in 0..4usize {
                let word = row[wx * 4]
                    | (row[wx * 4 + 1] << 4)
                    | (row[wx * 4 + 2] << 8)
                    | (row[wx * 4 + 3] << 12);
                seed.push((512 + wx as u16, y as u16, word));
            }
        }
        let tpage = 8u32; // 4bpp
        let clut = (480u32 << 6) << 16;
        let mut words = line_env();
        words.extend_from_slice(&[
            0x2D00_0000,
            pack_xy((8, 8)),
            clut,
            pack_xy((24, 8)),
            16 | (tpage << 16),
            pack_xy((8, 24)),
            16 << 8,
            pack_xy((24, 24)),
            16 | (16 << 8),
        ]);
        let mut shots = Vec::new();
        for mode in [FILTER_NONE, FILTER_EDGE] {
            r.set_texture_filter(mode);
            run_both_backends(&words, &mut r, &seed);
            let (_, _, rgba) = r.read_subrect_rgba8(8, 8, 16, 16);
            shots.push(rgba);
        }
        let distinct: std::collections::HashSet<_> = shots[0].chunks_exact(4).collect();
        assert!(distinct.len() >= 12, "texture did not reach the screen");
        {
            let (mode, shot) = (FILTER_EDGE, &shots[1]);
            let worst = shot
                .iter()
                .zip(&shots[0])
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap();
            assert!(worst <= 1, "mode {mode} differs from nearest by {worst}");
        }
    }

    #[test]
    fn filters_respect_the_texture_window() {
        let Some(mut r) = filter_renderer() else {
            return;
        };
        // 16x8 texture: columns 0..8 shades of red, columns 8..16 magenta.
        // A window that repeats the first 8 columns means no filter tap may
        // ever read the magenta half.
        let texels: Vec<u16> = (0..16 * 8)
            .map(|i| {
                let x = (i % 16) as u16;
                if x < 8 {
                    5 + x * 3
                } else {
                    0x7C1F
                }
            })
            .collect();
        {
            let mode = FILTER_EDGE;
            let out = render_texture(&mut r, &texels, 16, 8, 4, mode, &[0xE200_0001]);
            assert!(
                out.iter().all(|p| p[2] == 0),
                "mode {mode} read outside the texture window"
            );
            assert!(out.iter().any(|p| p[0] > 0));
        }
    }

    /// GPU cost of Edge against nearest: four stacked full-screen (320x240) 4bpp CLUT
    /// quads at internal scale 4, i.e. 4.9 million filtered fragments per
    /// frame. Run with `cargo test --release -p psx-gpu-render --lib
    /// texture_filter_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "benchmark"]
    fn texture_filter_cost() {
        let Some(mut r) = filter_renderer() else {
            return;
        };
        r.set_internal_scale(4, None);
        let mut cpu = Gpu::new();
        for i in 0..16u16 {
            cpu.vram.set_pixel(i, 480, 0x0421 + i * 0x0735);
        }
        let mut state = 0x2468_ace1u32;
        for y in 0..64u16 {
            for wx in 0..16u16 {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                cpu.vram.set_pixel(512 + wx, y, (state >> 16) as u16);
            }
        }
        let start_vram = cpu.vram.words().to_vec();
        cpu.enable_cmd_log();
        let mut words = line_env();
        for _ in 0..4 {
            words.extend_from_slice(&[
                0x2D00_0000,
                pack_xy((0, 0)),
                (480 << 6) << 16,
                pack_xy((320, 0)),
                64 | (8 << 16),
                pack_xy((0, 240)),
                64 << 8,
                pack_xy((320, 240)),
                64 | (64 << 8),
            ]);
        }
        for &w in &words {
            cpu.gp0_push(w);
        }
        for (name, mode) in [("none", 0), ("edge", 1)] {
            r.set_texture_filter(mode);
            for _ in 0..5 {
                r.render_frame(&cpu, &cpu.cmd_log, &start_vram);
                let _ = r.read_subrect_rgba8(0, 0, 1, 1);
            }
            let n = 40;
            let t = std::time::Instant::now();
            for _ in 0..n {
                r.render_frame(&cpu, &cpu.cmd_log, &start_vram);
                let _ = r.read_subrect_rgba8(0, 0, 1, 1);
            }
            eprintln!(
                "texture_filter_cost {name}: {:.3} ms/frame",
                t.elapsed().as_secs_f64() * 1000.0 / f64::from(n)
            );
        }
    }

    /// Writes the nearest and Edge views of a synthetic pixel-art texture (a disc,
    /// diagonal lines, a letter, a checker patch, a ramp and a cut-out hole)
    /// magnified 7x (the quad has to fit the 240 line draw area), as `<dir>/synthetic-<filter>.ppm`. Run with
    /// `PSOXIDE_FILTER_SHEET_DIR=<dir> cargo test --release -p psx-gpu-render
    /// --lib texture_filter_sheet -- --ignored`.
    #[test]
    #[ignore = "writes images"]
    fn texture_filter_sheet() {
        let Some(dir) = std::env::var_os("PSOXIDE_FILTER_SHEET_DIR") else {
            eprintln!("set PSOXIDE_FILTER_SHEET_DIR to write the sheets");
            return;
        };
        let Some(mut r) = filter_renderer() else {
            return;
        };
        let n = 32usize;
        let rgb = |r: u16, g: u16, b: u16| r | (g << 5) | (b << 10);
        let mut t = vec![rgb(4, 5, 10); n * n];
        let mut put = |x: usize, y: usize, c: u16| t[y * n + x] = c;
        for y in 0..n {
            for x in 0..n {
                let (dx, dy) = (x as i32 - 9, y as i32 - 9);
                if dx * dx + dy * dy <= 36 {
                    put(x, y, rgb(31, 28, 3));
                }
                // Cut-out hole (transparent) in the top right corner.
                let (hx, hy) = (x as i32 - 28, y as i32 - 3);
                if hx * hx + hy * hy <= 9 {
                    put(x, y, 0);
                }
            }
        }
        for i in 0..13 {
            put(17 + i, 2 + i, rgb(31, 31, 31)); // thin diagonal
            for k in 0..3 {
                put(12 + i + k, 17 + i / 2 + k, rgb(6, 26, 14)); // shallow band
            }
        }
        // A "Z": bars and a diagonal.
        for i in 0..11 {
            put(2 + i, 19, rgb(31, 12, 12));
            put(2 + i, 29, rgb(31, 12, 12));
            put(12 - i, 19 + i, rgb(31, 12, 12));
        }
        // Checker patch and a ramp.
        for y in 0..6 {
            for x in 0..6 {
                put(
                    22 + x,
                    22 + y,
                    if (x + y) % 2 == 0 {
                        rgb(31, 4, 4)
                    } else {
                        rgb(6, 6, 6)
                    },
                );
            }
        }
        for x in 0..n {
            put(
                x,
                31,
                rgb(
                    (x * 31 / (n - 1)) as u16,
                    6,
                    31 - (x * 31 / (n - 1)) as u16 + 1,
                ),
            );
        }
        for (name, mode) in [("none", 0), ("edge", 1)] {
            let out = render_texture(&mut r, &t, n as u32, n as u32, 7, mode, &[]);
            let mut ppm = format!("P6\n{} {}\n255\n", n * 7, n * 7).into_bytes();
            for p in out {
                ppm.extend_from_slice(&p[..3]);
            }
            std::fs::write(
                std::path::Path::new(&dir).join(format!("synthetic-{name}.ppm")),
                ppm,
            )
            .unwrap();
        }
    }
}
