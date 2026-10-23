//! Batches that run through SPU sample deadlines.
//!
//! The SPU makes a sample every 768 clocks. A batch used to stop before
//! each one so that the step after it produced the sample; now it runs
//! through the deadline when no SPU interrupt can latch, and the samples are
//! produced late, at the next branch boundary or before the next device
//! access. These programs play SPU voices and CD audio while looking at the
//! SPU and the CD-ROM controller in every way a guest can: reading the
//! voices' live volumes, the end flags and the status register, writing
//! volumes and key on/off, and changing the CD-ROM audio attenuation that
//! the samples mix. They run through `Cpu::run` (plain and with the native
//! tier) in lockstep with `Cpu::step`, whole machine compared (the SPUs
//! caught up to their clocks first, as anything that looks at them does).
//! The IRQ variants arm the SPU interrupt on the capture buffer, where the
//! samples must keep arriving on time.

use emulator_core::Bus;
use psoxide_jit::testgen::{lockstep_with, Rng, CODE_BASE};

const fn i(op: u32, rs: u32, rt: u32, imm: u32) -> u32 {
    op << 26 | rs << 21 | rt << 16 | (imm & 0xffff)
}
const fn r(funct: u32, rs: u32, rt: u32, rd: u32, sa: u32) -> u32 {
    rs << 21 | rt << 16 | rd << 11 | sa << 6 | funct
}
const NOP: u32 = 0;
const T0: u32 = 8;
const T1: u32 = 9;
const T2: u32 = 10;
const T3: u32 = 11;
const T4: u32 = 12;
/// 0x1f80_1c00, the SPU's voices.
const SPU: u32 = 16;
/// 0x1f80_1800, the CD-ROM controller.
const CD: u32 = 17;
/// 0x1f80_1000, the I/O registers.
const IO: u32 = 18;
/// RAM data area.
const RAM: u32 = 19;
/// A running sum of what the program has read from devices.
const ACC: u32 = 20;

const SPUCNT: u32 = 0x1f80_1daa;
const IRQ_ADDR: u32 = 0x1f80_1da4;
const TRANSFER_ADDR: u32 = 0x1f80_1da6;

/// What the SPU is set up to do before the program starts.
#[derive(Clone, Copy)]
struct Scene {
    /// Play CD audio through the SPU (from a disc in the drive).
    cd_audio: bool,
    /// Arm the SPU interrupt on a capture-buffer address and let the
    /// guest take it.
    irq: bool,
}

fn spu_write(bus: &mut Bus, addr: u32, value: u16) {
    bus.write16(addr, value);
}

/// Four looping voices at different pitches over audible ADPCM, the main
/// volume up, the SPU unmuted.
fn start_voices(bus: &mut Bus, cd_audio: bool) {
    // One 16-byte block per voice, flagged "end and repeat" so it loops.
    let mut words = Vec::new();
    for v in 0..4u16 {
        let mut block = [0u8; 16];
        block[0] = 0x04 + v as u8; // shift 4+v, filter 0
        block[1] = 0x03;
        for (k, b) in block[2..].iter_mut().enumerate() {
            *b = (0x13u8)
                .wrapping_mul(k as u8 + 1)
                .wrapping_add(0x29 * v as u8);
        }
        for pair in block.chunks_exact(2) {
            words.push(u16::from(pair[0]) | u16::from(pair[1]) << 8);
        }
    }
    spu_write(bus, SPUCNT, 0xc000);
    spu_write(bus, TRANSFER_ADDR, 0x1000 >> 3);
    bus.spu.dma_write(&words);
    spu_write(bus, 0x1f80_1d80, 0x3fff);
    spu_write(bus, 0x1f80_1d82, 0x3fff);
    for v in 0..4u32 {
        let base = 0x1f80_1c00 + 16 * v;
        spu_write(bus, base, 0x2000 + 0x400 * v as u16);
        spu_write(bus, base + 2, 0x3000 - 0x300 * v as u16);
        spu_write(bus, base + 4, 0x0300 + 0x0170 * v as u16);
        spu_write(bus, base + 6, ((0x1000 + 16 * v) >> 3) as u16);
        spu_write(bus, base + 8, 0x00ff);
        spu_write(bus, base + 10, 0x1fc0);
        spu_write(bus, base + 14, ((0x1000 + 16 * v) >> 3) as u16);
    }
    if cd_audio {
        spu_write(bus, 0x1f80_1db0, 0x4000);
        spu_write(bus, 0x1f80_1db2, 0x4000);
        spu_write(bus, SPUCNT, 0xc001);
    }
    spu_write(bus, 0x1f80_1d88, 0x000f);
}

/// A drive with an audio disc playing: the controller's audio is mixed by
/// the SPU as the samples are produced.
fn start_cd_audio(bus: &mut Bus) {
    use psx_iso::{Disc, Track, TrackType};
    const SECTORS: u32 = 400;
    let mut bytes = Vec::new();
    for n in 0..SECTORS * 588 {
        let left = ((n * 37) % 20000) as i16 - 10000;
        let right = ((n * 91) % 16000) as i16 - 8000;
        bytes.extend_from_slice(&left.to_le_bytes());
        bytes.extend_from_slice(&right.to_le_bytes());
    }
    let disc = Disc::from_tracks(vec![Track {
        number: 1,
        track_type: TrackType::Audio,
        start_lba: 0,
        sector_count: SECTORS,
        pregap: 0,
        file_pregap: 0,
        source: Box::new(bytes),
    }]);
    bus.cdrom.insert_disc(Some(disc));
    let cd = |bus: &mut Bus, index: u8, reg: u32, value: u8| {
        bus.write8(0x1f80_1800, index);
        bus.write8(0x1f80_1800 + reg, value);
    };
    // Let the drive work, acknowledging its interrupts as a driver would.
    let settle = |bus: &mut Bus| {
        for k in 0..300_000 {
            bus.tick(100);
            bus.drain_scheduler_events_post_op();
            if k % 50 == 49 {
                cd(bus, 1, 3, 0x1f);
                bus.write32(0x1f80_1070, 0);
            }
        }
    };
    // Mode: CD-DA on. Then play track 1.
    cd(bus, 0, 2, 0x01);
    cd(bus, 0, 1, 0x0e);
    settle(bus);
    cd(bus, 0, 2, 0x01);
    cd(bus, 0, 1, 0x03);
    settle(bus);
}

/// The guest program: a loop of snippets drawn from `seed`.
fn program(seed: u64, scene: Scene) -> Vec<u32> {
    let mut rng = Rng::new(seed);
    let mut code = vec![
        i(0x0f, 0, SPU, 0x1f80),
        i(0x0d, SPU, SPU, 0x1c00),
        i(0x0f, 0, CD, 0x1f80),
        i(0x0d, CD, CD, 0x1800),
        i(0x0f, 0, IO, 0x1f80),
        i(0x0d, IO, IO, 0x1000),
        i(0x0f, 0, RAM, 0x8010),
        i(0x09, 0, T0, if scene.irq { 0x0200 } else { 0 }),
        i(0x2b, IO, T0, 0x1074), // I_MASK: the SPU's bit
        i(0x0f, 0, T0, 0x4000),
        i(0x0d, T0, T0, 0x0401),
        0x4088_6000, // mtc0 t0, sr
        i(0x09, 0, T1, 1),
        i(0x09, 0, T2, 3),
    ];
    let loop_start = code.len();
    let mut length = 0;
    while length < 120 {
        let mut snippet = snippet(&mut rng, scene);
        // Some of them behind a branch, for blocks of every length.
        if rng.chance(35) {
            code.extend([
                i(0x0c, ACC, T0, 1 << rng.below(4)),
                i(0x04, T0, 0, snippet.len() as u32 + 1), // beq t0, r0, past
                NOP,
            ]);
            length += 3;
        }
        length += snippet.len();
        code.append(&mut snippet);
    }
    let back = -((code.len() - loop_start) as i32) - 1;
    code.push(i(0x04, 0, 0, back as u32)); // b loop
    code.push(NOP);
    code
}

/// A run of ALU instructions of random length: time passing between the
/// guest's looks at the devices.
fn filler(rng: &mut Rng) -> Vec<u32> {
    let n = if rng.chance(20) {
        1 + rng.below(40)
    } else {
        1 + rng.below(6)
    };
    let mut out = Vec::new();
    if n > 8 {
        // A counted loop, a block that runs for a while.
        out.push(i(0x09, 0, T4, n * 3));
        out.push(r(0x21, T1, T2, T3, 0));
        out.push(r(0x26, T3, T2, T1, 0));
        out.push(i(0x09, T4, T4, 0xffff));
        out.push(i(0x05, T4, 0, 0xfffc)); // bne t4, r0, loop
        out.push(r(0x21, T3, T1, T2, 0));
    } else {
        for k in 0..n {
            out.push(match k % 3 {
                0 => r(0x21, T1, T2, T3, 0),
                1 => r(0x26, T3, T2, T1, 0),
                _ => i(0x09, T2, T2, 1 + k),
            });
        }
    }
    out
}

/// One snippet of the program.
fn snippet(rng: &mut Rng, scene: Scene) -> Vec<u32> {
    let mut out = filler(rng);
    let voice = rng.below(4);
    match rng.below(if scene.cd_audio { 10 } else { 7 }) {
        // Live voice volume, end flags, status: the SPU's state as of now.
        0 => out.extend([
            i(0x25, SPU, T0, 0x0c + 16 * voice),
            r(0x21, ACC, T0, ACC, 0),
            i(0x2b, RAM, ACC, 4 * rng.below(64)),
        ]),
        1 => out.extend([
            i(0x25, SPU, T0, 0x19c),
            r(0x21, ACC, T0, ACC, 0),
            i(0x2b, RAM, ACC, 4 * rng.below(64)),
        ]),
        2 => out.extend([
            i(0x25, SPU, T0, 0x1ae),
            r(0x21, ACC, T0, ACC, 0),
            i(0x2b, RAM, ACC, 4 * rng.below(64)),
        ]),
        // A voice's volume set from what the guest has seen.
        3 => out.extend([i(0x0c, ACC, T0, 0x3fff), i(0x29, SPU, T0, 16 * voice)]),
        // Key on and off.
        4 => out.extend([
            i(0x0c, ACC, T0, 0x000f),
            i(0x29, SPU, T0, if rng.chance(50) { 0x188 } else { 0x18c }),
        ]),
        // The current volume as the guest reads it twice in a row.
        5 => out.extend([
            i(0x25, SPU, T0, 0x0c + 16 * voice),
            i(0x25, SPU, T3, 0x0c + 16 * voice),
            r(0x23, T0, T3, ACC, 0),
        ]),
        // Plain RAM traffic, loads with their stalls.
        6 => out.extend([
            i(0x23, RAM, T0, 4 * rng.below(64)),
            r(0x21, ACC, T0, ACC, 0),
            i(0x2b, RAM, ACC, 4 * rng.below(64)),
        ]),
        // CD-ROM audio attenuation, committed.
        7 | 8 => out.extend([
            i(0x0c, ACC, T0, 0x7f),
            i(0x0d, T0, T0, 0x20),
            i(0x09, 0, T4, 2),
            i(0x28, CD, T4, 0),
            i(0x28, CD, T0, 2),
            i(0x28, CD, T0, 3),
            i(0x09, 0, T4, 3),
            i(0x28, CD, T4, 0),
            i(0x28, CD, T0, 2),
            i(0x09, 0, T4, 0x20),
            i(0x28, CD, T4, 3),
            i(0x09, 0, T4, 0),
            i(0x28, CD, T4, 0),
        ]),
        // The main volume of the SPU, written mid-sample.
        _ => out.extend([i(0x0c, ACC, T0, 0x3fff), i(0x29, SPU, T0, 0x180)]),
    }
    if scene.irq && rng.chance(50) {
        // Arm the interrupt again: clearing the bit acknowledges it.
        out.extend([
            i(0x09, 0, T0, 0xc000),
            i(0x0f, 0, T4, 0x1f80),
            i(0x29, T4, T0, 0x1daa),
            i(0x09, 0, T0, 0xc040),
            i(0x29, T4, T0, 0x1daa),
        ]);
    }
    out
}

/// The exception handler: acknowledge the interrupt unit, return.
fn handler() -> Vec<u32> {
    vec![
        i(0x0f, 0, 27, 0x1f80), // lui k1, 0x1f80
        i(0x2b, 27, 0, 0x1070), // sw r0, 0x1070(k1)
        0x401a_7000,            // mfc0 k0, epc
        NOP,
        r(0x08, 26, 0, 0, 0), // jr k0
        0x4200_0010,          // rfe
    ]
}

fn run(scene: Scene, seed: u64, instructions: u64) {
    let code = program(seed, scene);
    let handler = handler();
    for tier in [false, true] {
        if tier && !cfg!(target_arch = "aarch64") {
            continue;
        }
        let setup = |bus: &mut Bus| {
            if scene.cd_audio {
                start_cd_audio(bus);
            }
            start_voices(bus, scene.cd_audio);
            if scene.irq {
                // The capture buffer's CD-L bank is written by every sample.
                spu_write(bus, IRQ_ADDR, 0x0040 >> 3);
                spu_write(bus, SPUCNT, 0xc040 | u16::from(scene.cd_audio));
            }
        };
        match lockstep_with(&code, &handler, instructions, 7, seed, tier, &setup) {
            Ok(_) => {}
            Err(e) => panic!(
                "cd audio {} irq {} seed {seed} tier {tier} (code at {CODE_BASE:#x}): {e}",
                scene.cd_audio, scene.irq
            ),
        }
    }
}

#[test]
fn voices_alone_stay_exact_through_sample_deadlines() {
    for seed in 1..=6 {
        run(
            Scene {
                cd_audio: false,
                irq: false,
            },
            seed,
            300_000,
        );
    }
}

#[test]
fn cd_audio_attenuation_writes_see_the_samples_due_before_them() {
    for seed in 1..=6 {
        run(
            Scene {
                cd_audio: true,
                irq: false,
            },
            seed,
            300_000,
        );
    }
}

#[test]
fn an_armed_spu_interrupt_arrives_on_time() {
    for seed in 1..=6 {
        run(
            Scene {
                cd_audio: false,
                irq: true,
            },
            seed,
            300_000,
        );
    }
}

#[test]
fn an_armed_spu_interrupt_with_cd_audio() {
    for seed in 1..=3 {
        run(
            Scene {
                cd_audio: true,
                irq: true,
            },
            seed,
            300_000,
        );
    }
}

/// `Cpu::run` stops on the step after which `stop_after` holds; counting an
/// interrupt source's raises is what it is meant for, so a batch must not
/// run through the sample that raises the SPU interrupt.
#[test]
fn run_stops_on_the_step_that_raises_the_spu_interrupt() {
    let scene = Scene {
        cd_audio: false,
        irq: true,
    };
    for tier in [false, true] {
        if tier && !cfg!(target_arch = "aarch64") {
            continue;
        }
        for seed in 1..=3 {
            let code = program(seed, scene);
            let (mut ci, mut bi) = psoxide_jit::testgen::machine(&code, &handler());
            let (mut cr, mut br) = psoxide_jit::testgen::machine(&code, &handler());
            if tier {
                psoxide_jit::install_tier(&mut cr).expect("native tier");
            }
            for bus in [&mut bi, &mut br] {
                start_voices(bus, false);
                spu_write(bus, IRQ_ADDR, 0x0040 >> 3);
                spu_write(bus, SPUCNT, 0xc040);
            }
            let mut stops = 0;
            while stops < 6 {
                let seen = br.irq().raise_counts()[9];
                let (_, result) = cr.run(&mut br, 1 << 22, u64::MAX, |bus| {
                    bus.irq().raise_counts()[9] != seen
                });
                result.expect("runs");
                while bi.irq().raise_counts()[9] == seen {
                    ci.step(&mut bi).expect("steps");
                }
                assert_eq!(ci.tick(), cr.tick(), "tier {tier} seed {seed} stop {stops}");
                assert_eq!(bi.cycles(), br.cycles());
                assert_eq!(ci.gprs(), cr.gprs());
                stops += 1;
            }
        }
    }
}

/// The programs are not vacuous: voices make sound, the SPU interrupt
/// fires, the CD audio plays.
#[test]
fn the_scenes_do_what_they_say() {
    let scene = Scene {
        cd_audio: true,
        irq: true,
    };
    let code = program(1, scene);
    let (mut cpu, mut bus) = psoxide_jit::testgen::machine(&code, &handler());
    start_cd_audio(&mut bus);
    assert!(
        bus.cdrom.debug_stat_byte() & 0x80 != 0,
        "the drive is playing: stat {:02x}",
        bus.cdrom.debug_stat_byte()
    );
    start_voices(&mut bus, true);
    spu_write(&mut bus, IRQ_ADDR, 0x0040 >> 3);
    spu_write(&mut bus, SPUCNT, 0xc041);
    let before = bus.irq().raise_counts()[9];
    let (_, result) = cpu.run(&mut bus, 200_000, u64::MAX, |_| false);
    result.expect("runs");
    bus.run_spu_to_current_cycle();
    let audio = bus.spu.drain_audio();
    assert!(audio.len() > 100, "{} samples", audio.len());
    assert!(
        audio.iter().any(|&(l, r)| l != 0 || r != 0),
        "the voices are audible"
    );
    assert!(
        bus.irq().raise_counts()[9] > before,
        "the SPU raised its interrupt"
    );
}
