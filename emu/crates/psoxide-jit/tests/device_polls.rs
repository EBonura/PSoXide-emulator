//! The device accesses of a guest's polling loops, run through the native
//! tier's device paths, must leave the machine where plain `Cpu::step`
//! leaves it: CD-ROM index and status polls, command and parameter writes,
//! response and data FIFO pops, interrupt acknowledges; word loads of the
//! interrupt, root-counter, DMA, GPU and controller-port registers; stores to
//! the interrupt registers; with and without interrupts enabled, in branch
//! delay slots, next to load delays, and mixed with RAM accesses. Lockstep
//! against the interpreter, whole-machine compare.
#![cfg(target_arch = "aarch64")]

use psoxide_jit::testgen::{lockstep, program, Rng, CODE_BASE, DATA_BASE};

const fn r(funct: u32, rs: u32, rt: u32, rd: u32, sa: u32) -> u32 {
    rs << 21 | rt << 16 | rd << 11 | sa << 6 | funct
}
const fn i(op: u32, rs: u32, rt: u32, imm: u32) -> u32 {
    op << 26 | rs << 21 | rt << 16 | (imm & 0xffff)
}
const NOP: u32 = 0;
/// Holds 0x1F80_1800, the CD-ROM registers.
const CD: u32 = 16;
/// Holds 0x1F80_1000.
const IO: u32 = 17;
/// Holds the RAM data area.
const RAM: u32 = 20;
/// Loop counter, outside the random pool.
const COUNTER: u32 = 28;

/// A temporary: r1..r15.
fn tmp(rng: &mut Rng) -> u32 {
    1 + rng.below(15)
}

/// CD-ROM commands that need no parameters.
const COMMANDS: [u32; 8] = [0x01, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x13];

/// One instruction that touches the CD-ROM registers, or something that
/// sits next to such accesses in real code.
fn access(rng: &mut Rng) -> u32 {
    match rng.below(14) {
        0..=2 => i(0x28, CD, tmp(rng), rng.below(4)), // sb
        3..=5 => i(0x24, CD, tmp(rng), rng.below(4)), // lbu
        6 => i(0x20, CD, tmp(rng), rng.below(4)),     // lb
        7 => {
            // Index select with a clean small value.
            i(0x0d, 0, tmp(rng), rng.below(4))
        }
        8 => i(0x0d, 0, tmp(rng), COMMANDS[rng.below(8) as usize]),
        9 => i(0x23, IO, tmp(rng), 0x044), // lw JOY_STAT
        10 => i(0x2b, RAM, tmp(rng), 4 * rng.below(64)), // sw to RAM
        11 => i(0x23, RAM, tmp(rng), 4 * rng.below(64)), // lw from RAM
        12 => io_access(rng),
        _ => r(0x21, tmp(rng), tmp(rng), tmp(rng), 0), // addu
    }
}

/// An access to an on-die register that polling loops use.
fn io_access(rng: &mut Rng) -> u32 {
    // Word loads: I_STAT, I_MASK, the root counters, DMA registers, GPUREAD
    // and GPUSTAT, the controller port.
    const LOADS: [u32; 28] = [
        0x070, 0x074, 0x070, 0x074, 0x100, 0x104, 0x108, 0x110, 0x114, 0x118, 0x120, 0x124, 0x128,
        0x0a0, 0x0a8, 0x0e8, 0x0f0, 0x0f4, 0x810, 0x814, 0x814, 0x040, 0x044, 0x044, 0x044, 0x048,
        0x04a, 0x04e,
    ];
    match rng.below(10) {
        0..=6 => i(
            0x23,
            IO,
            tmp(rng),
            LOADS[rng.below(LOADS.len() as u32) as usize],
        ),
        // Stores: acknowledge and mask interrupts, set up a root counter's
        // mode and target, send a byte to the controller port.
        7 => i(0x2b, IO, tmp(rng), [0x070, 0x074][rng.below(2) as usize]),
        8 => i(
            0x2b,
            IO,
            tmp(rng),
            [0x104, 0x108, 0x114, 0x118, 0x124, 0x128][rng.below(6) as usize],
        ),
        9 => i(0x28, IO, tmp(rng), 0x040),
        // A display-control command to the GPU, which the GPU applies when
        // the clock reaches it.
        _ => i(0x2b, IO, tmp(rng), 0x814),
    }
}

fn alu(rng: &mut Rng) -> u32 {
    let (rd, rs, rt) = (tmp(rng), tmp(rng), tmp(rng));
    let imm = rng.next_u64() as u32 & 0xffff;
    match rng.below(10) {
        0 => r(0x21, rs, rt, rd, 0),
        1 => r(0x24, rs, rt, rd, 0),
        2 => r(0x25, rs, rt, rd, 0),
        3 => r(0x26, rs, rt, rd, 0),
        4 => i(0x0c, rs, rd, imm & 0xff),
        5 => i(0x0d, rs, rd, imm),
        6 => i(0x09, rs, rd, imm),
        7 => r(0x00, 0, rt, rd, rng.below(32)),
        8 => r(0x02, 0, rt, rd, rng.below(32)),
        _ => r(0x2b, rs, rt, rd, 0),
    }
}

/// An access or ALU instruction, for bodies and delay slots.
fn plain(rng: &mut Rng) -> u32 {
    if rng.chance(70) {
        access(rng)
    } else {
        alu(rng)
    }
}

/// A program polling the CD-ROM: `irq` enables its interrupt too.
fn cdrom_program(rng: &mut Rng, body_len: usize, irq: bool) -> Vec<u32> {
    let mut code = vec![
        i(0x0f, 0, CD, 0x1f80),
        i(0x0d, CD, CD, 0x1800),
        i(0x0f, 0, IO, 0x1f80),
        i(0x0d, IO, IO, 0x1000),
        i(0x0f, 0, RAM, DATA_BASE >> 16),
    ];
    // The controller port: mode, baud and control as the pad protocol sets
    // them, so the byte stores start transfers.
    code.extend([
        i(0x09, 0, 2, 0x000d),
        i(0x29, IO, 2, 0x048),
        i(0x09, 0, 2, 0x0088),
        i(0x29, IO, 2, 0x04e),
        i(0x09, 0, 2, 0x1003),
        i(0x29, IO, 2, 0x04a),
    ]);
    if irq {
        // Timer 2 interrupts every few hundred cycles; I_MASK = VBlank,
        // CD-ROM, Timer 2 and the controller; SR = IEc | IM2 | CU2.
        let target = 0x100 + rng.below(0x700);
        code.extend([
            i(0x09, 0, 2, target), // addiu r2, r0, target
            i(0x29, IO, 2, 0x128), // sh r2, 0x128(r17)
            i(0x09, 0, 2, 0x0058), // addiu r2, r0, 0x58
            i(0x29, IO, 2, 0x124), // sh r2, 0x124(r17)
            i(0x09, 0, 2, 0x00c5), // addiu r2, r0, 0xc5
            i(0x2b, IO, 2, 0x074), // sw r2, 0x74(r17): I_MASK
            i(0x0f, 0, 2, 0x4000), // lui r2, 0x4000
            i(0x0d, 2, 2, 0x0401), // ori r2, r2, 0x401
            0x4082_6000,           // mtc0 r2, sr
        ]);
    }
    for rd in 1..=15 {
        let v = rng.next_u64() as u32;
        code.push(i(0x0f, 0, rd, v >> 16));
        code.push(i(0x0d, rd, rd, v));
    }
    let start = code.len();
    let mut n = 0;
    while n < body_len {
        match rng.below(100) {
            0..=14 => {
                // A forward branch over 1..=3 instructions, with a CD-ROM
                // access or ALU op in its delay slot.
                let skip = 1 + rng.below(3);
                let (rs, rt) = (tmp(rng), tmp(rng));
                code.push(if rng.chance(50) {
                    i(0x04, rs, rt, skip)
                } else {
                    i(0x05, rs, rt, skip)
                });
                code.push(plain(rng));
                for _ in 0..skip {
                    code.push(plain(rng));
                }
                n += 2 + skip as usize;
            }
            15..=24 => {
                // A counted loop around a few accesses: the poll shape.
                let count = 1 + rng.below(12);
                let k = 1 + rng.below(4);
                code.push(i(0x0d, 0, COUNTER, count));
                for _ in 0..k {
                    code.push(plain(rng));
                }
                code.push(i(0x09, COUNTER, COUNTER, 0xffff));
                code.push(i(0x05, COUNTER, 0, (-(k as i32 + 2)) as u32));
                code.push(plain(rng));
                n += k as usize + 4;
            }
            25..=34 => {
                // A load and a use of its register in the load delay slot
                // and after it.
                let t = tmp(rng);
                code.push(i(0x24, CD, t, rng.below(4)));
                code.push(r(0x21, t, t, tmp(rng), 0));
                code.push(r(0x21, t, 0, tmp(rng), 0));
                n += 3;
            }
            35..=39 => {
                // A store of a register that a load in flight is changing.
                let t = tmp(rng);
                code.push(i(0x24, CD, t, rng.below(4)));
                code.push(i(0x28, CD, t, rng.below(4)));
                n += 2;
            }
            40..=46 => {
                // Issue a command, then enable all CD-ROM interrupt bits
                // under index 1: a write that raises the interrupt when a
                // response is already waiting.
                let t = tmp(rng);
                code.push(i(0x28, CD, 0, 0));
                code.push(i(0x0d, 0, t, COMMANDS[rng.below(8) as usize]));
                code.push(i(0x28, CD, t, 1));
                code.push(i(0x0d, 0, t, 1));
                code.push(i(0x28, CD, t, 0));
                code.push(i(0x0d, 0, t, 0x1f));
                code.push(i(0x28, CD, t, 2));
                code.push(i(0x28, CD, 0, 0));
                n += 8;
            }
            _ => {
                code.push(plain(rng));
                n += 1;
            }
        }
    }
    let back = start as i32 - (code.len() as i32 + 1);
    code.push(i(0x04, 0, 0, back as u32)); // b start
    code.push(NOP);
    let _ = CODE_BASE;
    code
}

fn run(seed: u64, body: usize, irq: bool) {
    let mut rng = Rng::new(seed);
    let code = cdrom_program(&mut rng, body, irq);
    let (_, handler) = program(&mut Rng::new(1), 1, 0);
    lockstep(&code, &handler, 200_000, 100, seed, true)
        .unwrap_or_else(|e| panic!("seed {seed} irq {irq}: {e}"));
}

#[test]
fn device_polls_without_interrupts() {
    for seed in 1..=24 {
        run(seed, 60, false);
    }
}

#[test]
fn device_polls_with_interrupts() {
    for seed in 100..=124 {
        run(seed, 60, true);
    }
}

/// A body larger than the instruction cache: the accesses come right after
/// line fills, whose bus time the register reads wait for.
#[test]
fn device_polls_next_to_instruction_cache_fills() {
    for seed in 200..=205 {
        run(seed, 1_800, seed % 2 == 0);
    }
}
