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
    submitted_epoch: u32,
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
            submitted_epoch: 0,
            adapter: format!("{} ({:?})", info.name, info.backend),
        })
    }

    pub fn enable_submitted_trace(&mut self, text: &str, epoch: u32) -> Result<(), String> {
        let roi = parse_submitted_roi(text)?;
        self.submitted_epoch = epoch;
        if let Some((_, hw)) = self.scales.iter_mut().max_by_key(|(s, _)| *s) {
            hw.enable_submitted_trace(roi);
        }
        Ok(())
    }

    pub fn save_submitted_trace(&self, dir: &std::path::Path) -> Result<Option<String>, String> {
        use sha2::{Digest, Sha256};
        let Some((scale, hw)) = self.scales.iter().max_by_key(|(s, _)| *s) else {
            return Ok(None);
        };
        let Some((bytes, truncated)) = hw.submitted_trace_bytes() else {
            return Ok(None);
        };
        let receipt = std::fs::read(dir.join("receipt.toml")).map_err(|e| e.to_string())?;
        let packets = std::fs::read(dir.join("packets.bin")).map_err(|e| e.to_string())?;
        std::fs::write(dir.join("hw-submitted.bin"), &bytes).map_err(|e| e.to_string())?;
        let text = format!("schema = 1\nscale = {scale}\nbytes = {}\ntruncated = {truncated}\nhardware_winner_verified = false\ncoverage = \"ROI bounding-box candidates; exact submitted vertices/runs and sampler regions\"\ntrace_receipt_sha256 = \"{:x}\"\nraw_packets_sha256 = \"{:x}\"\nsubmitted_sha256 = \"{:x}\"\n", bytes.len(), Sha256::digest(&receipt), Sha256::digest(&packets), Sha256::digest(&bytes));
        std::fs::write(dir.join("hw-submitted-receipt.toml"), text).map_err(|e| e.to_string())?;
        Ok(Some(format!("bounded HW submitted candidate trace scale {scale}, {} bytes, truncated={truncated}; winner unknown",bytes.len())))
    }

    /// Call before running a tick.
    pub fn before_tick(&mut self, bus: &Bus) {
        self.pre_tick_vram.clear();
        self.pre_tick_vram.extend_from_slice(bus.gpu.vram.words());
    }

    /// Call after a tick: replay its command log on every renderer.
    pub fn after_tick_traced(&mut self, bus: &mut Bus, tick: u64) {
        let log = bus.gpu.drain_completed_cmd_log();
        for (_, hw) in &mut self.scales {
            hw.submitted_trace_context(tick, self.submitted_epoch);
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

fn parse_submitted_roi(text: &str) -> Result<[u16; 4], String> {
    let values: Result<Vec<u32>, _> = text.split(',').map(str::parse).collect();
    let v = values.map_err(|_| "trace HW ROI requires x,y,width,height".to_string())?;
    if v.len() != 4
        || v[2] == 0
        || v[3] == 0
        || v[0].checked_add(v[2]).is_none_or(|n| n > 1024)
        || v[1].checked_add(v[3]).is_none_or(|n| n > 512)
    {
        return Err("trace HW ROI must be a nonempty native VRAM rectangle within 1024x512".into());
    }
    Ok([
        v[0] as u16,
        v[1] as u16,
        (v[0] + v[2] - 1) as u16,
        (v[1] + v[3] - 1) as u16,
    ])
}
#[cfg(test)]
mod submitted_tests {
    use super::*;
    #[test]
    fn roi_parser_fails_closed_and_keeps_edge_coordinates() {
        assert_eq!(
            parse_submitted_roi("1023,511,1,1").unwrap(),
            [1023, 511, 1023, 511]
        );
        for bad in [
            "0,0,0,1",
            "0,0,1,0",
            "1023,0,2,1",
            "0,511,1,2",
            "4294967295,0,1,1",
            "0,0,1",
            "no",
        ] {
            assert!(parse_submitted_roi(bad).is_err());
        }
    }
}
