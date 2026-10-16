//! Game boots through the native tier must leave the machine the interpreter
//! leaves, cycle for cycle: lockstep against plain `Cpu::step`, comparing the
//! CPU, the clock and the load-delay state after every run call and the whole
//! serialized machine along the way.
//!
//! The discs are the developer's library (`PSOXIDE_GAMES_DIR`, default
//! `~/Downloads/ps1 games`); a disc that is not there is skipped with a note.
//! WipEout (a GPU DMA streams from RAM while the game reads it) and Crash
//! Bandicoot both diverged within the first few hundred frames when compiled
//! RAM loads ignored that DMA.
#![cfg(target_arch = "aarch64")]

#[path = "../../emulator-core/examples/support/disc.rs"]
mod disc_support;

use std::path::PathBuf;

use emulator_core::{fast_boot_disc, Bus, Cpu, EmulatorStateRef};

fn games_dir() -> PathBuf {
    std::env::var_os("PSOXIDE_GAMES_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Downloads/ps1 games")))
        .unwrap_or_default()
}

fn boot(cue: &std::path::Path) -> (Cpu, Bus) {
    let disc = disc_support::load_disc_path(cue).unwrap_or_else(|e| panic!("{e}"));
    let mut bus = Bus::new_without_bios();
    let mut cpu = Cpu::new();
    fast_boot_disc(&mut bus, &mut cpu, &disc).unwrap_or_else(|e| panic!("boot: {e:?}"));
    bus.cdrom.insert_disc(Some(disc));
    bus.attach_digital_pad_port1();
    (cpu, bus)
}

fn state(cpu: &Cpu, bus: &Bus) -> Vec<u8> {
    postcard::to_allocvec(&EmulatorStateRef { cpu, bus }).expect("serialize state")
}

fn same_cpu(a: (&Cpu, &Bus), b: (&Cpu, &Bus)) -> bool {
    let ((ca, ba), (cb, bb)) = (a, b);
    ca.tick() == cb.tick()
        && ca.pc() == cb.pc()
        && ca.gprs() == cb.gprs()
        && ca.hi() == cb.hi()
        && ca.lo() == cb.lo()
        && ca.cop0() == cb.cop0()
        && ba.cycles() == bb.cycles()
        && ca.jit_debug_state() == cb.jit_debug_state()
        && ba.jit_fetch_state() == bb.jit_fetch_state()
}

/// Run `frames` VBlanks of the disc on the tier, the reference stepping
/// beside it; `Err` describes the first difference.
fn lockstep_disc(cue: &std::path::Path, frames: u64) -> Result<(), String> {
    let (mut cr, mut br) = boot(cue);
    let (mut ci, mut bi) = boot(cue);
    psoxide_jit::install_tier(&mut cr).ok_or("no native tier")?;
    let base = br.irq().raise_counts()[0];
    for frame in 0..frames {
        let (_, result) = cr.run(&mut br, 1 << 20, u64::MAX, |bus| {
            bus.irq().raise_counts()[0] - base != frame
        });
        result.map_err(|e| format!("tier error in frame {frame}: {e}"))?;
        br.run_spu_to_current_cycle();
        let _ = br.spu.drain_audio();
        while ci.tick() < cr.tick() {
            ci.step(&mut bi)
                .map_err(|e| format!("step error in frame {frame}: {e}"))?;
        }
        bi.run_spu_to_current_cycle();
        let _ = bi.spu.drain_audio();
        if !same_cpu((&ci, &bi), (&cr, &br)) {
            return Err(format!(
                "frame {frame}: step pc={:08x} cycles={}, tier pc={:08x} cycles={} (instruction {})",
                ci.pc(),
                bi.cycles(),
                cr.pc(),
                br.cycles(),
                cr.tick()
            ));
        }
        if frame % 25 == 24 && state(&ci, &bi) != state(&cr, &br) {
            return Err(format!("frame {frame}: serialized machines differ"));
        }
    }
    if state(&ci, &bi) != state(&cr, &br) {
        return Err("final serialized machines differ".into());
    }
    Ok(())
}

fn check(cue: &str, frames: u64) {
    let path = games_dir().join(cue);
    if !path.exists() {
        eprintln!("skipping {cue}: {} not found", path.display());
        return;
    }
    if let Err(e) = lockstep_disc(&path, frames) {
        panic!("{cue}: {e}");
    }
}

#[test]
fn wipeout_boot() {
    check("WipEout PSoXide.cue", 150);
}

#[test]
fn crash_bandicoot_boot() {
    check("Crash Bandicoot (USA)/Crash Bandicoot (USA).cue", 400);
}

#[test]
fn half_life_boot() {
    check("Half-Life (hl-psx).cue", 300);
}

#[test]
fn voxide_boot() {
    check("VoXide/VoXide.cue", 300);
}
