//! SPU -- Sound Processing Unit.
//!
//! The SPU is a 24-voice ADPCM sample engine with per-voice ADSR
//! envelopes, pitch-controlled playback, a 512 KiB sample RAM, and
//! stereo output mixed at 44.1 kHz.
//!
//! Not modelled: the external audio input (nothing feeds it on a console).
//! XA-ADPCM and CD-DA samples arrive already decoded from the CD-ROM
//! module through [`Spu::feed_cd_audio`].
//!
//! Pipeline per 44.1 kHz sample:
//!
//! 1. For each voice, take in the decoded ADPCM samples its pitch counter
//!    has passed (decoding the next 16-byte block when the current one is
//!    used up and acting on its loop flags), interpolate at the counter's
//!    fractional position through the 4-point Gaussian table, step the
//!    ADSR envelope, apply it, then apply the per-voice L/R volumes. A
//!    noise voice substitutes the shared noise generator's level for the
//!    interpolated sample.
//! 2. Sum the voices into `(sum_l, sum_r)`; voices enabled for reverb (and
//!    the CD input, when routed there) also feed the reverb bus.
//! 3. Mix in the CD input at its volume, write the four capture buffers,
//!    run the reverb for this sample, add its wet output, saturate, and
//!    scale by the main volume.
//! 4. Push `(l, r)` to the host-facing output ring.
//!
//! SPU IRQ: if SPUCNT bit 6 (IRQ enable) is set and the IRQ address matches
//! a voice's block read, a transfer FIFO access or the capture-buffer
//! cursor in low SPU RAM, STATUS bit 6 latches and the bus is signalled.
//!
//! Sample rate: 44_100 Hz. PSX clock is 33_868_800 Hz, so 1 sample =
//! 768 cycles. The scheduler ticks the SPU every [`SAMPLE_CYCLES`] cycles.
//!
//! ## Provenance
//!
//! The voice engine is written from nocash PSX-SPX: ADPCM blocks and loop
//! flags ("SPU ADPCM Samples"), the pitch counter and pitch modulation
//! ("SPU ADPCM Pitch"), the ADSR and volume sweeps ("SPU Volume and ADSR
//! Generator", in `envelope.rs`), the noise generator, and the reverb
//! formulas ("SPU Reverb Formula"). The Gaussian table is the hardware
//! constant PSX-SPX lists. Transfer FIFO, DMA, SPUSTAT mirroring, capture
//! buffers, key-on latency and the IRQ rules are this project's own
//! measurements on its console (hardware tests PA2-PA5, SB1-SB4). Where a
//! value is a legacy scaling kept because nothing measured contradicts it,
//! the code says so:
//!
//! - the reverb output volume register is applied as Q14 (0x4000 = unity)
//!   and the main volume register word as signed Q15, both as the earlier
//!   mixer did; the documented Q15 treatment of the reverb output volume
//!   would halve the wet level;
//! - a reverb work area at or below 0x0200, or 0xFFFF, is treated as off.
//!
//! This module no longer contains code derived from another emulator.
//! See `LICENSE` and `docs/PROVENANCE.md`.

use crate::scheduler::{EventSlot, Scheduler};

mod envelope;
mod xa;
pub use xa::{xa_decode_block, XaDecoderState};

use envelope::{
    attack_slope, decay_slope, release_slope, sustain_slope, sustain_threshold, Pacer,
    VolumeEnvelope,
};

// ===============================================================
//  Register addresses -- voice bank + global + reverb config.
// ===============================================================

/// Base of the SPU MMIO window. 512 bytes total spanning voice bank,
/// global control regs, and reverb coefficient registers.
pub const SPU_BASE: u32 = 0x1F80_1C00;
/// One past the end of the SPU MMIO window.
pub const SPU_END: u32 = 0x1F80_1E00;

/// Base of the 24-voice register bank (16 bytes per voice).
pub const VOICE_BASE: u32 = 0x1F80_1C00;
/// One past the end of the voice bank (24 * 16 = 0x180 bytes → 0x1F80_1D80).
pub const VOICE_END: u32 = 0x1F80_1D80;

/// Main Volume Left (16-bit, Q14).
pub const MAIN_VOL_L: u32 = 0x1F80_1D80;
/// Main Volume Right (16-bit, Q14).
pub const MAIN_VOL_R: u32 = 0x1F80_1D82;
/// Reverb output volume Left.
pub const REVERB_VOL_L: u32 = 0x1F80_1D84;
/// Reverb output volume Right.
pub const REVERB_VOL_R: u32 = 0x1F80_1D86;
/// Key-On low (voices 0..15).
pub const KON_LO: u32 = 0x1F80_1D88;
/// Key-On high (voices 16..23).
pub const KON_HI: u32 = 0x1F80_1D8A;
/// Key-Off low (voices 0..15).
pub const KOFF_LO: u32 = 0x1F80_1D8C;
/// Key-Off high (voices 16..23).
pub const KOFF_HI: u32 = 0x1F80_1D8E;
/// Pitch modulation enable low.
pub const PMON_LO: u32 = 0x1F80_1D90;
/// Pitch modulation enable high.
pub const PMON_HI: u32 = 0x1F80_1D92;
/// Noise mode enable low.
pub const NON_LO: u32 = 0x1F80_1D94;
/// Noise mode enable high.
pub const NON_HI: u32 = 0x1F80_1D96;
/// Reverb enable low.
pub const EON_LO: u32 = 0x1F80_1D98;
/// Reverb enable high.
pub const EON_HI: u32 = 0x1F80_1D9A;
/// ENDX low (per-voice "reached loop-end block" latch, write-1-to-clear).
pub const ENDX_LO: u32 = 0x1F80_1D9C;
/// ENDX high.
pub const ENDX_HI: u32 = 0x1F80_1D9E;
/// Reverb work-area start address (halfword, scaled by 8 → byte addr).
pub const REVERB_BASE: u32 = 0x1F80_1DA2;
/// IRQ address (halfword * 8 = byte addr into SPU RAM).
pub const IRQ_ADDR: u32 = 0x1F80_1DA4;
/// Data transfer address (halfword * 8 = byte addr into SPU RAM).
pub const TRANSFER_ADDR: u32 = 0x1F80_1DA6;
/// Data transfer FIFO (reads pop, writes push at TRANSFER_ADDR, which advances).
pub const TRANSFER_FIFO: u32 = 0x1F80_1DA8;
/// SPU control register.
pub const SPUCNT: u32 = 0x1F80_1DAA;
/// SPUCNT bit 0: route CD-DA / XA-ADPCM input through the SPU mixer.
const SPUCNT_CD_AUDIO_ENABLE: u16 = 1 << 0;
/// SPUCNT bit 2: route CD-DA / XA-ADPCM input into the reverb engine.
const SPUCNT_CD_REVERB_ENABLE: u16 = 1 << 2;
/// SPUCNT bit 7: enable reverb steady-state processing.
const SPUCNT_REVERB_MASTER_ENABLE: u16 = 1 << 7;
/// SPUCNT bit 14: 0 = muted, 1 = unmuted.
const SPUCNT_UNMUTE: u16 = 1 << 14;
/// Data transfer control (typically 0x0004 -- 4-bit transfer step).
pub const TRANSFER_CTRL: u32 = 0x1F80_1DAC;
/// SPU status register.
pub const SPUSTAT: u32 = 0x1F80_1DAE;
/// CD audio input volume Left.
pub const CD_VOL_L: u32 = 0x1F80_1DB0;
/// CD audio input volume Right.
pub const CD_VOL_R: u32 = 0x1F80_1DB2;
/// External audio input volume Left.
pub const EXT_VOL_L: u32 = 0x1F80_1DB4;
/// External audio input volume Right.
pub const EXT_VOL_R: u32 = 0x1F80_1DB6;
/// Current Main Volume Left backing register.
pub const CURRENT_MAIN_VOL_L: u32 = 0x1F80_1DB8;
/// Current Main Volume Right backing register.
pub const CURRENT_MAIN_VOL_R: u32 = 0x1F80_1DBA;

/// Start of reverb configuration area (32 × 16-bit coefficient regs).
pub const REVERB_CFG_BASE: u32 = 0x1F80_1DC0;

/// Per-voice offsets within the 16-byte voice block.
#[allow(dead_code)]
mod voice_offset {
    /// +0..1 volume left (Q14, or sweep config if bit 15 set).
    pub const VOLUME_L: u32 = 0x0;
    /// +2..3 volume right.
    pub const VOLUME_R: u32 = 0x2;
    /// +4..5 ADPCM pitch. `0x1000` = base rate (44.1 kHz). 16-bit R/W
    /// register (full readback); the effective rate clamps to 0x3FFF.
    pub const PITCH: u32 = 0x4;
    /// +6..7 ADPCM start address (in 8-byte units; <<3 = byte addr).
    pub const START_ADDR: u32 = 0x6;
    /// +8..9 ADSR config low -- attack mode + rate + decay rate + sustain level.
    pub const ADSR_LO: u32 = 0x8;
    /// +A..B ADSR config high -- sustain mode + sustain rate + release mode + release rate.
    pub const ADSR_HI: u32 = 0xA;
    /// +C..D Current ADSR volume (read-only; returns current envelope level).
    pub const ADSR_CURRENT: u32 = 0xC;
    /// +E..F Repeat (loop) address (in 8-byte units).
    pub const REPEAT_ADDR: u32 = 0xE;
}

// ===============================================================
//  Sizing / timing constants.
// ===============================================================

/// Number of voices in the PSX SPU.
pub const NUM_VOICES: usize = 24;

/// SPU RAM size in bytes (512 KiB).
pub const SPU_RAM_BYTES: usize = 512 * 1024;
/// SPU RAM size in 16-bit words.
pub const SPU_RAM_HALFWORDS: usize = SPU_RAM_BYTES / 2;

/// System clock cycles per SPU sample. 33_868_800 Hz / 44_100 Hz = 768.
pub const SAMPLE_CYCLES: u64 = 768;

/// ADPCM block size in bytes (1 header + 1 flags + 14 data bytes).
pub const ADPCM_BLOCK_BYTES: usize = 16;
/// Samples produced per ADPCM block.
pub const ADPCM_SAMPLES_PER_BLOCK: usize = 28;

/// Host-facing audio output buffer cap. Frontend drains periodically;
/// if it falls behind we discard the oldest samples.
const OUTPUT_BUFFER_CAP: usize = 44100 * 2; // 2 seconds of stereo samples

// ===============================================================
//  ADPCM filter table (5 filters × 2 coefficients, matches PSX-SPX).
// ===============================================================

/// ADPCM prediction filters: weights, in 64ths, of the previous sample and the
/// one before it (PSX-SPX "SPU ADPCM Samples" and the CD-XA ADPCM tables).
/// The header's filter field is capped at 4, the last defined filter.
/// One whole sample of a voice's counter (12 fractional bits).
const COUNTER_ONE: u32 = 1 << 12;
/// A new note's starting counter: three whole samples to take in before the
/// first output.
const NEW_NOTE_COUNTER: u32 = 3 * COUNTER_ONE;

const ADPCM_FILTERS: [(i32, i32); 5] = [(0, 0), (60, 0), (115, -52), (98, -55), (122, -60)];

// ===============================================================
//  ADSR phase + per-voice envelope state.
// ===============================================================

/// ADSR state-machine phase. Voices start at `Off`; KON transitions to
/// `Attack` and resets the envelope. `Off` voices contribute silence.
#[derive(Copy, Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum AdsrPhase {
    Off,
    Attack,
    Decay,
    Sustain,
    Release,
}

// ===============================================================
//  Voice state.
// ===============================================================

/// Per-voice runtime state. Holds decode buffers, ADSR envelope,
/// volumes, and loop pointers. Kept plain (no padding or SIMD) --
/// 24 copies of this struct live in `Spu::voices` and mix together on
/// each sample.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Voice {
    /// Left volume envelope.
    vol_l: VolumeEnvelope,
    /// Right volume envelope.
    vol_r: VolumeEnvelope,
    /// Raw pitch register, full 16-bit R/W -- reads echo the written value
    /// like hardware. `0x1000` plays at the sample's source rate (typically
    /// 44.1 kHz); the rate counter clamps to 0x3FFF at use.
    raw_pitch: u16,
    /// Byte address into SPU RAM where playback begins on KON. `<<3`
    /// of the register value, 16-byte aligned for the decoder.
    start_addr: u32,
    /// The raw 16-bit START_ADDR register value, kept verbatim so reads
    /// echo it back like hardware (the decoder uses `start_addr`, the
    /// `<<3`/aligned form derived from it).
    start_addr_raw: u16,
    /// Loop address (byte address). Set by software via REPEAT_ADDR
    /// register and by the ADPCM flag-4 bit (loop-start).
    loop_addr: u32,
    /// Raw REPEAT_ADDR register value for readback: the software-written
    /// 16-bit word, or `loop_addr >> 3` when the decoder sets the loop.
    loop_addr_raw: u16,
    /// True if software wrote REPEAT_ADDR directly since voice start;
    /// suppresses the ADPCM loop-start flag's update of the repeat address.
    loop_addr_locked: bool,
    /// Raw ADSR_LO / ADSR_HI words. Stored so reads echo them back.
    adsr_lo: u16,
    adsr_hi: u16,
    /// Current ADSR phase.
    phase: AdsrPhase,
    /// Envelope level, 0..=0x7FFF (Q15). Multiplies the decoded sample.
    envelope: i32,
    /// Spaces the envelope's level updates (see [`envelope::Slope`]).
    pacer: Pacer,
    /// Current byte address into SPU RAM for the *next* ADPCM block to
    /// decode. Updated after each block consumed.
    current_addr: u32,
    /// Decoded samples from the most recent 16-byte block (28 samples).
    /// Indexed by `sample_index`. Each sample is saturated to i16
    /// (-0x8000..=0x7FFF) at decode time before it is stored here and fed
    /// back into the ADPCM predictor history, as on real hardware (the
    /// filter runs on the 16-bit-saturated value).
    sample_buf: [i32; ADPCM_SAMPLES_PER_BLOCK],
    /// Index into `sample_buf`; when it reaches 28 we decode the next
    /// block before taking the next sample.
    sample_index: usize,
    /// Position within the sample stream, with 12 fractional bits (PSX-SPX
    /// "SPU ADPCM Pitch"): every output sample adds the voice's pitch step,
    /// and each time the counter passes a whole sample the next decoded
    /// sample is taken in. A new note starts at three whole samples so the
    /// interpolator's window is full before the first output.
    counter: u32,
    /// The four most recent decoded samples, oldest first: the interpolator's
    /// window. It carries across ADPCM block boundaries, so the previous
    /// block's tail is still seen when the next one starts.
    taps: [i16; 4],
    /// Previous two decoded samples -- ADPCM filter history. Preserved
    /// across block boundaries; reset on KON.
    s_1: i32,
    s_2: i32,
    /// Set when a decoded block had the stop flag without a valid
    /// loop. The current 28-sample block must still play out fully; the
    /// voice is turned off when the decoder reaches the *next* block
    /// boundary.
    stop_after_block: bool,
    /// ENDX latch is deferred: a decoded loop-end (flag-1) block sets
    /// this, and ENDX is latched at the *next* block boundary in
    /// `fetch_voice_sample`, i.e. only after the loop-end block's 28
    /// samples have actually played. PSX-SPX set ENDX at
    /// the boundary crossing, not when the loop-end block is decoded.
    endx_pending: bool,
    /// Number of ADPCM blocks decoded since the last key-on. The first
    /// decoded block is the voice's "first block" (`== 1`); that is the
    /// window in which a REPEAT_ADDR write must NOT lock the loop
    /// address so the sample's own loop-start flag can still override it
    /// (the PSX hardware first-block window).
    decoded_block_count: u32,
    /// Most recent interpolated sample output by this voice (post-ADSR,
    /// pre-volume). Kept for reads of the ADSR_CURRENT register and
    /// pitch modulation consumers.
    last_sample: i16,
    /// Ticks remaining before a freshly keyed voice starts stepping its
    /// envelope and producing output. Calibrated against the v1.17 SB4
    /// capture (2026-08-07), where every segment's ring shows nine zero
    /// samples and the first envelope step landing at ring index 9; the
    /// KON-applied-at-end-of-tick model accounts for one of those.
    #[serde(default)]
    start_delay: u8,
}

impl Default for Voice {
    fn default() -> Self {
        Self {
            vol_l: VolumeEnvelope::new(),
            vol_r: VolumeEnvelope::new(),
            raw_pitch: 0,
            start_addr: 0,
            start_addr_raw: 0,
            loop_addr: 0,
            loop_addr_raw: 0,
            loop_addr_locked: false,
            adsr_lo: 0,
            adsr_hi: 0,
            phase: AdsrPhase::Off,
            envelope: 0,
            pacer: Pacer::default(),
            current_addr: 0,
            sample_buf: [0; ADPCM_SAMPLES_PER_BLOCK],
            sample_index: ADPCM_SAMPLES_PER_BLOCK, // forces decode on first tick
            counter: NEW_NOTE_COUNTER,
            taps: [0; 4],
            s_1: 0,
            s_2: 0,
            stop_after_block: false,
            endx_pending: false,
            decoded_block_count: 0,
            last_sample: 0,
            start_delay: 0,
        }
    }
}

impl Voice {
    /// Reset envelope + decode state on KON (key-on). The voice will
    /// start decoding from `start_addr` on the next sample tick.
    fn key_on(&mut self) {
        self.phase = AdsrPhase::Attack;
        self.start_delay = 8;
        self.envelope = 0;
        self.pacer.reset();
        self.current_addr = self.start_addr;
        self.sample_index = ADPCM_SAMPLES_PER_BLOCK;
        self.counter = NEW_NOTE_COUNTER;
        self.taps = [0; 4];
        self.s_1 = 0;
        self.s_2 = 0;
        self.stop_after_block = false;
        self.endx_pending = false;
        self.decoded_block_count = 0;
        self.loop_addr_locked = false;
        self.last_sample = 0;
    }

    /// Trigger release phase on KOFF -- envelope drops toward zero at
    /// the configured release rate. Voice stays audible until envelope
    /// reaches 0, then moves to `Off`.
    fn key_off(&mut self) {
        if self.phase != AdsrPhase::Off {
            self.phase = AdsrPhase::Release;
        }
    }

    /// Take a decoded sample into the interpolation window.
    fn push_tap(&mut self, sample: i16) {
        self.taps = [self.taps[1], self.taps[2], self.taps[3], sample];
    }

    /// Advance the ADSR envelope by one sample. Returns the current
    /// envelope level after the step (0..=0x7FFF, Q15).
    fn step_envelope(&mut self) -> i32 {
        match self.phase {
            // An inactive generator leaves a software-written ENVX value
            // latched. KON explicitly resets it to zero, and Release writes
            // zero when it transitions to Off. Resetting on every sample
            // erased manual negative values before the CPU could read them.
            AdsrPhase::Off => self.envelope,
            AdsrPhase::Attack => self.step_attack(),
            AdsrPhase::Decay => self.step_decay(),
            AdsrPhase::Sustain => self.step_sustain(),
            AdsrPhase::Release => self.step_release(),
        }
    }

    fn step_attack(&mut self) -> i32 {
        if let Some(delta) = self.pacer.tick(attack_slope(self.adsr_lo), self.envelope) {
            self.envelope += delta;
        }
        if self.envelope >= 0x7FFF {
            self.envelope = 0x7FFF;
            self.phase = AdsrPhase::Decay;
        }
        self.envelope
    }

    fn step_decay(&mut self) -> i32 {
        // Decay is always an exponential decrease; the release-mode bit
        // belongs to the release segment only.
        if let Some(delta) = self.pacer.tick(decay_slope(self.adsr_lo), self.envelope) {
            self.envelope = (self.envelope + delta).max(0);
        }
        if self.envelope < sustain_threshold(self.adsr_lo) {
            self.phase = AdsrPhase::Sustain;
        }
        self.envelope
    }

    fn step_sustain(&mut self) -> i32 {
        if let Some(delta) = self.pacer.tick(sustain_slope(self.adsr_hi), self.envelope) {
            self.envelope = (self.envelope + delta).clamp(0, 0x7FFF);
        }
        self.envelope
    }

    fn step_release(&mut self) -> i32 {
        if let Some(delta) = self.pacer.tick(release_slope(self.adsr_hi), self.envelope) {
            self.envelope += delta;
        }
        if self.envelope <= 0 {
            self.envelope = 0;
            self.phase = AdsrPhase::Off;
        }
        self.envelope
    }
}

// ===============================================================
//  Reverb state.
// ===============================================================

/// Indices of the reverb registers in [`Spu::reverb_cfg`], in address order
/// from 0x1F801DC0 (PSX-SPX "SPU Reverb Registers"). `d` registers are
/// displacements, `m` registers buffer addresses and `v` registers volumes;
/// all addresses count in units of 8 bytes. Left/right pairs sit next to
/// each other.
mod reverb_reg {
    pub const D_APF1: usize = 0;
    pub const D_APF2: usize = 1;
    pub const V_IIR: usize = 2;
    pub const V_COMB1: usize = 3;
    pub const V_COMB2: usize = 4;
    pub const V_COMB3: usize = 5;
    pub const V_COMB4: usize = 6;
    pub const V_WALL: usize = 7;
    pub const V_APF1: usize = 8;
    pub const V_APF2: usize = 9;
    pub const M_LSAME: usize = 10;
    pub const M_RSAME: usize = 11;
    pub const M_LCOMB1: usize = 12;
    pub const M_RCOMB1: usize = 13;
    pub const M_LCOMB2: usize = 14;
    pub const M_RCOMB2: usize = 15;
    pub const D_LSAME: usize = 16;
    pub const D_RSAME: usize = 17;
    pub const M_LDIFF: usize = 18;
    pub const M_RDIFF: usize = 19;
    pub const M_LCOMB3: usize = 20;
    pub const M_RCOMB3: usize = 21;
    pub const M_LCOMB4: usize = 22;
    pub const M_RCOMB4: usize = 23;
    pub const D_LDIFF: usize = 24;
    pub const D_RDIFF: usize = 25;
    pub const M_LAPF1: usize = 26;
    pub const M_RAPF1: usize = 27;
    pub const M_LAPF2: usize = 28;
    pub const M_RAPF2: usize = 29;
    pub const V_LIN: usize = 30;
    pub const V_RIN: usize = 31;
}

/// The registers one channel's reverb pass reads.
struct ReverbChannel {
    /// Same-side reflection: target, and the displaced source it mixes in.
    m_same: usize,
    d_same: usize,
    /// Different-side reflection: target, and the other channel's
    /// displaced source.
    m_diff: usize,
    d_other_diff: usize,
    /// The four comb filter source addresses.
    m_comb: [usize; 4],
    m_apf1: usize,
    m_apf2: usize,
    v_in: usize,
}

const REVERB_LEFT: ReverbChannel = ReverbChannel {
    m_same: reverb_reg::M_LSAME,
    d_same: reverb_reg::D_LSAME,
    m_diff: reverb_reg::M_LDIFF,
    d_other_diff: reverb_reg::D_RDIFF,
    m_comb: [
        reverb_reg::M_LCOMB1,
        reverb_reg::M_LCOMB2,
        reverb_reg::M_LCOMB3,
        reverb_reg::M_LCOMB4,
    ],
    m_apf1: reverb_reg::M_LAPF1,
    m_apf2: reverb_reg::M_LAPF2,
    v_in: reverb_reg::V_LIN,
};

const REVERB_RIGHT: ReverbChannel = ReverbChannel {
    m_same: reverb_reg::M_RSAME,
    d_same: reverb_reg::D_RSAME,
    m_diff: reverb_reg::M_RDIFF,
    d_other_diff: reverb_reg::D_LDIFF,
    m_comb: [
        reverb_reg::M_RCOMB1,
        reverb_reg::M_RCOMB2,
        reverb_reg::M_RCOMB3,
        reverb_reg::M_RCOMB4,
    ],
    m_apf1: reverb_reg::M_RAPF1,
    m_apf2: reverb_reg::M_RAPF2,
    v_in: reverb_reg::V_RIN,
};

/// Runtime state for the SPU reverb work area. The coefficient and
/// offset registers live in `Spu::reverb_cfg`; this tracks the moving
/// buffer address and the two channels' latest outputs.
///
/// The hardware spends one 44.1 kHz cycle on the left channel and the next
/// on the right, advancing the buffer address once per pair (so the effect
/// runs at 22.05 kHz); each channel's output holds until it is next
/// computed.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct ReverbState {
    /// Current buffer address in SPU RAM halfwords.
    curr_addr: u32,
    /// Latest output of each channel, volume applied.
    wet_l: i32,
    wet_r: i32,
    /// True when the next processed sample is the right channel's.
    right_next: bool,
}

impl ReverbState {
    fn new() -> Self {
        Self::default()
    }

    fn reset_output(&mut self) {
        self.wet_l = 0;
        self.wet_r = 0;
    }
}

// ===============================================================
//  SPU top-level state.
// ===============================================================

/// Full SPU state. Owns SPU RAM, all 24 voices, the register bank,
/// and the output audio buffer.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Spu {
    /// 512 KiB SPU RAM as u16 (256K halfwords). ADPCM blocks + reverb
    /// work area + decoded-buffer captures live here. DMA channel 4
    /// writes streams of u16s via [`Spu::dma_write`]; software sees
    /// round-trip consistency through the TRANSFER_FIFO register.
    /// Boxed array is well past serde's built-in 32-element cap, so it
    /// round-trips through [`crate::serde_big_array::boxed_array`].
    #[serde(with = "crate::serde_big_array::boxed_array")]
    ram: Box<[u16; SPU_RAM_HALFWORDS]>,
    /// 24 voices.
    voices: [Voice; NUM_VOICES],

    /// SPU control register (0x1F80_1DAA). Bit 15 = SPU enable, bit 6 =
    /// IRQ enable, bits 5..4 = RAM transfer mode, bit 7 = reverb master.
    spucnt: u16,
    /// SPU status register (0x1F80_1DAE). Lower 6 bits mirror SPUCNT.
    /// Bit 6 = IRQ-triggered latch (cleared by SPUCNT write with bit 6 clear).
    spustat: u16,
    /// Applied and pending copies of SPUSTAT's low-six-bit SPUCNT mirror.
    /// Silicon updates this mirror on the next 44.1 kHz sample boundary,
    /// independently from the immediately-readable SPUCNT write latch.
    #[serde(default)]
    spustat_control: u16,
    #[serde(default)]
    spustat_control_pending: Option<(u16, u64)>,
    /// DMA request bits 7..9 are asserted only while channel 4 is actively
    /// transferring, not merely because SPUCNT selects a DMA mode.
    #[serde(default)]
    dma_active: bool,
    /// Most recent non-Stop RAM transfer mode. SCPH-9902 returns to Stop a
    /// little sooner after DMA Read than after the write/manual paths.
    #[serde(default)]
    last_active_transfer_mode: u8,
    /// Enable transfer/capture timing measured on the late PAL SCPH-9902.
    /// Earlier consoles expose the conventional capture-half polarity and
    /// accept ManualWrite FIFO contents as soon as the control latch changes.
    #[serde(default)]
    scph_9902_timing: bool,
    /// IRQ address (byte addr into SPU RAM). Written as halfword value
    /// which is scaled by 8.
    irq_addr: u32,
    /// Data transfer address (current write position in SPU RAM, bytes).
    transfer_addr: u32,
    /// Raw transfer-address register value (halfword / 8 address).
    /// Stored so reads round-trip the software-visible value.
    transfer_addr_raw: u16,
    /// Data transfer control (usually 0x0004). Stored for round-trip.
    transfer_ctrl: u16,
    /// 32-halfword hardware transfer FIFO. CPU writes may fill this while
    /// SPUCNT is stopped; switching to ManualWrite drains the queued words to
    /// SPU RAM. Keeping the queue in architectural state is required for
    /// save-state fidelity and for the public ps1-tests memory-transfer case.
    transfer_fifo: std::collections::VecDeque<u16>,

    /// Main output volume Left.
    main_vol_l: VolumeEnvelope,
    /// Main output volume Right.
    main_vol_r: VolumeEnvelope,
    /// Current-main-volume registers, read back at 0x1F801DB8/BA. Writes to
    /// the main volume registers do not update them.
    current_main_vol_l: u16,
    current_main_vol_r: u16,
    /// Reverb output volume Left.
    reverb_vol_l: VolumeEnvelope,
    /// Reverb output volume Right.
    reverb_vol_r: VolumeEnvelope,
    /// CD audio input volume Left.
    cd_vol_l: VolumeEnvelope,
    /// CD audio input volume Right.
    cd_vol_r: VolumeEnvelope,
    /// External audio input volume Left.
    ext_vol_l: VolumeEnvelope,
    /// External audio input volume Right.
    ext_vol_r: VolumeEnvelope,
    /// Raw reverb work-area start register. Reads must round-trip the
    /// CPU-visible word even when the effective mixer start is disabled.
    reverb_base_raw: u16,
    /// Reverb work-area start (byte address).
    reverb_base: u32,
    /// Reverb cursor / wet-output history.
    reverb: ReverbState,

    /// KON last-written register value -- echoed back on reads so the
    /// BIOS's round-trip verification sees consistency. Real hardware
    /// latches the write into the register and acts on it one sample
    /// later; we decouple into a separate pending bitmap below.
    kon_raw: u32,
    /// KON pending bitmap -- set by software writes, consumed by the
    /// sample tick (voices start on their next tick to match hardware's
    /// one-sample KON latency). Drained via `mem::take` each sample.
    kon_pending: u32,
    /// KOFF last-written register value.
    koff_raw: u32,
    /// KOFF pending bitmap.
    koff_pending: u32,
    /// Diagnostic per-voice activity: key-on count and number of samples
    /// that produced nonzero output. A voice keyed but never voiced is the
    /// signature of a missing-instrument bug (ADSR/decode failure). Not on
    /// any hot register path. Excluded from save states.
    #[serde(skip)]
    dbg_kon_count: [u32; NUM_VOICES],
    #[serde(skip)]
    dbg_voiced_samples: [u32; NUM_VOICES],
    /// Per-voice note-end reason tally: key-off (game gate) vs sample-stop
    /// (one-shot/loop end). Distinguishes a gate/release cutoff from samples
    /// ending early. Excluded from save states.
    #[serde(skip)]
    dbg_koff_count: [u32; NUM_VOICES],
    #[serde(skip)]
    dbg_sampstop_count: [u32; NUM_VOICES],
    /// Voice config captured at the last key-on: (start_addr, adsr_lo,
    /// adsr_hi, raw_pitch, vol_l, vol_r). Reveals why a keyed voice stays
    /// silent (bad start address, dead ADSR, zero volume). Excluded from
    /// save states.
    #[serde(skip)]
    dbg_keyon_cfg: [(u32, u16, u16, u16, i16, i16); NUM_VOICES],
    /// Per-voice envelope/decode trace, snapshotted every 1024 output
    /// samples: (decoded_sample_pre_adsr, envelope_level, phase). Bisects
    /// premature note cutoff -- decoded->0 with envelope held = decode/loop
    /// bug; envelope->0 = ADSR/key-off. Capped, diagnostic-only. Excluded
    /// from save states.
    #[serde(skip)]
    dbg_trace: [Vec<(i16, i32, u8)>; NUM_VOICES],
    /// Output-sample clock that gates dbg_trace snapshots. Excluded from
    /// save states.
    #[serde(skip)]
    dbg_sample_idx: u32,
    /// Per-voice running max of |decoded sample| and envelope within the
    /// current trace window; pushed + reset at each snapshot boundary.
    /// Excluded from save states.
    #[serde(skip)]
    dbg_acc_smax: [i32; NUM_VOICES],
    #[serde(skip)]
    dbg_acc_emax: [i32; NUM_VOICES],
    /// Accumulated |dry| and |wet (reverb)| output magnitude over the run.
    /// A near-zero wet/dry ratio means the reverb bus is not contributing --
    /// missing reverb tails read as dry, "cut-off" notes vs the oracle.
    /// Excluded from save states.
    #[serde(skip)]
    dbg_dry_energy: u64,
    #[serde(skip)]
    dbg_wet_energy: u64,
    /// OR-accumulated pmon / noise bitmaps over the whole run -- captures
    /// any voice ever pitch-modulated or noise-mode, not just the end state.
    /// Excluded from save states.
    #[serde(skip)]
    dbg_pmon_ever: u32,
    #[serde(skip)]
    dbg_noise_ever: u32,
    /// Voice pitch-modulation enable bitmap. Bit N means voice N takes
    /// its pitch from voice N-1's output sample.
    pmon: u32,
    /// Noise-mode enable bitmap. Bit N means voice N plays noise
    /// instead of its ADPCM sample.
    noise_on: u32,
    /// Reverb enable bitmap per voice.
    reverb_on: u32,
    /// ENDX latch -- each voice sets its bit when an ADPCM block with
    /// flag-1 (loop-end) was decoded. Software reads + write-1-to-clears.
    endx_latched: u32,

    /// Reverb configuration area (0x1F80_1DC0..=0x1F80_1DFE). Stored
    /// verbatim for round-trip reads. Mix path is not wired yet.
    reverb_cfg: [u16; 32],

    /// Host-facing stereo output buffer. Frontend pulls periodically via
    /// [`Spu::drain_audio`]. Oldest-sample-dropped when cap exceeded.
    /// Transient host-audio plumbing -- excluded from save states, reset
    /// empty on load (there is nothing to "resume" in an audio queue).
    #[serde(skip)]
    audio_out: std::collections::VecDeque<(i16, i16)>,

    /// CD audio input queue -- stereo samples fed by the CDROM
    /// controller during CD-DA or XA ADPCM playback. The SPU's
    /// `tick_sample` path drains one sample per output sample and
    /// mixes it via `CD_VOL_L/R` into the main output. When the
    /// queue is empty, CD contribution is zero. Bounded at
    /// ~0.5 s to prevent runaway growth during emulator fast-
    /// forward. This is pending emulated input rather than host
    /// output: consuming it can mutate SPU RAM/capture state, so it
    /// must round-trip through save states.
    cd_audio_in: std::collections::VecDeque<(i16, i16)>,

    /// Absolute cycle count at which we last produced an audio sample.
    /// Used to catch up when the scheduler delivers a burst of ticks.
    last_sample_cycle: u64,
    /// Total samples produced since reset -- diagnostic counter.
    /// Excluded from save states.
    #[serde(skip)]
    samples_produced: u64,
    /// Capture-buffer IRQ cursor. The first 0x1000 bytes of SPU RAM hold
    /// four 0x400-byte capture rings; the cursor advances by one halfword
    /// per output sample and can trigger SPU IRQs for games that
    /// synchronise audio streaming on that low-memory address range.
    decode_irq_cursor: u32,
    /// SPU capture-buffer write index (byte offset 0..=0x3FE, even).
    /// Per the PSX-SPX spec the SPU mirrors CD-L/R and Voice1/Voice3 into
    /// SPU RAM 0x000/0x400/0x800/0xC00 each 44.1 kHz sample, all sharing
    /// this 0x400-byte ring index. SPUSTAT bit 11 reports which half of
    /// the ring is currently being written.
    capture_buffer_pos: u16,
    /// SPU IRQ pending flag -- bus drains this to decide whether to
    /// raise `IrqSource::Spu`. Set when an enabled IRQ-addr match
    /// occurs on a voice's read pointer or the transfer-FIFO write.
    irq_pending: bool,

    /// Current noise-generator output sample. Updated on each SPU
    /// tick at a rate controlled by SPUCNT bits 8-13 (noise clock /
    /// shift). Voices with their NON_LO/HI bit set emit this value
    /// instead of their ADPCM sample.
    noise_val: i16,
    /// Countdown to the next noise-level update (see [`Spu::noise_tick`]),
    /// zero at power-on.
    noise_timer: i32,
}

impl Default for Spu {
    fn default() -> Self {
        Self::new()
    }
}

impl Spu {
    /// Freshly-reset SPU. RAM is zeroed, voices are silent, registers
    /// at hardware defaults (SPUCNT = 0, SPUSTAT = 0). Software's first
    /// job is to write SPUCNT with the enable bit set, then seed voice
    /// registers + sample RAM before key-on.
    pub fn new() -> Self {
        // SAFETY: zeroed Box<[u16; N]> requires a zeroed alloc.
        let ram = vec![0u16; SPU_RAM_HALFWORDS]
            .into_boxed_slice()
            .try_into()
            .expect("exact size");
        Self {
            ram,
            voices: std::array::from_fn(|_| Voice::default()),
            spucnt: 0,
            spustat: 0,
            spustat_control: 0,
            spustat_control_pending: None,
            dma_active: false,
            last_active_transfer_mode: 0,
            scph_9902_timing: false,
            irq_addr: 0,
            transfer_addr: 0,
            transfer_addr_raw: 0,
            transfer_ctrl: 0x0004,
            transfer_fifo: std::collections::VecDeque::with_capacity(32),
            main_vol_l: VolumeEnvelope::new(),
            main_vol_r: VolumeEnvelope::new(),
            current_main_vol_l: 0,
            current_main_vol_r: 0,
            reverb_vol_l: VolumeEnvelope::new(),
            reverb_vol_r: VolumeEnvelope::new(),
            cd_vol_l: VolumeEnvelope::new(),
            cd_vol_r: VolumeEnvelope::new(),
            ext_vol_l: VolumeEnvelope::new(),
            ext_vol_r: VolumeEnvelope::new(),
            reverb_base_raw: 0,
            reverb_base: 0,
            reverb: ReverbState::new(),
            kon_raw: 0,
            kon_pending: 0,
            koff_raw: 0,
            koff_pending: 0,
            dbg_kon_count: [0; NUM_VOICES],
            dbg_voiced_samples: [0; NUM_VOICES],
            dbg_koff_count: [0; NUM_VOICES],
            dbg_sampstop_count: [0; NUM_VOICES],
            dbg_keyon_cfg: [(0, 0, 0, 0, 0, 0); NUM_VOICES],
            dbg_trace: core::array::from_fn(|_| Vec::new()),
            dbg_sample_idx: 0,
            dbg_acc_smax: [0; NUM_VOICES],
            dbg_acc_emax: [0; NUM_VOICES],
            dbg_dry_energy: 0,
            dbg_wet_energy: 0,
            dbg_pmon_ever: 0,
            dbg_noise_ever: 0,
            pmon: 0,
            noise_on: 0,
            reverb_on: 0,
            endx_latched: 0,
            reverb_cfg: [0; 32],
            audio_out: std::collections::VecDeque::with_capacity(OUTPUT_BUFFER_CAP),
            cd_audio_in: std::collections::VecDeque::with_capacity(OUTPUT_BUFFER_CAP),
            last_sample_cycle: 0,
            samples_produced: 0,
            decode_irq_cursor: 0,
            capture_buffer_pos: 0,
            irq_pending: false,
            noise_val: 1,
            noise_timer: 0,
        }
    }

    /// Select the transfer/capture behavior measured on a late PAL PSone.
    pub fn apply_scph_9902_profile(&mut self) {
        self.scph_9902_timing = true;
        if self.capture_buffer_pos >= 0x200 {
            self.spustat |= 1 << 11;
        } else {
            self.spustat &= !(1 << 11);
        }
    }

    /// Apply the retail BIOS shell audio state inherited by a disc executable.
    ///
    /// A real-console PA5 capture established that the BIOS hands off with a
    /// fully configured reverb preset: all voices routed to EON, non-zero wet
    /// depth, and a work area at `0xE128 * 8`. Warm disc fast boot must retain
    /// this observable peripheral state even though it skips the license/shell
    /// path that normally programs it. Games remain responsible for resetting
    /// the SPU before loading their own banks.
    pub fn apply_retail_bios_shell_audio_profile(&mut self) {
        const BIOS_REVERB_CFG: [u16; 32] = [
            0x033D, 0x0231, 0x7E00, 0x5000, 0xB400, 0xB000, 0x4C00, 0xB000, 0x6000, 0x5400, 0x1ED6,
            0x1A31, 0x1D14, 0x183B, 0x1BC2, 0x16B2, 0x1A32, 0x15EF, 0x15EE, 0x1055, 0x1334, 0x0F2D,
            0x11F6, 0x0C5D, 0x1056, 0x0AE1, 0x0AE0, 0x07A2, 0x0464, 0x0232, 0x8000, 0x8000,
        ];

        self.write16(REVERB_VOL_L, 0x5EBC);
        self.write16(REVERB_VOL_R, 0x5EBC);
        self.write16(REVERB_BASE, 0xE128);
        self.write16(EON_LO, 0xFFFF);
        self.write16(EON_HI, 0x00FF);
        self.write16(EXT_VOL_L, 0);
        self.write16(EXT_VOL_R, 0);
        for (index, value) in BIOS_REVERB_CFG.into_iter().enumerate() {
            self.write16(REVERB_CFG_BASE + index as u32 * 2, value);
        }

        // PA5 sampled SPUCNT/SPUSTAT as C085/0805 before the SDK touched the
        // device. Apply the already-settled control mirror rather than leaving
        // a synthetic one-sample pending transition at the EXE entry point.
        self.spucnt = 0xC085;
        self.spustat_control = 0x0005;
        self.spustat_control_pending = None;
        self.spustat = (self.spustat & !0x083F) | 0x0800 | self.spustat_control;
        self.reverb.reset_output();
    }

    /// SPU state at EXE entry for the HLE boot path: the retail shell
    /// profile above plus the main volume (3FFFh/37EFh), CD input volume
    /// (0/0) and transfer control (0004h) measured at EXE entry under the
    /// retail BIOS on every census disc.
    pub fn apply_hle_entry_audio_profile(&mut self) {
        self.apply_retail_bios_shell_audio_profile();
        self.write16(MAIN_VOL_L, 0x3FFF);
        self.write16(MAIN_VOL_R, 0x37EF);
        self.write16(CD_VOL_L, 0);
        self.write16(CD_VOL_R, 0);
        self.write16(TRANSFER_CTRL, 0x0004);
    }

    /// Advance the noise generator by one SPU sample (PSX-SPX "SPU Noise
    /// Generator"). SPUCNT bits 13..10 are the shift and bits 9..8 the step
    /// (4 to 7). Every sample the timer drops by the step; when it goes
    /// negative the 16-bit noise level shifts left, taking in the XNOR of
    /// its bits 15, 12, 11 and 10, and the timer is topped up by
    /// `0x20000 >> shift` (twice if one top-up is not enough).
    fn noise_tick(&mut self) {
        let shift = (self.spucnt >> 10) & 0xF;
        let step = 4 + i32::from((self.spucnt >> 8) & 3);
        self.noise_timer -= step;
        if self.noise_timer >= 0 {
            return;
        }
        let level = self.noise_val as u16;
        let parity = !((level >> 15) ^ (level >> 12) ^ (level >> 11) ^ (level >> 10)) & 1;
        self.noise_val = ((level << 1) | parity) as i16;
        let reload = 0x2_0000 >> shift;
        self.noise_timer += reload;
        if self.noise_timer < 0 {
            self.noise_timer += reload;
        }
    }

    /// Enqueue a batch of stereo samples from the CDROM -- either
    /// CD-DA (Red Book) or decoded XA ADPCM. Consumed one sample
    /// per SPU output sample during `tick_sample`. Scaled by
    /// `CD_VOL_L/R` before mix. Caps at ~0.5 s of queued audio to
    /// keep memory bounded under fast-forward.
    pub fn feed_cd_audio(&mut self, samples: &[(i16, i16)]) {
        let cap = 22_050; // ~0.5 s at 44.1 kHz
        let overflow = (self.cd_audio_in.len() + samples.len()).saturating_sub(cap);
        for _ in 0..overflow {
            self.cd_audio_in.pop_front();
        }
        self.cd_audio_in.extend(samples.iter().copied());
    }

    /// Depth of the CD audio input queue. Diagnostic.
    pub fn cd_audio_queue_len(&self) -> usize {
        self.cd_audio_in.len()
    }

    /// Low edge of the SPU MMIO range.
    pub const BASE: u32 = SPU_BASE;
    /// High edge (exclusive). 0x200 bytes total.
    pub const END: u32 = SPU_END;

    /// `true` when `phys` falls inside the SPU register region.
    pub fn contains(phys: u32) -> bool {
        (Self::BASE..Self::END).contains(&phys)
    }

    /// Schedule the first SPU sample tick. Bus calls this once during
    /// construction -- subsequent reschedules happen inside the drain
    /// handler. We tick every [`SAMPLE_CYCLES`] cycles.
    pub fn seed_scheduler(scheduler: &mut Scheduler, now: u64) {
        scheduler.schedule(EventSlot::SpuMix, now, SAMPLE_CYCLES);
    }

    /// Current SPUCNT value.
    pub fn spucnt(&self) -> u16 {
        self.spucnt
    }

    /// Current SPUSTAT value. Lower 6 bits mirror SPUCNT; bit 6 is the
    /// IRQ latch (set when an enabled SPU IRQ has fired, cleared by
    /// software writing SPUCNT with bit 6 clear). Bits 7..9 are the
    /// DMA-request bits synthesised from the SPUCNT transfer mode, and
    /// bit 11 (capture-buffer half) is held in `self.spustat`.
    pub fn spustat(&self) -> u16 {
        self.spustat_at(u64::MAX)
    }

    /// SPUSTAT sampled at a CPU cycle. The low control mirror crosses into
    /// the status domain at the next SPU sample edge.
    pub fn spustat_at(&self, now: u64) -> u16 {
        // Bits 0..5 mirror SPUCNT; bits 6/10/11 are held in self.spustat
        // (IRQ latch / transfer-busy / capture-buffer half).
        let control = match self.spustat_control_pending {
            Some((value, deadline)) if deadline <= now => value,
            _ => self.spustat_control,
        };
        let mut s = (self.spustat & !0x3F) | control;
        // DMA-request status bits (PSX-SPX SPUSTAT): bit 7 = DMA
        // read/write request (mirrors SPUCNT bit 5, i.e. set for both
        // DMA transfer modes), bit 8 = DMA write request (transfer
        // mode 2), bit 9 = DMA read request (transfer mode 3).
        match ((self.spucnt >> 4) & 3, self.dma_active) {
            (2, true) => s |= (1 << 7) | (1 << 8),
            (3, true) => s |= (1 << 7) | (1 << 9),
            _ => {}
        }
        s
    }

    /// Mark DMA channel 4 as owning the SPU transfer engine.
    pub fn begin_dma(&mut self, now: u64) {
        self.dma_active = true;
        // DMA-read mode's control mirror is gated by ownership of the
        // transfer engine on SCPH-9902. Once channel 4 is armed it crosses at
        // the normal sample boundary, just like the other control modes.
        if (self.spucnt >> 4) & 3 == 3 {
            let next_sample = now
                .saturating_div(SAMPLE_CYCLES)
                .saturating_add(1)
                .saturating_mul(SAMPLE_CYCLES);
            self.spustat_control_pending = Some((self.spucnt & 0x3F, next_sample));
        }
    }

    /// Release the SPU transfer engine when the scheduled DMA completes.
    pub fn end_dma(&mut self) {
        self.dma_active = false;
    }

    /// Diagnostic: total samples produced since reset. One sample pair
    /// per [`SAMPLE_CYCLES`] cycles.
    pub fn samples_produced(&self) -> u64 {
        self.samples_produced
    }

    /// Raw SPU RAM for deterministic headless diagnostics and save tooling.
    pub fn ram_halfwords(&self) -> &[u16] {
        &self.ram[..]
    }

    /// Diagnostic SPU IRQ state: (irq_addr, spucnt, spustat, decode cursor,
    /// cd-audio queue len). For tracing games that sync on the SPU IRQ.
    pub fn debug_irq_state(&self) -> (u32, u16, u16, u32, usize) {
        (
            self.irq_addr,
            self.spucnt,
            self.spustat,
            self.decode_irq_cursor,
            self.cd_audio_in.len(),
        )
    }

    /// Drain pending host-facing stereo samples. Frontend calls this
    /// every frame to feed its audio output. Returns `(left, right)`
    /// pairs in playback order, oldest first.
    pub fn drain_audio(&mut self) -> Vec<(i16, i16)> {
        self.audio_out.drain(..).collect()
    }

    /// Drop the queued host-output samples, for callers with nowhere to play
    /// them (headless runs). The output queue is host-side only: what it
    /// holds never feeds back into the emulated SPU.
    pub fn discard_audio(&mut self) {
        self.audio_out.clear();
    }

    /// How many stereo samples are queued but not yet drained.
    pub fn audio_queue_len(&self) -> usize {
        self.audio_out.len()
    }

    /// Diagnostic: per-voice (key-on count, samples that produced nonzero
    /// output). A voice with key-ons but ~no voiced samples is keyed but
    /// silent -- the signature of a missing-instrument bug.
    pub fn voice_debug_counts(&self) -> ([u32; NUM_VOICES], [u32; NUM_VOICES]) {
        (self.dbg_kon_count, self.dbg_voiced_samples)
    }

    /// Per-voice ADSR envelope level (0..=0x7FFF), zero for a voice whose
    /// envelope is off. For the debug UI's voice meters.
    pub fn voice_envelope_levels(&self) -> [u16; NUM_VOICES] {
        std::array::from_fn(|v| {
            let voice = &self.voices[v];
            if voice.phase == AdsrPhase::Off {
                0
            } else {
                voice.envelope.clamp(0, 0x7FFF) as u16
            }
        })
    }

    /// Diagnostic: per-voice note-end tally (key-off count, sample-stop count).
    pub fn voice_end_counts(&self) -> ([u32; NUM_VOICES], [u32; NUM_VOICES]) {
        (self.dbg_koff_count, self.dbg_sampstop_count)
    }

    /// Diagnostic: per-voice (start_addr, adsr_lo, adsr_hi, raw_pitch,
    /// vol_l, vol_r) captured at the last key-on.
    pub fn voice_trace(&self) -> &[Vec<(i16, i32, u8)>; NUM_VOICES] {
        &self.dbg_trace
    }

    /// Diagnostic: (dry_energy, wet_energy, spucnt, reverb_on, pmon, noise_on)
    /// to gauge reverb activity + which voices are pitch-modulated / noise.
    pub fn reverb_debug(&self) -> (u64, u64, u16, u32, u32, u32) {
        (
            self.dbg_dry_energy,
            self.dbg_wet_energy,
            self.spucnt,
            self.reverb_on,
            self.dbg_pmon_ever,
            self.dbg_noise_ever,
        )
    }

    /// Diagnostic: per-voice (start_addr, adsr_lo, adsr_hi, raw_pitch,
    /// vol_l, vol_r) captured at the last key-on.
    pub fn voice_keyon_cfg(&self) -> [(u32, u16, u16, u16, i16, i16); NUM_VOICES] {
        self.dbg_keyon_cfg
    }

    /// True when the SPU has an IRQ to report. Bus drains this flag
    /// once per scheduler tick and raises `IrqSource::Spu` if so.
    pub fn take_irq_pending(&mut self) -> bool {
        std::mem::replace(&mut self.irq_pending, false)
    }

    /// True when SPU DMA (channel 4) is currently accepting transfers.
    /// Bus uses this to gate CHCR start-bit triggers on channel 4.
    pub fn dma_transfer_enabled(&self) -> bool {
        // SPUCNT bits 5..4 = 2 (DMA write) or 3 (DMA read) means RAM
        // transfer is DMA-driven.
        matches!((self.spucnt >> 4) & 3, 2 | 3)
    }

    /// Whether DMA-write mode has crossed into the SPU sample-clock domain.
    /// The SPUCNT latch changes immediately, but the transfer engine cannot
    /// accept RAM data until the corresponding SPUSTAT mirror is applied.
    pub fn dma_write_ready_at(&self, now: u64) -> bool {
        (self.spustat_at(now) >> 4) & 3 == 2
    }

    /// Diagnostic: current SPU-RAM transfer (write/read) cursor.
    pub fn transfer_addr(&self) -> u32 {
        self.transfer_addr
    }

    // ============================================================
    //  Register access -- byte / halfword / word, with cycle context.
    // ============================================================

    /// 8-bit read. Pulls the right byte out of the underlying 16-bit
    /// register.
    pub fn read8(&self, phys: u32) -> u8 {
        let word = self.read16_at(phys & !1, 0);
        if phys & 1 == 0 {
            word as u8
        } else {
            (word >> 8) as u8
        }
    }

    /// 8-bit write. Merges into the existing 16-bit register.
    pub fn write8(&mut self, phys: u32, value: u8) {
        let aligned = phys & !1;
        let word = self.read16_at(aligned, 0);
        let merged = if phys & 1 == 0 {
            (word & 0xFF00) | value as u16
        } else {
            (word & 0x00FF) | ((value as u16) << 8)
        };
        self.write16_at(aligned, merged, 0);
    }

    /// 32-bit read. Splits into two halfword reads; upper half is 0 on
    /// real hardware for most registers.
    pub fn read32(&self, phys: u32) -> u32 {
        self.read32_at(phys, 0)
    }

    /// 32-bit read with cycle context.
    pub fn read32_at(&self, phys: u32, now: u64) -> u32 {
        let lo = self.read16_at(phys, now) as u32;
        let hi = self.read16_at(phys.wrapping_add(2), now) as u32;
        lo | (hi << 16)
    }

    /// 16-bit read (no cycle context -- used by legacy callers; all
    /// registers we care about are cycle-independent).
    pub fn read16(&self, phys: u32) -> u16 {
        self.read16_at(phys, 0)
    }

    /// 16-bit read with cycle context.
    pub fn read16_at(&self, phys: u32, now: u64) -> u16 {
        let phys = phys & !1;
        if let Some((v, off)) = decode_voice(phys) {
            return self.read_voice_reg(v, off);
        }
        match phys {
            MAIN_VOL_L => self.main_vol_l.reg_read(),
            MAIN_VOL_R => self.main_vol_r.reg_read(),
            CURRENT_MAIN_VOL_L => self.current_main_vol_l,
            CURRENT_MAIN_VOL_R => self.current_main_vol_r,
            REVERB_VOL_L => self.reverb_vol_l.reg_read(),
            REVERB_VOL_R => self.reverb_vol_r.reg_read(),
            KON_LO => self.kon_raw as u16,
            KON_HI => (self.kon_raw >> 16) as u16,
            KOFF_LO => self.koff_raw as u16,
            KOFF_HI => (self.koff_raw >> 16) as u16,
            PMON_LO => self.pmon as u16,
            PMON_HI => (self.pmon >> 16) as u16,
            NON_LO => self.noise_on as u16,
            NON_HI => (self.noise_on >> 16) as u16,
            EON_LO => self.reverb_on as u16,
            EON_HI => (self.reverb_on >> 16) as u16,
            ENDX_LO => self.endx_latched as u16,
            ENDX_HI => (self.endx_latched >> 16) as u16,
            REVERB_BASE => self.reverb_base_raw,
            IRQ_ADDR => (self.irq_addr >> 3) as u16,
            TRANSFER_ADDR => self.transfer_addr_raw,
            TRANSFER_FIFO => self.transfer_fifo_read(),
            SPUCNT => self.spucnt,
            TRANSFER_CTRL => self.transfer_ctrl,
            SPUSTAT => self.spustat_at(now),
            CD_VOL_L => self.cd_vol_l.reg_read(),
            CD_VOL_R => self.cd_vol_r.reg_read(),
            EXT_VOL_L => self.ext_vol_l.reg_read(),
            EXT_VOL_R => self.ext_vol_r.reg_read(),
            a if (REVERB_CFG_BASE..REVERB_CFG_BASE + 64).contains(&a) => {
                let idx = ((a - REVERB_CFG_BASE) >> 1) as usize;
                self.reverb_cfg[idx]
            }
            _ => 0,
        }
    }

    /// 16-bit write.
    pub fn write16(&mut self, phys: u32, value: u16) {
        self.write16_at(phys, value, 0);
    }

    /// 16-bit write with cycle context.
    pub fn write16_at(&mut self, phys: u32, value: u16, now: u64) {
        let phys = phys & !1;
        if let Some((v, off)) = decode_voice(phys) {
            self.write_voice_reg(v, off, value);
            return;
        }
        match phys {
            MAIN_VOL_L => self.main_vol_l.write(value),
            MAIN_VOL_R => self.main_vol_r.write(value),
            CURRENT_MAIN_VOL_L => self.current_main_vol_l = value,
            CURRENT_MAIN_VOL_R => self.current_main_vol_r = value,
            REVERB_VOL_L => self.reverb_vol_l.write_signed_q15(value),
            REVERB_VOL_R => self.reverb_vol_r.write_signed_q15(value),
            KON_LO => self.queue_kon(value, 0),
            KON_HI => self.queue_kon(value, 16),
            KOFF_LO => self.queue_koff(value, 0),
            KOFF_HI => self.queue_koff(value, 16),
            PMON_LO => self.pmon = (self.pmon & 0xFFFF_0000) | value as u32,
            PMON_HI => self.pmon = (self.pmon & 0x0000_FFFF) | ((value as u32) << 16),
            NON_LO => self.noise_on = (self.noise_on & 0xFFFF_0000) | value as u32,
            NON_HI => self.noise_on = (self.noise_on & 0x0000_FFFF) | ((value as u32) << 16),
            EON_LO => self.reverb_on = (self.reverb_on & 0xFFFF_0000) | value as u32,
            EON_HI => self.reverb_on = (self.reverb_on & 0x0000_FFFF) | ((value as u32) << 16),
            ENDX_LO => self.endx_latched &= !(value as u32),
            ENDX_HI => self.endx_latched &= !((value as u32) << 16),
            REVERB_BASE => self.write_reverb_base(value),
            IRQ_ADDR => self.irq_addr = (value as u32) << 3,
            TRANSFER_ADDR => {
                self.transfer_addr_raw = value;
                self.transfer_addr = (value as u32) << 3;
            }
            TRANSFER_FIFO => self.transfer_fifo_write(value, now),
            SPUCNT => self.write_spucnt(value, now),
            TRANSFER_CTRL => self.transfer_ctrl = value,
            SPUSTAT => { /* read-only -- writes dropped */ }
            CD_VOL_L => self.cd_vol_l.write_signed_q15(value),
            CD_VOL_R => self.cd_vol_r.write_signed_q15(value),
            EXT_VOL_L => self.ext_vol_l.write_signed_q15(value),
            EXT_VOL_R => self.ext_vol_r.write_signed_q15(value),
            a if (REVERB_CFG_BASE..REVERB_CFG_BASE + 64).contains(&a) => {
                let idx = ((a - REVERB_CFG_BASE) >> 1) as usize;
                self.reverb_cfg[idx] = value;
            }
            _ => {}
        }
    }

    /// 32-bit write -- splits into two halfword writes.
    pub fn write32(&mut self, phys: u32, value: u32) {
        self.write32_at(phys, value, 0);
    }

    /// 32-bit write with cycle context.
    pub fn write32_at(&mut self, phys: u32, value: u32, now: u64) {
        self.write16_at(phys, value as u16, now);
        self.write16_at(phys.wrapping_add(2), (value >> 16) as u16, now);
    }

    fn write_reverb_base(&mut self, value: u16) {
        self.reverb_base_raw = value;
        // A work area at or inside the capture-buffer region, or the
        // cleared value 0xFFFF, is treated as "reverb off": keep the
        // register readable, but disable the effective work area so games
        // don't smear over low SPU RAM when they are just clearing the
        // mixer. Hardware has no such rule; this is an unmeasured guard.
        if value == 0xFFFF || value <= 0x0200 {
            self.reverb_base = 0;
            self.reverb.curr_addr = 0;
            self.reverb.reset_output();
            return;
        }

        let byte_addr = ((value as u32) << 3) & (SPU_RAM_BYTES as u32 - 1);
        if self.reverb_base != byte_addr {
            self.reverb_base = byte_addr;
            self.reverb.curr_addr = byte_addr >> 1;
            self.reverb.reset_output();
        }
    }

    fn write_spucnt(&mut self, value: u16, now: u64) {
        let prev = self.spucnt;
        if let Some((pending, deadline)) = self.spustat_control_pending {
            if deadline <= now {
                self.spustat_control = pending;
                self.spustat_control_pending = None;
            }
        }
        self.spucnt = value;
        let transfer_mode = (value >> 4) & 3;
        if self.scph_9902_timing {
            // All transfer modes cross into SPUSTAT at the next sample
            // boundary (SB4 2026-08-07: the mode-3 mirror settles in 24-27
            // polls with DMA verifiably not yet armed); Stop keeps its
            // measured asynchronous settle.
            if transfer_mode != 0 {
                self.last_active_transfer_mode = transfer_mode as u8;
            }
            let next_sample = if transfer_mode == 0 {
                let settle_cycles = if self.last_active_transfer_mode == 3 {
                    832
                } else {
                    928
                };
                now.saturating_add(settle_cycles)
            } else {
                now.saturating_div(SAMPLE_CYCLES)
                    .saturating_add(1)
                    .saturating_mul(SAMPLE_CYCLES)
            };
            self.spustat_control_pending = Some((value & 0x3F, next_sample));
        } else {
            let next_sample = now
                .saturating_div(SAMPLE_CYCLES)
                .saturating_add(1)
                .saturating_mul(SAMPLE_CYCLES);
            self.spustat_control_pending = Some((value & 0x3F, next_sample));
            if transfer_mode == 1 {
                self.drain_transfer_fifo_to_ram();
            }
        }
        // SPU IRQ enable transitioned to 0 → clear status latch (ack).
        if (prev & (1 << 6)) != 0 && (value & (1 << 6)) == 0 {
            self.spustat &= !(1 << 6);
        }
    }

    fn read_voice_reg(&self, v: usize, off: u32) -> u16 {
        let voice = &self.voices[v];
        match off {
            voice_offset::VOLUME_L => voice.vol_l.reg_read(),
            voice_offset::VOLUME_R => voice.vol_r.reg_read(),
            voice_offset::PITCH => voice.raw_pitch,
            voice_offset::START_ADDR => voice.start_addr_raw,
            voice_offset::ADSR_LO => voice.adsr_lo,
            voice_offset::ADSR_HI => voice.adsr_hi,
            voice_offset::ADSR_CURRENT => {
                // Current ADSR volume (ENVX), signed 16-bit when written by
                // software. The automatic generator normally occupies
                // 0..=7FFF, but SCPH-9902 reads back manual FFFF writes
                // verbatim, matching PSX-SPX's documented -8000..+7FFF
                // manual range. Games also poll the live generated value:
                // Some titles wait for every voice to reach zero.
                //
                // The live envelope is the hardware's value.
                self.voices[v]
                    .envelope
                    .clamp(i16::MIN as i32, i16::MAX as i32) as i16 as u16
            }
            voice_offset::REPEAT_ADDR => voice.loop_addr_raw,
            _ => 0,
        }
    }

    fn write_voice_reg(&mut self, v: usize, off: u32, value: u16) {
        let voice = &mut self.voices[v];
        match off {
            voice_offset::VOLUME_L => voice.vol_l.write(value),
            voice_offset::VOLUME_R => voice.vol_r.write(value),
            voice_offset::PITCH => voice.raw_pitch = value,
            voice_offset::START_ADDR => {
                // Echo the full 16-bit register on read; the decoder uses
                // the <<3, 16-byte-aligned byte address.
                voice.start_addr_raw = value;
                voice.start_addr = ((value as u32) << 3) & (SPU_RAM_BYTES as u32 - 1) & !0xF;
            }
            voice_offset::ADSR_LO => {
                voice.adsr_lo = value;
            }
            voice_offset::ADSR_HI => {
                voice.adsr_hi = value;
            }
            voice_offset::ADSR_CURRENT => {
                // Manual writes are signed 16-bit. The ADSR generator will
                // overwrite/step this value on its next active envelope tick.
                voice.envelope = value as i16 as i32;
            }
            voice_offset::REPEAT_ADDR => {
                // A software REPEAT_ADDR write stores the loop address and
                // normally locks it (suppresses the ADPCM loop-start flag's
                // auto-update). But while the voice is ON and still in its
                // very first ADPCM block, hardware lets the sample's own
                // loop-start flag override the written value, so we do NOT
                // lock in that window (PSX-SPX: `ignore =
                // !is_on || !first-block`, OR-ed into the loop-address lock;
                // Tron Bonne / Valkyrie Profile / Re-Loaded depend on this).
                // `decoded_block_count <= 1` is that first-block window
                // (0 = before the first decode, 1 = within the first block).
                voice.loop_addr_raw = value;
                voice.loop_addr = ((value as u32) << 3) & (SPU_RAM_BYTES as u32 - 1) & !0xF;
                let ignore = voice.phase == AdsrPhase::Off || voice.decoded_block_count > 1;
                voice.loop_addr_locked |= ignore;
            }
            _ => {}
        }
    }

    fn queue_kon(&mut self, mask: u16, shift: u32) {
        let bits = (mask as u32) << shift;
        // Raw register -- reads echo this back verbatim. Whole-half
        // overwrite semantics: writing KON_LO replaces the low 16 bits,
        // KON_HI replaces the high 16 bits.
        let clear_mask = !(0xFFFFu32 << shift);
        self.kon_raw = (self.kon_raw & clear_mask) | bits;
        // Pending bitmap -- OR-accumulates so multiple writes before a
        // sample tick all fire.
        self.kon_pending |= bits;
        // A fresh KON clears the ENDX bits for those voices.
        self.endx_latched &= !bits;
    }

    fn queue_koff(&mut self, mask: u16, shift: u32) {
        let bits = (mask as u32) << shift;
        let clear_mask = !(0xFFFFu32 << shift);
        self.koff_raw = (self.koff_raw & clear_mask) | bits;
        self.koff_pending |= bits;
    }

    // ============================================================
    //  Data transfer FIFO -- software-driven SPU RAM access.
    // ============================================================

    fn transfer_fifo_write(&mut self, value: u16, now: u64) {
        const FIFO_HALFWORDS: usize = 32;
        if self.transfer_fifo.len() == FIFO_HALFWORDS {
            // A full hardware FIFO cannot accept another halfword. CPU-side
            // bus stalling is not yet represented, so preserve the existing
            // contents and ignore the overflow instead of corrupting order.
            return;
        }
        self.transfer_fifo.push_back(value);
        let manual_write_active = if self.scph_9902_timing {
            (self.spustat_at(now) >> 4) & 0x3 == 1
        } else {
            (self.spucnt >> 4) & 0x3 == 1
        };
        if manual_write_active {
            self.drain_transfer_fifo_to_ram();
        }
    }

    fn drain_transfer_fifo_to_ram(&mut self) {
        while let Some(value) = self.transfer_fifo.pop_front() {
            let idx = (self.transfer_addr >> 1) as usize % SPU_RAM_HALFWORDS;
            self.ram[idx] = value;
            self.check_irq_on_transfer();
            self.transfer_addr = (self.transfer_addr + 2) & (SPU_RAM_BYTES as u32 - 1);
        }
        self.transfer_addr_raw = (self.transfer_addr >> 3) as u16;
    }

    fn transfer_fifo_read(&self) -> u16 {
        // Real hardware post-increments the transfer address on reads
        // too -- but reads can't come from `&self`. We return the value
        // at the current address and let a caller (`peek_transfer_fifo`)
        // do the increment if they want. For a const-read, this is
        // enough: writes are the common case.
        let idx = (self.transfer_addr >> 1) as usize % SPU_RAM_HALFWORDS;
        self.ram[idx]
    }

    /// An SPU RAM IRQ may only latch when IRQ9 is enabled (SPUCNT bit 6)
    /// **and** the sticky IRQ9 flag (SPUSTAT bit 6) is not already set.
    /// Once latched, no further IRQ can fire until software acknowledges
    /// by clearing SPUCNT bit 6 (which clears the SPUSTAT flag in
    /// `write_spucnt`); PSX-SPX: SPUSTAT bit 6 is a sticky flag that blocks
    /// re-latching until acknowledged.
    fn irq_triggerable(&self) -> bool {
        (self.spucnt & (1 << 6)) != 0 && (self.spustat & (1 << 6)) == 0
    }

    fn check_irq_on_transfer(&mut self) {
        if !self.irq_triggerable() {
            return;
        }
        // IRQ fires when the transfer pointer reaches the IRQ address
        // (within a 2-byte window -- IRQ_ADDR granularity is 8 bytes
        // after the <<3 decode, so any write into that 8-byte range
        // triggers).
        let irq = self.irq_addr & !0x7;
        let cur = self.transfer_addr & !0x7;
        if irq == cur {
            self.spustat |= 1 << 6;
            self.irq_pending = true;
        }
    }

    /// Write one SPU capture-buffer halfword. The four capture banks live
    /// at SPU RAM 0x000 (CD-L), 0x400 (CD-R), 0x800 (Voice1) and 0xC00
    /// (Voice3); each is a 0x400-byte ring written at the shared
    /// `capture_buffer_pos`. Per the PSX-SPX spec a capture write can also
    /// latch an SPU IRQ when the armed IRQ address falls on the written
    /// halfword (same sticky IRQ9 gate as every other SPU-RAM access).
    fn write_to_capture(&mut self, bank: u32, value: u16) {
        let byte_addr = bank * 0x400 + self.capture_buffer_pos as u32;
        self.ram[(byte_addr >> 1) as usize] = value;
        if self.irq_triggerable() && (self.irq_addr & !0x7) == (byte_addr & !0x7) {
            self.spustat |= 1 << 6;
            self.irq_pending = true;
        }
    }

    // ============================================================
    //  DMA -- SPU channel 4.
    // ============================================================

    /// Stream halfwords from main RAM into SPU RAM at the current
    /// transfer address. Called by the bus when DMA channel 4 triggers
    /// a RAM→SPU transfer. `words` is a slice of halfwords to copy.
    pub fn dma_write(&mut self, words: &[u16]) {
        for &w in words {
            let idx = (self.transfer_addr >> 1) as usize % SPU_RAM_HALFWORDS;
            self.ram[idx] = w;
            self.check_irq_on_transfer();
            self.transfer_addr = (self.transfer_addr + 2) & (SPU_RAM_BYTES as u32 - 1);
        }
        self.transfer_addr_raw = (self.transfer_addr >> 3) as u16;
    }

    /// Stream halfwords from SPU RAM back to main RAM at the current
    /// transfer address. Called on SPU→RAM DMA (rare; some games use
    /// it for live audio capture).
    pub fn dma_read(&mut self, words: &mut [u16]) {
        for w in words {
            let idx = (self.transfer_addr >> 1) as usize % SPU_RAM_HALFWORDS;
            *w = self.ram[idx];
            self.check_irq_on_transfer();
            self.transfer_addr = (self.transfer_addr + 2) & (SPU_RAM_BYTES as u32 - 1);
        }
        self.transfer_addr_raw = (self.transfer_addr >> 3) as u16;
    }

    /// Stream SPU RAM through the block-shaped read FIFO. When the memory
    /// controller's DMA timing override is disabled, silicon inserts `FFFF`
    /// as the first halfword of every block and drops the block's last source
    /// halfword. This is the documented unstable SPU-read mode; callers that
    /// set bits 24..27 of the SPU delay register use the ordinary stable path.
    pub fn dma_read_blocks(&mut self, words: &mut [u16], block_halfwords: usize, stable: bool) {
        if stable || block_halfwords == 0 {
            self.dma_read(words);
            return;
        }

        for block in words.chunks_mut(block_halfwords) {
            if block.is_empty() {
                continue;
            }
            block[0] = 0xFFFF;
            for output in &mut block[1..] {
                let idx = (self.transfer_addr >> 1) as usize % SPU_RAM_HALFWORDS;
                *output = self.ram[idx];
                self.check_irq_on_transfer();
                self.transfer_addr = (self.transfer_addr + 2) & (SPU_RAM_BYTES as u32 - 1);
            }
            // The FIFO-boundary insertion consumes the final source slot even
            // though that halfword is not delivered to main RAM.
            self.check_irq_on_transfer();
            self.transfer_addr = (self.transfer_addr + 2) & (SPU_RAM_BYTES as u32 - 1);
        }
        self.transfer_addr_raw = (self.transfer_addr >> 3) as u16;
    }

    // ============================================================
    //  Reverb (PSX-SPX "SPU Reverb Formula").
    // ============================================================

    fn reverb_base_halfword(&self) -> u32 {
        self.reverb_base >> 1
    }

    fn reverb_active(&self) -> bool {
        self.reverb_base != 0 && self.reverb_base < SPU_RAM_BYTES as u32
    }

    /// A reverb register as a signed 16-bit value.
    fn reverb_cfg_s(&self, idx: usize) -> i32 {
        i32::from(self.reverb_cfg[idx] as i16)
    }

    /// The SPU RAM halfword a reverb access lands on. `offset` is a register
    /// value in 8-byte units relative to the current buffer address and
    /// `extra` a further halfword displacement. The buffer occupies the top
    /// of RAM from `mBASE`; an address past the end wraps to its start and
    /// one before the start wraps to its end.
    fn reverb_ram_index(&self, offset: i32, extra: i32) -> usize {
        let start = self.reverb_base_halfword() as i32;
        if start >= SPU_RAM_HALFWORDS as i32 {
            return 0;
        }
        let span = SPU_RAM_HALFWORDS as i32 - start;
        let at = self.reverb.curr_addr as i32 + offset.saturating_mul(4) + extra;
        (start + (at - start).rem_euclid(span)) as usize
    }

    fn reverb_read(&self, offset: i32) -> i32 {
        i32::from(self.ram[self.reverb_ram_index(offset, 0)] as i16)
    }

    /// Read the halfword before the one `offset` selects: the previous value
    /// of the cell the reflection filters are about to overwrite.
    fn reverb_read_before(&self, offset: i32) -> i32 {
        i32::from(self.ram[self.reverb_ram_index(offset, -1)] as i16)
    }

    fn reverb_write(&mut self, offset: i32, value: i32) {
        let idx = self.reverb_ram_index(offset, 0);
        self.ram[idx] = saturate_i16(value) as u16;
    }

    /// Multiply by a signed 16-bit volume, dividing the product by 0x8000.
    fn reverb_mul(a: i32, volume: i32) -> i32 {
        ((i64::from(a) * i64::from(volume)) >> 15) as i32
    }

    /// Apply the reverb output volume register. The register is taken as
    /// Q14 here (0x4000 is unity); see the Provenance notes in the module
    /// header.
    fn scale_reverb_output(sample: i32, vol: i16) -> i32 {
        ((i64::from(sample) * i64::from(vol)) / 0x4000)
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
    }

    /// Advance the reverb by one 44.1 kHz sample and return the current
    /// wet output of both channels.
    fn mix_reverb(&mut self, input_l: i32, input_r: i32) -> (i32, i32) {
        if !self.reverb_active() {
            self.reverb.reset_output();
            return (0, 0);
        }
        // Clearing the reverb master bit stops only the writes: the read,
        // all-pass and output stages keep traversing SPU RAM (PA5 on the
        // project's console: a DMA into the work area changed the audible
        // wet output with the master bit and EON clear), so the network
        // always runs and `reverb_channel` gates its stores.
        if self.reverb.right_next {
            self.reverb.wet_r =
                self.reverb_channel(&REVERB_RIGHT, input_r, self.reverb_vol_r.current);
            // The buffer address advances once per left/right pair.
            let mut next = self.reverb.curr_addr + 1;
            if next >= SPU_RAM_HALFWORDS as u32 || next < self.reverb_base_halfword() {
                next = self.reverb_base_halfword();
            }
            self.reverb.curr_addr = next;
        } else {
            self.reverb.wet_l =
                self.reverb_channel(&REVERB_LEFT, input_l, self.reverb_vol_l.current);
        }
        self.reverb.right_next = !self.reverb.right_next;
        (self.reverb.wet_l, self.reverb.wet_r)
    }

    /// One channel's reverb pass: input volume, same-side and different-side
    /// reflections (written back to the buffer), four comb filters, two
    /// all-pass filters, then the output volume.
    fn reverb_channel(&mut self, ch: &ReverbChannel, input: i32, out_volume: i16) -> i32 {
        use reverb_reg::*;
        let writes = self.spucnt & SPUCNT_REVERB_MASTER_ENABLE != 0;
        let v_iir = self.reverb_cfg_s(V_IIR);
        let v_wall = self.reverb_cfg_s(V_WALL);
        let lin = Self::reverb_mul(input, self.reverb_cfg_s(ch.v_in));

        // Reflections: [m] = (in + [d] * vWALL - [m-1]) * vIIR + [m-1],
        // where m-1 is the cell just behind the one being written.
        for (m, d) in [
            (ch.m_same, self.reverb_cfg_s(ch.d_same)),
            (ch.m_diff, self.reverb_cfg_s(ch.d_other_diff)),
        ] {
            let target = self.reverb_cfg_s(m);
            let behind = self.reverb_read_before(target);
            let mixed = lin + Self::reverb_mul(self.reverb_read(d), v_wall) - behind;
            if writes {
                self.reverb_write(target, Self::reverb_mul(mixed, v_iir) + behind);
            }
        }

        // Early echo: the four comb taps, each with its own volume.
        let combs = [V_COMB1, V_COMB2, V_COMB3, V_COMB4];
        let mut out = 0;
        for (m, v) in ch.m_comb.into_iter().zip(combs) {
            out += Self::reverb_mul(self.reverb_read(self.reverb_cfg_s(m)), self.reverb_cfg_s(v));
        }

        // Two all-pass filters in series:
        //   out = out - v * [m - d];  [m] = out;  out = out * v + [m - d]
        for (m, d, v) in [(ch.m_apf1, D_APF1, V_APF1), (ch.m_apf2, D_APF2, V_APF2)] {
            let target = self.reverb_cfg_s(m);
            let delayed = self.reverb_read(target - self.reverb_cfg_s(d));
            let v = self.reverb_cfg_s(v);
            out -= Self::reverb_mul(delayed, v);
            if writes {
                self.reverb_write(target, out);
            }
            out = Self::reverb_mul(out, v) + delayed;
        }
        Self::scale_reverb_output(out, out_volume)
    }

    // ============================================================
    //  Per-sample tick -- called from the bus scheduler.
    // ============================================================

    /// Produce one stereo sample's worth of audio. Called from the bus
    /// each time `EventSlot::SpuMix` fires. Returns the number of
    /// samples produced (currently always 1 -- future batching could
    /// amortise voice-state fetches across several samples).
    pub fn tick_sample(&mut self, now: u64) -> usize {
        self.last_sample_cycle = now;
        if let Some((pending, deadline)) = self.spustat_control_pending {
            if deadline <= now {
                self.spustat_control = pending;
                self.spustat_control_pending = None;
                if (pending >> 4) & 3 == 1 {
                    self.drain_transfer_fifo_to_ram();
                }
            }
        }
        // KON / KOFF are applied at the END of this tick (after the
        // sample is emitted and the capture buffers written), not here --
        // see the apply_kon_koff() call below. A KON/KOFF write is latched
        // and acted on at the next 44.1 kHz tick, so a keyed-on voice's
        // first Attack sample lands on the sample AFTER the one in
        // progress (the SB4 silicon capture shows this).

        // 1b. Advance the noise generator by one sample.
        self.noise_tick();

        // 1c. Advance the global volume sweeps. Per-voice volume sweeps
        //     are ticked in `tick_voice` after each voice's sample is
        //     applied (apply, then tick).
        //     CD/external/reverb volumes are fixed (`write_signed_q15`,
        //     a no-op tick); main volume may be sweep-programmed.
        self.main_vol_l.tick();
        self.main_vol_r.tick();
        self.cd_vol_l.tick();
        self.cd_vol_r.tick();
        self.ext_vol_l.tick();
        self.ext_vol_r.tick();
        self.reverb_vol_l.tick();
        self.reverb_vol_r.tick();

        // 2. For each voice, step envelope + ADPCM playback, accumulate
        //    stereo contribution. A voice that modulates the next one
        //    publishes its output in `last_sample` for that voice's pitch
        //    and stays audible like any other voice (software silences a
        //    modulator by setting its volume to zero).
        // Each accumulator receives at most 24 i16 voice samples and one
        // Q15-scaled i16 CD sample: its magnitude is at most 25 * 32768.
        // i32 addition is exact here; final output saturation stays below.
        let mut sum_l: i32 = 0;
        let mut sum_r: i32 = 0;
        let mut reverb_in_l: i32 = 0;
        let mut reverb_in_r: i32 = 0;
        for v in 0..NUM_VOICES {
            // A keyed-off voice past its start delay with fixed volumes
            // outputs silence and changes nothing but its diagnostics:
            // exactly what `tick_voice` does for it, without the walk
            // through decode, envelope and volume.
            let voice = &self.voices[v];
            if voice.phase == AdsrPhase::Off
                && voice.start_delay == 0
                && !voice.vol_l.sweep_active
                && !voice.vol_r.sweep_active
            {
                self.idle_voice_diagnostics(v);
                self.voices[v].last_sample = 0;
                continue;
            }
            let (l, r) = self.tick_voice(v);
            if l != 0 || r != 0 {
                self.dbg_voiced_samples[v] = self.dbg_voiced_samples[v].saturating_add(1);
            }
            sum_l += i32::from(l);
            sum_r += i32::from(r);
            if self.reverb_on & (1 << v) != 0 {
                reverb_in_l += i32::from(l);
                reverb_in_r += i32::from(r);
            }
        }
        self.dbg_sample_idx = self.dbg_sample_idx.wrapping_add(1);
        self.dbg_pmon_ever |= self.pmon;
        self.dbg_noise_ever |= self.noise_on;

        // 3. Mix CD audio input at CD_VOL_L/R. Source is the CDROM's
        //    CD-DA sample stream or the decoded XA-ADPCM payload,
        //    both fed via [`Spu::feed_cd_audio`]. When the queue is
        //    empty, CD contribution is zero -- matches real hardware
        //    where "no CD playing" means no CD input signal.
        // CD post-volume samples. These are also tapped into the CD-L/R
        // capture buffers (PSX-SPX: SPU RAM 0x000/0x400 capture the CD
        // input *after* the CD-input volume), independently of whether the
        // CD input is routed to the main mix or the reverb bus.
        let mut cd_cap_l: i32 = 0;
        let mut cd_cap_r: i32 = 0;
        if let Some((cd_l, cd_r)) = self.cd_audio_in.pop_front() {
            // CD_VOL regs are Q15 signed -- range -0x8000..=0x7FFF.
            // `>> 15` brings them back to i16 scale. Always consume
            // the stream so timing stays live while muted/disabled;
            // only route it into the mixer when SPUCNT bit 0 is set.
            let cl = ((cd_l as i32) * self.cd_vol_l.current as i32) >> 15;
            let cr = ((cd_r as i32) * self.cd_vol_r.current as i32) >> 15;
            cd_cap_l = cl;
            cd_cap_r = cr;
            if self.spucnt & SPUCNT_CD_AUDIO_ENABLE != 0 {
                sum_l += cl;
                sum_r += cr;
            }
            if self.spucnt & SPUCNT_CD_REVERB_ENABLE != 0 {
                reverb_in_l += cl;
                reverb_in_r += cr;
            }
        }
        // External-audio input is not wired (no hardware source
        // available on a closed console); EXT_VOL_L/R are stored for
        // round-trip reads only.

        // 3b. Write the four SPU capture buffers (PSX-SPX). The SPU mirrors
        //     CD-L, CD-R, Voice1 and Voice3 into SPU RAM 0x000/0x400/
        //     0x800/0xC00 every sample, advancing a shared 0x400-byte ring
        //     index. Games read these back for CD-DA sync and audio
        //     visualisers, and an SPU IRQ armed on the capture region
        //     latches from these writes. Voice1/Voice3 use their post-ADSR
        //     `last_sample` (already i16); CD-L/R saturate the post-volume
        //     value to i16.
        self.write_to_capture(0, saturate_i16(cd_cap_l) as u16);
        self.write_to_capture(1, saturate_i16(cd_cap_r) as u16);
        self.write_to_capture(2, self.voices[1].last_sample as u16);
        self.write_to_capture(3, self.voices[3].last_sample as u16);
        self.capture_buffer_pos = (self.capture_buffer_pos + 2) & 0x3FF;
        // SB4 silicon 2026-08-07: bit 11 = 1 while the second half is being
        // written, on SCPH-9902 too (edge-keyed measurement with both
        // phases exercised). The previous 9902 inversion came from a single
        // snapshot against a free-running ring index.
        if self.capture_buffer_pos >= 0x200 {
            self.spustat |= 1 << 11;
        } else {
            self.spustat &= !(1 << 11);
        }

        // 4. Process the wet reverb bus, then apply MAIN VOLUME as the final
        //    stage: the clamped (dry + wet) sum is scaled by the main-volume
        //    register word as a signed Q15 value, `(s * vol) >> 15`. Leaving
        //    it out makes the output about twice as loud as the console
        //    and clips the 24-voice sum far more often.
        let (wet_l, wet_r) = self.mix_reverb(reverb_in_l, reverb_in_r);
        let dry_l = sum_l;
        let dry_r = sum_r;
        let mixed_l = saturate_i16(dry_l.saturating_add(wet_l)) as i32;
        let mixed_r = saturate_i16(dry_r.saturating_add(wet_r)) as i32;
        let out_l = saturate_i16((mixed_l * self.main_vol_l.raw() as i16 as i32) >> 15);
        let out_r = saturate_i16((mixed_r * self.main_vol_r.raw() as i16 as i32) >> 15);
        self.dbg_dry_energy = self
            .dbg_dry_energy
            .saturating_add(dry_l.unsigned_abs() as u64 + dry_r.unsigned_abs() as u64);
        self.dbg_wet_energy = self
            .dbg_wet_energy
            .saturating_add(wet_l.unsigned_abs() as u64 + wet_r.unsigned_abs() as u64);

        // 5. Push to output ring, discarding oldest if full.
        if self.audio_out.len() >= OUTPUT_BUFFER_CAP {
            self.audio_out.pop_front();
        }
        self.audio_out.push_back((out_l, out_r));

        self.tick_decode_buffer_irq();

        // 6. Apply pending KON / KOFF AFTER this sample was emitted and
        //    the capture buffer was written. The keyed voice therefore first
        //    contributes on the NEXT tick.
        self.apply_kon_koff();
        self.samples_produced = self.samples_produced.saturating_add(1);
        1
    }

    fn tick_decode_buffer_irq(&mut self) {
        if self.irq_triggerable() && self.irq_addr < 0x1000 {
            for bank in 0..4 {
                let cursor = self.decode_irq_cursor + bank * 0x400;
                if self.irq_addr >= cursor && self.irq_addr < cursor + 2 {
                    self.spustat |= 1 << 6;
                    self.irq_pending = true;
                    break;
                }
            }
        }

        self.decode_irq_cursor += 2;
        if self.decode_irq_cursor > 0x3ff {
            self.decode_irq_cursor = 0;
        }
    }

    fn apply_kon_koff(&mut self) {
        let kon = std::mem::take(&mut self.kon_pending);
        let koff = std::mem::take(&mut self.koff_pending);
        if kon | koff == 0 {
            return;
        }
        for v in 0..NUM_VOICES {
            let bit = 1u32 << v;
            let key_on = kon & bit != 0;
            if key_on {
                self.voices[v].key_on();
                self.dbg_kon_count[v] = self.dbg_kon_count[v].saturating_add(1);
                let vc = &self.voices[v];
                let cfg = (
                    vc.start_addr,
                    vc.adsr_lo,
                    vc.adsr_hi,
                    vc.raw_pitch,
                    vc.vol_l.current,
                    vc.vol_r.current,
                );
                self.dbg_keyon_cfg[v] = cfg;
            }
            if !key_on && koff & bit != 0 {
                self.voices[v].key_off();
                self.dbg_koff_count[v] = self.dbg_koff_count[v].saturating_add(1);
            }
        }
    }

    /// The diagnostic trace bookkeeping `tick_voice` does for an idle
    /// voice (decoded sample 0, envelope held at its latched level).
    #[inline(always)]
    fn idle_voice_diagnostics(&mut self, v: usize) {
        let env = self.voices[v].envelope;
        if env > self.dbg_acc_emax[v] {
            self.dbg_acc_emax[v] = env;
        }
        if self.dbg_sample_idx & 0x3FF == 0 && self.dbg_trace[v].len() < 2600 {
            let ph = self.voices[v].phase as u8;
            self.dbg_trace[v].push((self.dbg_acc_smax[v] as i16, self.dbg_acc_emax[v], ph));
            self.dbg_acc_smax[v] = 0;
            self.dbg_acc_emax[v] = 0;
        }
    }

    /// Advance one voice by one output sample. Returns `(l, r)`
    /// pre-main-volume, post-voice-volume contribution in i16 scale.
    fn tick_voice(&mut self, v: usize) -> (i16, i16) {
        // SB4 silicon 2026-08-07: a freshly keyed voice is silent, envelope
        // included, for ~7 ticks after KON lands before it starts stepping.
        if self.voices[v].start_delay > 0 {
            self.voices[v].start_delay -= 1;
            self.voices[v].last_sample = 0;
            return (0, 0);
        }
        // Fetch raw sample using the SPU's Gaussian interpolation path.
        let sample_i16 = self.fetch_voice_sample(v);

        // Advance ADSR envelope.
        let env = self.voices[v].step_envelope();
        let mixed_i16 = apply_adsr_volume(sample_i16, env);

        // Diagnostic trace: track per-window max decode amplitude + envelope,
        // snapshot every 1024 samples to bisect premature cutoffs
        // (decode-silence with envelope held vs envelope-drop).
        let s_abs = (sample_i16 as i32).abs();
        if s_abs > self.dbg_acc_smax[v] {
            self.dbg_acc_smax[v] = s_abs;
        }
        if env > self.dbg_acc_emax[v] {
            self.dbg_acc_emax[v] = env;
        }
        if self.dbg_sample_idx & 0x3FF == 0 && self.dbg_trace[v].len() < 2600 {
            let ph = self.voices[v].phase as u8;
            self.dbg_trace[v].push((self.dbg_acc_smax[v] as i16, self.dbg_acc_emax[v], ph));
            self.dbg_acc_smax[v] = 0;
            self.dbg_acc_emax[v] = 0;
        }

        let voice = &mut self.voices[v];
        voice.last_sample = mixed_i16;

        // Apply per-voice L / R volumes in full signed Q15 (sample *
        // current_level >> 15), matching PSX-SPX
        // `apply_volume`. `current` is the live sweep/fixed level, so
        // sweep-configured voices fade and negative-phase volumes
        // invert correctly. Tick the sweep AFTER applying this sample,
        // mirroring both oracles' apply-then-`tick` order.
        let l = ((mixed_i16 as i32) * voice.vol_l.current as i32) >> 15;
        let r = ((mixed_i16 as i32) * voice.vol_r.current as i32) >> 15;
        voice.vol_l.tick();
        voice.vol_r.tick();
        (saturate_i16(l), saturate_i16(r))
    }

    /// The pitch step of voice `v` for this sample (PSX-SPX "SPU ADPCM
    /// Pitch"): the pitch register, optionally modulated by the previous
    /// voice's output, capped at 0x4000 (four times the sample rate).
    fn voice_step(&self, v: usize) -> u32 {
        let pitch = u32::from(self.voices[v].raw_pitch);
        let step = if v > 0 && self.pmon & (1 << v) != 0 {
            // The modulator's output (-0x8000..=0x7FFF) shifted to
            // 0..=0xFFFF scales the pitch, read as a signed 16-bit number.
            let factor = i32::from(self.voices[v - 1].last_sample) + 0x8000;
            let signed_pitch = i32::from(pitch as u16 as i16);
            (((signed_pitch * factor) >> 15) as u32) & 0xFFFF
        } else {
            pitch
        };
        step.min(0x4000)
    }

    /// Voice `v`'s next interpolated sample, advancing its counter by one
    /// output sample's worth of pitch.
    fn fetch_voice_sample(&mut self, v: usize) -> i16 {
        // Voices in Off contribute nothing.
        if self.voices[v].phase == AdsrPhase::Off {
            return 0;
        }
        let noise_mode = self.noise_on & (1 << v) != 0;
        let feeds_fmod = v + 1 < NUM_VOICES && (self.pmon & (1 << (v + 1))) != 0;
        let mute_voice_sample = (self.spucnt & SPUCNT_UNMUTE) == 0 && !feeds_fmod;
        let step = self.voice_step(v);

        // Take in every source sample the counter has passed. Crossing the
        // end of a block first settles that block: ENDX latches only once
        // the block's last sample has been consumed, and a block without
        // the repeat bit ends the note.
        while self.voices[v].counter >= COUNTER_ONE {
            if self.voices[v].sample_index >= ADPCM_SAMPLES_PER_BLOCK {
                if self.voices[v].endx_pending {
                    self.endx_latched |= 1 << v;
                    self.voices[v].endx_pending = false;
                }
                if self.voices[v].stop_after_block {
                    self.dbg_sampstop_count[v] = self.dbg_sampstop_count[v].saturating_add(1);
                    let voice = &mut self.voices[v];
                    voice.phase = AdsrPhase::Off;
                    voice.envelope = 0;
                    voice.stop_after_block = false;
                    voice.last_sample = 0;
                    return 0;
                }
                self.decode_next_block(v);
            }
            let voice = &mut self.voices[v];
            let sample = if mute_voice_sample {
                0
            } else {
                voice.sample_buf[voice.sample_index].clamp(-0x8000, 0x7FFF) as i16
            };
            voice.sample_index += 1;
            voice.push_tap(sample);
            voice.counter -= COUNTER_ONE;
        }

        // A noise voice keeps decoding and stepping as usual; only its
        // audible sample is replaced by the shared noise level.
        let out = if noise_mode {
            self.noise_val
        } else {
            gauss_interpolate(self.voices[v].taps, self.voices[v].counter)
        };
        self.voices[v].counter += step;
        out
    }

    /// Decode the 16-byte ADPCM block at voice `v`'s read address into its
    /// sample buffer, act on the block's flags, and move the read address on
    /// (to the repeat address after a loop-end block).
    ///
    /// Block layout (PSX-SPX "SPU ADPCM Samples"): byte 0 holds the shift in
    /// its low nibble and the filter in its high nibble; byte 1 the flags;
    /// bytes 2..16 the 28 samples as 4-bit signed nibbles, low nibble first.
    /// A sample is its nibble scaled to 16 bits and shifted right by the
    /// shift, plus the two previous samples weighted by the filter's pair
    /// of coefficients (in 64ths); the result saturates to 16 bits before
    /// it becomes history. Shifts above 12 behave like 9.
    fn decode_next_block(&mut self, v: usize) {
        let current = self.voices[v].current_addr;

        // An enabled IRQ address inside the block being read raises the IRQ.
        if self.irq_triggerable() && (current & !0xF) == (self.irq_addr & !0xF) {
            self.spustat |= 1 << 6;
            self.irq_pending = true;
        }

        let block = read_adpcm_block(&self.ram[..], current);
        let filter = usize::from(block[0] >> 4).min(ADPCM_FILTERS.len() - 1);
        let raw_shift = block[0] & 0x0F;
        let shift = u32::from(if raw_shift > 12 { 9 } else { raw_shift });
        let flags = block[1];

        let voice = &mut self.voices[v];
        let (weight1, weight2) = ADPCM_FILTERS[filter];
        for i in 0..ADPCM_SAMPLES_PER_BLOCK {
            let byte = i32::from(block[2 + (i >> 1)]);
            let nibble = if i & 1 == 0 { byte & 0xF } else { byte >> 4 };
            let scaled = (((nibble << 28) >> 28) << 12) >> shift;
            let predicted = ((voice.s_1 * weight1) >> 6) + ((voice.s_2 * weight2) >> 6);
            let sample = (scaled + predicted).clamp(-0x8000, 0x7FFF);
            voice.sample_buf[i] = sample;
            voice.s_2 = voice.s_1;
            voice.s_1 = sample;
        }
        voice.sample_index = 0;
        // The first block of a note is the window in which a write to the
        // repeat address still yields to the sample's own loop-start flag.
        voice.decoded_block_count = voice.decoded_block_count.saturating_add(1);

        // Flag bit 2 (loop start): this block becomes the repeat target,
        // unless software has written the repeat address itself.
        if flags & 0x4 != 0 && !voice.loop_addr_locked {
            voice.loop_addr = current;
            voice.loop_addr_raw = (current >> 3) as u16;
        }
        if flags & 0x1 != 0 {
            // Flag bit 0 (loop end): after this block plays, ENDX latches
            // and playback continues at the repeat address; without bit 1
            // (repeat) the note ends there. Noise voices are never ended by
            // flags.
            let noise = self.noise_on & (1 << v) != 0;
            let voice = &mut self.voices[v];
            voice.endx_pending = true;
            voice.current_addr = voice.loop_addr;
            voice.stop_after_block = (flags & 0x2 == 0) && !noise;
        } else {
            voice.current_addr = (current + ADPCM_BLOCK_BYTES as u32) & (SPU_RAM_BYTES as u32 - 1);
            voice.stop_after_block = false;
        }
    }
}

// ===============================================================
//  Gaussian interpolation table (PSX hardware).
// ===============================================================

/// The SPU's 512-entry Gaussian interpolation table (the nocash PSX-SPX
/// table, a hardware constant). The first 16 entries are -1 and the peak is
/// `GAUSS_TABLE[0x1FF] == 0x59B3`. A phase `i` in 0..=0xFF selects the four
/// weights `T[0xFF-i]`, `T[0x1FF-i]`, `T[0x100+i]` and `T[i]` for the oldest
/// to newest sample; their sum is about 0x7F80, which is the deliberate gain
/// droop of the real SPU (a full-scale DC input comes out near 32639).
const GAUSS_TABLE: [i32; 0x200] = [
    -0x001, -0x001, -0x001, -0x001, -0x001, -0x001, -0x001, -0x001, -0x001, -0x001, -0x001, -0x001,
    -0x001, -0x001, -0x001, -0x001, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0001,
    0x0001, 0x0001, 0x0001, 0x0002, 0x0002, 0x0002, 0x0003, 0x0003, 0x0003, 0x0004, 0x0004, 0x0005,
    0x0005, 0x0006, 0x0007, 0x0007, 0x0008, 0x0009, 0x0009, 0x000A, 0x000B, 0x000C, 0x000D, 0x000E,
    0x000F, 0x0010, 0x0011, 0x0012, 0x0013, 0x0015, 0x0016, 0x0018, 0x0019, 0x001B, 0x001C, 0x001E,
    0x0020, 0x0021, 0x0023, 0x0025, 0x0027, 0x0029, 0x002C, 0x002E, 0x0030, 0x0033, 0x0035, 0x0038,
    0x003A, 0x003D, 0x0040, 0x0043, 0x0046, 0x0049, 0x004D, 0x0050, 0x0054, 0x0057, 0x005B, 0x005F,
    0x0063, 0x0067, 0x006B, 0x006F, 0x0074, 0x0078, 0x007D, 0x0082, 0x0087, 0x008C, 0x0091, 0x0096,
    0x009C, 0x00A1, 0x00A7, 0x00AD, 0x00B3, 0x00BA, 0x00C0, 0x00C7, 0x00CD, 0x00D4, 0x00DB, 0x00E3,
    0x00EA, 0x00F2, 0x00FA, 0x0101, 0x010A, 0x0112, 0x011B, 0x0123, 0x012C, 0x0135, 0x013F, 0x0148,
    0x0152, 0x015C, 0x0166, 0x0171, 0x017B, 0x0186, 0x0191, 0x019C, 0x01A8, 0x01B4, 0x01C0, 0x01CC,
    0x01D9, 0x01E5, 0x01F2, 0x0200, 0x020D, 0x021B, 0x0229, 0x0237, 0x0246, 0x0255, 0x0264, 0x0273,
    0x0283, 0x0293, 0x02A3, 0x02B4, 0x02C4, 0x02D6, 0x02E7, 0x02F9, 0x030B, 0x031D, 0x0330, 0x0343,
    0x0356, 0x036A, 0x037E, 0x0392, 0x03A7, 0x03BC, 0x03D1, 0x03E7, 0x03FC, 0x0413, 0x042A, 0x0441,
    0x0458, 0x0470, 0x0488, 0x04A0, 0x04B9, 0x04D2, 0x04EC, 0x0506, 0x0520, 0x053B, 0x0556, 0x0572,
    0x058E, 0x05AA, 0x05C7, 0x05E4, 0x0601, 0x061F, 0x063E, 0x065C, 0x067C, 0x069B, 0x06BB, 0x06DC,
    0x06FD, 0x071E, 0x0740, 0x0762, 0x0784, 0x07A7, 0x07CB, 0x07EF, 0x0813, 0x0838, 0x085D, 0x0883,
    0x08A9, 0x08D0, 0x08F7, 0x091E, 0x0946, 0x096F, 0x0998, 0x09C1, 0x09EB, 0x0A16, 0x0A40, 0x0A6C,
    0x0A98, 0x0AC4, 0x0AF1, 0x0B1E, 0x0B4C, 0x0B7A, 0x0BA9, 0x0BD8, 0x0C07, 0x0C38, 0x0C68, 0x0C99,
    0x0CCB, 0x0CFD, 0x0D30, 0x0D63, 0x0D97, 0x0DCB, 0x0E00, 0x0E35, 0x0E6B, 0x0EA1, 0x0ED7, 0x0F0F,
    0x0F46, 0x0F7F, 0x0FB7, 0x0FF1, 0x102A, 0x1065, 0x109F, 0x10DB, 0x1116, 0x1153, 0x118F, 0x11CD,
    0x120B, 0x1249, 0x1288, 0x12C7, 0x1307, 0x1347, 0x1388, 0x13C9, 0x140B, 0x144D, 0x1490, 0x14D4,
    0x1517, 0x155C, 0x15A0, 0x15E6, 0x162C, 0x1672, 0x16B9, 0x1700, 0x1747, 0x1790, 0x17D8, 0x1821,
    0x186B, 0x18B5, 0x1900, 0x194B, 0x1996, 0x19E2, 0x1A2E, 0x1A7B, 0x1AC8, 0x1B16, 0x1B64, 0x1BB3,
    0x1C02, 0x1C51, 0x1CA1, 0x1CF1, 0x1D42, 0x1D93, 0x1DE5, 0x1E37, 0x1E89, 0x1EDC, 0x1F2F, 0x1F82,
    0x1FD6, 0x202A, 0x207F, 0x20D4, 0x2129, 0x217F, 0x21D5, 0x222C, 0x2282, 0x22DA, 0x2331, 0x2389,
    0x23E1, 0x2439, 0x2492, 0x24EB, 0x2545, 0x259E, 0x25F8, 0x2653, 0x26AD, 0x2708, 0x2763, 0x27BE,
    0x281A, 0x2876, 0x28D2, 0x292E, 0x298B, 0x29E7, 0x2A44, 0x2AA1, 0x2AFF, 0x2B5C, 0x2BBA, 0x2C18,
    0x2C76, 0x2CD4, 0x2D33, 0x2D91, 0x2DF0, 0x2E4F, 0x2EAE, 0x2F0D, 0x2F6C, 0x2FCC, 0x302B, 0x308B,
    0x30EA, 0x314A, 0x31AA, 0x3209, 0x3269, 0x32C9, 0x3329, 0x3389, 0x33E9, 0x3449, 0x34A9, 0x3509,
    0x3569, 0x35C9, 0x3629, 0x3689, 0x36E8, 0x3748, 0x37A8, 0x3807, 0x3867, 0x38C6, 0x3926, 0x3985,
    0x39E4, 0x3A43, 0x3AA2, 0x3B00, 0x3B5F, 0x3BBD, 0x3C1B, 0x3C79, 0x3CD7, 0x3D35, 0x3D92, 0x3DEF,
    0x3E4C, 0x3EA9, 0x3F05, 0x3F62, 0x3FBD, 0x4019, 0x4074, 0x40D0, 0x412A, 0x4185, 0x41DF, 0x4239,
    0x4292, 0x42EB, 0x4344, 0x439C, 0x43F4, 0x444C, 0x44A3, 0x44FA, 0x4550, 0x45A6, 0x45FC, 0x4651,
    0x46A6, 0x46FA, 0x474E, 0x47A1, 0x47F4, 0x4846, 0x4898, 0x48E9, 0x493A, 0x498A, 0x49D9, 0x4A29,
    0x4A77, 0x4AC5, 0x4B13, 0x4B5F, 0x4BAC, 0x4BF7, 0x4C42, 0x4C8D, 0x4CD7, 0x4D20, 0x4D68, 0x4DB0,
    0x4DF7, 0x4E3E, 0x4E84, 0x4EC9, 0x4F0E, 0x4F52, 0x4F95, 0x4FD7, 0x5019, 0x505A, 0x509A, 0x50DA,
    0x5118, 0x5156, 0x5194, 0x51D0, 0x520C, 0x5247, 0x5281, 0x52BA, 0x52F3, 0x532A, 0x5361, 0x5397,
    0x53CC, 0x5401, 0x5434, 0x5467, 0x5499, 0x54CA, 0x54FA, 0x5529, 0x5558, 0x5585, 0x55B2, 0x55DE,
    0x5609, 0x5632, 0x565B, 0x5684, 0x56AB, 0x56D1, 0x56F6, 0x571B, 0x573E, 0x5761, 0x5782, 0x57A3,
    0x57C3, 0x57E2, 0x57FF, 0x581C, 0x5838, 0x5853, 0x586D, 0x5886, 0x589E, 0x58B5, 0x58CB, 0x58E0,
    0x58F4, 0x5907, 0x5919, 0x592A, 0x593A, 0x5949, 0x5958, 0x5965, 0x5971, 0x597C, 0x5986, 0x598F,
    0x5997, 0x599E, 0x59A4, 0x59A9, 0x59AD, 0x59B0, 0x59B2, 0x59B3,
];

/// Interpolate between the four most recent samples (oldest first) at the
/// fractional position held in a voice's counter: the phase index is bits
/// 4..11 of the counter. The four weighted samples are summed and the sum
/// shifted down 15 places.
fn gauss_interpolate(taps: [i16; 4], counter: u32) -> i16 {
    let i = ((counter >> 4) & 0xFF) as usize;
    let sum = GAUSS_TABLE[0xFF - i] * i32::from(taps[0])
        + GAUSS_TABLE[0x1FF - i] * i32::from(taps[1])
        + GAUSS_TABLE[0x100 + i] * i32::from(taps[2])
        + GAUSS_TABLE[i] * i32::from(taps[3]);
    saturate_i16(sum >> 15)
}

// ===============================================================
//  XA ADPCM decoder.
// ===============================================================

// ===============================================================
//  Helpers.
// ===============================================================

// `decode_volume` has been subsumed by `VolumeEnvelope::write`, which
// both snaps static levels AND starts sweep animations. The per-
// write decode + animate path is now centralised so every volume
// register (voice L/R × 24, main L/R, CD L/R, ext L/R, reverb L/R)
// shares the same behaviour.

/// Decode a voice-bank byte address into `(voice_index, byte_offset)`.
fn decode_voice(phys: u32) -> Option<(usize, u32)> {
    if !(VOICE_BASE..VOICE_END).contains(&phys) {
        return None;
    }
    let rel = phys - VOICE_BASE;
    Some(((rel / 16) as usize, rel % 16))
}

/// Read one ADPCM block (16 bytes) from SPU RAM at the given byte
/// address. Wraps modulo the RAM size.
fn read_adpcm_block(ram: &[u16], addr: u32) -> [u8; ADPCM_BLOCK_BYTES] {
    let mut out = [0u8; ADPCM_BLOCK_BYTES];
    let base = (addr & (SPU_RAM_BYTES as u32 - 1)) as usize;
    for (i, out_byte) in out.iter_mut().enumerate().take(ADPCM_BLOCK_BYTES) {
        let byte_addr = (base + i) & (SPU_RAM_BYTES - 1);
        let halfword = ram[byte_addr >> 1];
        *out_byte = if byte_addr & 1 == 0 {
            halfword as u8
        } else {
            (halfword >> 8) as u8
        };
    }
    out
}

fn apply_adsr_volume(sample: i16, envelope: i32) -> i16 {
    // SB4 silicon 2026-08-07: the voice output is the full 15-bit envelope
    // applied as (sample * env) >> 15 with floor rounding. Seven
    // consecutive attack/decay/sustain onset samples in the NOISE capture
    // match this exactly; the previous (env>>5)/1023 form was off by 1-2
    // LSB on five of them. No saturation needed: |result| <= 32767, and
    // Rust's >> on i32 is arithmetic, matching the floor behavior the
    // 0x3FFF decay sample confirms.
    let env = envelope.clamp(0, 0x7FFF);
    (((sample as i32) * env) >> 15) as i16
}

/// Clamp a 32-bit sample to signed 16-bit range.
fn saturate_i16(v: i32) -> i16 {
    v.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

// ===============================================================
//  Tests.
// ===============================================================

#[cfg(test)]
#[cfg(test)]
mod tests;
