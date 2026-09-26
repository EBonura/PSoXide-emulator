//! Boot a disc through the HLE kernel and run it with the interpreter, the
//! recompiler, or both in lockstep.
//!
//! usage:
//!   jit_run bench    <disc.cue> <frames> <interp|jit> [--pulses P] [--hash-log F]
//!   jit_run lockstep <disc.cue> <frames> [--pulses P] [--full-every N]
//!
//! `bench` reports host CPU seconds and emulated frames per CPU-second.
//! `lockstep` runs two machines from the same boot: one steps the
//! interpreter, the other the recompiler. After every recompiler step the
//! interpreter catches up to the same retired-instruction count and the
//! CPU state, the cycle counter and the frame boundaries are compared; the
//! full serialized machine state is compared every N steps and at the end.

#[path = "../../emulator-core/examples/support/disc.rs"]
mod disc_support;
#[path = "../../emulator-core/examples/support/pad.rs"]
mod pad_support;

use std::path::Path;

use emulator_core::{fast_boot_disc, Bus, Cpu, EmulatorStateRef};
use psoxide_jit::Jit;

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

extern "C" {
    fn getrusage(who: i32, usage: *mut Rusage) -> i32;
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

fn boot(cue: &Path) -> (Cpu, Bus) {
    let disc = disc_support::load_disc_path(cue).unwrap_or_else(|e| panic!("{e}"));
    let mut bus = Bus::new_without_bios();
    let mut cpu = Cpu::new();
    fast_boot_disc(&mut bus, &mut cpu, &disc).unwrap_or_else(|e| panic!("boot: {e:?}"));
    bus.cdrom.insert_disc(Some(disc));
    bus.attach_digital_pad_port1();
    bus.attach_memcard_port1(Vec::new());
    (cpu, bus)
}

/// What the harness does between instructions, identically for every
/// engine: SPU catch-up and audio drain as the frontends do, and the pad
/// pulses at frame boundaries.
struct Host {
    pulses: Vec<pad_support::PadPulse>,
    mask: Option<u16>,
    base_vblank: u64,
    frame: u64,
    /// Retired-instruction count at each frame boundary.
    boundaries: Vec<u64>,
    hashes: Vec<u64>,
    /// Record the display hash at each frame boundary.
    hash: bool,
}

impl Host {
    fn new(bus: &mut Bus, pulses: &str, hash: bool) -> Self {
        let pulses = pad_support::parse_pad_pulses(pulses).expect("--pulses");
        let mut host = Self {
            pulses,
            mask: None,
            base_vblank: bus.irq().raise_counts()[0],
            frame: 0,
            boundaries: Vec::new(),
            hashes: Vec::new(),
            hash,
        };
        pad_support::sync_pad_mask(bus, 0, &host.pulses, &mut host.mask);
        host
    }

    /// Returns true on a new frame.
    fn after_step(&mut self, cpu: &Cpu, bus: &mut Bus) -> bool {
        bus.run_spu_to_current_cycle();
        if bus.spu.audio_queue_len() != 0 {
            let _ = bus.spu.drain_audio();
        }
        let vblank = bus.irq().raise_counts()[0] - self.base_vblank;
        if vblank == self.frame {
            return false;
        }
        self.frame = vblank;
        self.boundaries.push(cpu.tick());
        if self.hash {
            self.hashes.push(bus.gpu.display_hash().0);
        }
        pad_support::sync_pad_mask(bus, 0, &self.pulses, &mut self.mask);
        true
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

fn bench(cue: &Path, frames: u64, mode: &str, pulses: &str, hash_log: Option<&str>, from: u64) {
    let (mut cpu, mut bus) = boot(cue);
    let mut host = Host::new(&mut bus, pulses, hash_log.is_some());
    let mut jit = Jit::new().expect("map code buffer");
    let tier = if mode == "tier" {
        psoxide_jit::install_tier(&mut cpu)
    } else {
        None
    };
    let mut t0 = cpu_seconds();
    let mut tick0 = cpu.tick();
    let mut cycles0 = bus.cycles();
    let mut timing = from == 0;
    while host.frame < frames {
        if !timing && host.frame >= from {
            timing = true;
            t0 = cpu_seconds();
            tick0 = cpu.tick();
            cycles0 = bus.cycles();
        }
        // `run`: the interpreter's batched path, driven as the emu-bench
        // harness (emu_bench_run) and hle_compat drive it: run to the next
        // VBlank, then the per-frame host work. `step`: the plain
        // per-instruction loop. `jit`: the recompiler, which returns at
        // event boundaries.
        let result = match mode {
            "jit" => jit.run(&mut cpu, &mut bus, u64::MAX),
            "run" | "tier" => {
                let base = host.base_vblank;
                let frame = host.frame;
                cpu.run(&mut bus, 1 << 20, u64::MAX, |bus| {
                    bus.irq().raise_counts()[0] - base != frame
                })
                .1
            }
            _ => cpu.step(&mut bus),
        };
        if let Err(e) = result {
            println!("cpu_error frame={} {e}", host.frame);
            break;
        }
        host.after_step(&cpu, &mut bus);
    }
    let cpu_s = cpu_seconds() - t0;
    let stats = jit.stats();
    println!(
        "mode={mode} frames={} timed_from={from} cpu_s={cpu_s:.3} fps_per_cpu_s={:.1} instructions={} cycles={} display_hash={:016x} state_hash={:016x}",
        host.frame,
        (host.frame - from) as f64 / cpu_s,
        cpu.tick() - tick0,
        bus.cycles() - cycles0,
        bus.gpu.display_hash().0,
        fnv(&state_bytes(&cpu, &bus)),
    );
    if let Some(stats) = &tier {
        println!("tier: {:?}", *stats.lock().expect("tier stats"));
    }
    if mode == "jit" {
        let census = jit.mode_census();
        println!(
            "jit: {stats:?} code_in_use={} census(native,plain,full,delay,last)={census:?} branch_blocks={}",
            jit.code_bytes_in_use(),
            jit.branch_blocks()
        );
        let by_pc = std::env::var_os("PSOXIDE_JIT_PROFILE_PC").is_some();
        for ((reason, key), n) in jit.fallback_profile().into_iter().take(16) {
            if by_pc {
                println!(
                    "fallback reason={reason} pc={key:08x} steps={n} icache={:08x?} ram={:08x?} hook={}",
                    cpu.jit_cached_word(key),
                    bus.peek_instruction(key),
                    bus.peek_instruction(key).is_some_and(|w| emulator_core::cpu::jit_abi::is_hle_hook(key, w)),
                );
            } else {
                println!("fallback reason={reason} page={:08x} steps={n}", key << 12);
            }
        }
        let total = stats.native_instructions + stats.interpreter_steps;
        println!(
            "jit: {:.1}% of instructions retired in compiled blocks, {:.2} instructions per block run",
            100.0 * stats.native_instructions as f64 / total.max(1) as f64,
            stats.native_instructions as f64 / stats.runs.max(1) as f64
        );
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

fn lockstep(cue: &Path, frames: u64, pulses: &str, full_every: u64, engine: &str) {
    let (mut ci, mut bi) = boot(cue);
    let (mut cj, mut bj) = boot(cue);
    let mut hi = Host::new(&mut bi, pulses, true);
    let mut hj = Host::new(&mut bj, pulses, true);
    let mut jit = Jit::new().expect("map code buffer");
    let tier = (engine == "tier").then(|| psoxide_jit::install_tier(&mut cj).expect("tier"));
    let mut steps = 0u64;
    let mut full_checks = 0u64;
    let t0 = cpu_seconds();
    let mut last_good = (cj.pc(), cj.tick());
    let outcome = loop {
        if hj.frame >= frames {
            break "ok".to_string();
        }
        let pc_before = cj.pc();
        let tick_before = cj.tick();
        let result = if engine == "run" || engine == "tier" {
            // The interpreter's batched path, the way hle_compat drives it.
            let base = hj.base_vblank;
            let frame = hj.frame;
            cj.run(&mut bj, 1 << 20, u64::MAX, |bus| {
                bus.irq().raise_counts()[0] - base != frame
            })
            .1
        } else {
            jit.run(&mut cj, &mut bj, u64::MAX)
        };
        if let Err(e) = result {
            break format!("{engine} error: {e}");
        }
        hj.after_step(&cj, &mut bj);
        while ci.tick() < cj.tick() {
            if let Err(e) = ci.step(&mut bi) {
                eprintln!("interpreter error: {e}");
                break;
            }
            hi.after_step(&ci, &mut bi);
        }
        steps += 1;
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
                "MISMATCH after step {steps} (block pc {pc_before:08x}, from tick {tick_before}; last good pc {:08x} tick {})\n interp: {}\n jit:    {}\n boundaries interp {:?}\n boundaries jit    {:?}",
                last_good.0,
                last_good.1,
                describe(&ci, &bi),
                describe(&cj, &bj),
                hi.boundaries.iter().rev().take(3).collect::<Vec<_>>(),
                hj.boundaries.iter().rev().take(3).collect::<Vec<_>>(),
            );
        }
        if full_every != 0 && steps.is_multiple_of(full_every) {
            full_checks += 1;
            if state_bytes(&ci, &bi) != state_bytes(&cj, &bj) {
                break format!(
                    "FULL STATE MISMATCH at step {steps} (tick {}, block pc {pc_before:08x}); CPU matched",
                    cj.tick()
                );
            }
        }
        last_good = (cj.pc(), cj.tick());
    };
    let final_same = state_bytes(&ci, &bi) == state_bytes(&cj, &bj);
    let stats = jit.stats();
    println!(
        "lockstep {outcome}: frames={} jit_steps={steps} instructions={} full_checks={full_checks} final_state_equal={final_same} display_hash interp={:016x} jit={:016x} host_cpu_s={:.1}",
        hj.frame,
        cj.tick(),
        bi.gpu.display_hash().0,
        bj.gpu.display_hash().0,
        cpu_seconds() - t0
    );
    if let Some(tier) = &tier {
        println!("tier: {:?}", *tier.lock().expect("tier stats"));
    }
    let total = stats.native_instructions + stats.interpreter_steps;
    println!(
        "jit: {stats:?}\njit: {:.1}% of instructions retired in compiled blocks",
        100.0 * stats.native_instructions as f64 / total.max(1) as f64
    );
    if outcome != "ok" || !final_same {
        std::process::exit(1);
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
    let hash_log = take("--hash-log");
    let from: u64 = take("--from")
        .map(|v| v.parse().expect("--from"))
        .unwrap_or(0);
    let full_every: u64 = take("--full-every")
        .map(|v| v.parse().expect("--full-every"))
        .unwrap_or(20_000);
    match args.first().map(String::as_str) {
        Some("bench") => bench(
            Path::new(&args[1]),
            args[2].parse().expect("frames"),
            &args[3],
            &pulses,
            hash_log.as_deref(),
            from,
        ),
        Some("lockstep") => lockstep(
            Path::new(&args[1]),
            args[2].parse().expect("frames"),
            &pulses,
            full_every,
            "jit",
        ),
        Some("lockstep-tier") => lockstep(
            Path::new(&args[1]),
            args[2].parse().expect("frames"),
            &pulses,
            full_every,
            "tier",
        ),
        Some("lockstep-run") => lockstep(
            Path::new(&args[1]),
            args[2].parse().expect("frames"),
            &pulses,
            full_every,
            "run",
        ),
        Some("synth") => {
            // jit_run synth <interp|jit> <body> <alu%> <mem%> <branch%> <muldiv%> <instructions>
            use psoxide_jit::testgen::{machine, synthetic, Mix, Rng};
            let n = |i: usize| args[i].parse::<u64>().expect("number");
            let mix = Mix {
                alu: n(3) as u32,
                mem: n(4) as u32,
                branch: n(5) as u32,
                muldiv: n(6) as u32,
            };
            let instructions = n(7);
            let (code, handler) = synthetic(&mut Rng::new(7), n(2) as usize, mix);
            let (mut cpu, mut bus) = machine(&code, &handler);
            let mut jit = Jit::new().expect("map code buffer");
            let use_jit = args[1] == "jit";
            let t0 = cpu_seconds();
            while cpu.tick() < instructions {
                if use_jit {
                    let left = instructions - cpu.tick();
                    jit.run(&mut cpu, &mut bus, left).expect("run");
                } else {
                    cpu.step(&mut bus).expect("step");
                }
            }
            let s = cpu_seconds() - t0;
            println!(
                "synth {} {mix:?}: {} instructions in {s:.3} s = {:.2} ns/instruction, {:.1} M instructions/s, cycles={} state_hash={:016x}",
                args[1],
                cpu.tick(),
                s * 1e9 / cpu.tick() as f64,
                cpu.tick() as f64 / s / 1e6,
                bus.cycles(),
                fnv(&state_bytes(&cpu, &bus)),
            );
            if use_jit {
                println!("jit: {:?}", jit.stats());
            }
        }
        Some("fuzz-run") | Some("fuzz-tier") => {
            let tier = args[0] == "fuzz-tier";
            // jit_run fuzz-run <first seed> <seeds> <body> <gte%> <instructions>
            let n = |i: usize| args[i].parse::<u64>().expect("number");
            let (first, count, body, gte, instructions) = (n(1), n(2), n(3), n(4), n(5));
            let mut failed = 0;
            let mut total = 0;
            for seed in first..first + count {
                let mut rng = psoxide_jit::testgen::Rng::new(seed);
                let (code, handler) =
                    psoxide_jit::testgen::program(&mut rng, body as usize, gte as u32);
                match psoxide_jit::testgen::lockstep_run(
                    &code,
                    &handler,
                    instructions,
                    500,
                    seed,
                    tier,
                ) {
                    Err(e) => {
                        failed += 1;
                        println!("seed {seed}: {e}");
                    }
                    Ok(n) => total += n,
                }
            }
            println!(
                "fuzz-run: {count} programs, {failed} failed; {total} instructions in passing runs"
            );
            if failed != 0 {
                std::process::exit(1);
            }
        }
        Some("fuzz") => {
            // jit_run fuzz <first seed> <seeds> <body> <gte%> <instructions>
            let n = |i: usize| args[i].parse::<u64>().expect("number");
            let (first, count, body, gte, instructions) = (n(1), n(2), n(3), n(4), n(5));
            let mut failed = 0;
            let mut total = psoxide_jit::testgen::Coverage::default();
            for seed in first..first + count {
                let mut rng = psoxide_jit::testgen::Rng::new(seed);
                let (code, handler) =
                    psoxide_jit::testgen::program(&mut rng, body as usize, gte as u32);
                match psoxide_jit::testgen::lockstep(&code, &handler, instructions, 5_000) {
                    Err(e) => {
                        failed += 1;
                        println!("seed {seed}: {e}");
                    }
                    Ok(c) => {
                        total.instructions += c.instructions;
                        total.native += c.native;
                        total.interrupts += c.interrupts;
                        total.address_errors += c.address_errors;
                        total.overflows += c.overflows;
                    }
                }
            }
            println!("fuzz: {count} programs, {failed} failed; passing runs covered {total:?}");
            if failed != 0 {
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!("usage: jit_run bench <cue> <frames> <interp|jit> | lockstep <cue> <frames> | fuzz <seed> <n> <body> <gte%> <instructions>");
            std::process::exit(2);
        }
    }
}
