//! Check exact HLE idle skipping against plain stepping, and time it.
//!
//! usage:
//!   idle_lockstep lockstep <disc.cue> <frames> [pulses] [full-every]
//!   idle_lockstep bench <disc.cue> <frames> <from> <plain|skip> [pulses]
//!
//! `lockstep` runs two machines from the same boot: one calls `Cpu::step`,
//! the other also calls `Cpu::skip_hle_wait` before each step. After every
//! skip or step of the second, the first catches up to the same retired
//! count; CPU state, cycles and frame boundaries are compared each time
//! and the whole serialized machine every `full-every` skips and at the end.
#[path = "support/disc.rs"]
mod disc_support;
#[path = "support/pad.rs"]
mod pad_support;

use emulator_core::{fast_boot_disc, Bus, Cpu, EmulatorStateRef};
use psoxide_settings::savestate::SaveStateV1;

fn boot(path: &str) -> (Cpu, Bus) {
    let disc = disc_support::load_disc_path(std::path::Path::new(path)).unwrap();
    let mut bus = Bus::new_without_bios();
    let mut cpu = Cpu::new();
    fast_boot_disc(&mut bus, &mut cpu, &disc).unwrap();
    bus.cdrom.insert_disc(Some(disc));
    bus.attach_digital_pad_port1();
    bus.attach_memcard_port1(Vec::new());
    (cpu, bus)
}

struct Host {
    pulses: Vec<pad_support::PadPulse>,
    mask: Option<u16>,
    base: u64,
    frame: u64,
    boundaries: Vec<u64>,
}

impl Host {
    fn new(bus: &mut Bus, pulses: &str) -> Self {
        let mut h = Host {
            pulses: pad_support::parse_pad_pulses(pulses).unwrap(),
            mask: None,
            base: bus.irq().raise_counts()[0],
            frame: 0,
            boundaries: Vec::new(),
        };
        pad_support::sync_pad_mask(bus, 0, &h.pulses, &mut h.mask);
        h
    }
    fn after(&mut self, cpu: &Cpu, bus: &mut Bus) {
        bus.run_spu_to_current_cycle();
        if bus.spu.audio_queue_len() != 0 {
            let _ = bus.spu.drain_audio();
        }
        let v = bus.irq().raise_counts()[0] - self.base;
        if v != self.frame {
            self.frame = v;
            self.boundaries.push(cpu.tick());
            pad_support::sync_pad_mask(bus, 0, &self.pulses, &mut self.mask);
        }
    }
}

fn state(cpu: &Cpu, bus: &Bus) -> Vec<u8> {
    SaveStateV1::new_at(EmulatorStateRef { cpu, bus }, "idle", 0, 0)
        .to_bytes()
        .unwrap()
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
    })
}

fn cpu_seconds() -> f64 {
    #[repr(C)]
    #[derive(Default)]
    struct Tv(i64, i32, i32);
    #[repr(C)]
    #[derive(Default)]
    struct Ru(Tv, Tv, [i64; 14]);
    extern "C" {
        fn getrusage(who: i32, usage: *mut Ru) -> i32;
    }
    let mut r = Ru::default();
    // SAFETY: getrusage fills the struct it is given.
    unsafe { getrusage(0, &mut r) };
    r.0 .0 as f64 + r.0 .1 as f64 * 1e-6 + r.1 .0 as f64 + r.1 .1 as f64 * 1e-6
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a[1].as_str() {
        "lockstep" => {
            let frames: u64 = a[3].parse().unwrap();
            let pulses = a.get(4).cloned().unwrap_or_default();
            let every: u64 = a.get(5).map_or(2000, |v| v.parse().unwrap());
            let (mut ci, mut bi) = boot(&a[2]);
            let (mut cs, mut bs) = boot(&a[2]);
            let mut hi = Host::new(&mut bi, &pulses);
            let mut hs = Host::new(&mut bs, &pulses);
            let (mut skips, mut skipped, mut checks) = (0u64, 0u64, 0u64);
            while hs.frame < frames {
                let n = cs.skip_hle_wait(&mut bs, u64::MAX);
                if n == 0 {
                    cs.step(&mut bs).unwrap();
                } else {
                    skips += 1;
                    skipped += n;
                }
                hs.after(&cs, &mut bs);
                while ci.tick() < cs.tick() {
                    ci.step(&mut bi).unwrap();
                    hi.after(&ci, &mut bi);
                }
                let same = ci.tick() == cs.tick()
                    && ci.pc() == cs.pc()
                    && ci.gprs() == cs.gprs()
                    && ci.cop0() == cs.cop0()
                    && bi.cycles() == bs.cycles()
                    && hi.boundaries == hs.boundaries;
                if !same {
                    println!(
                        "MISMATCH after skip {skips} at tick {}: pc {:08x}/{:08x} cycles {}/{}",
                        cs.tick(),
                        ci.pc(),
                        cs.pc(),
                        bi.cycles(),
                        bs.cycles()
                    );
                    std::process::exit(1);
                }
                if n != 0 && skips.is_multiple_of(every) {
                    checks += 1;
                    if state(&ci, &bi) != state(&cs, &bs) {
                        println!("FULL STATE MISMATCH at skip {skips}, tick {}", cs.tick());
                        std::process::exit(1);
                    }
                }
            }
            let equal = state(&ci, &bi) == state(&cs, &bs);
            println!(
                "idle lockstep {}: frames={} instructions={} skips={skips} skipped_instructions={skipped} full_checks={checks} final_state_equal={equal} display {:016x} {:016x}",
                if equal { "ok" } else { "FAIL" },
                hs.frame,
                cs.tick(),
                bi.gpu.display_hash().0,
                bs.gpu.display_hash().0
            );
            if !equal {
                std::process::exit(1);
            }
        }
        "bench" => {
            let frames: u64 = a[3].parse().unwrap();
            let from: u64 = a[4].parse().unwrap();
            let skip = a[5] == "skip";
            let pulses = a.get(6).cloned().unwrap_or_default();
            let (mut cpu, mut bus) = boot(&a[2]);
            let mut host = Host::new(&mut bus, &pulses);
            let mut t0 = cpu_seconds();
            let mut timing = from == 0;
            let mut skipped = 0;
            let mut skipped_timed = 0;
            while host.frame < frames {
                if !timing && host.frame >= from {
                    timing = true;
                    t0 = cpu_seconds();
                    skipped_timed = skipped;
                }
                let n = if skip {
                    cpu.skip_hle_wait(&mut bus, u64::MAX)
                } else {
                    0
                };
                skipped += n;
                if n == 0 {
                    cpu.step(&mut bus).unwrap();
                }
                host.after(&cpu, &mut bus);
            }
            let s = cpu_seconds() - t0;
            println!(
                "{} frames {from}..{frames}: cpu_s={s:.3} fps_per_cpu_s={:.1} skipped={skipped} skipped_in_window={} instructions={} cycles={} display={:016x} state={:016x} frame_ticks={:016x}",
                a[5],
                (frames - from) as f64 / s,
                skipped - skipped_timed,
                cpu.tick(),
                bus.cycles(),
                bus.gpu.display_hash().0,
                fnv(&state(&cpu, &bus)),
                fnv(&host.boundaries.iter().flat_map(|b| b.to_le_bytes()).collect::<Vec<_>>()),
            );
        }
        _ => eprintln!("usage: see the file header"),
    }
}
