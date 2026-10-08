//! Level generators of the SPU: the per-voice ADSR envelope and the
//! sweepable volume registers.
//!
//! Both are the same machine (nocash PSX-SPX, "SPU Volume and ADSR
//! Generator"): a signed 16-bit level that, every `cycles` samples at
//! 44.1 kHz, moves by a step. A segment is described by
//!
//! - `shift` (0..=31, "fast to slow"): the update period is
//!   `1 << max(0, shift - 11)` samples, and for shifts below 11 the step is
//!   scaled up by `1 << (11 - shift)` instead, so a lower shift is always
//!   faster;
//! - `step`: +7..=+4 when increasing, -8..=-5 when decreasing;
//! - `exponential`: a decreasing step is scaled by the current level
//!   (`step * level / 0x8000`), and an increasing one runs four times slower
//!   while the level is above 0x6000.

/// One segment of a level generator.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct Slope {
    /// Rate field, 0..=31.
    pub shift: u8,
    /// Base step: positive increases the level, negative decreases it.
    pub step: i8,
    /// Whether the step follows the level (see the module notes).
    pub exponential: bool,
}

impl Slope {
    /// The step for an increasing segment, from its 2-bit selector.
    pub const fn rising(shift: u8, selector: u8, exponential: bool) -> Self {
        Self {
            shift,
            step: 7 - (selector & 3) as i8,
            exponential,
        }
    }

    /// The step for a decreasing segment, from its 2-bit selector.
    pub const fn falling(shift: u8, selector: u8, exponential: bool) -> Self {
        Self {
            shift,
            step: (selector & 3) as i8 - 8,
            exponential,
        }
    }

    /// Samples between two updates while the level is `level`.
    fn period(self, level: i32) -> u32 {
        let base = 1u32 << self.shift.saturating_sub(11);
        if self.exponential && self.step > 0 && level > 0x6000 {
            base * 4
        } else {
            base
        }
    }

    /// The change applied at an update while the level is `level`.
    fn delta(self, level: i32) -> i32 {
        let scaled = i32::from(self.step) << 11u8.saturating_sub(self.shift);
        if self.exponential && self.step < 0 {
            (scaled * level) >> 15
        } else {
            scaled
        }
    }
}

/// The countdown that spaces a [`Slope`]'s updates.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct Pacer {
    /// Samples left before the next update; 0 means "reload from the slope".
    remaining: u32,
}

impl Pacer {
    /// Restart the countdown (a new note, or a fresh sweep programming).
    pub fn reset(&mut self) {
        self.remaining = 0;
    }

    /// Advance one sample. Returns the level change due at this sample, if
    /// any.
    pub fn tick(&mut self, slope: Slope, level: i32) -> Option<i32> {
        if self.remaining == 0 {
            self.remaining = slope.period(level);
        }
        self.remaining -= 1;
        (self.remaining == 0).then(|| slope.delta(level))
    }
}

/// A volume register of the SPU: a fixed level, or a sweep that moves the
/// level toward its end stop.
#[derive(Copy, Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct VolumeEnvelope {
    /// The last word written, echoed back on reads.
    raw: u16,
    /// Current level, signed 16 bits; applied to a sample as
    /// `(sample * current) >> 15`.
    pub current: i16,
    /// True while a sweep is still moving.
    pub sweep_active: bool,
    slope: Slope,
    pacer: Pacer,
    /// Sweeping in the negative phase (bit 12): the level lives below zero.
    negative: bool,
    /// Sweeping toward zero rather than toward the full-scale end stop.
    toward_zero: bool,
}

impl VolumeEnvelope {
    pub fn new() -> Self {
        Self::default()
    }

    /// A voice or main volume write. Bit 15 clear: a fixed level of
    /// `(bits 14..0 sign-extended from bit 14) * 2`. Bit 15 set: program a
    /// sweep that starts from the current level.
    pub fn write(&mut self, raw: u16) {
        self.raw = raw;
        if raw & 0x8000 == 0 {
            let half = ((raw & 0x7FFF) << 1) as i16 >> 1;
            self.current = half.wrapping_mul(2);
            self.sweep_active = false;
            return;
        }
        let shift = ((raw >> 2) & 0x1F) as u8;
        let selector = (raw & 3) as u8;
        let exponential = raw & (1 << 14) != 0;
        let decreasing = raw & (1 << 13) != 0;
        // Bit 12 has no effect on an exponential decrease: it always
        // shrinks the magnitude by a proportion of the level.
        self.negative = raw & (1 << 12) != 0 && !(decreasing && exponential);
        self.toward_zero = decreasing;
        // Moving the level down uses the falling step table. A decrease in
        // the positive phase and an increase in the negative phase both
        // move down; the other two combinations move up.
        self.slope = if decreasing != self.negative {
            Slope::falling(shift, selector, exponential)
        } else {
            Slope::rising(shift, selector, exponential)
        };
        self.pacer.reset();
        // The slowest setting (period of 2^20 samples, about 24 s) is
        // treated as stalled.
        self.sweep_active = !(shift == 31 && selector == 3);
    }

    /// A plain signed volume register (CD, external and reverb output).
    pub fn write_signed_q15(&mut self, raw: u16) {
        self.raw = raw;
        self.current = raw as i16;
        self.sweep_active = false;
    }

    /// The register as software last wrote it.
    pub fn reg_read(&self) -> u16 {
        self.raw
    }

    /// Alias of [`VolumeEnvelope::reg_read`] for code that mixes with the
    /// register word itself.
    pub fn raw(&self) -> u16 {
        self.raw
    }

    /// Advance one 44.1 kHz sample.
    pub fn tick(&mut self) {
        if !self.sweep_active {
            return;
        }
        let level = i32::from(self.current);
        let Some(delta) = self.pacer.tick(self.slope, level) else {
            return;
        };
        let moved = level + delta;
        if self.toward_zero {
            // Stops at zero from either side.
            let floor = if self.negative { -0x8000 } else { 0 };
            let ceil = if self.negative { 0 } else { 0x7FFF };
            let clamped = moved.clamp(floor, ceil);
            self.current = clamped as i16;
            self.sweep_active = clamped != 0;
        } else {
            let clamped = moved.clamp(-0x8000, 0x7FFF);
            self.current = clamped as i16;
            let end_stop = if self.negative { -0x8000 } else { 0x7FFF };
            self.sweep_active = clamped != end_stop;
        }
    }
}

// ---------------------------------------------------------------
//  Voice ADSR: the segment each phase of the 32-bit register pair
//  describes (`lo` = register at +8, `hi` = register at +A).
// ---------------------------------------------------------------

/// Attack: bit 15 exponential, bits 14..10 shift, bits 9..8 step.
pub(super) fn attack_slope(lo: u16) -> Slope {
    Slope::rising(((lo >> 10) & 0x1F) as u8, (lo >> 8) as u8, lo & 0x8000 != 0)
}

/// Decay: bits 7..4 shift; always an exponential decrease with step -8.
pub(super) fn decay_slope(lo: u16) -> Slope {
    Slope::falling(((lo >> 4) & 0xF) as u8, 0, true)
}

/// The level at which decay hands over to sustain: `(N + 1) * 0x800` for the
/// register's 4-bit sustain field `N`.
pub(super) fn sustain_threshold(lo: u16) -> i32 {
    (i32::from(lo & 0xF) + 1) * 0x800
}

/// Sustain: bit 15 (of `hi`) exponential, bit 14 decrease, bits 12..8
/// shift, bits 7..6 step.
pub(super) fn sustain_slope(hi: u16) -> Slope {
    let shift = ((hi >> 8) & 0x1F) as u8;
    let selector = (hi >> 6) as u8;
    let exponential = hi & 0x8000 != 0;
    if hi & 0x4000 != 0 {
        Slope::falling(shift, selector, exponential)
    } else {
        Slope::rising(shift, selector, exponential)
    }
}

/// Release: bit 5 exponential, bits 4..0 shift; always a decrease with
/// step -8.
pub(super) fn release_slope(hi: u16) -> Slope {
    Slope::falling((hi & 0x1F) as u8, 0, hi & (1 << 5) != 0)
}
