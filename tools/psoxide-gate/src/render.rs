//! The hardware renderer driven the way the app drives it, headless.
//!
//! One `HwRenderer` per internal scale, fed every tick with that tick's GP0
//! command log and the CPU VRAM as it stood before the tick. That is what the
//! window does each frame, so the target's persistent contents (and every
//! upload and VRAM copy) match what a player sees.

use emulator_core::Bus;
use psx_gpu_render::{HwRenderer, VRAM_HEIGHT, VRAM_WIDTH};

use crate::img::Img;

pub struct HwSet {
    device: wgpu::Device,
    scales: Vec<(u32, HwRenderer)>,
    /// CPU VRAM before the tick about to run.
    pre_tick_vram: Vec<u16>,
    ticks_since_wait: u32,
    pub adapter: String,
}

/// Why a frame could not come from the hardware renderer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HwSkip {
    Bpp24,
    FieldRendering,
}

impl HwSkip {
    pub fn describe(&self) -> &'static str {
        match self {
            HwSkip::Bpp24 => "24bpp display (hardware renderer defers to the CPU frame)",
            HwSkip::FieldRendering => {
                "480i field rendering (hardware renderer defers to the CPU frame)"
            }
        }
    }
}

impl HwSet {
    pub fn new(scales: &[u32]) -> Result<HwSet, String> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .ok_or("no compatible wgpu adapter")?;
        let info = adapter.get_info();
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("psoxide-gate-device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))
        .map_err(|e| format!("request device: {e:?}"))?;
        let initial = vec![0u16; (VRAM_WIDTH * VRAM_HEIGHT) as usize];
        let mut renderers = Vec::new();
        for &s in scales {
            let mut hw = HwRenderer::new_headless(device.clone(), queue.clone());
            hw.set_internal_scale(s, None);
            hw.sync_target_from_vram(&initial);
            renderers.push((s, hw));
        }
        Ok(HwSet {
            device,
            scales: renderers,
            pre_tick_vram: initial,
            ticks_since_wait: 0,
            adapter: format!("{} ({:?})", info.name, info.backend),
        })
    }

    /// Call before running a tick.
    pub fn before_tick(&mut self, bus: &Bus) {
        self.pre_tick_vram.clear();
        self.pre_tick_vram.extend_from_slice(bus.gpu.vram.words());
    }

    /// Call after a tick: replay its command log on every renderer.
    pub fn after_tick(&mut self, bus: &mut Bus) {
        let log = bus.gpu.drain_completed_cmd_log();
        for (_, hw) in &mut self.scales {
            hw.render_frame(&bus.gpu, &log, &self.pre_tick_vram);
        }
        self.ticks_since_wait += 1;
        // Keep the GPU queue from running far ahead of the emulation.
        if self.ticks_since_wait >= 32 {
            self.ticks_since_wait = 0;
            self.device.poll(wgpu::Maintain::Wait);
        } else {
            self.device.poll(wgpu::Maintain::Poll);
        }
    }

    /// Read the displayed region at every scale, or say why it cannot be.
    pub fn capture(&self, bus: &Bus, cpu_w: u32, cpu_h: u32) -> Result<Vec<(u32, Img)>, HwSkip> {
        let area = bus.gpu.display_area();
        if area.bpp24 {
            return Err(HwSkip::Bpp24);
        }
        if bus.gpu.field_rendering_active() {
            return Err(HwSkip::FieldRendering);
        }
        self.device.poll(wgpu::Maintain::Wait);
        let mut out = Vec::new();
        for (s, hw) in &self.scales {
            let (w, h, rgba) = hw.read_subrect_rgba8(
                u32::from(area.x) * s,
                u32::from(area.y) * s,
                cpu_w * s,
                cpu_h * s,
            );
            out.push((*s, offset_scanout(
                Img::from_rgba(w, h, rgba),
                bus.gpu.horizontal_display_offset_px(),
                bus.gpu.vertical_display_offset_px(),
                *s,
            )));
        }
        Ok(out)
    }
}

/// Compose the shifted display window exclusively from hardware readback.
/// Offsets are native display pixels; readback and output are at `scale`.
/// Exposed borders are opaque black and content outside the window is clipped,
/// matching the display-sized canvas used by the CPU scanout.
fn offset_scanout(source: Img, offset_x: i32, offset_y: i32, scale: u32) -> Img {
    if offset_x == 0 && offset_y == 0 {
        return source;
    }
    let w = i64::from(source.w);
    let h = i64::from(source.h);
    let x = (i64::from(offset_x) * i64::from(scale)).clamp(-w, w);
    let y = (i64::from(offset_y) * i64::from(scale)).clamp(-h, h);
    let mut rgba = [0, 0, 0, 255].repeat(source.w as usize * source.h as usize);
    let x0 = x.max(0);
    let x1 = (w + x).min(w);
    let y0 = y.max(0);
    let y1 = (h + y).min(h);
    if x0 < x1 && y0 < y1 {
        let len = (x1 - x0) as usize * 4;
        for dy in y0..y1 {
            let src = ((dy - y) * w + x0 - x) as usize * 4;
            let dst = (dy * w + x0) as usize * 4;
            rgba[dst..dst + len].copy_from_slice(&source.rgba[src..src + len]);
        }
    }
    Img::from_rgba(source.w, source.h, rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(scale: u32) -> Img {
        let rgba = (1..=6).flat_map(|n| [n, n + 10, n + 20, 255]).collect();
        Img::from_rgba(3, 2, rgba).enlarge(scale)
    }

    fn pixel(image: &Img, x: u32, y: u32) -> [u8; 4] {
        image.rgba[((y * image.w + x) * 4) as usize..][..4].try_into().unwrap()
    }

    #[test]
    fn zero_offset_preserves_every_readback_byte() {
        for scale in [1, 3] {
            let source = fixture(scale);
            let expected = source.rgba.clone();
            let output = offset_scanout(source, 0, 0, scale);
            assert_eq!(output.rgba, expected);
            assert_eq!((output.w, output.h), (3 * scale, 2 * scale));
        }
    }

    #[test]
    fn signed_offsets_shift_clip_and_black_fill_at_native_and_three_times() {
        for scale in [1, 3] {
            for (ox, oy) in [(1, 0), (-1, 0), (0, 1), (0, -1),
                (1, 1), (1, -1), (-1, 1), (-1, -1)] {
                let source = fixture(scale);
                let output = offset_scanout(source.clone(), ox, oy, scale);
                assert_eq!((output.w, output.h), (3 * scale, 2 * scale));
                for y in 0..output.h {
                    for x in 0..output.w {
                        let sx = i64::from(x) - i64::from(ox) * i64::from(scale);
                        let sy = i64::from(y) - i64::from(oy) * i64::from(scale);
                        let expected = if sx >= 0 && sy >= 0
                            && sx < i64::from(source.w) && sy < i64::from(source.h) {
                            pixel(&source, sx as u32, sy as u32)
                        } else { [0, 0, 0, 255] };
                        assert_eq!(pixel(&output, x, y), expected,
                            "scale={scale}, offset=({ox},{oy}), pixel=({x},{y})");
                    }
                }
            }
        }
    }

    #[test]
    fn offsets_at_or_beyond_extent_are_entirely_opaque_black() {
        for scale in [1, 3] {
            for (ox, oy) in [(3, 0), (-3, 0), (0, 2), (0, -2),
                (100, 100), (-100, -100), (i32::MAX, 0), (i32::MIN, 0),
                (0, i32::MAX), (0, i32::MIN)] {
                let output = offset_scanout(fixture(scale), ox, oy, scale);
                assert!(output.rgba.chunks_exact(4).all(|px| px == [0, 0, 0, 255]));
            }
        }
    }

    #[test]
    fn three_times_scanout_matches_enlarged_native_clipping() {
        for (ox, oy) in [(1, 0), (-1, 0), (0, 1), (0, -1), (1, -1), (-1, 1)] {
            let native = offset_scanout(fixture(1), ox, oy, 1).enlarge(3);
            let scaled = offset_scanout(fixture(3), ox, oy, 3);
            assert_eq!(scaled.rgba, native.rgba);
        }
    }

    #[test]
    fn three_times_offset_preserves_independent_hardware_subpixels() {
        // Every readback pixel is distinct, including pixels within the same
        // native 3x3 block. Scanout must copy them without resampling.
        let rgba = (0..54).flat_map(|n| [n, n + 64, n + 128, 255]).collect();
        let source = Img::from_rgba(9, 6, rgba);
        let output = offset_scanout(source.clone(), 1, -1, 3);
        for y in 0..6 {
            for x in 0..9 {
                let expected = if x >= 3 && y < 3 {
                    pixel(&source, x - 3, y + 3)
                } else { [0, 0, 0, 255] };
                assert_eq!(pixel(&output, x, y), expected);
            }
        }
        assert_ne!(pixel(&output, 3, 0), pixel(&output, 4, 0));
        assert_ne!(pixel(&output, 3, 0), pixel(&output, 3, 1));
    }
}
