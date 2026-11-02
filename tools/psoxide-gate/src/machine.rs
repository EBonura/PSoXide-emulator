//! The emulated console driven tick by tick, with the per-tick history the
//! window asserts (presenting, stuck PC, audio) are evaluated against.
//!
//! A tick is the same unit the headless frontend's route clock uses: one
//! vblank period of emulated cycles, capped at one million instructions. Input
//! set between ticks is therefore frame-for-frame what `launch --press` does.

use std::path::Path;

use emulator_core::{button, fast_boot_disc, Bus, ButtonState, Cpu, Deinterlace};

use crate::journey::{MemcardMode, Pad2Mode, PadMode};

/// Return the sample live at a completed port-1 poll. A tape holds its final
/// sample until the step observes exhaustion and releases both pads.
pub(crate) fn tape_sample(
    tape: &emulator_core::input_tape::Tape,
    polls: u64,
) -> Option<emulator_core::input_tape::PadSample> {
    let index = usize::try_from(polls.checked_sub(tape.start_poll)?).unwrap_or(usize::MAX);
    tape.samples
        .get(index)
        .copied()
        .or_else(|| tape.samples.last().copied())
}

pub(crate) fn tape_end(tape: &emulator_core::input_tape::Tape) -> Result<u64, String> {
    let count = u64::try_from(tape.samples.len()).map_err(|_| "tape sample count overflow")?;
    tape.start_poll
        .checked_add(count)
        .ok_or_else(|| "tape end poll overflow".into())
}

/// Polls port 1 has started: command bytes 0x42 the pad has taken. The count moves when the
/// pad answers the command byte, before it has sampled a button.
fn poll_starts(bus: &Bus) -> u64 {
    bus.port1_pad_command_histogram()
        .map_or(0, |h| u64::from(h[0x42]))
}

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

/// One port's pad: button bits plus the `[x, y]` left and right sticks.
#[derive(Clone, Copy, Debug)]
struct PadSet {
    mask: u16,
    left: [u8; 2],
    right: [u8; 2],
}

/// A tick `advance` has started and not finished.
#[derive(Clone, Copy, Debug)]
struct OpenTick {
    pc_start: u32,
    steps: u64,
    audio_peak: u32,
}

/// `--audit-pad`: judges every completed port-1 poll. The game's button word has to be the wire
/// form of one pad state the gate had set while the poll ran; a word that is none of them took its
/// d-pad byte from one state and its face-button byte from another (a torn read).
#[derive(Debug)]
pub struct PadAudit {
    /// Completed polls judged.
    pub polls: u64,
    /// Polls whose button word matched no single pad state.
    pub torn: u64,
    /// The first torn poll: (tick, word the game read, wire words of the states it could have read).
    pub first_torn: Option<(u64, u16, Vec<u16>)>,
    /// Polls completed when the last judgement was made.
    seen: u64,
    /// Wire words of the pad states set since the last completed poll, plus the state it started in.
    states: Vec<u16>,
}

impl Default for PadAudit {
    fn default() -> PadAudit {
        PadAudit {
            polls: 0,
            torn: 0,
            first_torn: None,
            seen: 0,
            // A freshly plugged pad has nothing pressed.
            states: vec![!0],
        }
    }
}

impl PadAudit {
    /// A pad state (button mask) takes effect now.
    fn set(&mut self, mask: u16) {
        let wire = !mask;
        if self.states.last() != Some(&wire) {
            self.states.push(wire);
        }
    }

    /// Judge the polls completed since the last call; `read` is the button word of the newest.
    fn judge(&mut self, completed: u64, read: Option<u16>, tick: u64) {
        let new = completed.saturating_sub(self.seen);
        self.seen = completed;
        if new == 0 {
            return;
        }
        self.polls += new;
        if let Some(word) = read {
            if !self.states.contains(&word) {
                self.torn += 1;
                self.first_torn
                    .get_or_insert_with(|| (tick, word, self.states.clone()));
            }
        }
        let last = self.states.last().copied();
        self.states.clear();
        self.states.extend(last);
    }
}

/// What `Machine::advance` stopped on.
#[derive(Clone, Copy, Debug)]
pub struct Advance {
    /// The tick ended (its record is in `history`).
    pub tick_done: bool,
    /// A port-1 poll started and the machine is stopped right behind its command byte. Always
    /// false unless the caller asked to stop at polls.
    pub at_poll: bool,
}

pub struct Machine {
    pub cpu: Cpu,
    pub bus: Bus,
    pub ticks: u64,
    pub history: Vec<TickRec>,
    /// Sampled code lines (pc >> 4), per tick via `TickRec`.
    pub pc_samples: Vec<u32>,
    pub card_observer: Option<crate::card::Observer>,
    pub cut: Option<(u8, usize, u32)>,
    pub cut_hit: Option<String>,
    pub tape: Option<emulator_core::input_tape::Tape>,
    pub irq_injection: Option<(u8, usize, u32, u64)>,
    pub injected: Option<(u64, u64)>,
    pub irq_ack_seen: Option<u64>,
    pub irq_missed: Option<String>,
    /// Per-tick CSV, when `--tick-log` asked for one.
    pub tick_log: Option<crate::tick_log::TickLog>,
    /// Torn-read audit of the pad polls, when `--audit-pad` asked for one.
    pub pad_audit: Option<PadAudit>,
    /// Pad changes waiting for the next port-1 poll to start (see `set_pad_at_poll`).
    pending_pads: [Option<PadSet>; 2],
    /// The port-1 poll start count last looked at, to notice a poll starting.
    seen_starts: u64,
    /// The machine stopped on a poll start and has run nothing since: the command byte is
    /// exchanged and no button byte has been, so the pad can still be changed whole.
    at_poll: bool,
    /// The tick in progress, when `advance` returned before it ended.
    open_tick: Option<OpenTick>,
    deadline: u64,
    period: u64,
    last_display: (u16, u16),
    track_display: bool,
    /// The displayed picture at the previous tick: its size and bytes (see
    /// `Gpu::display_bytes_into`); `None` size is a display not yet configured.
    last_shape: Option<(u16, u16)>,
    last_bytes: Vec<u8>,
    /// Scratch for the current tick's displayed bytes, swapped with `last_bytes`.
    scratch_bytes: Vec<u8>,
}

pub struct Boot<'a> {
    pub disc: &'a Path,
    pub pad: PadMode,
    pub pad2: Pad2Mode,
    pub memcard: MemcardMode,
    /// Arm the GP0 command log (needed by the hardware renderer).
    pub cmd_log: bool,
    /// Hash the display every tick so `TickRec::changed` is real. Off, `changed` stays false
    /// (only the `presenting` check reads it).
    pub track_display: bool,
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
            other => {
                return Err(format!(
                    "unsupported disc extension .{other}: {}",
                    opts.disc.display()
                ))
            }
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

        Ok(Machine::new(cpu, bus, opts.track_display))
    }

    /// A machine around a CPU and bus that are ready to run, its first tick starting now.
    fn new(cpu: Cpu, bus: Bus, track_display: bool) -> Machine {
        let period = bus.vblank_period().max(1);
        let deadline = bus.cycles().saturating_add(period);
        let area = bus.gpu.display_area();
        Machine {
            cpu,
            bus,
            ticks: 0,
            history: Vec::new(),
            pc_samples: Vec::new(),
            card_observer: None,
            cut: None,
            cut_hit: None,
            tape: None,
            irq_injection: None,
            injected: None,
            irq_ack_seen: None,
            irq_missed: None,
            tick_log: None,
            pad_audit: None,
            pending_pads: [None; 2],
            seen_starts: 0,
            at_poll: false,
            open_tick: None,
            deadline,
            period,
            last_display: (area.x, area.y),
            track_display,
            last_shape: None,
            last_bytes: Vec::new(),
            scratch_bytes: Vec::new(),
        }
    }

    /// Set one port's pad for the next tick: face/shoulder/d-pad bits plus sticks as
    /// `(left, right)` `[x, y]` pairs.
    pub fn set_pad(&mut self, port: u8, mask: u16, left: [u8; 2], right: [u8; 2]) {
        if let (Some(audit), 1) = (self.pad_audit.as_mut(), port) {
            audit.set(mask);
        }
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

    /// Set one port's pad so that it reaches the game whole, with the next poll that starts
    /// (the one it is in, when the machine is stopped on a poll start). The change waits for
    /// the command byte of the next port-1 poll, which comes before any button byte, so no
    /// poll reads half of the old pad and half of the new one. `set_pad` writes the pad right
    /// away, which can land between the bytes of a poll (the d-pad byte before it, the
    /// face-button byte after it).
    pub fn set_pad_at_poll(&mut self, port: u8, mask: u16, left: [u8; 2], right: [u8; 2]) {
        let set = PadSet { mask, left, right };
        if self.at_poll {
            self.set_pad(port, set.mask, set.left, set.right);
        } else {
            self.pending_pads[usize::from(port == 2)] = Some(set);
        }
    }

    /// Both pads released and centred, at the next poll start (see `set_pad_at_poll`).
    pub fn release_pads_at_poll(&mut self) {
        for port in [1, 2] {
            self.set_pad_at_poll(port, 0, Self::centre(), Self::centre());
        }
    }

    /// The machine is stopped on a port-1 poll start with nothing run since.
    pub fn is_at_poll(&self) -> bool {
        self.at_poll
    }

    /// A tick has started and `advance` has not run it to its end yet.
    pub fn tick_open(&self) -> bool {
        self.open_tick.is_some()
    }

    fn apply_pending_pads(&mut self) {
        for (i, slot) in self.pending_pads.into_iter().enumerate() {
            if let Some(p) = slot {
                self.set_pad(i as u8 + 1, p.mask, p.left, p.right);
            }
        }
        self.pending_pads = [None; 2];
    }

    /// Run one tick (the rest of it, when `advance` left one open). Returns an error if the
    /// guest hits an execution fault.
    pub fn tick(&mut self) -> Result<(), String> {
        self.advance(false).map(|_| ())
    }

    /// Run until the tick ends, or, with `stop_at_poll`, until a port-1 poll starts if that
    /// comes first. A poll starts when the pad has taken the command byte (0x42); the stop is
    /// on the instruction that sent it, before any button byte is exchanged. The game's state
    /// is then what its last simulation tick left, which is what the poll's tick is about to
    /// act on, so a caller that reads it and sets the pad here reacts with no tick of delay,
    /// and the pad it sets is the one this very poll reads, whole. A later call goes on with
    /// the same tick; the tick's length, history row, tick log row and the instructions run do
    /// not depend on where it was split. A poll that starts in the last instruction of a tick
    /// is reported with the tick's end. Queued pad changes (`set_pad_at_poll`) are applied
    /// whenever a poll starts.
    pub fn advance(&mut self, stop_at_poll: bool) -> Result<Advance, String> {
        let mut open = self.open_tick.take().unwrap_or(OpenTick {
            pc_start: self.pc_samples.len() as u32,
            steps: 0,
            audio_peak: 0,
        });
        let mut at_poll;
        self.at_poll = false;
        loop {
            let polls = self.bus.port1_completed_polls();
            let starts = poll_starts(&self.bus);
            if let Some(tape) = &self.tape {
                if let Some(sample) = tape_sample(tape, polls) {
                    sample.apply_to_bus(&mut self.bus);
                    if let Some(audit) = self.pad_audit.as_mut() {
                        audit.set(sample.buttons);
                    }
                }
            }
            let observing =
                self.card_observer.is_some() || self.cut.is_some() || self.irq_injection.is_some();
            let fine = observing
                && (crate::card::active(&self.bus)
                    || self.bus.sio0().debug_awaiting_ack()
                    || self.card_observer.as_ref().is_some_and(|o| o.in_irq()));
            let n = (STEPS_PER_TICK - open.steps)
                .min(if fine { 1 } else { PC_CHUNK })
                .max(1);
            let watch_tape = self.tape.is_some() || self.pad_audit.is_some();
            let per_poll_log = self.tick_log.as_ref().is_some_and(|l| l.per_poll);
            let watch_starts =
                stop_at_poll || per_poll_log || self.pending_pads.iter().any(Option::is_some);
            let (ran, result) = if fine {
                (1, self.cpu.step(&mut self.bus))
            } else {
                self.cpu.run(&mut self.bus, n, self.deadline, |bus| {
                    (observing && crate::card::active(bus))
                        || (watch_tape && bus.port1_completed_polls() != polls)
                        || (watch_starts && poll_starts(bus) != starts)
                })
            };
            if let Some(observer) = &mut self.card_observer {
                observer.sample(&self.cpu, &self.bus);
            }
            if let Some((command, index, occurrence, lead)) = self.irq_injection {
                let (cmd, _, accepted) = crate::card::progress(&self.bus);
                let count = self
                    .bus
                    .port1_memcard_command_histogram()
                    .map(|h| h[usize::from(command)])
                    .unwrap_or(0);
                if cmd == command && accepted == index && count == occurrence {
                    if let Some(ack) = self.bus.sio0().debug_ack_deadline() {
                        let now = self.bus.cycles();
                        let target = ack.saturating_sub(lead);
                        if self.irq_ack_seen != Some(ack) {
                            self.irq_ack_seen = Some(ack);
                            if now > target {
                                self.irq_missed = Some(format!("requested IRQ cycle {target} preceded observable ACK schedule at {now}, ACK {ack}"));
                                self.irq_injection = None;
                            }
                        }
                        if self.irq_injection.is_some() && now >= target && now < ack {
                            self.bus
                                .irq_mut()
                                .raise(emulator_core::irq::IrqSource::VBlank);
                            self.injected = Some((now, ack));
                            self.irq_injection = None;
                            if let Some(observer) = &mut self.card_observer {
                                observer.target_ack = Some(ack);
                                observer.target_observed = false;
                                observer.target_overlap = None;
                            }
                        }
                    }
                }
            }
            open.steps += ran;
            self.pc_samples.push(self.cpu.pc() >> 4);
            self.bus.run_spu_to_current_cycle();
            if self.bus.spu.audio_queue_len() != 0 {
                for (l, r) in self.bus.spu.drain_audio() {
                    let peak = u32::from(l.unsigned_abs()).max(u32::from(r.unsigned_abs()));
                    open.audio_peak = open.audio_peak.max(peak);
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
            // A poll that started in the run just made is behind the stop, since the run stops
            // on the instruction that started it (when poll starts are watched).
            if let Some(audit) = self.pad_audit.as_mut() {
                let read = self
                    .bus
                    .port1_recent_polls()
                    .last()
                    .map(|p| u16::from_le_bytes([p.rx[2], p.rx[3]]));
                audit.judge(self.bus.port1_completed_polls(), read, self.ticks);
            }
            let started = poll_starts(&self.bus) != self.seen_starts;
            if started {
                self.seen_starts = poll_starts(&self.bus);
                self.apply_pending_pads();
                if let Some(log) = self.tick_log.as_mut().filter(|l| l.per_poll) {
                    log.record(self.ticks, &mut self.bus, self.cpu.tick(), false);
                }
            }
            at_poll = started && watch_starts;
            if let Some((command, byte_index, occurrence)) = self.cut {
                let (cmd, frame, index) = crate::card::progress(&self.bus);
                let count = self
                    .bus
                    .port1_memcard_command_histogram()
                    .map(|h| h[usize::from(command)])
                    .unwrap_or(0);
                if cmd == command && index == byte_index && count == occurrence {
                    self.cut_hit = Some(format!("power cut at cycle {}, command {cmd:#04x}, frame {frame}, accepted bytes {index}, command occurrence {count}", self.bus.cycles()));
                    self.cut = None;
                    break;
                }
            }
            if self.bus.cycles() >= self.deadline || open.steps >= STEPS_PER_TICK {
                break;
            }
            if ran == 0 {
                return Err(format!(
                    "cpu made no progress at tick {} pc={:#010x}",
                    self.ticks,
                    self.cpu.pc()
                ));
            }
            if at_poll && stop_at_poll {
                self.at_poll = true;
                self.open_tick = Some(open);
                return Ok(Advance {
                    tick_done: false,
                    at_poll: true,
                });
            }
        }
        self.at_poll = at_poll;
        self.deadline = self.bus.cycles().saturating_add(self.period);
        self.ticks += 1;

        let area = self.bus.gpu.display_area();
        let start = (area.x, area.y);
        let flipped = start != self.last_display;
        self.last_display = start;
        // Whether the picture differs from the previous tick's. Comparing the pixels
        // themselves is the same test as comparing a hash of them without the
        // collision, and cheaper than hashing: a copy and a compare against the last
        // tick's, where the serial hash was about 6% of a gate run and the word-wise
        // fingerprint after it 1 to 1.4%.
        let changed = if self.track_display {
            let shape = self.bus.gpu.display_bytes_into(&mut self.scratch_bytes);
            let changed = shape != self.last_shape || self.scratch_bytes != self.last_bytes;
            self.last_shape = shape;
            std::mem::swap(&mut self.last_bytes, &mut self.scratch_bytes);
            changed
        } else {
            false
        };
        if let Some(log) = self.tick_log.as_mut().filter(|l| !l.per_poll) {
            log.record(self.ticks, &mut self.bus, self.cpu.tick(), flipped);
        }
        self.history.push(TickRec {
            flipped,
            changed,
            audio_peak: open.audio_peak,
            pc_start: open.pc_start,
            pc_end: self.pc_samples.len() as u32,
        });
        Ok(Advance {
            tick_done: true,
            at_poll,
        })
    }

    /// The last `window` ticks of history (fewer early in a run).
    pub fn recent(&self, window: u64) -> &[TickRec] {
        let n = self.history.len();
        &self.history[n.saturating_sub(window as usize)..]
    }

    /// Distinct code lines executed over the last `window` ticks.
    pub fn distinct_pc_lines(&self, window: u64) -> usize {
        let recent = self.recent(window);
        let Some(first) = recent.first() else {
            return 0;
        };
        let last = recent.last().expect("non-empty");
        let mut seen: Vec<u32> =
            self.pc_samples[first.pc_start as usize..last.pc_end as usize].to_vec();
        seen.sort_unstable();
        seen.dedup();
        seen.len()
    }

    pub fn ram_bytes(&self, addr: u32, len: usize) -> Option<&[u8]> {
        let ram = self.bus.ram();
        let off = (addr & 0x1f_ffff) as usize;
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

impl crate::expr::Memory for Machine {
    fn load(&self, addr: u32, width: u8) -> Option<u32> {
        let b = self.ram_bytes(addr, usize::from(width))?;
        Some(b.iter().rev().fold(0u32, |v, x| v << 8 | u32::from(*x)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOAD: u32 = 0x8001_0000;
    /// Where the guest stores the two button bytes of each poll, two bytes a poll.
    const RING: u32 = 0x8003_0000;

    const ZERO: u32 = 0;
    const T0: u32 = 8;
    const T1: u32 = 9;
    const T2: u32 = 10;
    const T3: u32 = 11;
    const S0: u32 = 16;
    const S1: u32 = 17;

    fn lui(rt: u32, imm: u32) -> u32 {
        0x3c00_0000 | rt << 16 | imm
    }
    fn ori(rt: u32, rs: u32, imm: u32) -> u32 {
        0x3400_0000 | rs << 21 | rt << 16 | imm
    }
    fn addiu(rt: u32, rs: u32, imm: i32) -> u32 {
        0x2400_0000 | rs << 21 | rt << 16 | (imm as u32 & 0xffff)
    }
    fn sb(rt: u32, base: u32, off: u32) -> u32 {
        0xa000_0000 | base << 21 | rt << 16 | off
    }
    fn sh(rt: u32, base: u32, off: u32) -> u32 {
        0xa400_0000 | base << 21 | rt << 16 | off
    }
    fn lbu(rt: u32, base: u32, off: u32) -> u32 {
        0x9000_0000 | base << 21 | rt << 16 | off
    }
    fn lw(rt: u32, base: u32, off: u32) -> u32 {
        0x8c00_0000 | base << 21 | rt << 16 | off
    }
    fn andi(rt: u32, rs: u32, imm: u32) -> u32 {
        0x3000_0000 | rs << 21 | rt << 16 | imm
    }
    fn beq(rs: u32, rt: u32, words: i32) -> u32 {
        0x1000_0000 | rs << 21 | rt << 16 | (words as u32 & 0xffff)
    }
    fn bne(rs: u32, rt: u32, words: i32) -> u32 {
        0x1400_0000 | rs << 21 | rt << 16 | (words as u32 & 0xffff)
    }
    fn jump(target: u32) -> u32 {
        0x0800_0000 | (target & 0x0fff_ffff) >> 2
    }

    /// A guest that does nothing but poll a digital pad on port 1 the way a game's pad
    /// handler does, byte by byte with a wait for each transfer, and stores the two button
    /// bytes of every poll in `RING`.
    fn polling_guest() -> Vec<u32> {
        let mut code = vec![lui(S0, 0x1f80), ori(S0, S0, 0x1040), lui(S1, 0x8003)];
        let top = LOAD + 4 * code.len() as u32;
        code.extend([addiu(T0, ZERO, 2), sh(T0, S0, 0x0a)]);
        for (i, byte) in [0x01, 0x42, 0x00, 0x00, 0x00].into_iter().enumerate() {
            // Send the byte, wait for the reply to reach the receive FIFO (SIO0_STAT bit 1).
            code.extend([
                addiu(T1, ZERO, byte),
                sb(T1, S0, 0),
                lw(T2, S0, 4),
                0, // load delay slot
                andi(T2, T2, 2),
                beq(T2, ZERO, -4),
                0,
                lbu(T3, S0, 0),
                0, // load delay slot
            ]);
            match i {
                3 => code.push(sb(T3, S1, 0)),
                4 => code.push(sb(T3, S1, 1)),
                _ => {}
            }
        }
        code.extend([
            sh(ZERO, S0, 0x0a),
            addiu(S1, S1, 2),
            ori(T2, ZERO, 40),
            addiu(T2, T2, -1),
            bne(T2, ZERO, -2),
            0,
            jump(top),
            0,
        ]);
        code
    }

    fn machine_with(program: &[u32]) -> Machine {
        let mut bus = Bus::new_without_bios();
        bus.attach_original_digital_pad_port1();
        for (i, word) in program.iter().enumerate() {
            bus.write32(LOAD + 4 * i as u32, *word);
        }
        let mut cpu = Cpu::new();
        cpu.seed_from_exe_with_args(LOAD, 0, Some(0x801f_f000), 0, 0);
        Machine::new(cpu, bus, false)
    }

    /// Every completed poll's button word as the wire carried it.
    fn polled_words(m: &Machine) -> Vec<u16> {
        let ram = m.ram_bytes(RING, 0x8000).expect("ring in RAM");
        // The last completed poll may not be stored yet: the guest stores after the poll ends.
        let polls = (m.bus.port1_completed_polls() as usize).saturating_sub(1);
        (0..polls.min(ram.len() / 2 - 1))
            .map(|n| u16::from_le_bytes([ram[2 * n], ram[2 * n + 1]]))
            .collect()
    }

    /// The pad flips every tick between a d-pad press (first button byte) and a face-button
    /// press (second byte). A poll that reads half of one and half of the other is torn.
    fn flip_run(set: impl Fn(&mut Machine, u16)) -> (Vec<u16>, usize) {
        let (a, b) = (button::UP, button::SQUARE);
        let mut m = machine_with(&polling_guest());
        for tick in 0..200 {
            set(&mut m, if tick % 2 == 0 { a } else { b });
            m.tick().expect("tick");
        }
        let words = polled_words(&m);
        let ok = [!0u16, !a, !b];
        let torn = words.iter().filter(|w| !ok.contains(w)).count();
        (words, torn)
    }

    #[test]
    fn the_audit_counts_the_torn_polls_a_synchronous_pad_does_not_have() {
        let run = |sync: bool| {
            let mut m = machine_with(&polling_guest());
            m.pad_audit = Some(PadAudit::default());
            for tick in 0..200 {
                let mask = if tick % 2 == 0 {
                    button::UP
                } else {
                    button::SQUARE
                };
                if sync {
                    m.set_pad_at_poll(1, mask, Machine::centre(), Machine::centre());
                } else {
                    m.set_pad(1, mask, Machine::centre(), Machine::centre());
                }
                m.tick().expect("tick");
            }
            m.pad_audit.take().expect("audit")
        };
        let loose = run(false);
        let latched = run(true);
        assert!(loose.polls > 1000 && latched.polls > 1000);
        assert!(
            loose.torn > 0,
            "the audit saw no torn poll in {}",
            loose.polls
        );
        assert_eq!(latched.torn, 0);
        assert!(loose.first_torn.is_some());
    }

    #[test]
    fn a_per_poll_tick_log_has_a_row_for_every_poll_start() {
        let dir = std::env::temp_dir().join(format!("poll-log-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("polls.csv");
        let symbols = crate::symbols::Symbols::default();
        let mut log = crate::tick_log::TickLog::create(&path, &[], &symbols).unwrap();
        log.per_poll = true;
        let mut m = machine_with(&polling_guest());
        m.tick_log = Some(log);
        for _ in 0..5 {
            m.tick().unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let rows: Vec<&str> = text.lines().skip(1).collect();
        // The poll in progress when the last tick ends has started and not completed.
        let started = m.bus.port1_completed_polls() as usize;
        assert!(
            rows.len() == started || rows.len() == started + 1,
            "{} rows, {started} polls",
            rows.len()
        );
        assert!(rows.len() > 100);
        // Column 8 is the completed-poll count, so the rows count up by one.
        let polls: Vec<u64> = rows
            .iter()
            .map(|r| r.split(',').nth(7).unwrap().parse().unwrap())
            .collect();
        assert!(polls.windows(2).all(|w| w[1] == w[0] + 1), "{polls:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pad_written_between_ticks_can_be_read_torn() {
        let (words, torn) =
            flip_run(|m, mask| m.set_pad(1, mask, Machine::centre(), Machine::centre()));
        assert!(words.len() > 400, "the guest polled {} times", words.len());
        assert!(
            torn > 0,
            "no torn poll in {} polls: the test cannot tell the fix",
            words.len()
        );
    }

    #[test]
    fn a_pad_set_at_poll_is_never_read_torn() {
        let (words, torn) =
            flip_run(|m, mask| m.set_pad_at_poll(1, mask, Machine::centre(), Machine::centre()));
        assert_eq!(torn, 0, "{torn} of {} polls were torn", words.len());
        assert!(
            words.contains(&!button::UP) && words.contains(&!button::SQUARE),
            "both pads reached the game"
        );
    }

    #[test]
    fn stopping_at_polls_does_not_change_the_run() {
        let mut whole = machine_with(&polling_guest());
        let mut split = machine_with(&polling_guest());
        let mut stops = 0;
        for _ in 0..30 {
            whole.tick().expect("tick");
            loop {
                let step = split.advance(true).expect("advance");
                if step.at_poll {
                    stops += 1;
                    assert!(split.is_at_poll());
                }
                if step.tick_done {
                    break;
                }
                assert!(split.tick_open());
            }
        }
        assert!(stops > 100, "only {stops} stops");
        assert_eq!(whole.ticks, split.ticks);
        assert_eq!(whole.bus.cycles(), split.bus.cycles());
        assert_eq!(whole.cpu.tick(), split.cpu.tick());
        assert_eq!(
            whole.bus.port1_completed_polls(),
            split.bus.port1_completed_polls()
        );
        assert_eq!(polled_words(&whole), polled_words(&split));
    }

    #[test]
    fn a_pad_set_on_a_stop_is_read_by_that_poll() {
        let mut m = machine_with(&polling_guest());
        let (a, b) = (button::UP, button::SQUARE);
        let mut set = Vec::new();
        while set.len() < 60 {
            let step = m.advance(true).expect("advance");
            if step.at_poll {
                let mask = if set.len() % 2 == 0 { a } else { b };
                m.set_pad_at_poll(1, mask, Machine::centre(), Machine::centre());
                set.push(mask);
            }
        }
        // The stop is on the poll's command byte, before it reads a button: poll n reads
        // exactly what was set on its own stop, whole.
        let words = polled_words(&m);
        assert!(words.len() >= 58);
        for (n, word) in words.iter().enumerate() {
            assert_eq!(*word, !set[n], "poll {n}");
        }
    }
}
