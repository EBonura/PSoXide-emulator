//! SPU mixing throughput: 24 looping voices, reverb on, noise on two
//! voices, pitch modulation on one.
//!
//! ```bash
//! cargo run -p emulator-core --release --example spu_bench -- [samples]
//! ```
//!
//! Prints nanoseconds per 44.1 kHz sample (best of five runs). The sample
//! data and register setup are fixed, so two builds are comparable on the
//! same machine state.

use emulator_core::spu::{
    Spu, EON_HI, EON_LO, KON_HI, KON_LO, NON_LO, PMON_LO, REVERB_BASE, REVERB_CFG_BASE,
    REVERB_VOL_L, REVERB_VOL_R, SAMPLE_CYCLES, SPUCNT, VOICE_BASE,
};
use std::time::Instant;

fn main() {
    let samples: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(2_000_000);

    let mut best = f64::MAX;
    let mut checksum = 0u64;
    for _ in 0..5 {
        let mut spu = Spu::new();
        // Eight ADPCM blocks of pseudo-random nibbles at SPU RAM 0x1000;
        // the last carries loop-end + repeat so the voices loop forever.
        let mut ram = Vec::new();
        let mut state = 0x1234_5678u32;
        for block in 0..8 {
            let header = 0x0000 | (block & 3) << 4 | 6; // filter 0..3, shift 6
            let flags = if block == 7 { 0x03 } else { 0x00 };
            ram.push((header as u16) | (flags << 8));
            for _ in 0..7 {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ram.push((state >> 16) as u16);
            }
        }
        spu.write16(0x1F80_1DA6, 0x1000 >> 3); // transfer address
        spu.write16(SPUCNT, 0xC000 | 0x0080); // enabled, unmuted, reverb master
        spu.dma_write(&ram);

        spu.write16(REVERB_BASE, 0xE000);
        spu.write16(REVERB_VOL_L, 0x3000);
        spu.write16(REVERB_VOL_R, 0x3000);
        for i in 0..32u32 {
            spu.write16(REVERB_CFG_BASE + i * 2, (0x1000 + i * 0x111) as u16);
        }
        for v in 0..24u32 {
            let base = VOICE_BASE + v * 16;
            spu.write16(base, 0x2000); // volume L
            spu.write16(base + 2, 0x2000); // volume R
            spu.write16(base + 4, 0x0800 + (v as u16) * 0x40); // pitch
            spu.write16(base + 6, 0x1000 >> 3); // start address
            spu.write16(base + 8, 0x00FF); // ADSR low: fast attack, sustain level 15
            spu.write16(base + 10, 0x5FC0); // ADSR high: sustain decrease, slow
            spu.write16(base + 14, 0x1000 >> 3); // repeat address
        }
        spu.write16(EON_LO, 0xFFFF);
        spu.write16(EON_HI, 0x00FF);
        spu.write16(NON_LO, 0x0030); // voices 4 and 5 play noise
        spu.write16(PMON_LO, 0x0004); // voice 2 is modulated by voice 1
        spu.write16(KON_LO, 0xFFFF);
        spu.write16(KON_HI, 0x00FF);

        let start = Instant::now();
        let mut now = 0u64;
        for _ in 0..samples {
            now += SAMPLE_CYCLES;
            spu.tick_sample(now);
            if spu.audio_queue_len() > 4096 {
                for (l, r) in spu.drain_audio() {
                    checksum = checksum
                        .wrapping_mul(31)
                        .wrapping_add(l as u64 ^ (r as u64) << 16);
                }
            }
        }
        let ns = start.elapsed().as_secs_f64() * 1e9 / samples as f64;
        best = best.min(ns);
    }
    println!("{best:.1} ns per sample (checksum {checksum:016x})");
}
