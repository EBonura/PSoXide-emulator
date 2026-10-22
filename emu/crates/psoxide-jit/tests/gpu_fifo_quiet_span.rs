//! A packet waiting in the GPU's input FIFO behind a busy GPU ends the FIFO's
//! quiet span for one cycle when the busy credit runs out. A batch that
//! reaches that cycle is past its soft clock limit and applies each step on
//! its own; a load or store among them settles its issue cycle at a clock
//! that is not below the quiet limit, and must be accepted as long as it is
//! below the hard limit. Lockstep against plain `Cpu::step` over a loop that
//! queues a draw behind a fill and reads RAM while it drains, so some load
//! lands on that cycle.
#![cfg(target_arch = "aarch64")]

use psoxide_jit::testgen::DATA_BASE;

const fn r(funct: u32, rs: u32, rt: u32, rd: u32, sa: u32) -> u32 {
    rs << 21 | rt << 16 | rd << 11 | sa << 6 | funct
}
const fn i(op: u32, rs: u32, rt: u32, imm: u32) -> u32 {
    op << 26 | rs << 21 | rt << 16 | (imm & 0xffff)
}
const NOP: u32 = 0;
const GP0: u32 = 0x810;

/// Queue a fill rectangle and a flat quad behind it through GP0, then read
/// RAM `loads` times per trip of a `spin`-trip counted loop.
fn program(loads: u32, spin: u32, fill_size: u32) -> Vec<u32> {
    let mut c = vec![
        i(0x0f, 0, 1, 0x1f80), // lui r1, 0x1f80
        i(0x0d, 1, 1, 0x1000), // ori r1, r1, 0x1000
        i(0x0f, 0, 2, DATA_BASE >> 16),
    ];
    let outer = c.len();
    c.extend([
        i(0x0f, 0, 3, 0x0200), // fill rectangle, black
        i(0x2b, 1, 3, GP0),
        i(0x2b, 1, 0, GP0), // origin (0, 0)
        i(0x0f, 0, 3, fill_size >> 16),
        i(0x0d, 3, 3, fill_size & 0xffff),
        i(0x2b, 1, 3, GP0),
        i(0x0f, 0, 3, 0x2800), // flat quad, four vertices
        i(0x2b, 1, 3, GP0),
        i(0x2b, 1, 0, GP0),
        i(0x2b, 1, 0, GP0),
        i(0x2b, 1, 0, GP0),
        i(0x2b, 1, 0, GP0),
        i(0x0d, 0, 10, spin),
    ]);
    let inner = c.len();
    for k in 0..loads {
        c.push(i(0x23, 2, 8, 4 * k)); // lw r8, 4k(r2)
        c.push(r(0x21, 9, 8, 9, 0)); // addu r9, r9, r8
    }
    c.push(i(0x09, 10, 10, 0xffff)); // addiu r10, r10, -1
    let back = inner as i32 - (c.len() as i32 + 1);
    c.push(i(0x05, 10, 0, back as u32)); // bne r10, r0, inner
    c.push(NOP);
    let back = outer as i32 - (c.len() as i32 + 1);
    c.push(i(0x04, 0, 0, back as u32)); // b outer
    c.push(NOP);
    c
}

#[test]
fn loads_past_the_fifo_quiet_span_match_the_interpreter() {
    for (loads, spin, fill) in [
        (1, 40, 0x0010_0010),
        (2, 25, 0x0020_0020),
        (3, 17, 0x0008_0008),
        (1, 60, 0x0030_0030),
        (5, 9, 0x0010_0020),
    ] {
        let code = program(loads, spin, fill);
        let handler = [0x4200_0010, NOP];
        psoxide_jit::testgen::lockstep(&code, &handler, 300_000, 25, u64::from(loads), true)
            .unwrap_or_else(|e| panic!("{loads} loads x {spin}, fill {fill:#x}: {e}"));
    }
}
