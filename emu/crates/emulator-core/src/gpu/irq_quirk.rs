//! The MiSTer core's GP0(1Fh) behaviour, selected by [`HardwareTarget`].
//!
//! Evidence experiment; the retail target never reaches any of this. On a
//! SuperStation One running the MiSTer PSX core (480094b), GPUSTAT bit 24 and
//! I_STAT bit 1 mostly never rise after a GP0(1Fh) (see [`GpuIrqModel`] for
//! the cause in the core's `gpu.vhd`).
//!
//! The model keeps one "chain": the draws the GPU has run back to back since
//! a word last arrived at an idle GPU (or since the last 1Fh). A 1Fh then
//! raises the request or is silently dropped:
//!
//! * `never`: every 1Fh is dropped.
//! * `core`: dropped when the chain is empty (idle GPU, measured 0/24 on the
//!   core), when it is two or more draws long (measured almost always lost),
//!   or when the single draw is of the even kind. One rect of 16 pixels or
//!   more or one polygon passes (measured 24/24 and 23/24); rects of 1x1, 2x2
//!   and 5x1 and short fills fail; a VRAM copy or transfer passes about half the
//!   time. A lost request still passes 1 time in [`JITTER_ONE_IN`] (the core's
//!   occasional 1/24 on long chains).
//! * `parity`: each draw has a parity (the same classes as `core`), chains
//!   XOR them, and a 1Fh passes when the chain is odd. Mixed chains then pass
//!   about half the time.
//!
//! Values come from the hardware-test rows run on the core (fpga-irq NOTES);
//! the thresholds between the measured shapes are assumptions. The coin flips
//! come from a fixed-seed generator.

use crate::hardware_target::{GpuIrqModel, HardwareTarget};

/// One lost request in this many would still have passed on the core.
const JITTER_ONE_IN: u64 = 64;
/// A rect or polygon of at least this many pixels counts as an odd draw.
const ODD_PIXELS: u64 = 16;

/// Counters for the report; not part of any save state.
#[derive(Clone, Copy, Debug, Default)]
#[non_exhaustive]
pub struct IrqQuirkStats {
    /// GP0(1Fh) commands that reached the GPU.
    pub requested: u64,
    /// Of those, the ones that raised bit 24 and I_STAT bit 1.
    pub raised: u64,
    /// Dropped because the GPU was idle (no draw ahead of the word).
    pub dropped_idle: u64,
    /// Dropped behind a chain of several draws.
    pub dropped_chain: u64,
    /// Dropped behind a single draw of the even kind.
    pub dropped_even: u64,
    /// Passed only by the jitter rule.
    pub jitter_passes: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct IrqQuirk {
    model: GpuIrqModel,
    rng: u64,
    /// `trace=N` in the hardware spec: print the first N events to stderr.
    trace: u32,
    chain_len: u32,
    chain_odd: bool,
    /// `exact`: the last draw ended before a next word could arrive (lost).
    last_short: bool,
    /// `exact`: the last draw sits at the edge of the arrival gap (coin).
    last_marginal: bool,
    /// Verdict of the last 1Fh that reached the GPU (`None` until one does).
    pub(crate) verdict: Option<bool>,
    /// The next CPU GP0 store was stalled behind a draw by the bus.
    pub(crate) stalled: bool,
    /// GPUSTAT was read since the last CPU GP0 store.
    pub(crate) polled: bool,
    pub(crate) stats: IrqQuirkStats,
}

impl IrqQuirk {
    /// The model `target` asks for.
    pub(crate) fn for_target(target: &HardwareTarget) -> Self {
        Self::new(target.gpu_irq, target.seed, target.trace)
    }

    /// The model `PSOXIDE_HARDWARE` asks for (none on a retail console).
    pub(crate) fn from_env() -> Self {
        Self::for_target(&HardwareTarget::from_env())
    }

    fn new(model: GpuIrqModel, seed: u64, trace: u32) -> Self {
        Self {
            model,
            // xorshift64 must not start at zero.
            rng: seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1,
            trace,
            chain_len: 0,
            chain_odd: false,
            last_short: false,
            last_marginal: false,
            verdict: None,
            stalled: false,
            polled: false,
            stats: IrqQuirkStats::default(),
        }
    }

    #[inline(always)]
    pub(crate) fn active(&self) -> bool {
        self.model != GpuIrqModel::Reliable
    }

    fn next(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn coin(&mut self) -> bool {
        self.next() >> 63 != 0
    }

    /// A word arrived at an idle GPU in command position: the chain restarts.
    pub(crate) fn idle_arrival(&mut self, word: u32) {
        if self.trace > 0 {
            eprintln!(
                "[gpu-irq-quirk] idle arrival word={word:#010x} (chain was {})",
                self.chain_len
            );
        }
        self.chain_len = 0;
        self.chain_odd = false;
        self.last_short = false;
        self.last_marginal = false;
    }

    /// A draw packet `op` of `pixels` pixels ran.
    pub(crate) fn note_draw(&mut self, op: u8, pixels: u64) {
        if self.model == GpuIrqModel::Exact {
            return self.note_draw_exact(op, pixels);
        }
        let odd = match op {
            // Fill: 16 wide by 1 to 6 lines measured 0/24, 7 lines 12/24, 8
            // lines 22/24. Even below 112 pixels, a coin up to 127, then odd.
            0x02 if pixels < 112 => false,
            0x02 if pixels < 128 => self.coin(),
            0x02 => true,
            // VRAM-to-VRAM copy: 1x1 measured 1/24, 3x3 to 40x6 10 to 15/24.
            0x80..=0x9f if pixels < 4 => false,
            // Copy and CPU-to-VRAM and VRAM-to-CPU transfers: a coin.
            0x80..=0xdf => self.coin(),
            // Polygons, lines and rects: by size.
            0x20..=0x7f => pixels >= ODD_PIXELS,
            // State commands do not move the phase.
            _ => return,
        };
        if self.trace > 0 {
            eprintln!("[gpu-irq-quirk] draw op={op:#04x} pixels={pixels} odd={odd}");
        }
        self.chain_len += 1;
        self.chain_odd ^= odd;
    }

    /// [`IrqQuirk::note_draw`] for the `exact` model.
    fn note_draw_exact(&mut self, op: u8, pixels: u64) {
        // (adds an odd duration, ended before a next word could arrive, at the edge of that gap)
        let (odd, short, marginal) = match op {
            // Fill: 16 wide by h lines is 3 + 4h clk1x, always odd. The gap
            // cut-offs follow the v3 rows: 16x1 to 16x6 lost, 16x7 and 16x8 at the edge.
            0x02 => (true, pixels < 112, (112..128).contains(&pixels)),
            // VRAM copy: duration depends on read latency, so a coin.
            0x80..=0x9f => (self.coin(), pixels < 4, false),
            0xa0..=0xdf => (self.coin(), false, false),
            // Polygons: a flat big triangle adds an odd duration.
            0x20..=0x3f => (true, pixels < ODD_PIXELS, false),
            // Lines: unmeasured, a coin.
            0x40..=0x5f => (self.coin(), pixels < ODD_PIXELS, false),
            // Opaque rect of N pixels: 11 + N, odd when N is even.
            0x60..=0x7f => (
                pixels > 0 && pixels.is_multiple_of(2),
                pixels < ODD_PIXELS,
                false,
            ),
            _ => return,
        };
        if self.trace > 0 {
            eprintln!(
                "[gpu-irq-quirk] draw op={op:#04x} pixels={pixels} odd={odd} short={short} marginal={marginal}"
            );
        }
        self.chain_len += 1;
        self.chain_odd ^= odd;
        self.last_short = short;
        self.last_marginal = marginal;
    }

    /// A GP0(1Fh) reached the GPU: raise the request or drop it. The chain
    /// restarts either way.
    pub(crate) fn decide(&mut self) -> bool {
        self.stats.requested += 1;
        let (len, odd) = (self.chain_len, self.chain_odd);
        let (short, marginal) = (self.last_short, self.last_marginal);
        self.chain_len = 0;
        self.chain_odd = false;
        self.last_short = false;
        self.last_marginal = false;
        let pass = match self.model {
            GpuIrqModel::Exact => {
                let natural = len >= 1 && odd && !short && (!marginal || self.coin());
                if natural {
                    true
                } else if len == 0 {
                    self.stats.dropped_idle += 1;
                    false
                } else if self.next().is_multiple_of(JITTER_ONE_IN) {
                    self.stats.jitter_passes += 1;
                    true
                } else {
                    self.stats.dropped_chain += 1;
                    false
                }
            }
            GpuIrqModel::Reliable => true,
            GpuIrqModel::Never => false,
            GpuIrqModel::Core | GpuIrqModel::Parity => {
                let natural = match self.model {
                    GpuIrqModel::Core => len == 1 && odd,
                    _ => len >= 1 && odd,
                };
                if natural {
                    true
                } else if len == 0 {
                    self.stats.dropped_idle += 1;
                    false
                } else if self.next().is_multiple_of(JITTER_ONE_IN) {
                    self.stats.jitter_passes += 1;
                    true
                } else {
                    if len == 1 {
                        self.stats.dropped_even += 1;
                    } else {
                        self.stats.dropped_chain += 1;
                    }
                    false
                }
            }
        };
        if pass {
            self.stats.raised += 1;
        }
        if self.trace > 0 {
            self.trace -= 1;
            eprintln!("[gpu-irq-quirk] 1Fh chain_len={len} odd={odd} -> {pass}");
        }
        self.verdict = Some(pass);
        pass
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quirk(model: GpuIrqModel) -> IrqQuirk {
        IrqQuirk::new(model, 1, 0)
    }

    #[test]
    fn idle_request_never_passes() {
        let mut q = quirk(GpuIrqModel::Core);
        for _ in 0..1000 {
            q.idle_arrival(0);
            assert!(!q.decide());
        }
    }

    #[test]
    fn core_one_big_polygon_passes_and_small_rects_fail() {
        let mut q = quirk(GpuIrqModel::Core);
        q.note_draw(0x24, 12_000);
        assert!(q.decide());
        let mut passes = 0;
        for _ in 0..1000 {
            q.idle_arrival(0);
            q.note_draw(0x60, 4);
            passes += u32::from(q.decide());
        }
        assert!(passes < 60, "even rects pass {passes}/1000");
    }

    #[test]
    fn core_long_chains_almost_always_fail() {
        let mut q = quirk(GpuIrqModel::Core);
        let mut passes = 0;
        for _ in 0..1000 {
            q.idle_arrival(0);
            for _ in 0..16 {
                q.note_draw(0x24, 12_000);
            }
            passes += u32::from(q.decide());
        }
        assert!(passes < 60, "chains pass {passes}/1000");
    }

    #[test]
    fn parity_xors_the_chain() {
        let mut q = quirk(GpuIrqModel::Parity);
        q.note_draw(0x24, 12_000);
        assert!(q.decide());
        q.note_draw(0x24, 12_000);
        q.note_draw(0x24, 12_000);
        q.note_draw(0x24, 12_000);
        assert!(q.decide());
    }

    #[test]
    fn copies_pass_about_half() {
        let mut q = quirk(GpuIrqModel::Core);
        let mut passes = 0;
        for _ in 0..2000 {
            q.idle_arrival(0);
            q.note_draw(0x80, 64);
            passes += u32::from(q.decide());
        }
        assert!((850..=1150).contains(&passes), "{passes}/2000");
    }

    #[test]
    fn exact_follows_the_duration_parities() {
        let mut q = quirk(GpuIrqModel::Exact);
        // One big triangle: odd. Two: even.
        q.note_draw(0x24, 12_000);
        assert!(q.decide());
        q.note_draw(0x24, 12_000);
        q.note_draw(0x24, 12_000);
        let mut passes = 0;
        for _ in 0..200 {
            q.idle_arrival(0);
            q.note_draw(0x24, 12_000);
            q.note_draw(0x24, 12_000);
            passes += u32::from(q.decide());
        }
        assert!(passes < 12, "{passes}/200");
        // A 16x4 rect (even pixel count) passes, a 5x1 one is too short.
        q.idle_arrival(0);
        q.note_draw(0x64, 64);
        assert!(q.decide());
        q.idle_arrival(0);
        q.note_draw(0x64, 5);
        assert!(!q.decide());
        // A 16x8 fill passes, 16x2 is too short.
        q.idle_arrival(0);
        q.note_draw(0x02, 128);
        assert!(q.decide());
        q.idle_arrival(0);
        q.note_draw(0x02, 32);
        assert!(!q.decide());
    }

    #[test]
    fn never_mode_drops_everything() {
        let mut q = quirk(GpuIrqModel::Never);
        q.note_draw(0x24, 12_000);
        assert!(!q.decide());
    }

    #[test]
    fn same_seed_same_verdicts() {
        let run = || {
            let mut q = IrqQuirk::new(GpuIrqModel::Core, 7, 0);
            (0..64)
                .map(|_| {
                    q.idle_arrival(0);
                    q.note_draw(0x80, 64);
                    q.decide()
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(run(), run());
    }
}
