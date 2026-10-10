//! The emulated console driven tick by tick, with the per-tick history the
//! window asserts (presenting, stuck PC, audio) are evaluated against.
//!
//! A tick is the same unit the headless frontend's route clock uses: one
//! vblank period of emulated cycles, capped at one million instructions. Input
//! set between ticks is therefore frame-for-frame what `launch --press` does.

use std::path::Path;

use emulator_core::{button, fast_boot_disc, Bus, ButtonState, Cpu, Deinterlace};

use crate::journey::{MemcardMode, Pad2Mode, PadMode};

/// Same cap the headless frontend and the app's frame step use.
const STEPS_PER_TICK: u64 = 1_000_000;
/// Instructions between program-counter samples.
const PC_CHUNK: u64 = 16_384;
const STICK_CENTRE: u8 = 0x80;

/// What one tick left behind.
#[derive(Clone, Copy, Debug, Default)]
pub struct TickRec {
    /// The display start moved (a buffer flip) during the tick.
    pub flipped: bool,
    /// The displayed pixels differ from the previous tick's.
    pub changed: bool,
    /// Largest absolute stereo sample the SPU produced.
    pub audio_peak: u32,
    /// Range of this tick's samples in `Machine::pc_samples`.
    pub pc_start: u32,
    pub pc_end: u32,
}

pub struct Machine {
    pub cpu: Cpu,
    pub bus: Bus,
    pub ticks: u64,
    pub history: Vec<TickRec>,
    /// Sampled code lines (pc >> 4), per tick via `TickRec`.
    pub pc_samples: Vec<u32>,
    deadline: u64,
    period: u64,
    last_display: (u16, u16),
    last_hash: u64,
}

pub struct Boot<'a> {
    pub disc: &'a Path,
    pub pad: PadMode,
    pub pad2: Pad2Mode,
    pub memcard: MemcardMode,
    /// Arm the GP0 command log (needed by the hardware renderer).
    pub cmd_log: bool,
}

impl Machine {
    pub fn boot(opts: &Boot<'_>) -> Result<Machine, String> {
        let ext = opts
            .disc
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        let disc = match ext.as_str() {
            "cue" => psoxide_settings::library::load_disc_from_cue(opts.disc)?,
            "bin" | "iso" => psoxide_settings::library::load_disc_from_bin(opts.disc)?,
            "ccd" => psoxide_settings::library::load_disc_from_ccd(opts.disc)?,
            other => return Err(format!("unsupported disc extension .{other}: {}", opts.disc.display())),
        };
        let mut cpu = Cpu::new();
        let mut bus = Bus::new_without_bios();
        if opts.cmd_log {
            bus.gpu.enable_cmd_log();
        }
        fast_boot_disc(&mut bus, &mut cpu, &disc)
            .map_err(|e| format!("boot {}: {e:?}", opts.disc.display()))?;
        bus.cdrom.insert_disc(Some(disc));
        if ext == "cue" {
            if let Ok(Some(lbas)) = psoxide_settings::library::load_sbi_for(opts.disc) {
                bus.cdrom.set_bad_subq_sectors(lbas);
            }
        }
        match opts.pad {
            PadMode::Digital => bus.attach_original_digital_pad_port1(),
            PadMode::Dualshock => bus.attach_digital_pad_port1(),
            PadMode::Analog => {
                bus.attach_digital_pad_port1();
                let _ = bus.force_port1_analog_mode();
            }
        }
        match opts.pad2 {
            Pad2Mode::None => {}
            Pad2Mode::Digital => bus.attach_original_digital_pad_port2(),
            Pad2Mode::Dualshock => bus.attach_digital_pad_port2(),
            Pad2Mode::Analog => {
                bus.attach_digital_pad_port2();
                let _ = bus.force_port2_analog_mode();
            }
        }
        if opts.memcard == MemcardMode::None {
            bus.detach_memcard_port1();
            bus.detach_memcard_port2();
        }
        #[cfg(feature = "native-jit")]
        let _ = psoxide_jit::install_tier(&mut cpu);

        let period = bus.vblank_period().max(1);
        let deadline = bus.cycles().saturating_add(period);
        let area = bus.gpu.display_area();
        Ok(Machine {
            cpu,
            bus,
            ticks: 0,
            history: Vec::new(),
            pc_samples: Vec::new(),
            deadline,
            period,
            last_display: (area.x, area.y),
            last_hash: 0,
        })
    }

    /// Set one port's pad for the next tick: face/shoulder/d-pad bits plus sticks as
    /// `(left, right)` `[x, y]` pairs.
    pub fn set_pad(&mut self, port: u8, mask: u16, left: [u8; 2], right: [u8; 2]) {
        if port == 2 {
            self.bus.set_port2_buttons(ButtonState::from_bits(mask));
            self.bus
                .set_port2_sticks(right[0], right[1], left[0], left[1]);
        } else {
            self.bus.set_port1_buttons(ButtonState::from_bits(mask));
            self.bus
                .set_port1_sticks(right[0], right[1], left[0], left[1]);
        }
    }

    /// Both pads released and centred.
    pub fn release_pads(&mut self) {
        for port in [1, 2] {
            self.set_pad(port, 0, Self::centre(), Self::centre());
        }
    }

    pub fn centre() -> [u8; 2] {
        [STICK_CENTRE, STICK_CENTRE]
    }

    /// Run one tick. Returns an error if the guest hits an execution fault.
    pub fn tick(&mut self) -> Result<(), String> {
        let pc_start = self.pc_samples.len() as u32;
        let mut steps = 0u64;
        let mut audio_peak = 0u32;
        loop {
            let n = (STEPS_PER_TICK - steps).min(PC_CHUNK).max(1);
            let (ran, result) = self.cpu.run(&mut self.bus, n, self.deadline, |_| false);
            steps += ran;
            self.pc_samples.push(self.cpu.pc() >> 4);
            self.bus.run_spu_to_current_cycle();
            if self.bus.spu.audio_queue_len() != 0 {
                for (l, r) in self.bus.spu.drain_audio() {
                    let peak = u32::from(l.unsigned_abs()).max(u32::from(r.unsigned_abs()));
                    audio_peak = audio_peak.max(peak);
                }
            }
            if let Err(e) = result {
                return Err(format!(
                    "guest fault at tick {} pc={:#010x} ra={:#010x} sp={:#010x}: {e:?}",
                    self.ticks,
                    self.cpu.pc(),
                    self.cpu.gpr(31),
                    self.cpu.gpr(29)
                ));
            }
            if self.bus.cycles() >= self.deadline || steps >= STEPS_PER_TICK {
                break;
            }
            if ran == 0 {
                return Err(format!("cpu made no progress at tick {} pc={:#010x}", self.ticks, self.cpu.pc()));
            }
        }
        self.deadline = self.bus.cycles().saturating_add(self.period);
        self.ticks += 1;

        let area = self.bus.gpu.display_area();
        let start = (area.x, area.y);
        let flipped = start != self.last_display;
        self.last_display = start;
        let (hash, ..) = self.bus.gpu.display_hash();
        let changed = hash != self.last_hash;
        self.last_hash = hash;
        self.history.push(TickRec {
            flipped,
            changed,
            audio_peak,
            pc_start,
            pc_end: self.pc_samples.len() as u32,
        });
        Ok(())
    }

    /// The last `window` ticks of history (fewer early in a run).
    pub fn recent(&self, window: u64) -> &[TickRec] {
        let n = self.history.len();
        &self.history[n.saturating_sub(window as usize)..]
    }

    /// Distinct code lines executed over the last `window` ticks.
    pub fn distinct_pc_lines(&self, window: u64) -> usize {
        let recent = self.recent(window);
        let Some(first) = recent.first() else { return 0 };
        let last = recent.last().expect("non-empty");
        let mut seen: Vec<u32> = self.pc_samples[first.pc_start as usize..last.pc_end as usize].to_vec();
        seen.sort_unstable();
        seen.dedup();
        seen.len()
    }

    pub fn ram_bytes(&self, addr: u32, len: usize) -> Option<&[u8]> {
        let ram = self.bus.ram();
        let off = (addr & 0x1F_FFFF) as usize;
        // KUSEG/KSEG0/KSEG1 main-RAM aliases only.
        let seg = addr >> 29;
        if !matches!(seg, 0 | 4 | 5) {
            return None;
        }
        ram.get(off..off + len)
    }

    pub fn display_image(&self) -> crate::img::Img {
        let (rgba, w, h) = self.bus.gpu.display_rgba8_with(Deinterlace::from_env());
        crate::img::Img::from_rgba(w, h, rgba)
    }
}

pub fn button_mask(name: &str) -> u16 {
    match name.to_ascii_lowercase().as_str() {
        "cross" => button::CROSS,
        "circle" => button::CIRCLE,
        "square" => button::SQUARE,
        "triangle" => button::TRIANGLE,
        "start" => button::START,
        "select" => button::SELECT,
        "up" => button::UP,
        "down" => button::DOWN,
        "left" => button::LEFT,
        "right" => button::RIGHT,
        "l1" => button::L1,
        "r1" => button::R1,
        "l2" => button::L2,
        "r2" => button::R2,
        "l3" => button::L3,
        "r3" => button::R3,
        // Journey validation rejects unknown names before a run starts.
        _ => 0,
    }
}
