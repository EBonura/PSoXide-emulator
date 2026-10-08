//! MDEC colour decode throughput: macroblocks per second through the
//! upload, decode and read-back path, 24-bit output.
//!
//! ```bash
//! cargo run -p emulator-core --release --example mdec_bench -- [macroblocks]
//! ```
//!
//! Prints microseconds per macroblock (best of five). The stream is fixed
//! pseudo-random run-length data, so two builds are comparable on the same
//! machine state.

use emulator_core::mdec::{Mdec, MDEC_CMD_DATA, MDEC_CTRL_STAT};
use std::time::Instant;

fn main() {
    let blocks: u32 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(20_000);
    let mut best = f64::MAX;
    let mut sum = 0u64;
    for _ in 0..5 {
        let mut mdec = Mdec::new();
        mdec.write32(MDEC_CTRL_STAT, 0x8000_0000);
        mdec.write32(MDEC_CTRL_STAT, 0x6000_0000);
        // Quantisation tables: flat 8 for luma and chroma.
        mdec.write32(MDEC_CMD_DATA, 0x4000_0001);
        mdec.dma_write_in(&[0x0808_0808; 32]);
        // IDCT matrix: a plausible DCT table (cosine rows scaled to 0x5A82).
        let mut table = [0u32; 32];
        for (i, word) in table.iter_mut().enumerate() {
            let lo = (((i * 2) as i32 * 977 % 46000) - 23000) as i16 as u16 as u32;
            let hi = (((i * 2 + 1) as i32 * 977 % 46000) - 23000) as i16 as u16 as u32;
            *word = lo | (hi << 16);
        }
        mdec.write32(MDEC_CMD_DATA, 0x6000_0000);
        mdec.dma_write_in(&table);

        // One macroblock = 6 blocks of DC, 12 AC terms, end marker.
        let mut state = 0xC0FF_EE11u32;
        let mut halfwords: Vec<u16> = Vec::new();
        for _ in 0..blocks * 6 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            halfwords.push(0x2000 | ((state >> 20) as u16 & 0x3FF)); // qscale 8, DC
            for _ in 0..12 {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                halfwords.push((((state >> 28) & 3) as u16) << 10 | ((state >> 12) as u16 & 0x3FF));
            }
            halfwords.push(0xFE00);
        }
        if halfwords.len() % 2 == 1 {
            halfwords.push(0xFE00);
        }
        let words: Vec<u32> = halfwords
            .chunks(2)
            .map(|p| u32::from(p[0]) | (u32::from(p[1]) << 16))
            .collect();

        let start = Instant::now();
        mdec.write32(MDEC_CMD_DATA, 0x3800_0000 | (words.len() as u32 & 0xFFFF));
        let mut out = vec![0u32; 192];
        mdec.dma_write_in(&words);
        for _ in 0..blocks {
            mdec.dma_read_out(&mut out);
            sum = sum.wrapping_add(u64::from(out[7]));
        }
        best = best.min(start.elapsed().as_secs_f64() * 1e6 / f64::from(blocks));
    }
    println!("{best:.2} us per macroblock (checksum {sum:016x})");
}
