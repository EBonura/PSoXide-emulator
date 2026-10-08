//! CD-XA sample-rate conversion to the SPU's 44.1 kHz.
//!
//! ## Provenance
//!
//! Written from the nocash PSX-SPX chapter "CDROM XA Audio ADPCM
//! Compression", section "25-point Zigzag Interpolation". That section
//! describes the CD controller's conversion of the 37.8 kHz XA output to
//! 44.1 kHz: every six input samples produce seven output samples, and each
//! output is a 29-tap weighted sum over the last 29 inputs, with a separate
//! weight table for each of the seven outputs. The 29 x 7 weights below are
//! that chapter's table. Nothing is taken from another emulator.
//!
//! PSX-SPX calls the formula "nearly correct, with small rounding errors",
//! so it is a model, not a bit-exact silicon capture. Choices that are ours:
//!
//! - The products are summed at full width and shifted once, instead of
//!   dividing each product by 8000h as the chapter's pseudocode does. That
//!   removes up to 29 LSB of truncation bias per output.
//! - For 18.9 kHz streams the chapter only says the same filter "gets
//!   spread across about fifty 44100Hz samples". Each 18.9 kHz sample is
//!   first brought to 37.8 kHz by inserting the average of it and the
//!   previous sample ahead of it (linear interpolation), then goes through
//!   the same filter. Repeating each sample instead leaves the images of
//!   the 18.9 kHz band about 18 dB closer to the signal at 1.5 kHz.
//!
//! - The table is used exactly as printed. Each output table sums to about
//!   0.906 of 8000h, so low frequencies come out about 0.86 dB below the
//!   decoded level. The chapter gives no other normalisation, and there is
//!   no console capture of XA output to decide against it.
//!
//! The previous conversion repeated the nearest source sample. Against a
//! band-limited reference that costs about 14 dB of signal-to-error ratio
//! on music and puts the images of the source band all the way up to the
//! Nyquist frequency; this filter removes them.

/// Number of taps per output.
const TAPS: usize = 29;
/// Outputs produced per group of six input samples.
const PHASES: usize = 7;
/// Input samples per group.
const GROUP: u32 = 6;
/// History ring length. A power of two at least [`TAPS`] long.
const RING: usize = 32;

/// PSX-SPX zigzag table: row `i - 1` holds the weight of the input `i`
/// samples back (`1` is the newest) for output tables 1 to 7, in 1/8000h.
#[rustfmt::skip]
pub(super) const ZIGZAG: [[i32; PHASES]; TAPS] = [
    [0x0000, 0x0000, 0x0000, 0x0000, -0x0001, 0x0002, -0x0005],
    [0x0000, 0x0000, 0x0000, -0x0001, 0x0003, -0x0008, 0x0011],
    [0x0000, 0x0000, -0x0001, 0x0003, -0x0008, 0x0010, -0x0023],
    [0x0000, -0x0002, 0x0003, -0x0008, 0x0011, -0x0023, 0x0046],
    [0x0000, 0x0000, -0x0002, 0x0006, -0x0010, 0x002B, -0x0017],
    [-0x0002, 0x0003, -0x0005, 0x0005, 0x000A, 0x001A, -0x0044],
    [0x000A, -0x0013, 0x001F, -0x001B, 0x006B, -0x00EB, 0x015B],
    [-0x0022, 0x003C, -0x004A, 0x00A6, -0x016D, 0x027B, -0x0347],
    [0x0041, -0x004B, 0x00B3, -0x01A8, 0x0350, -0x0548, 0x080E],
    [-0x0054, 0x00A2, -0x0192, 0x0372, -0x0623, 0x0AFA, -0x1249],
    [0x0034, -0x00E3, 0x02B1, -0x05BF, 0x0BCD, -0x16FA, 0x3C07],
    [0x0009, 0x0132, -0x039E, 0x09B8, -0x1780, 0x53E0, 0x53E0],
    [-0x010A, -0x0043, 0x04F8, -0x11B4, 0x6794, 0x3C07, -0x16FA],
    [0x0400, -0x0267, -0x05A6, 0x74BB, 0x234C, -0x1249, 0x0AFA],
    [-0x0A78, 0x0C9D, 0x7939, 0x0C9D, -0x0A78, 0x080E, -0x0548],
    [0x234C, 0x74BB, -0x05A6, -0x0267, 0x0400, -0x0347, 0x027B],
    [0x6794, -0x11B4, 0x04F8, -0x0043, -0x010A, 0x015B, -0x00EB],
    [-0x1780, 0x09B8, -0x039E, 0x0132, 0x0009, -0x0044, 0x001A],
    [0x0BCD, -0x05BF, 0x02B1, -0x00E3, 0x0034, -0x0017, 0x002B],
    [-0x0623, 0x0372, -0x0192, 0x00A2, -0x0054, 0x0046, -0x0023],
    [0x0350, -0x01A8, 0x00B3, -0x004B, 0x0041, -0x0023, 0x0010],
    [-0x016D, 0x00A6, -0x004A, 0x003C, -0x0022, 0x0011, -0x0008],
    [0x006B, -0x001B, 0x001F, -0x0013, 0x000A, -0x0005, 0x0002],
    [0x000A, 0x0005, -0x0005, 0x0003, -0x0001, 0x0000, 0x0000],
    [-0x0010, 0x0006, -0x0002, 0x0000, 0x0000, 0x0000, 0x0000],
    [0x0011, -0x0008, 0x0003, -0x0002, 0x0001, 0x0000, 0x0000],
    [-0x0008, 0x0003, -0x0001, 0x0000, 0x0000, 0x0000, 0x0000],
    [0x0003, -0x0001, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000],
    [-0x0001, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000],
];

/// Stereo 37.8 kHz to 44.1 kHz converter. State is the last [`RING`] input
/// frames per channel and the position inside the six-sample group, so a
/// stream converts identically however it is cut into sectors.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct XaResampler {
    left: [i16; RING],
    right: [i16; RING],
    /// Ring write position (counts up, used modulo [`RING`]).
    pos: u32,
    /// Inputs still needed before the next group of seven outputs.
    until_group: u32,
    /// Previous frame of an 18.9 kHz stream, for the interpolated half step.
    previous_half_rate: (i16, i16),
}

impl Default for XaResampler {
    fn default() -> Self {
        Self::new()
    }
}

impl XaResampler {
    /// Silent history, group boundary after the next six inputs.
    pub const fn new() -> Self {
        Self {
            left: [0; RING],
            right: [0; RING],
            pos: 0,
            until_group: GROUP,
            previous_half_rate: (0, 0),
        }
    }

    /// Back to silence, as at the start of a stream.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Feed one 37.8 kHz stereo frame. Every sixth call appends seven
    /// 44.1 kHz frames to `out`.
    pub fn push(&mut self, frame: (i16, i16), out: &mut Vec<(i16, i16)>) {
        let slot = (self.pos as usize) % RING;
        self.left[slot] = frame.0;
        self.right[slot] = frame.1;
        self.pos = self.pos.wrapping_add(1);
        self.until_group -= 1;
        if self.until_group != 0 {
            return;
        }
        self.until_group = GROUP;
        for phase in 0..PHASES {
            out.push((
                self.filter(&self.left, phase),
                self.filter(&self.right, phase),
            ));
        }
    }

    /// Feed one 18.9 kHz stereo frame: two 37.8 kHz frames, the average with
    /// the previous frame and then the frame itself.
    pub fn push_half_rate(&mut self, frame: (i16, i16), out: &mut Vec<(i16, i16)>) {
        let (pl, pr) = self.previous_half_rate;
        let mid = (
            ((i32::from(pl) + i32::from(frame.0)) >> 1) as i16,
            ((i32::from(pr) + i32::from(frame.1)) >> 1) as i16,
        );
        self.previous_half_rate = frame;
        self.push(mid, out);
        self.push(frame, out);
    }

    fn filter(&self, ring: &[i16; RING], phase: usize) -> i16 {
        let mut sum = 0i32;
        for (back, weights) in ZIGZAG.iter().enumerate() {
            let at = (self.pos as usize).wrapping_sub(back + 1) % RING;
            sum += i32::from(ring[at]) * weights[phase];
        }
        (sum >> 15).clamp(-0x8000, 0x7FFF) as i16
    }
}
