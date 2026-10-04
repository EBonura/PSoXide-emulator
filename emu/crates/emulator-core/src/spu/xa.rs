//! XA-ADPCM block decoder.
//!
//! ## Provenance
//!
//! Written from the nocash PSX-SPX chapter "CDROM XA Audio ADPCM
//! Compression": the header byte layout, the `decode_28_nibbles`
//! procedure and the pos/neg filter tables. Nothing here is taken from
//! another emulator. The decoded sector layout lives with the CD-ROM
//! sector reader in `cdrom.rs`; this module owns only the per-block
//! arithmetic.
//!
//! One block is 28 samples. Each sample is widened to 16 bits (a 4-bit
//! sample is shifted left by 12, an 8-bit sample by 8), shifted right by
//! the block's range, then added to a two-tap prediction made from the
//! two previous samples of the same channel.
//!
//! ## Numeric model
//!
//! PSX-SPX's pseudocode keeps 16-bit integer history and rounds each
//! prediction with `+32` before the divide by 64. This decoder does not
//! follow that rounding: the history keeps four fractional bits (units of
//! 1/16 of an output step), the prediction is truncated, and only the
//! sample handed to the SPU is clamped, not the history. That model is
//! what the project's compatibility references were recorded with, and it
//! matters: Crash Team Racing reads the SPU's CD-input capture buffer, and
//! with the textbook rounding its frame hashes diverge from frame 1628 of
//! the compat run. There is no silicon capture of XA output that decides
//! between the two, so the choice is recorded here rather than argued.

/// Per-channel decoder history for XA ADPCM blocks. The predictor needs
/// the last two output samples of the channel (`y0` the newest, `y1` the
/// one before it). Callers hold one of these per stereo channel (a mono
/// stream uses a single one).
#[derive(Default, Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct XaDecoderState {
    y0: i32,
    y1: i32,
}

impl XaDecoderState {
    /// Fresh decoder history: silence as the previous samples.
    pub fn new() -> Self {
        Self { y0: 0, y1: 0 }
    }

    /// Reset history to silence between XA files.
    pub fn reset(&mut self) {
        self.y0 = 0;
        self.y1 = 0;
    }
}

/// Predictor weights applied to the newest previous sample, indexed by the
/// block's filter number, in 1/64 units (PSX-SPX `pos_xa_adpcm_table`).
/// XA defines filters 0..=3 only.
const XA_FILTER_NEWEST: [i32; 4] = [0, 60, 115, 98];

/// Predictor weights applied to the older previous sample, in 1/64 units
/// (PSX-SPX `neg_xa_adpcm_table`).
const XA_FILTER_OLDER: [i32; 4] = [0, 0, -52, -55];

/// Largest range value that is used as written. PSX-SPX: ranges 13..=15
/// are reserved and behave like a range of 9.
const XA_MAX_RANGE: u8 = 12;
const XA_RESERVED_RANGE_AS: u8 = 9;

/// Fractional bits carried by the decoder history (see "Numeric model").
const HISTORY_FRAC_BITS: u32 = 4;

/// Decode one 28-sample XA-ADPCM block.
///
/// - `header` is the block's header byte: bits 0..=3 hold the range (the
///   right shift applied to the widened sample), bits 4..=5 the filter
///   number. Bits 6..=7 are unused and ignored.
/// - `widened` holds the block's samples already sign-extended and shifted
///   up to 16 bits (`nibble << 12` for 4-bit data, `byte << 8` for 8-bit).
/// - `state` is the channel's history, updated in place.
///
/// Each output is `(widened >> range) + prediction`, where the prediction
/// is `newest * pos + older * neg` in 1/64 units, truncated. The value is
/// clamped to the signed 16-bit range on the way out; the history keeps
/// the unclamped value with four extra fractional bits and wraps like a
/// 32-bit register if it ever overflows (only reachable with garbage
/// input, since a stable stream stays near the 16-bit range).
pub fn xa_decode_block(
    state: &mut XaDecoderState,
    header: u8,
    widened: &[i16; 28],
    out: &mut [i16; 28],
) {
    let range = match header & 0x0F {
        r if r > XA_MAX_RANGE => XA_RESERVED_RANGE_AS,
        r => r,
    };
    let filter = ((header >> 4) & 0x03) as usize;
    // 1/64 weights applied to a history with four fractional bits: scale
    // them to 1/1024 and shift the sum by 10, so the fraction survives.
    let pos = XA_FILTER_NEWEST[filter] << HISTORY_FRAC_BITS;
    let neg = XA_FILTER_OLDER[filter] << HISTORY_FRAC_BITS;

    let (mut newest, mut older) = (state.y0, state.y1);
    for (dst, &sample) in out.iter_mut().zip(widened) {
        let predicted = newest
            .wrapping_mul(pos)
            .wrapping_add(older.wrapping_mul(neg))
            >> 10;
        let value = ((i32::from(sample) >> range) << HISTORY_FRAC_BITS).wrapping_add(predicted);
        *dst = (value >> HISTORY_FRAC_BITS).clamp(-0x8000, 0x7FFF) as i16;
        older = newest;
        newest = value;
    }
    state.y0 = newest;
    state.y1 = older;
}
