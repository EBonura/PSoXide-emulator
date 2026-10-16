//! Main-RAM loads while a GPU block DMA streams from RAM wait for the bus
//! (`Bus::ram_read_stalls`), so compiled code's inline load path must not be
//! taken then. Lockstep against plain `Cpu::step` over a loop that kicks a
//! GPU DMA block transfer and reads RAM while it runs.
#![cfg(target_arch = "aarch64")]

use psoxide_jit::testgen::{lockstep, CODE_BASE, DATA_BASE};

const fn r(funct: u32, rs: u32, rt: u32, rd: u32, sa: u32) -> u32 {
    rs << 21 | rt << 16 | rd << 11 | sa << 6 | funct
}
const fn i(op: u32, rs: u32, rt: u32, imm: u32) -> u32 {
    op << 26 | rs << 21 | rt << 16 | (imm & 0xFFFF)
}
const NOP: u32 = 0;

/// A program that sets up GPU DMA (channel 2, CPU to GPU, block mode over
/// zeroed RAM, so the GPU just sees NOP commands), then loops: kick a
/// transfer, read RAM in a short counted loop, kick again.
fn dma_program(loads_per_iteration: u32, spin: u32) -> Vec<u32> {
    let mut c = vec![
        i(0x0F, 0, 1, 0x1F80), // lui r1, 0x1f80
        i(0x0D, 1, 1, 0x1000), // ori r1, r1, 0x1000
        i(0x0F, 0, 2, DATA_BASE >> 16),
        i(0x0F, 0, 3, 0x0400), // GP1 05: DMA direction CPU to GPU
        i(0x0D, 3, 3, 0x0002),
        i(0x2B, 1, 3, 0x0814), // sw r3, 0x814(r1)
        i(0x0F, 0, 3, 0x0765), // DPCR with channel 2 enabled
        i(0x0D, 3, 3, 0x4B21),
        i(0x2B, 1, 3, 0x00F0), // sw r3, 0xf0(r1)
    ];
    let outer = c.len();
    c.extend([
        i(0x0F, 0, 4, 0x0010), // MADR = 0x100000
        i(0x2B, 1, 4, 0x00A0),
        i(0x0F, 0, 5, 0x0004), // BCR: 4 blocks of 16 words
        i(0x0D, 5, 5, 0x0010),
        i(0x2B, 1, 5, 0x00A4),
        i(0x0F, 0, 6, 0x0100), // CHCR: from RAM, sync mode 1, start
        i(0x0D, 6, 6, 0x0201),
        i(0x2B, 1, 6, 0x00A8),
        i(0x0D, 0, 10, spin), // ori r10, r0, spin
    ]);
    let inner = c.len();
    for k in 0..loads_per_iteration {
        c.push(i(0x23, 2, 8, 4 * k)); // lw r8, 4k(r2)
        c.push(r(0x21, 9, 8, 9, 0)); // addu r9, r9, r8
    }
    c.push(i(0x09, 10, 10, 0xFFFF)); // addiu r10, r10, -1
    let back = inner as i32 - (c.len() as i32 + 1);
    c.push(i(0x05, 10, 0, back as u32)); // bne r10, r0, inner
    c.push(NOP);
    let back = outer as i32 - (c.len() as i32 + 1);
    c.push(i(0x04, 0, 0, back as u32)); // b outer
    c.push(NOP);
    let _ = CODE_BASE;
    c
}

#[test]
fn ram_loads_during_gpu_block_dma_match_the_interpreter() {
    for (loads, spin) in [(1, 300), (4, 60), (12, 20)] {
        let code = dma_program(loads, spin);
        let handler = [0x4200_0010, NOP];
        lockstep(&code, &handler, 400_000, 25, u64::from(loads), true)
            .unwrap_or_else(|e| panic!("{loads} loads x {spin}: {e}"));
    }
}
