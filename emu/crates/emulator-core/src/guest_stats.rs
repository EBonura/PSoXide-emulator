//! Cumulative guest-hardware counters for the debug UI.
//!
//! [`GuestCounters::sample`] copies every counter the core keeps about what
//! the emulated PS1 did (CPU cycle classes, GPU drawing, CD, SPU, DMA, MDEC,
//! interrupts) into one plain struct. The frontend samples it once per
//! emulated vblank and differences consecutive samples, so the per-frame
//! cost is a few hundred word copies and nothing here runs per instruction.
//!
//! The counters only observe: none of them feeds back into emulation, and
//! all of them are excluded from save states. The CPU cycle classes are the
//! exception to "always on": they need [`Cpu::set_cpu_cycle_profile_enabled`]
//! (a per-instruction cost), so [`GuestCounters::cpu_profiled`] says whether
//! the CPU fields hold data.

use crate::cdrom::CdWorkCounters;
use crate::cpu::{
    CpuCycleProfileSnapshot, CpuWaitProfileSnapshot, InstructionCacheProfileSnapshot,
};
use crate::gpu::GpuWorkCounters;
use crate::spu::NUM_VOICES;
use crate::{Bus, Cpu};

/// PS1 master (CPU) clock in Hz.
pub const MASTER_CLOCK_HZ: u64 = 33_868_800;

/// One snapshot of every cumulative guest counter. The difference of two
/// consecutive samples is one frame's worth of work.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GuestCounters {
    /// Bus cycles since power-on.
    pub bus_cycles: u64,
    /// Retired CPU instructions.
    pub instructions: u64,
    /// Cycles per vblank for the current video standard.
    pub vblank_period: u64,
    /// Whether CPU cycle attribution (`cpu`, `cpu_wait`) was being collected.
    pub cpu_profiled: bool,
    /// CPU cycles by class.
    pub cpu: CpuCycleProfileSnapshot,
    /// CPU cycles spent in wait loops, and RAM data traffic.
    pub cpu_wait: CpuWaitProfileSnapshot,
    /// Instruction-cache refills.
    pub icache: InstructionCacheProfileSnapshot,
    /// Display area start X in VRAM (a change is a presented frame).
    pub display_x: u16,
    /// Display area start Y in VRAM.
    pub display_y: u16,
    /// Displayed width in pixels.
    pub display_width: u16,
    /// Displayed height in pixels.
    pub display_height: u16,
    /// GP0 packets executed.
    pub gpu_packets: u64,
    /// Polygon packets (triangles and quads).
    pub gpu_polygons: u64,
    /// Rectangle (sprite) packets.
    pub gpu_rects: u64,
    /// Line packets.
    pub gpu_lines: u64,
    /// GP0(02h) fills.
    pub gpu_fills: u64,
    /// CPU-to-VRAM transfers started.
    pub vram_uploads: u64,
    /// VRAM-to-CPU transfers started.
    pub vram_downloads: u64,
    /// VRAM-to-VRAM copies.
    pub vram_copies: u64,
    /// GPU drawing time charged by the draw-time model, in CPU cycles.
    pub gpu_busy_cycles: u64,
    /// Part of `gpu_busy_cycles` spent on fills.
    pub gpu_fill_cycles: u64,
    /// Part of `gpu_busy_cycles` spent on polygons.
    pub gpu_polygon_cycles: u64,
    /// Part of `gpu_busy_cycles` spent on rectangles.
    pub gpu_rect_cycles: u64,
    /// Part of `gpu_busy_cycles` spent on VRAM copies.
    pub gpu_copy_cycles: u64,
    /// Pixel areas and texture-page use.
    pub gpu_work: GpuWorkCounters,
    /// CD sectors and seeks.
    pub cd: CdWorkCounters,
    /// CD controller commands.
    pub cd_commands: u64,
    /// SPU key-ons summed over voices.
    pub spu_key_ons: u64,
    /// Instantaneous per-voice envelope level (0..=0x7FFF), 0 when off.
    pub spu_voice_levels: [u16; NUM_VOICES],
    /// SPUCNT reverb master enable.
    pub spu_reverb_enabled: bool,
    /// DMA transfers started, per channel (MDECin, MDECout, GPU, CD, SPU,
    /// PIO, OTC).
    pub dma_starts: [u64; 7],
    /// DMA cycles in flight, per channel.
    pub dma_busy_cycles: [u64; 7],
    /// MDEC macroblocks decoded.
    pub mdec_macroblocks: u64,
    /// Interrupt raises per source (VBlank, GPU, CD, DMA, Timer0-2, pad,
    /// SIO, SPU, lightpen).
    pub irq_raises: [u64; 11],
    /// Completed controller polls on port 1.
    pub pad_polls: u64,
}

impl GuestCounters {
    /// Copy every cumulative counter out of the running machine.
    pub fn sample(cpu: &Cpu, bus: &Bus) -> Self {
        let ops = bus.gpu.gp0_opcode_histogram();
        let timing = bus.gpu.gp0_timing_histogram();
        let count = |range: std::ops::RangeInclusive<usize>| -> u64 {
            ops[range].iter().map(|&n| u64::from(n)).sum()
        };
        let cycles = |range: std::ops::RangeInclusive<usize>| -> u64 { timing[range].iter().sum() };
        let area = bus.gpu.display_area();
        let (key_ons, _) = bus.spu.voice_debug_counts();
        Self {
            bus_cycles: bus.cycles(),
            instructions: cpu.tick(),
            vblank_period: bus.vblank_period(),
            cpu_profiled: cpu.cpu_cycle_profile_enabled(),
            cpu: cpu.cpu_cycle_profile(),
            cpu_wait: cpu.cpu_wait_profile(),
            icache: cpu.instruction_cache_profile(),
            display_x: area.x,
            display_y: area.y,
            display_width: area.width,
            display_height: area.height,
            gpu_packets: count(0..=255),
            gpu_polygons: count(0x20..=0x3F),
            gpu_rects: count(0x60..=0x7F),
            gpu_lines: count(0x40..=0x5F),
            gpu_fills: count(0x02..=0x02),
            vram_uploads: count(0xA0..=0xBF),
            vram_downloads: count(0xC0..=0xDF),
            vram_copies: count(0x80..=0x9F),
            gpu_busy_cycles: timing.iter().sum(),
            gpu_fill_cycles: timing[0x02],
            gpu_polygon_cycles: cycles(0x20..=0x3F),
            gpu_rect_cycles: cycles(0x60..=0x7F),
            gpu_copy_cycles: cycles(0x80..=0x9F),
            gpu_work: *bus.gpu.work_counters(),
            cd: bus.cdrom.work_counters(),
            cd_commands: bus.cdrom.commands_dispatched(),
            spu_key_ons: key_ons.iter().map(|&n| u64::from(n)).sum(),
            spu_voice_levels: bus.spu.voice_envelope_levels(),
            spu_reverb_enabled: bus.spu.spucnt() & 0x80 != 0,
            dma_starts: bus.dma_start_triggers(),
            dma_busy_cycles: bus.dma_busy_cycles(),
            mdec_macroblocks: bus.mdec.macroblocks_decoded(),
            irq_raises: bus.irq().raise_counts(),
            pad_polls: bus.port1_completed_polls(),
        }
    }

    /// Video refresh rate implied by the vblank period (about 60 Hz NTSC,
    /// about 50 Hz PAL).
    pub fn refresh_hz(&self) -> f64 {
        if self.vblank_period == 0 {
            return 60.0;
        }
        MASTER_CLOCK_HZ as f64 / self.vblank_period as f64
    }
}
