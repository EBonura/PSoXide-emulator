//! Lockstep check of the decoded-block cache against the plain interpreter.
//!
//! Boots one disc twice through the HLE kernel, one machine running from
//! decoded blocks through `Cpu::run` (batched register-only runs included)
//! and one stepping the plain interpreter with the block cache off. After
//! every `run` call (at most `--chunk` instructions, 1 by default) the PC,
//! retired count, bus cycle and registers must agree; every `--full-every`
//! instructions the complete CPU and bus state (their save-state
//! serialisation) must agree too. The first difference is reported with
//! the instruction count.
//!
//! usage: block_lockstep <disc.cue> <frames> [--chunk N] [--full-every N] [--pulses SCHEDULE]
//!
//! `--pulses` takes the harness's `mask@frame+len,...` pad schedule, applied
//! to both machines at the same VBlank.

#[path = "support/disc.rs"]
mod disc_support;

use std::path::Path;

use emulator_core::{fast_boot_disc, Bus, ButtonState, Cpu};

fn boot(path: &Path, blocks: bool) -> (Cpu, Bus) {
    let disc = disc_support::load_disc_path(path).expect("load disc");
    let mut bus = Bus::new_without_bios();
    let mut cpu = Cpu::new();
    cpu.set_block_cache_enabled(blocks);
    fast_boot_disc(&mut bus, &mut cpu, &disc).expect("boot");
    bus.cdrom.insert_disc(Some(disc));
    bus.attach_digital_pad_port1();
    bus.attach_memcard_port1(Vec::new());
    (cpu, bus)
}

fn pulses(text: &str) -> Vec<(u16, u64, u64)> {
    text.split(',')
        .filter(|p| !p.trim().is_empty())
        .map(|p| {
            let (m, rest) = p.trim().split_once('@').expect("mask@frame+len");
            let (start, len) = rest.split_once('+').unwrap_or((rest, "1"));
            let mask = u16::from_str_radix(m.trim_start_matches("0x"), 16).expect("hex mask");
            (mask, start.parse().unwrap(), len.parse().unwrap())
        })
        .collect()
}

fn digest<T: serde::Serialize>(value: &T) -> u64 {
    let bytes = postcard::to_allocvec(value).expect("serialise");
    // FNV-1a 64.
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = Path::new(&args[1]);
    let frames: u64 = args[2].parse().expect("frames");
    let mut full_every = 50_000u64;
    let mut chunk = 1u64;
    let mut schedule = Vec::new();
    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--full-every" => full_every = args[i + 1].parse().expect("N"),
            "--chunk" => chunk = args[i + 1].parse().expect("N"),
            "--pulses" => schedule = pulses(&args[i + 1]),
            other => panic!("unknown argument {other}"),
        }
        i += 2;
    }
    let (mut a, mut abus) = boot(path, true);
    let (mut b, mut bbus) = boot(path, false);
    let base = abus.irq().raise_counts()[0];
    let mut frame = 0u64;
    let mut steps = 0u64;
    let apply = |bus: &mut Bus, frame: u64| {
        let mask = schedule
            .iter()
            .filter(|(_, s, l)| frame >= *s && frame < s + l)
            .fold(0u16, |m, (mask, _, _)| m | mask);
        bus.set_port1_buttons(ButtonState::from_bits(mask));
    };
    apply(&mut abus, 0);
    apply(&mut bbus, 0);
    while frame < frames {
        let before = steps;
        let (ran, ra) = a.run(&mut abus, chunk, u64::MAX, |_| false);
        let mut rb = Ok(());
        for _ in 0..ran + u64::from(ra.is_err()) {
            rb = b.step(&mut bbus);
            if rb.is_err() {
                break;
            }
        }
        steps += ran;
        if ra.is_err() || rb.is_err() {
            if format!("{ra:?}") != format!("{rb:?}") {
                println!("DIVERGED step={steps} frame={frame}: results {ra:?} vs {rb:?}");
                std::process::exit(1);
            }
            println!("both stopped at step={steps}: {ra:?}");
            break;
        }
        if a.pc() != b.pc()
            || a.tick() != b.tick()
            || abus.cycles() != bbus.cycles()
            || a.gprs() != b.gprs()
        {
            println!(
                "DIVERGED step={steps} frame={frame}: pc {:08x}/{:08x} tick {}/{} cycles {}/{}",
                a.pc(),
                b.pc(),
                a.tick(),
                b.tick(),
                abus.cycles(),
                bbus.cycles()
            );
            for r in 0..32 {
                if a.gprs()[r] != b.gprs()[r] {
                    println!("  r{r}: {:08x} vs {:08x}", a.gprs()[r], b.gprs()[r]);
                }
            }
            std::process::exit(1);
        }
        if steps / full_every != before / full_every {
            let (ca, cb) = (digest(&a), digest(&b));
            let (ba, bb) = (digest(&abus), digest(&bbus));
            if ca != cb || ba != bb {
                println!(
                    "DIVERGED step={steps} frame={frame}: full state cpu {} bus {}",
                    if ca == cb { "same" } else { "differs" },
                    if ba == bb { "same" } else { "differs" }
                );
                std::process::exit(1);
            }
        }
        let vblank = abus.irq().raise_counts()[0] - base;
        if vblank != frame {
            frame = vblank;
            abus.run_spu_to_current_cycle();
            bbus.run_spu_to_current_cycle();
            abus.spu.discard_audio();
            bbus.spu.discard_audio();
            apply(&mut abus, frame);
            apply(&mut bbus, frame);
        }
    }
    let same = digest(&a) == digest(&b) && digest(&abus) == digest(&bbus);
    println!(
        "{} frames={frame} steps={steps} blocks_built={} final_state={}",
        if same { "OK" } else { "DIVERGED" },
        a.blocks_built(),
        if same { "same" } else { "differs" }
    );
    if !same {
        std::process::exit(1);
    }
}
