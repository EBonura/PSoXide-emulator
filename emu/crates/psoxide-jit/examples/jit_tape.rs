//! Replay a pad tape on a disc and cross-check `Cpu::step` (the reference)
//! against `Cpu::run` with or without the native tier, run call by run call.
//!
//! usage:
//!   jit_tape <disc.cue> <tape.txt|tape.pxtape> <run|tier> [--card FILE] [--full-every N] [--max-ticks N]
//!
//! A `.pxtape` input tape (the frontend's recorder format) drives the pad
//! by completed controller polls, or by tick for a video-frame tape, with
//! full analog state; a text tape has one four-digit hex button mask per host tick (one VBlank
//! period of cycles), the format the WipEout journey driver writes. Each
//! tick runs the CPU in chunks of at most 16384 instructions until the clock
//! passes the tick's deadline, with the SPU caught up after every chunk. The
//! reference machine steps one instruction at a time to the same instruction
//! count and is compared after every chunk: CPU registers, clock, the
//! load-delay state, and every `--full-every` chunks the whole serialized
//! machine. The first difference is reported with the tick and chunk.

#[path = "../../emulator-core/examples/support/disc.rs"]
mod disc_support;

use std::path::Path;

use emulator_core::input_tape::PadSample;
use emulator_core::{fast_boot_disc, Bus, Cpu, EmulatorStateRef};

const CHUNK: u64 = 16_384;
const STEPS_PER_TICK: u64 = 1_000_000;

fn boot(cue: &Path, card: Option<Vec<u8>>) -> (Cpu, Bus) {
    let disc = disc_support::load_disc_path(cue).unwrap_or_else(|e| panic!("{e}"));
    let mut cpu = Cpu::new();
    let mut bus = Bus::new_without_bios();
    fast_boot_disc(&mut bus, &mut cpu, &disc).unwrap_or_else(|e| panic!("boot: {e:?}"));
    bus.cdrom.insert_disc(Some(disc));
    if let Ok(Some(lbas)) = psoxide_settings::library::load_sbi_for(cue) {
        bus.cdrom.set_bad_subq_sectors(lbas);
    }
    bus.attach_digital_pad_port1();
    let _ = bus.force_port1_analog_mode();
    if let Some(c) = card {
        bus.attach_memcard_port1(c);
    }
    (cpu, bus)
}

/// The tape sample a machine should hold now, if the tape has started.
fn live_sample(
    tape: &[PadSample],
    poll_bound: Option<(bool, u64)>,
    tick: usize,
    bus: &Bus,
) -> Option<usize> {
    match poll_bound {
        Some((true, start)) => {
            let polls = bus.port1_completed_polls();
            (polls >= start).then(|| ((polls - start) as usize).min(tape.len() - 1))
        }
        _ => Some(tick.min(tape.len() - 1)),
    }
}

fn catch_up(bus: &mut Bus) {
    bus.run_spu_to_current_cycle();
    if bus.spu.audio_queue_len() != 0 {
        let _ = bus.spu.drain_audio();
    }
}

fn state_bytes(cpu: &Cpu, bus: &Bus) -> Vec<u8> {
    postcard::to_allocvec(&EmulatorStateRef { cpu, bus }).expect("serialize state")
}

fn describe(cpu: &Cpu, bus: &Bus) -> String {
    format!(
        "pc={:08x} tick={} cycles={} hi={:08x} lo={:08x} sr={:08x} cause={:08x} epc={:08x} {:?}\n gprs={:08x?}",
        cpu.pc(),
        cpu.tick(),
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

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut take = |flag: &str| -> Option<String> {
        let i = args.iter().position(|a| a == flag)?;
        let v = args.get(i + 1).cloned();
        args.drain(i..(i + 2).min(args.len()));
        v
    };
    let card = take("--card").map(|p| std::fs::read(&p).unwrap_or_else(|e| panic!("{p}: {e}")));
    let full_every: u64 = take("--full-every").map_or(50, |v| v.parse().expect("--full-every"));
    let max_ticks: u64 = take("--max-ticks").map_or(u64::MAX, |v| v.parse().expect("--max-ticks"));
    // From instruction `--refine-at` on, run in pieces of `--sub`
    // instructions, comparing after each piece, to locate a mismatch.
    let refine_at: u64 = take("--refine-at").map_or(u64::MAX, |v| v.parse().expect("--refine-at"));
    // From instruction `--full-from` on, compare the whole machine after
    // every chunk.
    let full_from: u64 = take("--full-from").map_or(u64::MAX, |v| v.parse().expect("--full-from"));
    let sub: u64 = take("--sub").map_or(CHUNK, |v| v.parse().expect("--sub"));
    let mut poll_bound: Option<(bool, u64)> = None;
    let cue = Path::new(&args[0]);
    let tape: Vec<PadSample> = if args[1].ends_with(".pxtape") {
        let t = emulator_core::input_tape::read_tape_full(Path::new(&args[1])).expect("pxtape");
        poll_bound = Some((
            t.clock == emulator_core::input_tape::TapeClock::PadPoll,
            t.start_poll,
        ));
        t.samples
    } else {
        std::fs::read_to_string(&args[1])
            .expect("tape")
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| PadSample::from_buttons(u16::from_str_radix(l.trim(), 16).expect("tape line")))
            .collect()
    };
    let tier_mode = args[2] == "tier";

    let (mut ci, mut bi) = boot(cue, card.clone());
    let (mut cj, mut bj) = boot(cue, card);
    let stats = tier_mode.then(|| psoxide_jit::install_tier(&mut cj).expect("native tier"));
    let period = bj.vblank_period().max(1);
    let mut deadline = bj.cycles().saturating_add(period);
    // A poll-bound tape runs until its samples are used up (plus a margin
    // of ticks to see the end), a tick-clocked one for its length.
    let ticks = match poll_bound {
        Some((true, _)) => max_ticks.min(u64::MAX / 2) as usize,
        _ => tape.len().min(max_ticks as usize),
    };
    let mut held_i: Option<usize> = None;
    let mut held_j: Option<usize> = None;
    let mut ticks_run = 0u64;
    let mut chunks = 0u64;
    let mut fulls = 0u64;
    let mut outcome = "ok".to_string();
    'ticks: for tick in 0..ticks {
        let mut steps = 0u64;
        loop {
            // Tick-clocked tapes change at the tick start, poll-bound ones
            // whenever the poll count has moved; both machines identically.
            for (bus, held) in [(&mut bi, &mut held_i), (&mut bj, &mut held_j)] {
                if let Some(k) = live_sample(&tape, poll_bound, tick, bus) {
                    if *held != Some(k) {
                        *held = Some(k);
                        tape[k].apply_to_bus(bus);
                    }
                }
            }
            let mut n = (STEPS_PER_TICK - steps).clamp(1, CHUNK);
            if cj.tick() >= refine_at {
                n = n.min(sub);
            }
            let prev = (cj.pc(), bj.cycles(), *cj.gprs(), cj.tick());
            let (ran, result) = cj.run(&mut bj, n, deadline, |_| false);
            steps += ran;
            catch_up(&mut bj);
            if let Err(e) = result {
                outcome = format!("tick {tick}: {args2} error {e:?}", args2 = args[2]);
                break 'ticks;
            }
            while ci.tick() < cj.tick() {
                if let Err(e) = ci.step(&mut bi) {
                    outcome = format!("tick {tick}: step error {e:?}");
                    break 'ticks;
                }
            }
            catch_up(&mut bi);
            chunks += 1;
            let same = ci.tick() == cj.tick()
                && ci.pc() == cj.pc()
                && ci.gprs() == cj.gprs()
                && ci.hi() == cj.hi()
                && ci.lo() == cj.lo()
                && ci.cop0() == cj.cop0()
                && bi.cycles() == bj.cycles()
                && ci.jit_debug_state() == cj.jit_debug_state()
                && bi.jit_fetch_state() == bj.jit_fetch_state();
            if !same {
                let word = {
                    let off = (prev.0 & 0x1f_ffff) as usize;
                    u32::from_le_bytes(bi.ram()[off..off + 4].try_into().unwrap())
                };
                outcome = format!(
                    "MISMATCH tick {tick} chunk {chunks} (ran {ran}); chunk began at pc={:08x} word={word:08x} cycles={} gprs={:08x?}\n  step: {}\n  {}: {}",
                    prev.0,
                    prev.1,
                    prev.2,
                    describe(&ci, &bi),
                    args[2],
                    describe(&cj, &bj)
                );
                break 'ticks;
            }
            if cj.tick() >= full_from || (full_every != 0 && chunks.is_multiple_of(full_every)) {
                fulls += 1;
                if state_bytes(&ci, &bi) != state_bytes(&cj, &bj) {
                    outcome = format!(
                        "FULL STATE MISMATCH tick {tick} chunk {chunks} (CPU matched, tick {}; chunk began at tick {} pc={:08x} word={:08x}, code {}): {}",
                        cj.tick(),
                        prev.3,
                        prev.0,
                        u32::from_le_bytes(bi.ram()[(prev.0 & 0x1F_FFFF) as usize..][..4].try_into().unwrap()),
                        {
                            let at = (prev.0 & 0x1F_FFFF) as usize;
                            (0..12)
                                .map(|k| format!("{:08x}", u32::from_le_bytes(bi.ram()[at + 4 * k..][..4].try_into().unwrap())))
                                .collect::<Vec<_>>()
                                .join(" ")
                        },
                        psoxide_jit::testgen::state_diff(&ci, &bi, &cj, &bj)
                    );
                    break 'ticks;
                }
            }
            if bj.cycles() >= deadline || steps >= STEPS_PER_TICK {
                break;
            }
            if ran == 0 {
                outcome = format!("tick {tick}: no progress");
                break 'ticks;
            }
        }
        deadline = bj.cycles().saturating_add(period);
        ticks_run = tick as u64 + 1;
        if let Some((true, start)) = poll_bound {
            if bj.port1_completed_polls() >= start + tape.len() as u64 {
                break;
            }
        }
    }
    let final_same = state_bytes(&ci, &bi) == state_bytes(&cj, &bj);
    println!(
        "tape-lockstep {outcome}: mode={} ticks={} chunks={chunks} full_checks={fulls} instructions={} final_state_equal={final_same} display_hash step={:016x} {}={:016x}",
        args[2],
        ticks_run,
        cj.tick(),
        bi.gpu.display_hash().0,
        args[2],
        bj.gpu.display_hash().0
    );
    if let Some(stats) = &stats {
        println!("tier: {:?}", *stats.lock().expect("tier stats"));
    }
    if outcome != "ok" || !final_same {
        std::process::exit(1);
    }
}
