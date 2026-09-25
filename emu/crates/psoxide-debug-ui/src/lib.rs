//! Guest performance panel shared by the PSoXide frontends.
//!
//! The panel describes the emulated PS1, not the host: how often the game
//! presents a new frame, where each vblank's CPU cycles went, how busy the
//! GPU was, and what the CD, SPU, DMA, MDEC and interrupt hardware did.
//!
//! Wiring, for any frontend:
//!
//! 1. Keep one [`GuestStats`] next to the emulator.
//! 2. Once per host frame call [`GuestStats::set_enabled`] with whether the
//!    panel is visible. Enabling turns on the core's CPU cycle attribution
//!    (the only per-instruction cost); disabled, recording is a no-op.
//! 3. After every emulated vblank step call [`GuestStats::record`].
//! 4. Draw with [`draw`] and handle the returned [`PanelAction`].
//!
//! Recording copies the core's cumulative counters
//! ([`emulator_core::guest_stats::GuestCounters`]) and stores the
//! difference in a fixed ring allocated once, so it never allocates per
//! frame and never touches emulated state.

mod panel;
mod plot;

use emulator_core::guest_stats::GuestCounters;
use emulator_core::{Bus, Cpu};

pub use panel::{draw, PanelAction};

/// Vblanks of history kept (two minutes at 60 Hz).
pub const HISTORY_VBLANKS: usize = 60 * 120;

/// Number of SPU voices.
pub const VOICES: usize = 24;

/// Disjoint CPU cycle classes stored per vblank. Work classes exclude the
/// cycles spent inside wait loops, which get their own classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum CpuClass {
    /// One issue cycle per retired instruction.
    Issue,
    /// Main-RAM load stalls not addressed through `$sp`.
    RamLoad,
    /// Main-RAM load stalls addressed through `$sp` (stack reloads).
    StackLoad,
    /// Main-RAM store stalls.
    RamStore,
    /// Instruction-cache refills and uncached fetches.
    Fetch,
    /// Waiting on a busy GTE.
    Gte,
    /// Waiting on the multiply/divide unit.
    MulDiv,
    /// I/O accesses, including GPU and DMA back-pressure.
    Hardware,
    /// Anything the profile does not name.
    Other,
    /// Wait loops polling an I/O register (GPUSTAT, DMA, CD, timers).
    WaitHardware,
    /// Wait loops polling memory (vblank counters, event flags), plus HLE
    /// kernel waits.
    WaitMemory,
}

/// Number of [`CpuClass`] values.
pub const CPU_CLASSES: usize = 11;

impl CpuClass {
    /// Every class in stacking order.
    pub const ALL: [CpuClass; CPU_CLASSES] = [
        CpuClass::Issue,
        CpuClass::RamLoad,
        CpuClass::StackLoad,
        CpuClass::RamStore,
        CpuClass::Fetch,
        CpuClass::Gte,
        CpuClass::MulDiv,
        CpuClass::Hardware,
        CpuClass::Other,
        CpuClass::WaitHardware,
        CpuClass::WaitMemory,
    ];

    /// Short label.
    pub fn label(self) -> &'static str {
        match self {
            CpuClass::Issue => "Issue",
            CpuClass::RamLoad => "RAM loads",
            CpuClass::StackLoad => "Stack loads",
            CpuClass::RamStore => "RAM stores",
            CpuClass::Fetch => "I-cache refill",
            CpuClass::Gte => "GTE interlock",
            CpuClass::MulDiv => "Mul/div interlock",
            CpuClass::Hardware => "I/O + DMA stalls",
            CpuClass::Other => "Other",
            CpuClass::WaitHardware => "Wait: polling I/O",
            CpuClass::WaitMemory => "Wait: polling RAM",
        }
    }

    /// True for the two waiting classes.
    pub fn is_wait(self) -> bool {
        matches!(self, CpuClass::WaitHardware | CpuClass::WaitMemory)
    }
}

/// One emulated vblank's worth of guest activity (the difference of two
/// counter samples).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FrameSample {
    /// Monotonic vblank index since the last reset.
    pub vblank: u64,
    /// Bus cycles that elapsed.
    pub cycles: u32,
    /// Cycles in one vblank for the current video standard.
    pub budget: u32,
    /// The display start moved: the game presented a new frame.
    pub presented: bool,
    /// Vblanks since the previous present (0 on the first one seen).
    pub present_interval: u16,
    /// CPU classes hold data (cycle attribution was on).
    pub cpu_profiled: bool,
    /// CPU cycles by [`CpuClass`].
    pub cpu: [u32; CPU_CLASSES],
    /// Retired instructions.
    pub instructions: u32,
    /// I-cache refill events.
    pub icache_refills: u32,
    /// Main-RAM data loads (needs cycle attribution).
    pub ram_loads: u32,
    /// Main-RAM data stores (needs cycle attribution).
    pub ram_stores: u32,
    /// GPU drawing time charged by the draw-time model, in CPU cycles.
    pub gpu_busy: u32,
    /// Part of `gpu_busy` spent on fills.
    pub gpu_fill_cycles: u32,
    /// Part of `gpu_busy` spent on polygons.
    pub gpu_polygon_cycles: u32,
    /// Part of `gpu_busy` spent on rectangles.
    pub gpu_rect_cycles: u32,
    /// Part of `gpu_busy` spent on VRAM copies.
    pub gpu_copy_cycles: u32,
    /// GP0 packets.
    pub gpu_packets: u32,
    /// Polygon packets.
    pub polygons: u32,
    /// Rectangle packets.
    pub rects: u32,
    /// Line packets.
    pub lines: u32,
    /// Fill packets.
    pub fills: u32,
    /// Pixel area drawn.
    pub pixels: u32,
    /// Pixel area drawn by textured primitives.
    pub texture_pixels: u32,
    /// Pixels uploaded CPU-to-VRAM.
    pub vram_upload_pixels: u32,
    /// CPU-to-VRAM transfers.
    pub vram_uploads: u16,
    /// VRAM-to-CPU transfers.
    pub vram_downloads: u16,
    /// VRAM-to-VRAM copies.
    pub vram_copies: u16,
    /// Data sectors read.
    pub cd_data_sectors: u16,
    /// XA-ADPCM audio sectors streamed.
    pub cd_xa_sectors: u16,
    /// CD-DA sectors played.
    pub cd_cdda_sectors: u16,
    /// Seeks.
    pub cd_seeks: u16,
    /// Controller commands.
    pub cd_commands: u16,
    /// SPU key-ons.
    pub spu_key_ons: u16,
    /// Voices with a running envelope at the end of the vblank.
    pub spu_active_voices: u8,
    /// Reverb master enable at the end of the vblank.
    pub spu_reverb: bool,
    /// DMA cycles in flight per channel.
    pub dma_busy: [u32; 7],
    /// DMA transfers started per channel.
    pub dma_starts: [u16; 7],
    /// MDEC macroblocks decoded.
    pub mdec_macroblocks: u32,
    /// Interrupt raises per source.
    pub irqs: [u16; 11],
    /// Controller polls.
    pub pad_polls: u16,
}

impl FrameSample {
    /// CPU cycles doing work (every class but the waits).
    pub fn cpu_work(&self) -> u32 {
        CpuClass::ALL
            .iter()
            .filter(|class| !class.is_wait())
            .map(|&class| self.cpu[class as usize])
            .fold(0u32, u32::saturating_add)
    }

    /// CPU cycles in wait loops.
    pub fn cpu_wait(&self) -> u32 {
        self.cpu[CpuClass::WaitHardware as usize]
            .saturating_add(self.cpu[CpuClass::WaitMemory as usize])
    }

    /// Cycles not covered by the CPU profile (zero when profiled normally).
    pub fn cpu_unattributed(&self) -> u32 {
        self.cycles
            .saturating_sub(self.cpu_work().saturating_add(self.cpu_wait()))
    }
}

/// Which present interval counts as on time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameTarget {
    /// The most common interval in the visible window.
    Auto,
    /// A fixed number of vblanks per frame (1 = 60 fps NTSC, 2 = 30 fps).
    Vblanks(u16),
}

/// Chart view state: time window, pause/scrub position, hover, toggles.
#[derive(Clone, Debug)]
pub struct ViewState {
    /// Visible time window, in vblanks.
    pub window: u64,
    /// Frozen right edge while paused (vblank index).
    pub paused_at: Option<u64>,
    /// Vblank under the pointer in any chart.
    pub hover: Option<u64>,
    /// On-time frame definition.
    pub target: FrameTarget,
    /// Hidden CPU classes, bit per [`CpuClass`].
    pub hidden_cpu: u16,
    /// Hidden component sparklines, bit per series.
    pub hidden_series: u64,
    /// Seconds exported by the CSV button.
    pub export_secs: u32,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            window: 60 * 10,
            paused_at: None,
            hover: None,
            target: FrameTarget::Auto,
            hidden_cpu: 0,
            hidden_series: 0,
            export_secs: 30,
        }
    }
}

/// Per-vblank guest telemetry history plus the panel's view state.
pub struct GuestStats {
    enabled: bool,
    prev: Option<GuestCounters>,
    ring: Vec<FrameSample>,
    head: usize,
    len: usize,
    next_vblank: u64,
    last_present: Option<u64>,
    refresh_hz: f64,
    voice_levels: [u16; VOICES],
    voice_peaks: [f32; VOICES],
    voice_keyed: [u32; VOICES],
    texpage_heat: [f32; 32],
    /// Chart view state.
    pub view: ViewState,
}

impl Default for GuestStats {
    fn default() -> Self {
        Self::new()
    }
}

impl GuestStats {
    /// Empty history with its ring allocated up front.
    pub fn new() -> Self {
        Self {
            enabled: false,
            prev: None,
            ring: vec![FrameSample::default(); HISTORY_VBLANKS],
            head: 0,
            len: 0,
            next_vblank: 0,
            last_present: None,
            refresh_hz: 60.0,
            voice_levels: [0; VOICES],
            voice_peaks: [0.0; VOICES],
            voice_keyed: [0; VOICES],
            texpage_heat: [0.0; 32],
            view: ViewState::default(),
        }
    }

    /// Whether samples are being recorded.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Turn recording (and the core's CPU cycle attribution) on or off.
    /// Cheap to call every host frame.
    pub fn set_enabled(&mut self, cpu: &mut Cpu, enabled: bool) {
        if enabled != cpu.cpu_cycle_profile_enabled() {
            cpu.set_cpu_cycle_profile_enabled(enabled);
            // The profile restarted from zero; start a fresh baseline so
            // the next delta is not taken across the reset.
            self.prev = None;
        }
        if enabled != self.enabled {
            self.enabled = enabled;
            self.prev = None;
            // Vblanks while disabled are not in the history, so an interval
            // across the gap would be wrong.
            self.last_present = None;
        }
    }

    /// Forget all history (a different game was loaded, or reset).
    pub fn clear(&mut self) {
        self.prev = None;
        self.head = 0;
        self.len = 0;
        self.next_vblank = 0;
        self.last_present = None;
        self.voice_levels = [0; VOICES];
        self.voice_peaks = [0.0; VOICES];
        self.voice_keyed = [0; VOICES];
        self.texpage_heat = [0.0; 32];
        self.view.paused_at = None;
        self.view.hover = None;
    }

    /// Record one emulated vblank step. Call after the emulator ran one
    /// vblank period; a no-op while disabled.
    pub fn record(&mut self, cpu: &Cpu, bus: &Bus) {
        if !self.enabled {
            return;
        }
        let now = GuestCounters::sample(cpu, bus);
        let Some(prev) = self.prev.replace(now) else {
            return;
        };
        if now.bus_cycles <= prev.bus_cycles || now.instructions < prev.instructions {
            // A different game, a reset or an older save state: the clock
            // went backwards, so the history no longer describes this run.
            self.clear();
            self.prev = Some(now);
            return;
        }
        self.refresh_hz = now.refresh_hz();
        let vblank = self.next_vblank;
        self.next_vblank += 1;
        let presented = (now.display_x, now.display_y) != (prev.display_x, prev.display_y);
        let present_interval = if presented {
            let interval = self
                .last_present
                .map_or(0, |last| (vblank - last).min(u64::from(u16::MAX)) as u16);
            self.last_present = Some(vblank);
            interval
        } else {
            0
        };
        let sample = frame_sample(vblank, &prev, &now, presented, present_interval);
        self.ring[self.head] = sample;
        self.head = (self.head + 1) % self.ring.len();
        self.len = (self.len + 1).min(self.ring.len());

        // Live-only meters: voice envelopes with a falling peak, key-on
        // flashes, and a decaying texture-page heat map.
        let (key_ons, _) = bus.spu.voice_debug_counts();
        #[allow(clippy::needless_range_loop)] // four parallel per-voice arrays
        for voice in 0..VOICES {
            let level = now.spu_voice_levels[voice];
            self.voice_levels[voice] = level;
            let level = f32::from(level) / 32767.0;
            self.voice_peaks[voice] = (self.voice_peaks[voice] * 0.94).max(level);
            if key_ons[voice] != self.voice_keyed[voice] {
                self.voice_keyed[voice] = key_ons[voice];
                self.voice_peaks[voice] = 1.0f32.max(level);
            }
        }
        for page in 0..32 {
            let drawn = now.gpu_work.texpage_pixels[page]
                .saturating_sub(prev.gpu_work.texpage_pixels[page]) as f32;
            self.texpage_heat[page] = self.texpage_heat[page] * 0.97 + drawn * 0.03;
        }
    }

    /// Refresh rate of the running game's video standard.
    pub fn refresh_hz(&self) -> f64 {
        self.refresh_hz
    }

    /// Number of samples held.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True before the first sample.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Vblank index of the newest sample.
    pub fn newest_vblank(&self) -> Option<u64> {
        (self.len > 0).then(|| self.next_vblank - 1)
    }

    /// Vblank index of the oldest sample.
    pub fn oldest_vblank(&self) -> Option<u64> {
        (self.len > 0).then(|| self.next_vblank - self.len as u64)
    }

    /// Sample for a vblank index, if still held.
    pub fn get(&self, vblank: u64) -> Option<&FrameSample> {
        let oldest = self.oldest_vblank()?;
        if vblank < oldest || vblank >= self.next_vblank {
            return None;
        }
        let back = (self.next_vblank - 1 - vblank) as usize;
        let index = (self.head + self.ring.len() - 1 - back) % self.ring.len();
        Some(&self.ring[index])
    }

    /// Samples with vblank in `first..=last`, oldest first.
    pub fn range(&self, first: u64, last: u64) -> impl Iterator<Item = &FrameSample> + '_ {
        let (lo, hi) = match (self.oldest_vblank(), self.newest_vblank()) {
            (Some(oldest), Some(newest)) => (first.max(oldest), last.min(newest)),
            _ => (1, 0),
        };
        (lo..=hi).filter_map(move |vblank| self.get(vblank))
    }

    /// Latest per-voice envelope level (0..=0x7FFF).
    pub fn voice_levels(&self) -> &[u16; VOICES] {
        &self.voice_levels
    }

    /// Per-voice falling peak, 0..=1, jumping to 1 on key-on.
    pub fn voice_peaks(&self) -> &[f32; VOICES] {
        &self.voice_peaks
    }

    /// Decaying textured-pixel rate per texture page.
    pub fn texpage_heat(&self) -> &[f32; 32] {
        &self.texpage_heat
    }

    /// CSV of the newest `seconds` of samples, one row per vblank.
    pub fn csv(&self, seconds: u32) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        out.push_str("vblank,time_s,cycles,budget,presented,present_interval_vblanks,cpu_profiled");
        for class in CpuClass::ALL {
            let _ = write!(out, ",cpu_{}", snake(class.label()));
        }
        out.push_str(",cpu_work,cpu_wait,cpu_unattributed,instructions,icache_refills,ram_loads,ram_stores,gpu_busy,gpu_fill_cycles,gpu_polygon_cycles,gpu_rect_cycles,gpu_copy_cycles,gpu_packets,polygons,rects,lines,fills,pixels,texture_pixels,vram_upload_pixels,vram_uploads,vram_downloads,vram_copies,cd_data_sectors,cd_xa_sectors,cd_cdda_sectors,cd_seeks,cd_commands,spu_key_ons,spu_active_voices,spu_reverb");
        for name in DMA_NAMES {
            let _ = write!(out, ",dma_{}_busy", snake(name));
        }
        for name in DMA_NAMES {
            let _ = write!(out, ",dma_{}_starts", snake(name));
        }
        out.push_str(",mdec_macroblocks");
        for name in IRQ_NAMES {
            let _ = write!(out, ",irq_{}", snake(name));
        }
        out.push_str(",pad_polls\n");
        let Some(newest) = self.newest_vblank() else {
            return out;
        };
        let span = (f64::from(seconds) * self.refresh_hz).round() as u64;
        let first = newest.saturating_sub(span.saturating_sub(1));
        for s in self.range(first, newest) {
            let _ = write!(
                out,
                "{},{:.4},{},{},{},{},{}",
                s.vblank,
                s.vblank as f64 / self.refresh_hz,
                s.cycles,
                s.budget,
                u8::from(s.presented),
                s.present_interval,
                u8::from(s.cpu_profiled)
            );
            for value in s.cpu {
                let _ = write!(out, ",{value}");
            }
            let _ = write!(
                out,
                ",{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
                s.cpu_work(),
                s.cpu_wait(),
                s.cpu_unattributed(),
                s.instructions,
                s.icache_refills,
                s.ram_loads,
                s.ram_stores,
                s.gpu_busy,
                s.gpu_fill_cycles,
                s.gpu_polygon_cycles,
                s.gpu_rect_cycles,
                s.gpu_copy_cycles,
                s.gpu_packets,
                s.polygons,
                s.rects,
                s.lines,
                s.fills,
                s.pixels,
                s.texture_pixels,
                s.vram_upload_pixels,
                s.vram_uploads,
                s.vram_downloads,
                s.vram_copies,
                s.cd_data_sectors,
                s.cd_xa_sectors,
                s.cd_cdda_sectors,
                s.cd_seeks,
                s.cd_commands,
                s.spu_key_ons,
                s.spu_active_voices,
                u8::from(s.spu_reverb),
            );
            for value in s.dma_busy {
                let _ = write!(out, ",{value}");
            }
            for value in s.dma_starts {
                let _ = write!(out, ",{value}");
            }
            let _ = write!(out, ",{}", s.mdec_macroblocks);
            for value in s.irqs {
                let _ = write!(out, ",{value}");
            }
            let _ = writeln!(out, ",{}", s.pad_polls);
        }
        out
    }
}

/// DMA channel names, index = channel.
pub const DMA_NAMES: [&str; 7] = ["MDEC in", "MDEC out", "GPU", "CD-ROM", "SPU", "PIO", "OTC"];

/// Interrupt source names, index = I_STAT bit.
pub const IRQ_NAMES: [&str; 11] = [
    "VBlank", "GPU", "CD-ROM", "DMA", "Timer 0", "Timer 1", "Timer 2", "Pad/card", "SIO", "SPU",
    "Lightpen",
];

fn snake(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    for ch in label.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_matches('_').to_string()
}

fn clamp32(value: u64) -> u32 {
    value.min(u64::from(u32::MAX)) as u32
}

fn clamp16(value: u64) -> u16 {
    value.min(u64::from(u16::MAX)) as u16
}

fn frame_sample(
    vblank: u64,
    prev: &GuestCounters,
    now: &GuestCounters,
    presented: bool,
    present_interval: u16,
) -> FrameSample {
    let d = |a: u64, b: u64| a.saturating_sub(b);
    let mut cpu = [0u32; CPU_CLASSES];
    let cpu_profiled = now.cpu_profiled && prev.cpu_profiled;
    if cpu_profiled {
        let all = now.cpu.delta_since(prev.cpu);
        let wait_hw = now
            .cpu_wait
            .hardware_poll
            .delta_since(prev.cpu_wait.hardware_poll);
        let wait_mem = now
            .cpu_wait
            .memory_poll
            .delta_since(prev.cpu_wait.memory_poll);
        let hle = d(now.cpu_wait.hle_wait_cycles, prev.cpu_wait.hle_wait_cycles);
        let waits = wait_hw.saturating_add(wait_mem);
        let work = all.delta_since(waits);
        let set = |cpu: &mut [u32; CPU_CLASSES], class: CpuClass, value: u64| {
            cpu[class as usize] = clamp32(value);
        };
        set(&mut cpu, CpuClass::Issue, work.issue_cycles);
        set(
            &mut cpu,
            CpuClass::RamLoad,
            work.ram_load_stall_cycles
                .saturating_sub(work.stack_ram_load_stall_cycles),
        );
        set(
            &mut cpu,
            CpuClass::StackLoad,
            work.stack_ram_load_stall_cycles,
        );
        set(&mut cpu, CpuClass::RamStore, work.ram_store_stall_cycles);
        set(
            &mut cpu,
            CpuClass::Fetch,
            work.icache_refill_stall_cycles + work.uncached_fetch_stall_cycles,
        );
        set(&mut cpu, CpuClass::Gte, work.gte_busy_stall_cycles);
        set(
            &mut cpu,
            CpuClass::MulDiv,
            work.muldiv_interlock_stall_cycles,
        );
        set(&mut cpu, CpuClass::Hardware, work.mmio_stall_cycles);
        // HLE waits are charged as issue + other by the core.
        set(
            &mut cpu,
            CpuClass::Other,
            work.other_stall_cycles.saturating_sub(hle),
        );
        set(
            &mut cpu,
            CpuClass::WaitHardware,
            wait_hw.total_profiled_cycles(),
        );
        set(
            &mut cpu,
            CpuClass::WaitMemory,
            wait_mem.total_profiled_cycles().saturating_add(hle),
        );
    }
    let dma_busy =
        std::array::from_fn(|ch| clamp32(d(now.dma_busy_cycles[ch], prev.dma_busy_cycles[ch])));
    let dma_starts = std::array::from_fn(|ch| clamp16(d(now.dma_starts[ch], prev.dma_starts[ch])));
    let irqs = std::array::from_fn(|i| clamp16(d(now.irq_raises[i], prev.irq_raises[i])));
    FrameSample {
        vblank,
        cycles: clamp32(d(now.bus_cycles, prev.bus_cycles)),
        budget: clamp32(now.vblank_period),
        presented,
        present_interval,
        cpu_profiled,
        cpu,
        instructions: clamp32(d(now.instructions, prev.instructions)),
        icache_refills: clamp32(d(now.icache.refill_events, prev.icache.refill_events)),
        ram_loads: clamp32(d(now.cpu_wait.ram_loads, prev.cpu_wait.ram_loads)),
        ram_stores: clamp32(d(now.cpu_wait.ram_stores, prev.cpu_wait.ram_stores)),
        gpu_busy: clamp32(d(now.gpu_busy_cycles, prev.gpu_busy_cycles)),
        gpu_fill_cycles: clamp32(d(now.gpu_fill_cycles, prev.gpu_fill_cycles)),
        gpu_polygon_cycles: clamp32(d(now.gpu_polygon_cycles, prev.gpu_polygon_cycles)),
        gpu_rect_cycles: clamp32(d(now.gpu_rect_cycles, prev.gpu_rect_cycles)),
        gpu_copy_cycles: clamp32(d(now.gpu_copy_cycles, prev.gpu_copy_cycles)),
        gpu_packets: clamp32(d(now.gpu_packets, prev.gpu_packets)),
        polygons: clamp32(d(now.gpu_polygons, prev.gpu_polygons)),
        rects: clamp32(d(now.gpu_rects, prev.gpu_rects)),
        lines: clamp32(d(now.gpu_lines, prev.gpu_lines)),
        fills: clamp32(d(now.gpu_fills, prev.gpu_fills)),
        pixels: clamp32(d(now.gpu_work.pixels, prev.gpu_work.pixels)),
        texture_pixels: clamp32(d(now.gpu_work.texture_pixels, prev.gpu_work.texture_pixels)),
        vram_upload_pixels: clamp32(d(
            now.gpu_work.vram_upload_pixels,
            prev.gpu_work.vram_upload_pixels,
        )),
        vram_uploads: clamp16(d(now.vram_uploads, prev.vram_uploads)),
        vram_downloads: clamp16(d(now.vram_downloads, prev.vram_downloads)),
        vram_copies: clamp16(d(now.vram_copies, prev.vram_copies)),
        cd_data_sectors: clamp16(d(now.cd.data_sectors, prev.cd.data_sectors)),
        cd_xa_sectors: clamp16(d(now.cd.xa_audio_sectors, prev.cd.xa_audio_sectors)),
        cd_cdda_sectors: clamp16(d(now.cd.cdda_sectors, prev.cd.cdda_sectors)),
        cd_seeks: clamp16(d(now.cd.seeks, prev.cd.seeks)),
        cd_commands: clamp16(d(now.cd_commands, prev.cd_commands)),
        spu_key_ons: clamp16(d(now.spu_key_ons, prev.spu_key_ons)),
        spu_active_voices: now.spu_voice_levels.iter().filter(|&&l| l > 0).count() as u8,
        spu_reverb: now.spu_reverb_enabled,
        dma_busy,
        dma_starts,
        mdec_macroblocks: clamp32(d(now.mdec_macroblocks, prev.mdec_macroblocks)),
        irqs,
        pad_polls: clamp16(d(now.pad_polls, prev.pad_polls)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push(stats: &mut GuestStats, sample: FrameSample) {
        let vblank = stats.next_vblank;
        stats.next_vblank += 1;
        stats.ring[stats.head] = FrameSample { vblank, ..sample };
        stats.head = (stats.head + 1) % stats.ring.len();
        stats.len = (stats.len + 1).min(stats.ring.len());
    }

    #[test]
    fn ring_indexes_by_vblank_after_wrapping() {
        let mut stats = GuestStats::new();
        for i in 0..(HISTORY_VBLANKS as u32 + 10) {
            push(
                &mut stats,
                FrameSample {
                    cycles: i,
                    ..Default::default()
                },
            );
        }
        let newest = stats.newest_vblank().unwrap();
        let oldest = stats.oldest_vblank().unwrap();
        assert_eq!(newest - oldest + 1, HISTORY_VBLANKS as u64);
        assert_eq!(stats.get(newest).unwrap().cycles, newest as u32);
        assert_eq!(stats.get(oldest).unwrap().cycles, oldest as u32);
        assert!(stats.get(oldest - 1).is_none());
        assert_eq!(stats.range(newest - 4, newest + 10).count(), 5);
    }

    #[test]
    fn csv_has_one_row_per_sample_and_matching_columns() {
        let mut stats = GuestStats::new();
        for _ in 0..5 {
            push(&mut stats, FrameSample::default());
        }
        let csv = stats.csv(60);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 6);
        let columns = lines[0].split(',').count();
        assert!(lines.iter().all(|line| line.split(',').count() == columns));
    }
}
