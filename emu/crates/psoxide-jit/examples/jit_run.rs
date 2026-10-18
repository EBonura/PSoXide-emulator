//! Boot a disc through the HLE kernel and time or cross-check the ways of
//! running the CPU: `step` (plain `Cpu::step`), `run` (the batched
//! `Cpu::run` over the decoded-block cache) and `tier` (`Cpu::run` with the
//! native tier installed).
//!
//! usage:
//!   jit_run bench    <disc.cue> <frames> <step|run|tier> [--from N] [--pulses P | --tape T] [--hash-log F]
//!   jit_run lockstep <disc.cue> <frames> <run|tier> [--pulses P] [--full-every N]
//!   jit_run fuzz     <run|tier> <first seed> <seeds> <body> <gte%> <instructions>
//!   jit_run synth    <step|run|tier> <body> <alu%> <mem%> <branch%> <muldiv%> <instructions>
//!
//! `bench` runs frame by frame as the emu-bench harness does (to the next
//! VBlank, then the pad pulses and the SPU catch-up) and reports frames per
//! host CPU-second over frames `from..frames`, and per billion host cycles
//! (the macOS per-process cycle and instruction counters, which other load on
//! the machine moves far less than CPU time). `--tape` drives the pad from a
//! `.pxtape` or text tape instead of pulses. `lockstep` runs a second
//! machine with plain `Cpu::step` beside it, compares CPU state, cycles and
//! frame boundaries after every VBlank and the whole serialized machine
//! every `--full-every` run calls and at the end. `fuzz` does the same on
//! random programs (`testgen`); `synth` times a CPU-only random loop.

#[path = "../../emulator-core/examples/support/disc.rs"]
mod disc_support;
#[path = "../../emulator-core/examples/support/pad.rs"]
mod pad_support;

use std::path::Path;

use emulator_core::input_tape::PadSample;
use emulator_core::{fast_boot_disc, Bus, Cpu, EmulatorStateRef};

#[repr(C)]
#[derive(Default)]
struct Timeval {
    sec: i64,
    usec: i32,
    _pad: i32,
}

#[repr(C)]
#[derive(Default)]
struct Rusage {
    utime: Timeval,
    stime: Timeval,
    rest: [i64; 14],
}

/// `rusage_info_v4` from `<sys/resource.h>`: a uuid then 40 u64 counters.
#[repr(C)]
struct RusageInfoV4 {
    uuid: [u8; 16],
    counters: [u64; 40],
}

extern "C" {
    fn getrusage(who: i32, usage: *mut Rusage) -> i32;
    fn getpid() -> i32;
    fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut RusageInfoV4) -> i32;
}

/// Host instructions and cycles retired by this process so far, all threads
/// (zeros where the kernel does not report them).
fn host_counters() -> (u64, u64) {
    let mut info = RusageInfoV4 {
        uuid: [0; 16],
        counters: [0; 40],
    };
    // SAFETY: the kernel fills the buffer for flavour 4 (RUSAGE_INFO_V4).
    if unsafe { proc_pid_rusage(getpid(), 4, &mut info) } != 0 {
        return (0, 0);
    }
    // user_time .. diskio_byteswritten are 18 counters, then 7 QoS times,
    // billed and serviced system time, logical_writes and the lifetime max
    // footprint: instructions is the 30th counter, cycles the 31st.
    (info.counters[29], info.counters[30])
}

fn cpu_seconds() -> f64 {
    let mut ru = Rusage::default();
    // SAFETY: getrusage fills the struct it is given.
    unsafe { getrusage(0, &mut ru) };
    ru.utime.sec as f64
        + ru.utime.usec as f64 * 1e-6
        + ru.stime.sec as f64
        + ru.stime.usec as f64 * 1e-6
}

/// Boot a disc through the HLE kernel. A tape run also forces the analog pad
/// mode and loads the disc's SBI sidecar, as `jit_tape` does.
fn boot(cue: &Path, tape: bool) -> (Cpu, Bus) {
    let disc = disc_support::load_disc_path(cue).unwrap_or_else(|e| panic!("{e}"));
    let mut bus = Bus::new_without_bios();
    let mut cpu = Cpu::new();
    fast_boot_disc(&mut bus, &mut cpu, &disc).unwrap_or_else(|e| panic!("boot: {e:?}"));
    bus.cdrom.insert_disc(Some(disc));
    if tape {
        if let Ok(Some(lbas)) = psoxide_settings::library::load_sbi_for(cue) {
            bus.cdrom.set_bad_subq_sectors(lbas);
        }
    }
    bus.attach_digital_pad_port1();
    if tape {
        let _ = bus.force_port1_analog_mode();
    }
    bus.attach_memcard_port1(Vec::new());
    (cpu, bus)
}

/// A pad tape driving port 1 in place of pulses: a `.pxtape` (by completed
/// controller polls, or by video frame) or a text file with one four-digit
/// hex button mask per frame.
struct TapeDrive {
    samples: Vec<PadSample>,
    poll_bound: bool,
    start_poll: u64,
    held: Option<usize>,
}

impl TapeDrive {
    fn load(path: &str) -> Self {
        if path.ends_with(".pxtape") {
            let t = emulator_core::input_tape::read_tape_full(Path::new(path)).expect("pxtape");
            Self {
                poll_bound: t.clock == emulator_core::input_tape::TapeClock::PadPoll,
                start_poll: t.start_poll,
                samples: t.samples,
                held: None,
            }
        } else {
            let samples = std::fs::read_to_string(path)
                .expect("tape")
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| {
                    PadSample::from_buttons(u16::from_str_radix(l.trim(), 16).expect("tape line"))
                })
                .collect();
            Self {
                samples,
                poll_bound: false,
                start_poll: 0,
                held: None,
            }
        }
    }

    /// Apply the sample the machine should hold now, if it changed.
    fn apply(&mut self, bus: &mut Bus, frame: u64) {
        let last = self.samples.len() - 1;
        let index = if self.poll_bound {
            let polls = bus.port1_completed_polls();
            (polls >= self.start_poll).then(|| ((polls - self.start_poll) as usize).min(last))
        } else {
            Some((frame as usize).min(last))
        };
        if let Some(k) = index {
            if self.held != Some(k) {
                self.held = Some(k);
                self.samples[k].apply_to_bus(bus);
            }
        }
    }
}

/// What the harness does at frame boundaries, identically for every mode:
/// SPU catch-up and audio drain as the frontends do, and the pad pulses.
struct Host {
    pulses: Vec<pad_support::PadPulse>,
    tape: Option<TapeDrive>,
    mask: Option<u16>,
    base_vblank: u64,
    frame: u64,
    /// Retired-instruction count at each frame boundary.
    boundaries: Vec<u64>,
    /// Display hash at each frame boundary.
    hashes: Vec<u64>,
    /// Whether to hash the display at frame boundaries (lockstep and
    /// `--hash-log`; the plain timing run leaves it out, as the frontends do).
    record_hashes: bool,
}

impl Host {
    fn new(bus: &mut Bus, pulses: &str) -> Self {
        let pulses = pad_support::parse_pad_pulses(pulses).expect("--pulses");
        let mut host = Self {
            pulses,
            tape: None,
            mask: None,
            base_vblank: bus.irq().raise_counts()[0],
            frame: 0,
            boundaries: Vec::new(),
            hashes: Vec::new(),
            record_hashes: true,
        };
        pad_support::sync_pad_mask(bus, 0, &host.pulses, &mut host.mask);
        host
    }

    /// Run the CPU up to the next VBlank (one step in `step` mode).
    fn advance(&mut self, cpu: &mut Cpu, bus: &mut Bus, mode: &str) -> Result<(), String> {
        let polls = bus.port1_completed_polls();
        let mut watch_polls = false;
        if let Some(tape) = &mut self.tape {
            tape.apply(bus, self.frame);
            watch_polls = tape.poll_bound;
        }
        if mode == "step" {
            return cpu.step(bus).map_err(|e| e.to_string());
        }
        let base = self.base_vblank;
        let frame = self.frame;
        cpu.run(bus, 1 << 20, u64::MAX, |bus| {
            bus.irq().raise_counts()[0] - base != frame
                || (watch_polls && bus.port1_completed_polls() != polls)
        })
        .1
        .map_err(|e| e.to_string())
    }

    fn after(&mut self, cpu: &Cpu, bus: &mut Bus) {
        bus.run_spu_to_current_cycle();
        if bus.spu.audio_queue_len() != 0 {
            let _ = bus.spu.drain_audio();
        }
        let vblank = bus.irq().raise_counts()[0] - self.base_vblank;
        if vblank == self.frame {
            return;
        }
        self.frame = vblank;
        self.boundaries.push(cpu.tick());
        if self.record_hashes {
            self.hashes.push(bus.gpu.display_hash().0);
        }
        pad_support::sync_pad_mask(bus, 0, &self.pulses, &mut self.mask);
    }
}

fn state_bytes(cpu: &Cpu, bus: &Bus) -> Vec<u8> {
    postcard::to_allocvec(&EmulatorStateRef { cpu, bus }).expect("serialize state")
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
    })
}

fn install(
    cpu: &mut Cpu,
    mode: &str,
) -> Option<std::sync::Arc<std::sync::Mutex<psoxide_jit::tier::TierStats>>> {
    (mode == "tier").then(|| psoxide_jit::install_tier(cpu).expect("native tier"))
}

fn bench(
    cue: &Path,
    frames: u64,
    mode: &str,
    pulses: &str,
    tape: Option<&str>,
    hash_log: Option<&str>,
    from: u64,
) {
    let (mut cpu, mut bus) = boot(cue, tape.is_some());
    let mut host = Host::new(&mut bus, pulses);
    host.tape = tape.map(TapeDrive::load);
    host.record_hashes = hash_log.is_some();
    let tier = install(&mut cpu, mode);
    let mut t0 = cpu_seconds();
    let mut hc0 = host_counters();
    let mut tick0 = cpu.tick();
    let mut cycles0 = bus.cycles();
    let mut timing = from == 0;
    while host.frame < frames {
        if !timing && host.frame >= from {
            timing = true;
            hc0 = host_counters();
            t0 = cpu_seconds();
            tick0 = cpu.tick();
            cycles0 = bus.cycles();
        }
        if let Err(e) = host.advance(&mut cpu, &mut bus, mode) {
            println!("cpu_error frame={} {e}", host.frame);
            break;
        }
        host.after(&cpu, &mut bus);
    }
    let cpu_s = cpu_seconds() - t0;
    let hc1 = host_counters();
    let (host_insn, host_cycles) = (hc1.0 - hc0.0, hc1.1 - hc0.1);
    let timed_frames = host.frame - from;
    println!(
        "mode={mode} frames={} timed_from={from} cpu_s={cpu_s:.3} fps_per_cpu_s={:.1} host_ginsn={:.4} host_gcycles={:.4} fps_per_gcycle={:.2} instructions={} cycles={} display_hash={:016x} state_hash={:016x}",
        host.frame,
        timed_frames as f64 / cpu_s,
        host_insn as f64 * 1e-9,
        host_cycles as f64 * 1e-9,
        timed_frames as f64 / (host_cycles.max(1) as f64 * 1e-9),
        cpu.tick() - tick0,
        bus.cycles() - cycles0,
        bus.gpu.display_hash().0,
        fnv(&state_bytes(&cpu, &bus)),
    );
    if let Some(stats) = &tier {
        println!("tier: {:?}", *stats.lock().expect("tier stats"));
    }
    if let Some(path) = hash_log {
        let mut out = String::new();
        for (i, (h, b)) in host.hashes.iter().zip(&host.boundaries).enumerate() {
            out.push_str(&format!("{} {h:016x} {b}\n", i + 1));
        }
        std::fs::write(path, out).expect("write hash log");
    }
}

fn describe(cpu: &Cpu, bus: &Bus) -> String {
    format!(
        "pc={:08x} cycles={} hi={:08x} lo={:08x} sr={:08x} cause={:08x} epc={:08x} {:?}\n gprs={:08x?}",
        cpu.pc(),
        bus.cycles(),
        cpu.hi(),
        cpu.lo(),
        cpu.cop0()[12],
        cpu.cop0()[13],
        cpu.cop0()[14],
        cpu.jit_debug_state(),
        cpu.gprs()
    )
}

fn lockstep(cue: &Path, frames: u64, mode: &str, pulses: &str, full_every: u64) {
    let (mut ci, mut bi) = boot(cue, false);
    let (mut cj, mut bj) = boot(cue, false);
    let mut hi = Host::new(&mut bi, pulses);
    let mut hj = Host::new(&mut bj, pulses);
    let tier = install(&mut cj, mode);
    let mut calls = 0u64;
    let mut full_checks = 0u64;
    let t0 = cpu_seconds();
    let mut last_good = (cj.pc(), cj.tick());
    let outcome = loop {
        if hj.frame >= frames {
            break "ok".to_string();
        }
        if let Err(e) = hj.advance(&mut cj, &mut bj, mode) {
            break format!("{mode} error: {e}");
        }
        hj.after(&cj, &mut bj);
        while ci.tick() < cj.tick() {
            if let Err(e) = ci.step(&mut bi) {
                eprintln!("interpreter error: {e}");
                break;
            }
            hi.after(&ci, &mut bi);
        }
        calls += 1;
        let same = ci.tick() == cj.tick()
            && ci.pc() == cj.pc()
            && ci.gprs() == cj.gprs()
            && ci.hi() == cj.hi()
            && ci.lo() == cj.lo()
            && ci.cop0() == cj.cop0()
            && bi.cycles() == bj.cycles()
            && ci.jit_debug_state() == cj.jit_debug_state()
            && hi.boundaries == hj.boundaries
            && hi.hashes == hj.hashes;
        if !same {
            break format!(
                "MISMATCH after call {calls} (last good pc {:08x} tick {})\n  step: {}\n  {mode}: {}",
                last_good.0,
                last_good.1,
                describe(&ci, &bi),
                describe(&cj, &bj),
            );
        }
        if full_every != 0 && calls.is_multiple_of(full_every) {
            full_checks += 1;
            if state_bytes(&ci, &bi) != state_bytes(&cj, &bj) {
                break format!(
                    "FULL STATE MISMATCH at call {calls} (tick {}); CPU matched",
                    cj.tick()
                );
            }
        }
        last_good = (cj.pc(), cj.tick());
    };
    let final_same = state_bytes(&ci, &bi) == state_bytes(&cj, &bj);
    println!(
        "lockstep {outcome}: mode={mode} frames={} instructions={} full_checks={full_checks} final_state_equal={final_same} display_hash step={:016x} {mode}={:016x} host_cpu_s={:.1}",
        hj.frame,
        cj.tick(),
        bi.gpu.display_hash().0,
        bj.gpu.display_hash().0,
        cpu_seconds() - t0
    );
    if let Some(stats) = &tier {
        println!("tier: {:?}", *stats.lock().expect("tier stats"));
    }
    if outcome != "ok" || !final_same {
        std::process::exit(1);
    }
}

fn fuzz(mode: &str, first: u64, count: u64, body: usize, gte: u32, instructions: u64) {
    let mut failed = 0;
    let mut total = 0;
    for seed in first..first + count {
        let mut rng = psoxide_jit::testgen::Rng::new(seed);
        let (code, handler) = psoxide_jit::testgen::program(&mut rng, body, gte);
        match psoxide_jit::testgen::lockstep(
            &code,
            &handler,
            instructions,
            500,
            seed,
            mode == "tier",
        ) {
            Err(e) => {
                failed += 1;
                println!("seed {seed}: {e}");
            }
            Ok(n) => total += n,
        }
    }
    println!(
        "fuzz {mode}: {count} programs, {failed} failed; {total} instructions in passing runs"
    );
    if failed != 0 {
        std::process::exit(1);
    }
}

fn synth(mode: &str, body: usize, mix: psoxide_jit::testgen::Mix, instructions: u64) {
    use psoxide_jit::testgen::{machine, synthetic, Rng};
    let (code, handler) = synthetic(&mut Rng::new(7), body, mix);
    let (mut cpu, mut bus) = machine(&code, &handler);
    let tier = install(&mut cpu, mode);
    let t0 = cpu_seconds();
    while cpu.tick() < instructions {
        let left = instructions - cpu.tick();
        if mode == "step" {
            cpu.step(&mut bus).expect("step");
        } else {
            cpu.run(&mut bus, left, u64::MAX, |_| false).1.expect("run");
        }
    }
    let s = cpu_seconds() - t0;
    println!(
        "synth {mode} {mix:?}: {} instructions in {s:.3} s = {:.2} ns/instruction, {:.1} M instructions/s, cycles={} state_hash={:016x}",
        cpu.tick(),
        s * 1e9 / cpu.tick() as f64,
        cpu.tick() as f64 / s / 1e6,
        bus.cycles(),
        fnv(&state_bytes(&cpu, &bus)),
    );
    if let Some(stats) = &tier {
        println!("tier: {:?}", *stats.lock().expect("tier stats"));
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut take = |flag: &str| -> Option<String> {
        let i = args.iter().position(|a| a == flag)?;
        let v = args.get(i + 1).cloned();
        args.drain(i..(i + 2).min(args.len()));
        v
    };
    let pulses = take("--pulses").unwrap_or_default();
    let tape = take("--tape");
    let hash_log = take("--hash-log");
    let from: u64 = take("--from")
        .map(|v| v.parse().expect("--from"))
        .unwrap_or(0);
    let full_every: u64 = take("--full-every")
        .map(|v| v.parse().expect("--full-every"))
        .unwrap_or(100);
    let n = |i: usize| args[i].parse::<u64>().expect("number");
    match args.first().map(String::as_str) {
        Some("bench") => bench(
            Path::new(&args[1]),
            n(2),
            &args[3],
            &pulses,
            tape.as_deref(),
            hash_log.as_deref(),
            from,
        ),
        Some("lockstep") => lockstep(Path::new(&args[1]), n(2), &args[3], &pulses, full_every),
        Some("fuzz") => fuzz(&args[1], n(2), n(3), n(4) as usize, n(5) as u32, n(6)),
        Some("synth") => synth(
            &args[1],
            n(2) as usize,
            psoxide_jit::testgen::Mix {
                alu: n(3) as u32,
                mem: n(4) as u32,
                branch: n(5) as u32,
                muldiv: n(6) as u32,
            },
            n(7),
        ),
        _ => {
            eprintln!("usage: see the header of examples/jit_run.rs");
            std::process::exit(2);
        }
    }
}
