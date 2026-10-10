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
    ScreenOffset,
    FieldRendering,
}

impl HwSkip {
    pub fn describe(&self) -> &'static str {
        match self {
            HwSkip::Bpp24 => "24bpp display (hardware renderer defers to the CPU frame)",
            HwSkip::ScreenOffset => {
                "screen-offset display (hardware renderer defers to the CPU frame)"
            }
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
        if bus.gpu.horizontal_display_offset_px() != 0 || bus.gpu.vertical_display_offset_px() != 0
        {
            return Err(HwSkip::ScreenOffset);
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
            out.push((*s, Img::from_rgba(w, h, rgba)));
        }
        Ok(out)
    }
}
