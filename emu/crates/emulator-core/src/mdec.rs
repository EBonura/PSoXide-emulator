//! MDEC -- Motion Decoder. Hardware MPEG-ish macroblock decoder.
//!
//! Games use the MDEC to play back pre-compressed FMV (cutscenes,
//! intros, attract-mode loops). The pipeline is:
//!
//! 1. CPU uploads quantization tables via DMA0 with command 2.
//! 2. CPU issues a decode command (1) to `0x1F80_1820` with block count.
//! 3. CPU streams N×M run-length-encoded coefficient halfwords into
//!    the MDEC via DMA0 (CPU→MDEC, channel 0).
//! 4. MDEC dequantizes, IDCT's, and YUV→RGB converts each macroblock
//!    into either 15-bit or 24-bit pixel output.
//! 5. CPU pulls decoded pixels out via DMA1 (MDEC→CPU, channel 1),
//!    which streams 256 pixels (16×16) per macroblock into RAM --
//!    typically destined for VRAM via a follow-up GPU draw.
//!
//! Written from nocash PSX-SPX "Macroblock Decoder (MDEC)" and checked
//! against the console captures in the public ps1-tests `mdec` programs
//! (see Provenance).
//!
//! MMIO map:
//!
//! ```text
//!   0x1F80_1820 R/W : command / parameter FIFO (write: commands + RLE data,
//!                                               read:  decoded pixel words)
//!   0x1F80_1824 R/W : status read / control write
//! ```
//!
//! The CPU never pokes RLE data through the FIFO directly -- it always
//! goes through DMA0 for speed (at typical FMV rates the CPU would spend
//! all its time shipping data otherwise). We expose `dma_write_in` +
//! `dma_read_out` entry points for the bus to call on DMA channel 0 /
//! channel 1 triggers.
//!
//! ## Provenance
//!
//! - **Register file, commands, status word**: PSX-SPX. The reset timing
//!   window, the swallowed control write and the idle status word are
//!   measurements from this project's console (hardware tests v1.26).
//! - **Block decode**: PSX-SPX's run-length format, the uploaded
//!   quantisation tables and the uploaded 8x8 IDCT matrix, applied in two
//!   passes. The intermediate roundings were fitted against the console
//!   captures that ship with the ps1-tests `mdec` programs (MIT licence):
//!   mono output matches at the console's 5-bit precision; colour output
//!   agrees with the console frame to within one LSB on most bytes, and
//!   the exact internal precision of the hardware's colour path is not
//!   known (the remaining differences are listed in the emulator
//!   provenance document).
//! - **Colour conversion**: PSX-SPX's coefficients (1.402, 0.3437, 0.7143,
//!   1.772) in 16.16 fixed point, rounded once at the output depth. 15-bit
//!   output rounds each field to the nearest multiple of eight; that
//!   choice matches the console frame in 96% of pixels against 92% for
//!   rounding to a byte first.
//! - **Scheduling**: output-driven decode and the coupling of the input
//!   DMA's completion to the output drain are pinned by the compat FMV
//!   hashes and the ps1-tests MDEC programs, not by a source.
//!
//! This module no longer contains any code derived from another emulator.
//! See `LICENSE` and `docs/PROVENANCE.md`.

// ===============================================================
//  Register addresses + command constants.
// ===============================================================

/// Base address of the MDEC MMIO port.
pub const MDEC_BASE: u32 = 0x1f80_1820;
/// Command / parameter register (writes issue commands or deliver
/// parameter words; reads return queued output pixels).
pub const MDEC_CMD_DATA: u32 = 0x1f80_1820;
/// Status register (read) / control register (write).
pub const MDEC_CTRL_STAT: u32 = 0x1f80_1824;

// Command field in `reg0`:
//   31..29 : command code (1 = decode, 2 = load quantization, 3 = cosine table)
//   28..27 : (decode only) output depth (4/8/24/15-bit)
//   26     : (decode only) signed output
//   25     : (decode only) set bit 15 on 15-bit output pixels
//   15..0  : parameter count (words) for the command

const MDEC0_DEPTH_SHIFT: u32 = 27;
const MDEC0_DEPTH_MASK: u32 = 0x3;
const MDEC0_SIGNED: u32 = 0x0400_0000;
/// Decode-command STP flag -- sets bit 15 of each 15-bit RGB output pixel.
const MDEC0_STP: u32 = 0x0200_0000;

// Status register (`reg1`) bits:
//   31    : Data-Out FIFO Empty
//   30    : Data-In FIFO Full
//   29    : Command Busy (decode in progress)
//   28    : Data-In Request via DMA0
//   27    : Data-Out Request via DMA1
//   26..25: Output Depth (00=4bpp, 01=8bpp, 10=24bpp, 11=15bpp)
//   24    : Output Signed
//   23    : Output Bit-15
//   18..16: Current block (Y1..Y4, Cr, Cb)
//   15..0 : Words remaining in parameter FIFO minus 1

const MDEC1_BUSY: u32 = 0x2000_0000;
#[allow(dead_code)]
const MDEC1_DREQ: u32 = 0x1800_0000;
#[allow(dead_code)]
const MDEC1_FIFO: u32 = 0xc000_0000;
#[allow(dead_code)]
const MDEC1_RGB24: u32 = 0x0200_0000;
#[allow(dead_code)]
const MDEC1_STP: u32 = 0x0080_0000;
const MDEC1_RESET: u32 = 0x8000_0000;
/// Data-Out FIFO Empty bit.
#[allow(dead_code)]
const MDEC1_EMPTY: u32 = 0x8000_0000;
/// Command Busy bit -- set while a decode is in progress.
#[allow(dead_code)]
const MDEC1_COMMAND_BUSY: u32 = 0x2000_0000;
/// Data-Out Request via DMA1.
#[allow(dead_code)]
const MDEC1_DMA_OUT_REQ: u32 = 0x0800_0000;
/// Data-In Request via DMA0.
#[allow(dead_code)]
const MDEC1_DMA_IN_REQ: u32 = 0x1000_0000;
#[allow(dead_code)]
const MDEC1_OUTPUT_DEPTH_MASK: u32 = 0x0600_0000;
#[allow(dead_code)]
const MDEC1_OUTPUT_SIGNED: u32 = 0x0100_0000;
#[allow(dead_code)]
const MDEC1_OUTPUT_BIT15: u32 = 0x0080_0000;

/// How long a reset keeps the status port on its reset-time word, in CPU
/// cycles from the reset write, when the MDEC was idle. SCPH-9002, hardware
/// tests v1.26 (`mdec_reset_trace`): the first read after a reset from idle,
/// 13 clocks in, already shows the idle word, so this is an upper bound.
const RESET_IDLE_CYCLES: u64 = 13;
/// The same when the MDEC was busy, or when the word a swallowed control
/// write left in the status port reads busy: silicon reads busy until 39
/// clocks after the reset write (41 with an enable written straight behind
/// it).
const RESET_BUSY_CYCLES: u64 = 39;
/// A control write landing this soon after the reset write is overwritten by
/// the reset: it is lost, and the status port reads back the written word
/// until the reset completes. Silicon brackets this span without measuring
/// it. In PSoXide's write timing an open-source library's order (reset and
/// enable back to back) puts its enable 2 cycles behind the reset and lost it
/// in 8 runs of 8 (from idle and from busy), as did the v1.25 driver's build
/// on the v1.25 disc; the same driver
/// rebuilt for v1.26 put it 7 cycles behind and kept it in 8 of 8, including
/// resets from busy.
const RESET_CAPTURE_CYCLES: u64 = 5;

/// End-of-data sentinel in an RLE coefficient stream.
const MDEC_END_OF_DATA: u16 = 0xfe00;

/// Block size constants -- 8×8 DCT blocks, 6 blocks per macroblock
/// (Cb, Cr, Y1, Y2, Y3, Y4).
const DSIZE: usize = 8;
const DSIZE2: usize = DSIZE * DSIZE;
const BLOCKS_PER_MACROBLOCK: usize = 6;

/// Row-major form used by the fixed-point matrix IDCT. Keeping the transpose
/// here (rather than after the two rounded passes) is observable at ±1 LSB.
const ZIG_ZAG_MATRIX: [usize; DSIZE2] = [
    0, 8, 1, 2, 9, 16, 24, 17, 10, 3, 4, 11, 18, 25, 32, 40, 33, 26, 19, 12, 5, 6, 13, 20, 27, 34,
    41, 48, 56, 49, 42, 35, 28, 21, 14, 7, 15, 22, 29, 36, 43, 50, 57, 58, 51, 44, 37, 30, 23, 31,
    38, 45, 52, 59, 60, 53, 46, 39, 47, 54, 61, 62, 55, 63,
];

/// Bus lines MDEC can report data-in / data-out requests on. Lets the
/// bus decide whether a DMA trigger should fire right now or wait.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MdecState {
    /// Idle -- ready for a new command.
    Idle,
    /// Accepting parameter / RLE words through DMA0.
    AwaitingData,
    /// Decoding -- output pixels are available via DMA1.
    DecodeReady,
}

// ===============================================================
//  Halfword queue.
// ===============================================================

/// FIFO of 16-bit words held as one contiguous run, so the decoder can read
/// it as a slice and the output side can copy it out in bulk. It serialises
/// as the sequence of halfwords still queued, exactly as the `VecDeque<u16>`
/// it replaces did, so save states are unchanged.
#[derive(Default)]
struct HalfQueue {
    buf: Vec<u16>,
    /// Index in `buf` of the oldest queued halfword.
    head: usize,
}

impl HalfQueue {
    /// Smallest consumed prefix worth squeezing out of `buf`.
    const COMPACT_AT: usize = 1 << 14;

    fn len(&self) -> usize {
        self.buf.len() - self.head
    }

    fn is_empty(&self) -> bool {
        self.head == self.buf.len()
    }

    /// The queued halfwords, oldest first.
    fn as_slice(&self) -> &[u16] {
        &self.buf[self.head..]
    }

    fn front(&self) -> Option<u16> {
        self.buf.get(self.head).copied()
    }

    fn clear(&mut self) {
        self.buf.clear();
        self.head = 0;
    }

    /// Drop the `n` oldest halfwords (`n <= len`).
    fn consume(&mut self, n: usize) {
        self.head += n;
        if self.head == self.buf.len() {
            self.clear();
        }
    }

    /// Make room to append: a fully drained queue restarts at the front of
    /// the buffer, and a long consumed prefix is squeezed out.
    fn prepare_append(&mut self) {
        if self.head >= Self::COMPACT_AT && self.head * 2 >= self.buf.len() {
            self.buf.drain(..self.head);
            self.head = 0;
        }
    }

    fn push(&mut self, value: u16) {
        self.prepare_append();
        self.buf.push(value);
    }

    fn extend_from_slice(&mut self, values: &[u16]) {
        self.prepare_append();
        self.buf.extend_from_slice(values);
    }

    /// Append each word as its low then its high halfword.
    fn extend_from_words(&mut self, words: &[u32]) {
        self.prepare_append();
        self.buf.reserve(words.len() * 2);
        for &word in words {
            self.buf.push(word as u16);
            self.buf.push((word >> 16) as u16);
        }
    }

    /// Append the little-endian halfwords of `bytes` (a multiple of two long).
    fn extend_from_le_bytes(&mut self, bytes: &[u8]) {
        self.prepare_append();
        self.buf.extend(
            bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]])),
        );
    }
}

impl serde::Serialize for HalfQueue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.as_slice().serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for HalfQueue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self {
            buf: Vec::deserialize(deserializer)?,
            head: 0,
        })
    }
}

/// What a DMA0 transfer delivers: words, or the bytes of the words in RAM.
enum DmaIn<'a> {
    Words(&'a [u32]),
    Bytes(&'a [u8]),
}

impl DmaIn<'_> {
    fn word_count(&self) -> usize {
        match self {
            Self::Words(words) => words.len(),
            Self::Bytes(bytes) => bytes.len() / 4,
        }
    }

    fn to_words(&self) -> std::borrow::Cow<'_, [u32]> {
        match self {
            Self::Words(words) => std::borrow::Cow::Borrowed(words),
            Self::Bytes(bytes) => std::borrow::Cow::Owned(
                bytes
                    .chunks_exact(4)
                    .map(|word| u32::from_le_bytes([word[0], word[1], word[2], word[3]]))
                    .collect(),
            ),
        }
    }
}

// ===============================================================
//  MDEC state.
// ===============================================================

/// Full MDEC subsystem state. Owns quantization tables, a decode
/// output buffer, and the register pair visible to software.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Mdec {
    /// Last write to the command register (`reg0`).
    reg0: u32,
    /// Latched status register. The words the status port shows are built
    /// from this plus the FIFO and DMA state in [`Mdec::status_word`].
    reg1: u32,
    /// Luminance quantisation table, as uploaded (zig-zag order). `DSIZE2`
    /// (64) is past serde's built-in 32-element array cap, so this
    /// round-trips through [`crate::serde_big_array::array`].
    #[serde(with = "crate::serde_big_array::array")]
    quant_y: [u8; DSIZE2],
    /// Chrominance quantisation table, as uploaded.
    #[serde(with = "crate::serde_big_array::array")]
    quant_uv: [u8; DSIZE2],
    /// Host-provided 8x8 IDCT matrix, transposed on upload as in hardware.
    #[serde(with = "crate::serde_big_array::array")]
    scale_table: [i16; DSIZE2],
    /// `scale_table` in the layout the IDCT reads (see [`scale_columns`]),
    /// rebuilt from it when `scale_columns_valid` is false.
    #[serde(skip)]
    scale_columns: [[i16; DSIZE]; DSIZE],
    #[serde(skip)]
    scale_columns_valid: bool,
    /// Buffered RLE halfwords received via DMA0 since the decode
    /// command was issued. Drained during decode_macroblocks.
    rl_queue: HalfQueue,
    /// Decoded pixel output queue -- ready for DMA1 or data-port reads.
    /// All four output depths are packed into little-endian halfwords.
    out_queue: HalfQueue,
    /// Current decode command's RLE word count (from the low 16 bits
    /// of reg0). We stop decoding when we've consumed that many
    /// parameter words.
    #[allow(dead_code)]
    expected_param_words: u32,
    /// Parameter words already accepted for the current direct-FIFO upload.
    #[serde(default)]
    direct_param_words_received: u32,
    /// Diagnostic: raw command words seen since reset. Games might
    /// ship thousands; we just count so probes can tell "MDEC was
    /// spoken to" vs "MDEC was ignored". Excluded from save states.
    #[serde(skip)]
    commands_seen: u64,
    /// Diagnostic: raw parameter-data words seen since reset. Excluded
    /// from save states.
    #[serde(skip)]
    params_seen: u64,
    /// Enable DMA0 (data-in) -- bit 30 of the last control write.
    dma_in_enabled: bool,
    /// Enable DMA1 (data-out) -- bit 29 of the last control write.
    dma_out_enabled: bool,
    /// Diagnostic: total macroblocks decoded since reset. Excluded
    /// from save states.
    #[serde(skip)]
    macroblocks_decoded: u64,
    /// Recent command-register writes, newest at the end. Diagnostic
    /// -- excluded from save states.
    #[serde(skip)]
    command_history: Vec<u32>,
    /// CPU cycle at which the current reset completes; the status port
    /// reads [`Mdec::reset_status`] until then. 0 when no reset is running.
    #[serde(default)]
    reset_until: u64,
    /// Control writes before this cycle are swallowed by the reset.
    #[serde(default)]
    reset_capture_until: u64,
    /// The status word silicon shows while a reset completes.
    #[serde(default)]
    reset_status: u32,
    /// DMA0 was kicked while the MDEC raised no data-in request; the bus
    /// starts it when a later register write raises one.
    #[serde(default)]
    dma_in_waiting: bool,
    /// No command since the last reset: silicon then reads the status
    /// parameter-count field as 0 rather than FFFFh ("none").
    #[serde(default = "default_true")]
    fresh_from_reset: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Mdec {
    fn default() -> Self {
        Self::new()
    }
}

impl Mdec {
    /// Freshly-reset MDEC in its post-reset state.
    pub fn new() -> Self {
        Self {
            reg0: 0,
            reg1: status_idle(),
            quant_y: [0; DSIZE2],
            quant_uv: [0; DSIZE2],
            scale_table: [0; DSIZE2],
            scale_columns: [[0; DSIZE]; DSIZE],
            scale_columns_valid: false,
            rl_queue: HalfQueue::default(),
            out_queue: HalfQueue::default(),
            expected_param_words: 0,
            direct_param_words_received: 0,
            commands_seen: 0,
            params_seen: 0,
            dma_in_enabled: false,
            dma_out_enabled: false,
            macroblocks_decoded: 0,
            command_history: Vec::new(),
            reset_until: 0,
            reset_capture_until: 0,
            reset_status: 0,
            dma_in_waiting: false,
            fresh_from_reset: true,
        }
    }

    /// True when `phys` lies inside the MDEC MMIO block.
    pub fn contains(phys: u32) -> bool {
        (MDEC_BASE..MDEC_BASE + 8).contains(&phys)
    }

    /// Read a 32-bit word from an MDEC register.
    pub fn read32(&mut self, phys: u32) -> u32 {
        match phys & 0x1f80_1fff {
            MDEC_CMD_DATA => {
                if self.out_queue.len() < 2 {
                    self.decode_until_output_words(1);
                }
                if self.out_queue.is_empty() {
                    self.reg0
                } else {
                    let word = self.pop_output_word();
                    // Silicon keeps decoding while the CPU drains the output
                    // FIFO, so the next macroblock is ready by the next
                    // status poll (a CPU-fed frame comes out whole).
                    if self.out_queue.is_empty() {
                        self.decode_until_output_words(1);
                    }
                    word
                }
            }
            MDEC_CTRL_STAT => self.status_word(),
            _ => 0,
        }
    }

    /// Register read from the bus at CPU cycle `now`. Unlike [`Mdec::read32`]
    /// this sees a reset that is still completing.
    pub fn read32_at(&mut self, phys: u32, now: u64) -> u32 {
        let value = if phys & 0x1f80_1fff == MDEC_CTRL_STAT && self.resetting(now) {
            self.reset_status
        } else {
            self.read32(phys)
        };
        if crate::env_flag!("PSOXIDE_TRACE_MDEC_IO") {
            eprintln!("[mdec-io] r cycle={now} addr={phys:#010x} value={value:#010x}");
        }
        value
    }

    /// Register write from the bus at CPU cycle `now`. Unlike
    /// [`Mdec::write32`] a reset takes time here, and a control write that
    /// lands right behind it is lost (see [`RESET_CAPTURE_CYCLES`]).
    pub fn write32_at(&mut self, phys: u32, value: u32, now: u64) {
        if crate::env_flag!("PSOXIDE_TRACE_MDEC_IO") {
            eprintln!("[mdec-io] w cycle={now} addr={phys:#010x} value={value:#010x}");
        }
        if phys & 0x1f80_1fff == MDEC_CTRL_STAT {
            if value & MDEC1_RESET != 0 {
                let busy = if self.resetting(now) {
                    self.reset_status
                } else {
                    self.status_word()
                } & MDEC1_BUSY;
                self.control_write(value);
                self.reset_status = status_idle() | busy;
                self.reset_until = now + reset_cycles(busy);
                self.reset_capture_until = now + RESET_CAPTURE_CYCLES;
                return;
            }
            if now < self.reset_capture_until && self.resetting(now) {
                // Silicon: the word reads back from the status port until
                // the reset completes, and the enables it carried are gone.
                self.reset_status = value;
                self.reset_until = now + reset_cycles(value & MDEC1_BUSY);
                self.reset_capture_until = 0;
                return;
            }
        }
        self.write32(phys, value);
    }

    fn resetting(&self, now: u64) -> bool {
        now < self.reset_until
    }

    /// True when the MDEC is asking DMA0 for data: the data-in enable is
    /// set and the current command still takes parameter words. DMA0 moves
    /// nothing without it. Only a register write can raise it, so the bus
    /// re-checks a parked kick after each one.
    pub fn dma_in_request(&self) -> bool {
        self.dma_in_enabled && self.reg1 & MDEC1_BUSY != 0 && self.expected_param_words != 0
    }

    /// See [`Mdec::dma_in_request`]: whether a DMA0 kick is parked on it.
    pub fn dma_in_waiting(&self) -> bool {
        self.dma_in_waiting
    }

    /// Park (or release) a DMA0 kick on the data-in request.
    pub fn set_dma_in_waiting(&mut self, waiting: bool) {
        self.dma_in_waiting = waiting;
    }

    /// Write a 32-bit word to an MDEC register. Commands + data
    /// arrive at `0x1820`, control bits at `0x1824`.
    pub fn write32(&mut self, phys: u32, value: u32) {
        match phys & 0x1f80_1fff {
            MDEC_CMD_DATA => self.command_write(value),
            MDEC_CTRL_STAT => self.control_write(value),
            _ => {}
        }
    }

    /// Called by the bus for DMA channel 0 transfers -- CPU→MDEC.
    /// Each word becomes two halfwords in little-endian order.
    pub fn dma_write_in(&mut self, words: &[u32]) {
        self.dma_write_in_from(DmaIn::Words(words));
    }

    /// [`Mdec::dma_write_in`] for words held as little-endian bytes, as they
    /// sit in main RAM. `bytes.len()` is a whole number of words.
    pub fn dma_write_in_bytes(&mut self, bytes: &[u8]) {
        debug_assert!(bytes.len().is_multiple_of(4));
        self.dma_write_in_from(DmaIn::Bytes(bytes));
    }

    fn dma_write_in_from(&mut self, input: DmaIn) {
        let word_count = input.word_count();
        self.params_seen = self.params_seen.saturating_add(word_count as u64);
        self.reg1 |= MDEC1_STP;

        match self.command_code() {
            1 => {
                self.reg1 |= MDEC1_BUSY;
                // The words DMA0 delivers are the command's parameters: once
                // they are in, the next MDEC0 write is a new command again.
                self.expected_param_words =
                    self.expected_param_words.saturating_sub(word_count as u32);
                self.rl_queue.clear();
                self.out_queue.clear();
                match input {
                    DmaIn::Words(words) => self.rl_queue.extend_from_words(words),
                    DmaIn::Bytes(bytes) => self.rl_queue.extend_from_le_bytes(bytes),
                }
                // Silicon exposes data-out readiness before software starts
                // DMA1. Public ps1-tests polls bit 31 between DMA0 and DMA1.
                self.decode_until_output_words(1);
                if !self.can_continue_decode() && self.out_queue.is_empty() {
                    self.reg1 &= !MDEC1_BUSY;
                }
            }
            2 => {
                self.absorb_quant_upload(&input.to_words());
                self.expected_param_words = 0;
                self.reg1 &= !MDEC1_BUSY;
            }
            3 => {
                self.absorb_scale_upload(&input.to_words());
                self.expected_param_words = 0;
                self.reg1 &= !MDEC1_BUSY;
            }
            _ => {}
        }
    }

    /// Called by the bus for DMA channel 1 transfers -- MDEC→CPU.
    /// Fills `out` with decoded pixel words.
    pub fn dma_read_out(&mut self, out: &mut [u32]) {
        let mut done = 0;
        while done < out.len() {
            if self.out_queue.len() < 2 {
                self.decode_until_output_words(2);
            }
            // Every whole word already queued goes out without a decode check:
            // the check only matters when fewer than two halfwords are left.
            let words = (self.out_queue.len() / 2).min(out.len() - done);
            if words == 0 {
                out[done] = self.pop_output_word();
                done += 1;
                continue;
            }
            let queued = &self.out_queue.as_slice()[..words * 2];
            for (slot, pair) in out[done..done + words]
                .iter_mut()
                .zip(queued.chunks_exact(2))
            {
                *slot = u32::from(pair[0]) | (u32::from(pair[1]) << 16);
            }
            self.out_queue.consume(words * 2);
            done += words;
        }
    }

    /// [`Mdec::dma_read_out`] for a destination held as bytes: the words
    /// land little-endian, as they do in main RAM. `dst.len()` is a whole
    /// number of words.
    pub fn dma_read_out_bytes(&mut self, dst: &mut [u8]) {
        debug_assert!(dst.len().is_multiple_of(4));
        let mut done = 0;
        while done < dst.len() {
            if self.out_queue.len() < 2 {
                self.decode_until_output_words(2);
            }
            let words = (self.out_queue.len() / 2).min((dst.len() - done) / 4);
            if words == 0 {
                let word = self.pop_output_word();
                dst[done..done + 4].copy_from_slice(&word.to_le_bytes());
                done += 4;
                continue;
            }
            let queued = &self.out_queue.as_slice()[..words * 2];
            for (pair, &half) in dst[done..done + words * 4].chunks_exact_mut(2).zip(queued) {
                pair.copy_from_slice(&half.to_le_bytes());
            }
            self.out_queue.consume(words * 2);
            done += words * 4;
        }
    }

    /// Called by the bus when DMA channel 1's scheduled completion fires.
    /// A decode command's input transfer stays busy until the output side
    /// has drained the decoded frame (the compat FMV hashes and the
    /// ps1-tests MDEC programs pin this); this returns `true` exactly when
    /// channel 0 may be completed alongside channel 1.
    pub fn complete_dma_out(&mut self) -> bool {
        // The frame is finished only when every halfword still queued is
        // FE00 padding. A leading FE00 alone is not enough: a block that
        // fills all 64 coefficients completes without its end-of-block
        // code, so the encoder's trailing FE00 is still queued when a DMA1
        // slice ends exactly on that macroblock, with the rest of the frame
        // behind it.
        if self.command_code() == 1
            && self.reg1 & MDEC1_BUSY != 0
            && self.out_queue.is_empty()
            && self
                .rl_queue
                .as_slice()
                .iter()
                .all(|&word| word == MDEC_END_OF_DATA)
        {
            self.reg1 &= !(MDEC1_BUSY | MDEC1_STP);
            self.rl_queue.clear();
            return true;
        }
        false
    }

    /// Current coarse state -- diagnostic, for UI display / debug.
    pub fn state(&self) -> MdecState {
        if !self.out_queue.is_empty() {
            MdecState::DecodeReady
        } else if self.reg1 & MDEC1_BUSY != 0 {
            MdecState::AwaitingData
        } else {
            MdecState::Idle
        }
    }

    /// Are DMA channel 0 (in) or 1 (out) enabled?
    pub fn dma_in_enabled(&self) -> bool {
        self.dma_in_enabled
    }

    /// See [`Mdec::dma_in_enabled`].
    pub fn dma_out_enabled(&self) -> bool {
        self.dma_out_enabled
    }

    /// Diagnostic -- total command words the CPU has shipped.
    pub fn commands_seen(&self) -> u64 {
        self.commands_seen
    }

    /// Diagnostic -- total parameter words seen.
    pub fn params_seen(&self) -> u64 {
        self.params_seen
    }

    /// Diagnostic -- total macroblocks fully decoded since reset.
    pub fn macroblocks_decoded(&self) -> u64 {
        self.macroblocks_decoded
    }

    /// True when decoded pixel words are ready for DMA channel 1.
    pub fn output_ready(&self) -> bool {
        !self.out_queue.is_empty()
    }

    /// True when channel 1 can make forward progress, either from the
    /// decoded FIFO or by decoding another macroblock on demand.
    pub fn can_dma_out(&self) -> bool {
        self.output_ready() || self.can_continue_decode()
    }

    /// True when a decode DMA0 upload should remain busy until DMA1
    /// drains the corresponding output frame.
    pub fn decode_dma0_waits_for_output(&self) -> bool {
        self.command_code() == 1 && self.reg1 & MDEC1_BUSY != 0
    }

    /// Recent raw command-register writes, newest at the end.
    pub fn command_history(&self) -> &[u32] {
        &self.command_history
    }

    /// Diagnostic: queued compressed halfwords still held after the
    /// latest decode attempt.
    pub fn queued_rle_halfwords(&self) -> usize {
        self.rl_queue.len()
    }

    /// Diagnostic: next compressed halfword, if any.
    pub fn next_rle_halfword(&self) -> Option<u16> {
        self.rl_queue.front()
    }

    // ============================================================
    //  Register-level command / control / status.
    // ============================================================

    fn status_word(&self) -> u32 {
        let mut status = self.reg1
            & !(MDEC1_EMPTY
                | MDEC1_DMA_IN_REQ
                | MDEC1_DMA_OUT_REQ
                | MDEC1_OUTPUT_DEPTH_MASK
                | MDEC1_OUTPUT_SIGNED
                | MDEC1_OUTPUT_BIT15);

        if self.out_queue.is_empty() {
            status |= MDEC1_EMPTY;
        }
        if self.dma_in_enabled && self.reg1 & MDEC1_BUSY != 0 {
            status |= MDEC1_DMA_IN_REQ;
        }
        if self.dma_out_enabled && !self.out_queue.is_empty() {
            status |= MDEC1_DMA_OUT_REQ;
        }
        // Parameter words still expected, minus one (FFFFh when none). The
        // latched `reg1` never carries this field.
        status &= !0xffff;
        if !self.fresh_from_reset {
            status |= self.expected_param_words.wrapping_sub(1) & 0xffff;
        }
        if self.command_code() == 1 {
            status |= self.output_depth() << 25;
            if self.reg0 & MDEC0_SIGNED != 0 {
                status |= MDEC1_OUTPUT_SIGNED;
            }
            if self.reg0 & MDEC0_STP != 0 {
                status |= MDEC1_OUTPUT_BIT15;
            }
        }
        status
    }

    fn command_write(&mut self, value: u32) {
        if self.expected_param_words != 0 {
            self.params_seen = self.params_seen.saturating_add(1);
            match self.command_code() {
                1 => {
                    self.rl_queue.push(value as u16);
                    self.rl_queue.push((value >> 16) as u16);
                }
                2 => self.absorb_quant_word_at(self.direct_param_words_received, value),
                3 => self.absorb_scale_word_at(self.direct_param_words_received, value),
                _ => {}
            }
            self.direct_param_words_received += 1;
            self.expected_param_words -= 1;
            if self.expected_param_words == 0 {
                if self.command_code() == 1 {
                    self.decode_until_output_words(1);
                    if self.out_queue.is_empty() && !self.can_continue_decode() {
                        self.reg1 &= !MDEC1_BUSY;
                    }
                } else {
                    self.reg1 &= !MDEC1_BUSY;
                }
            }
            return;
        }

        self.reg0 = value;
        self.fresh_from_reset = false;
        self.commands_seen = self.commands_seen.saturating_add(1);
        if self.command_history.len() == 64 {
            self.command_history.remove(0);
        }
        self.command_history.push(value);
        self.direct_param_words_received = 0;
        self.expected_param_words = match self.command_code() {
            1 => value & 0xffff,
            2 => {
                if value & 1 != 0 {
                    32
                } else {
                    16
                }
            }
            3 => 32,
            _ => 0,
        };
        if self.expected_param_words != 0 {
            self.reg1 |= MDEC1_BUSY;
            if self.command_code() == 1 {
                self.rl_queue.clear();
                self.out_queue.clear();
            }
        }
    }

    fn control_write(&mut self, value: u32) {
        if value & MDEC1_RESET != 0 {
            // Reset -- clears state but preserves quantization tables
            // per PSX-SPX (they're written via command 0x4 and need
            // to survive MDEC resets so games don't re-upload).
            self.reg0 = 0;
            self.reg1 = status_idle();
            self.dma_in_enabled = false;
            self.dma_out_enabled = false;
            self.expected_param_words = 0;
            self.direct_param_words_received = 0;
            self.fresh_from_reset = true;
            self.rl_queue.clear();
            self.out_queue.clear();
            return;
        }
        self.dma_in_enabled = value & (1 << 30) != 0;
        self.dma_out_enabled = value & (1 << 29) != 0;
    }

    fn pop_output_word(&mut self) -> u32 {
        let queued = self.out_queue.as_slice();
        let lo = queued.first().copied().unwrap_or(0) as u32;
        let hi = queued.get(1).copied().unwrap_or(0) as u32;
        self.out_queue.consume(queued.len().min(2));
        lo | (hi << 16)
    }

    /// Take one 32-bit word of a quantisation-table upload. The upload is
    /// the 64 luminance bytes followed (when bit 0 of the command asked for
    /// it) by the 64 chrominance bytes, four bytes per word, low byte first.
    fn absorb_quant_word_at(&mut self, word_index: u32, value: u32) {
        for (byte_index, byte) in value.to_le_bytes().into_iter().enumerate() {
            let pos = word_index as usize * 4 + byte_index;
            match pos {
                0..=63 => self.quant_y[pos] = byte,
                64..=127 => self.quant_uv[pos - 64] = byte,
                _ => {}
            }
        }
    }

    fn absorb_quant_upload(&mut self, words: &[u32]) {
        for (word_index, &value) in words.iter().enumerate() {
            self.absorb_quant_word_at(word_index as u32, value);
        }
    }

    fn absorb_scale_word_at(&mut self, word_index: u32, value: u32) {
        for (lane, sample) in [value as u16, (value >> 16) as u16].into_iter().enumerate() {
            let source = word_index as usize * 2 + lane;
            if source < DSIZE2 {
                let x = source / DSIZE;
                let y = source % DSIZE;
                self.scale_table[y * DSIZE + x] = (sample as i16) >> 3;
                self.scale_columns_valid = false;
            }
        }
    }

    fn absorb_scale_upload(&mut self, words: &[u32]) {
        for (index, &value) in words.iter().take(32).enumerate() {
            self.absorb_scale_word_at(index as u32, value);
        }
    }

    #[inline]
    fn command_code(&self) -> u32 {
        (self.reg0 >> 29) & 0x7
    }

    #[inline]
    fn output_depth(&self) -> u32 {
        (self.reg0 >> MDEC0_DEPTH_SHIFT) & MDEC0_DEPTH_MASK
    }

    /// Decode enough macroblocks to make at least `min_words` 32-bit
    /// words available. Decoding is driven by the output side (the data
    /// port and DMA1) rather than eagerly when the input arrives, as the
    /// hardware's small output FIFO makes the decoder wait for its reader;
    /// this also keeps a frame's end marker from cutting off later DMA1
    /// chunks.
    fn decode_until_output_words(&mut self, min_words: usize) {
        let min_halfwords = min_words.saturating_mul(2);
        while self.out_queue.len() < min_halfwords && self.can_continue_decode() {
            if !self.decode_one_macroblock() {
                break;
            }
        }
    }

    fn can_continue_decode(&self) -> bool {
        self.command_code() == 1 && self.reg1 & MDEC1_BUSY != 0 && !self.rl_queue.is_empty()
    }

    /// Decode one mono block or one six-block colour macroblock.
    /// Returns `true` on success, `false` if we ran out of data.
    fn decode_one_macroblock(&mut self) -> bool {
        if self.output_depth() <= 1 {
            return self.decode_one_mono_block();
        }
        self.refresh_scale_columns();

        // Stream order is Cr, Cb, Y1, Y2, Y3, Y4; the two chroma blocks use
        // the chrominance table and the four luma blocks the luminance one.
        let mut blocks = [[0i16; DSIZE2]; BLOCKS_PER_MACROBLOCK];
        let mut pos = 0;
        let rl = self.rl_queue.as_slice();
        for (index, block) in blocks.iter_mut().enumerate() {
            let quant = if index < 2 {
                &self.quant_uv
            } else {
                &self.quant_y
            };
            if !decode_block(rl, &mut pos, block, quant) {
                self.rl_queue.consume(pos);
                return false;
            }
            idct(&self.scale_columns, block);
        }
        self.rl_queue.consume(pos);
        self.emit_macroblock_output(&blocks);
        self.macroblocks_decoded = self.macroblocks_decoded.saturating_add(1);
        true
    }

    /// Bring the IDCT matrix in the layout [`idct`] wants up to date with the
    /// uploaded table.
    fn refresh_scale_columns(&mut self) {
        if !self.scale_columns_valid {
            self.scale_columns = scale_columns(&self.scale_table);
            self.scale_columns_valid = true;
        }
    }

    fn decode_one_mono_block(&mut self) -> bool {
        self.refresh_scale_columns();
        let mut block = [0i16; DSIZE2];
        let mut pos = 0;
        let ok = decode_block(
            self.rl_queue.as_slice(),
            &mut pos,
            &mut block,
            &self.quant_y,
        );
        self.rl_queue.consume(pos);
        if !ok {
            return false;
        }
        idct(&self.scale_columns, &mut block);

        let add = if self.reg0 & MDEC0_SIGNED != 0 {
            0
        } else {
            0x80
        };
        let mut mono = [0u8; DSIZE2];
        for (dst, &sample) in mono.iter_mut().zip(block.iter()) {
            // The signed modes expose the two's-complement low bits rather
            // than saturating negative values to zero.
            *dst = (sign_extend_9(i32::from(sample)).clamp(-128, 127) + add) as u8;
        }

        match self.output_depth() {
            0 => {
                let mut packed = [0u16; DSIZE2 / 4];
                for (dst, samples) in packed.iter_mut().zip(mono.chunks_exact(4)) {
                    let nibble = |sample: u8| ((u16::from(sample) + 8) >> 4).min(15);
                    *dst = nibble(samples[0])
                        | (nibble(samples[1]) << 4)
                        | (nibble(samples[2]) << 8)
                        | (nibble(samples[3]) << 12);
                }
                self.out_queue.extend_from_slice(&packed);
            }
            1 => {
                let mut packed = [0u16; DSIZE2 / 2];
                for (dst, samples) in packed.iter_mut().zip(mono.chunks_exact(2)) {
                    *dst = u16::from(samples[0]) | (u16::from(samples[1]) << 8);
                }
                self.out_queue.extend_from_slice(&packed);
            }
            _ => unreachable!(),
        }

        self.macroblocks_decoded = self.macroblocks_decoded.saturating_add(1);
        true
    }

    /// Convert a decoded macroblock to pixels and queue them, row by row:
    /// 15-bit pixels as one halfword each, 24-bit pixels as R, G, B bytes
    /// packed little-endian into halfwords.
    fn emit_macroblock_output(&mut self, blocks: &[[i16; DSIZE2]; BLOCKS_PER_MACROBLOCK]) {
        let signed = self.reg0 & MDEC0_SIGNED != 0;
        if self.output_depth() == 2 {
            self.out_queue
                .extend_from_le_bytes(&macroblock_bytes24(blocks, signed));
        } else {
            let bit15: u16 = if self.reg0 & MDEC0_STP != 0 {
                0x8000
            } else {
                0
            };
            self.out_queue
                .extend_from_slice(&macroblock_pixels15(blocks, signed, bit15));
        }
    }
}

fn reset_cycles(busy: u32) -> u64 {
    if busy != 0 {
        RESET_BUSY_CYCLES
    } else {
        RESET_IDLE_CYCLES
    }
}

/// Default MDEC status measured on real hardware: output FIFO empty and
/// current-block field 4. The public I/O-width corpus observes this exact word
/// after every control-port write that does not request a reset.
const fn status_idle() -> u32 {
    0x8004_0000
}

// ===============================================================
//  Block-level decode: RLE → coefficients → IDCT.
// ===============================================================

/// Read one block from the RLE stream `rl`, starting at `*pos`, into the
/// fixed-point coefficients the IDCT matrix consumes: the DC term and each
/// AC term are dequantised with the uploaded table (and the block's
/// quantisation scale for AC terms), rounded and saturated to 15 bits, and
/// placed in matrix order. `*pos` moves past what was read. Returns `false`
/// when the stream ends before the block does, with `*pos` at its end. The
/// intermediate rounding is observable on silicon (ps1-tests' mono and frame
/// programs), so each step is kept exactly.
#[inline]
fn decode_block(
    rl: &[u16],
    pos: &mut usize,
    block: &mut [i16; DSIZE2],
    quant: &[u8; DSIZE2],
) -> bool {
    let mut at = *pos;
    let head = loop {
        match rl.get(at) {
            Some(&MDEC_END_OF_DATA) => at += 1,
            Some(&value) => {
                at += 1;
                break value;
            }
            None => {
                *pos = at;
                return false;
            }
        }
    };

    block.fill(0);
    let q_scale = (head >> 10) & 0x3f;
    let dc = rle_val(head);
    let coefficient = if q_scale == 0 {
        dc << 5
    } else {
        ((dc * i32::from(quant[0])) << 4)
            + if dc < 0 {
                8
            } else if dc > 0 {
                -8
            } else {
                0
            }
    };
    block[ZIG_ZAG_MATRIX[0]] = coefficient.clamp(-0x4000, 0x3fff) as i16;

    let mut index = 0usize;
    while index < DSIZE2 - 1 {
        let Some(&value) = rl.get(at) else {
            *pos = at;
            return false;
        };
        at += 1;
        index = index.saturating_add(rle_run(value) as usize + 1);
        if index < DSIZE2 {
            let ac = rle_val(value);
            let scaled_quant = i32::from(q_scale) * i32::from(quant[index]);
            let coefficient = if scaled_quant == 0 {
                ac << 5
            } else {
                (((ac * scaled_quant) >> 3) << 4)
                    + if ac < 0 {
                        8
                    } else if ac > 0 {
                        -8
                    } else {
                        0
                    }
            };
            block[ZIG_ZAG_MATRIX[index]] = coefficient.clamp(-0x4000, 0x3fff) as i16;
        }
    }
    *pos = at;
    true
}

/// The uploaded IDCT matrix as the passes of [`idct`] read it:
/// `columns[u][x]` is the weight of input `u` in output `x`.
fn scale_columns(scale_table: &[i16; DSIZE2]) -> [[i16; DSIZE]; DSIZE] {
    let mut columns = [[0i16; DSIZE]; DSIZE];
    for (u, column) in columns.iter_mut().enumerate() {
        for (x, weight) in column.iter_mut().enumerate() {
            *weight = scale_table[x * DSIZE + u];
        }
    }
    columns
}

/// Two-pass 8x8 inverse DCT as the hardware performs it: each pass multiplies
/// by the uploaded matrix, adds half an LSB and drops 15 fractional bits; the
/// second pass saturates its result to signed 9 bits and then to -128..=127.
/// The first pass stores its result transposed, which is observable at one
/// LSB when the two roundings differ.
///
/// The sums fit 32 bits: coefficients are saturated to 15 bits, matrix
/// entries are an uploaded halfword shifted right by three (13 bits), so a
/// sum of eight products stays under 2^30, and a first-pass result under
/// 2^15. An all-zero input row has an all-zero first-pass result, so the
/// second pass only visits the rows that held something.
#[inline]
fn idct(columns: &[[i16; DSIZE]; DSIZE], block: &mut [i16; DSIZE2]) {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: NEON is part of the aarch64 baseline.
        unsafe { idct_neon(columns, block) }
    }
    #[cfg(not(target_arch = "aarch64"))]
    idct_scalar(columns, block);
}

/// [`idct`] without vector instructions (the reference for [`idct_neon`]).
#[cfg(any(test, not(target_arch = "aarch64")))]
fn idct_scalar(columns: &[[i16; DSIZE]; DSIZE], block: &mut [i16; DSIZE2]) {
    let mut temp = [[0i32; DSIZE]; DSIZE];
    let mut rows = 0u32;
    for (row, input) in block.chunks_exact(DSIZE).enumerate() {
        if input.iter().all(|&coefficient| coefficient == 0) {
            continue;
        }
        rows |= 1 << row;
        let mut sum = [0i32; DSIZE];
        for (&coefficient, weights) in input.iter().zip(columns) {
            let coefficient = i32::from(coefficient);
            for (total, &weight) in sum.iter_mut().zip(weights) {
                *total += coefficient * i32::from(weight);
            }
        }
        for (x, &total) in sum.iter().enumerate() {
            temp[x][row] = (total + 0x4000) >> 15;
        }
    }
    for (output, input) in block.chunks_exact_mut(DSIZE).zip(&temp) {
        let mut sum = [0i32; DSIZE];
        let mut pending = rows;
        while pending != 0 {
            let u = pending.trailing_zeros() as usize;
            pending &= pending - 1;
            for (total, &weight) in sum.iter_mut().zip(&columns[u]) {
                *total += input[u] * i32::from(weight);
            }
        }
        for (dst, &total) in output.iter_mut().zip(&sum) {
            *dst = ((total + 0x4000) >> 15).clamp(-128, 127) as i16;
        }
    }
}

/// [`idct`] on NEON: a row of eight outputs is a vector of 16-bit weights
/// multiplied by one coefficient and accumulated in 32 bits (`smlal`), the
/// rounding shift is the instruction's own, and the final saturation to
/// -128..=127 is two saturating narrows.
///
/// # Safety
///
/// Needs NEON, which every aarch64 CPU has.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn idct_neon(columns: &[[i16; DSIZE]; DSIZE], block: &mut [i16; DSIZE2]) {
    use std::arch::aarch64::{
        vcombine_s16, vdupq_n_s16, vdupq_n_s32, vget_low_s16, vld1q_s16, vmaxvq_u16,
        vmlal_high_laneq_s16, vmlal_laneq_s16, vmovl_s8, vqmovn_s16, vqrshrn_n_s32,
        vreinterpretq_u16_s16, vrshrn_n_s32, vst1q_s16,
    };

    // Accumulate `coefficients[LANE] * weights` into the low and high four
    // outputs.
    macro_rules! mac {
        ($lo:expr, $hi:expr, $weights:expr, $coefficients:expr, $lane:literal) => {
            $lo = vmlal_laneq_s16::<$lane>($lo, vget_low_s16($weights), $coefficients);
            $hi = vmlal_high_laneq_s16::<$lane>($hi, $weights, $coefficients);
        };
    }

    // SAFETY (all loads and stores below): every pointer is to a row of eight
    // halfwords inside an array of eight rows.
    unsafe {
        let weights = [
            vld1q_s16(columns[0].as_ptr()),
            vld1q_s16(columns[1].as_ptr()),
            vld1q_s16(columns[2].as_ptr()),
            vld1q_s16(columns[3].as_ptr()),
            vld1q_s16(columns[4].as_ptr()),
            vld1q_s16(columns[5].as_ptr()),
            vld1q_s16(columns[6].as_ptr()),
            vld1q_s16(columns[7].as_ptr()),
        ];

        // First pass: one result row per non-zero input row.
        let mut first = [vdupq_n_s16(0); DSIZE];
        let mut rows = 0u32;
        for (row, result) in first.iter_mut().enumerate() {
            let input = vld1q_s16(block.as_ptr().add(row * DSIZE));
            if vmaxvq_u16(vreinterpretq_u16_s16(input)) == 0 {
                continue;
            }
            rows |= 1 << row;
            let (mut lo, mut hi) = (vdupq_n_s32(0), vdupq_n_s32(0));
            mac!(lo, hi, weights[0], input, 0);
            mac!(lo, hi, weights[1], input, 1);
            mac!(lo, hi, weights[2], input, 2);
            mac!(lo, hi, weights[3], input, 3);
            mac!(lo, hi, weights[4], input, 4);
            mac!(lo, hi, weights[5], input, 5);
            mac!(lo, hi, weights[6], input, 6);
            mac!(lo, hi, weights[7], input, 7);
            *result = vcombine_s16(vrshrn_n_s32::<15>(lo), vrshrn_n_s32::<15>(hi));
        }

        // Second pass: output row `r` takes lane `r` of every first-pass row.
        let mut lo = [vdupq_n_s32(0); DSIZE];
        let mut hi = [vdupq_n_s32(0); DSIZE];
        while rows != 0 {
            let u = rows.trailing_zeros() as usize;
            rows &= rows - 1;
            let (w, p) = (weights[u], first[u]);
            mac!(lo[0], hi[0], w, p, 0);
            mac!(lo[1], hi[1], w, p, 1);
            mac!(lo[2], hi[2], w, p, 2);
            mac!(lo[3], hi[3], w, p, 3);
            mac!(lo[4], hi[4], w, p, 4);
            mac!(lo[5], hi[5], w, p, 5);
            mac!(lo[6], hi[6], w, p, 6);
            mac!(lo[7], hi[7], w, p, 7);
        }
        for r in 0..DSIZE {
            let rounded = vcombine_s16(vqrshrn_n_s32::<15>(lo[r]), vqrshrn_n_s32::<15>(hi[r]));
            vst1q_s16(
                block.as_mut_ptr().add(r * DSIZE),
                vmovl_s8(vqmovn_s16(rounded)),
            );
        }
    }
}

/// Extract the quantization-scale / run-length field from an RLE word
/// (top 6 bits).
#[inline]
fn rle_run(v: u16) -> i32 {
    (v >> 10) as i32
}

/// Extract the signed 10-bit value field from an RLE word, sign-extended.
#[inline]
fn rle_val(v: u16) -> i32 {
    let bits = 10;
    let shift = 32 - bits;
    ((v as i32) << shift) >> shift
}

#[inline]
fn sign_extend_9(value: i32) -> i32 {
    (value << 23) >> 23
}

// ===============================================================
//  YUV -> RGB conversion and output packing.
// ===============================================================

/// Colour-space conversion fixed point: coefficients and results carry 16
/// fractional bits, so rounding happens once, at the output depth.
const CSC_SHIFT: u32 = 16;
/// `1.402`, the weight of Cr in red (PSX-SPX "MDEC Decompression").
const CSC_CR_TO_R: i32 = 91_881;
/// `0.3437`, the weight of Cb in green.
const CSC_CB_TO_G: i32 = 22_525;
/// `0.7143`, the weight of Cr in green.
const CSC_CR_TO_G: i32 = 46_813;
/// `1.772`, the weight of Cb in blue.
const CSC_CB_TO_B: i32 = 116_130;

/// Each component of a pixel is its luma plus a term from its 2x2 chroma
/// group, `weight * chroma` in 16.16 fixed point, saturated to -128.0..=127.0
/// and rounded once at the output depth. Luma is a whole number, so the
/// rounding only has to touch the chroma term: `floor((term + bias) / 2^16)`
/// is added to the luma, and the rest happens on whole numbers. Returns the
/// sums for the 16 pixels of row `y` of the macroblock, red, green, blue.
///
/// `blocks` are the six decoded blocks in stream order: Cr, Cb, Y1..Y4. The
/// luma of the row comes from the two blocks on its side of the macroblock.
#[inline(always)]
fn component_sums(
    blocks: &[[i16; DSIZE2]; BLOCKS_PER_MACROBLOCK],
    y: usize,
    bias: i32,
) -> [[i16; 16]; 3] {
    let [cr_block, cb_block, luma_blocks @ ..] = blocks;
    let chroma = (y / 2) * DSIZE;
    let mut terms = [[0i16; DSIZE]; 3];
    for i in 0..DSIZE {
        let cb = i32::from(cb_block[chroma + i]);
        let cr = i32::from(cr_block[chroma + i]);
        terms[0][i] = ((CSC_CR_TO_R * cr + bias) >> CSC_SHIFT) as i16;
        terms[1][i] = ((-(CSC_CB_TO_G * cb) - CSC_CR_TO_G * cr + bias) >> CSC_SHIFT) as i16;
        terms[2][i] = ((CSC_CB_TO_B * cb + bias) >> CSC_SHIFT) as i16;
    }
    let luma_row = (y % DSIZE) * DSIZE;
    let left = (y / DSIZE) * 2;
    let mut luma = [0i16; 16];
    luma[..DSIZE].copy_from_slice(&luma_blocks[left][luma_row..luma_row + DSIZE]);
    luma[DSIZE..].copy_from_slice(&luma_blocks[left + 1][luma_row..luma_row + DSIZE]);
    let mut sums = [[0i16; 16]; 3];
    for (sum, term) in sums.iter_mut().zip(&terms) {
        for (pixel, value) in sum.iter_mut().enumerate() {
            *value = luma[pixel] + term[pixel / 2];
        }
    }
    sums
}

/// The 24-bit output bytes of a macroblock, three per pixel (R, G, B), each
/// the sum saturated to -128..=127, offset into 0..=255 unless the command
/// asked for signed output, in which case the two's-complement byte is
/// delivered as is.
fn macroblock_bytes24(
    blocks: &[[i16; DSIZE2]; BLOCKS_PER_MACROBLOCK],
    signed: bool,
) -> [u8; 16 * 16 * 3] {
    let flip = if signed { 0 } else { 0x80 };
    let mut bytes = [0u8; 16 * 16 * 3];
    for (y, row) in bytes.chunks_exact_mut(16 * 3).enumerate() {
        let sums = component_sums(blocks, y, 1 << (CSC_SHIFT - 1));
        for (pixel, rgb) in row.chunks_exact_mut(3).enumerate() {
            for (byte, sum) in rgb.iter_mut().zip(&sums) {
                *byte = (sum[pixel].clamp(-128, 127) as i8 as u8) ^ flip;
            }
        }
    }
    bytes
}

/// The 15-bit output pixels of a macroblock. Each 5-bit field is the byte
/// value (`sum` + 128) rounded to the nearest multiple of eight, ties
/// upward, working from the unrounded component, and saturated at 31; signed
/// output reduces the two's-complement byte the same way, without
/// saturating (a field can come out as 32).
fn macroblock_pixels15(
    blocks: &[[i16; DSIZE2]; BLOCKS_PER_MACROBLOCK],
    signed: bool,
    bit15: u16,
) -> [u16; 16 * 16] {
    let mut pixels = [0u16; 16 * 16];
    let bias = if signed { 1 << (CSC_SHIFT - 1) } else { 0 };
    for (y, row) in pixels.chunks_exact_mut(16).enumerate() {
        let sums = component_sums(blocks, y, bias);
        for (pixel, out) in row.iter_mut().enumerate() {
            let [r, g, b] = [0, 1, 2].map(|c| {
                let sum = i32::from(sums[c][pixel]);
                if signed {
                    (u16::from(sum.clamp(-128, 127) as i8 as u8) + 4) >> 3
                } else {
                    // (value + 128 + 4) / 8, floored.
                    ((sum + 132) >> 3).clamp(0, 31) as u16
                }
            });
            *out = r | (g << 5) | (b << 10) | bit15;
        }
    }
    pixels
}

// ===============================================================
//  Tests.
// ===============================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// The decoder as it was before the vector rewrite: a 64-bit IDCT, one
    /// pixel at a time through the colour conversion, one halfword at a time
    /// through `VecDeque`s. The fast paths must agree with it word for word.
    mod reference {
        use super::*;
        use std::collections::VecDeque;

        pub const CSC_MIN: i32 = -128 << CSC_SHIFT;
        pub const CSC_MAX: i32 = 127 << CSC_SHIFT;

        pub fn decode_block(
            rl: &mut VecDeque<u16>,
            block: &mut [i16; DSIZE2],
            quant: &[u8; DSIZE2],
        ) -> bool {
            let head = loop {
                match rl.pop_front() {
                    Some(MDEC_END_OF_DATA) => continue,
                    Some(value) => break value,
                    None => return false,
                }
            };

            block.fill(0);
            let q_scale = (head >> 10) & 0x3f;
            let dc = rle_val(head);
            let coefficient = if q_scale == 0 {
                dc << 5
            } else {
                ((dc * i32::from(quant[0])) << 4)
                    + if dc < 0 {
                        8
                    } else if dc > 0 {
                        -8
                    } else {
                        0
                    }
            };
            block[ZIG_ZAG_MATRIX[0]] = coefficient.clamp(-0x4000, 0x3fff) as i16;

            let mut index = 0usize;
            while index < DSIZE2 - 1 {
                let value = match rl.pop_front() {
                    Some(value) => value,
                    None => return false,
                };
                index = index.saturating_add(rle_run(value) as usize + 1);
                if index < DSIZE2 {
                    let ac = rle_val(value);
                    let scaled_quant = i32::from(q_scale) * i32::from(quant[index]);
                    let coefficient = if scaled_quant == 0 {
                        ac << 5
                    } else {
                        (((ac * scaled_quant) >> 3) << 4)
                            + if ac < 0 {
                                8
                            } else if ac > 0 {
                                -8
                            } else {
                                0
                            }
                    };
                    block[ZIG_ZAG_MATRIX[index]] = coefficient.clamp(-0x4000, 0x3fff) as i16;
                }
            }
            true
        }

        pub fn idct(scale_table: &[i16; DSIZE2], block: &mut [i16; DSIZE2]) {
            let mut temp = [0i16; DSIZE2];
            for column in 0..DSIZE {
                for x in 0..DSIZE {
                    let mut sum = 0i64;
                    for u in 0..DSIZE {
                        sum += i64::from(block[column * DSIZE + u])
                            * i64::from(scale_table[x * DSIZE + u]);
                    }
                    temp[x * DSIZE + column] = ((sum + 0x4000) >> 15) as i16;
                }
            }
            for column in 0..DSIZE {
                for x in 0..DSIZE {
                    let mut sum = 0i64;
                    for u in 0..DSIZE {
                        sum += i64::from(temp[column * DSIZE + u])
                            * i64::from(scale_table[x * DSIZE + u]);
                    }
                    let rounded = ((sum + 0x4000) >> 15) as i32;
                    block[column * DSIZE + x] = rounded.clamp(-128, 127) as i16;
                }
            }
        }

        pub fn yuv_to_rgb(y: i32, cb: i32, cr: i32) -> [i32; 3] {
            let y = y << CSC_SHIFT;
            [
                y + CSC_CR_TO_R * cr,
                y - CSC_CB_TO_G * cb - CSC_CR_TO_G * cr,
                y + CSC_CB_TO_B * cb,
            ]
            .map(|component| component.clamp(CSC_MIN, CSC_MAX))
        }

        pub fn output_byte(component: i32, signed: bool) -> u8 {
            let rounded = (component + (1 << (CSC_SHIFT - 1))) >> CSC_SHIFT;
            (if signed { rounded } else { rounded + 128 }) as u8
        }

        pub fn output_field15(component: i32, signed: bool) -> u16 {
            if signed {
                (u16::from(output_byte(component, true)) + 4) >> 3
            } else {
                let biased = i64::from(component) + (132i64 << CSC_SHIFT);
                ((biased >> (CSC_SHIFT + 3)) as u16).min(31)
            }
        }

        pub fn macroblock_pixel(
            blocks: &[[i16; DSIZE2]; BLOCKS_PER_MACROBLOCK],
            x: usize,
            y: usize,
        ) -> [i32; 3] {
            let chroma = (y / 2) * DSIZE + x / 2;
            let luma_block = 2 + (y / DSIZE) * 2 + x / DSIZE;
            let luma = (y % DSIZE) * DSIZE + x % DSIZE;
            yuv_to_rgb(
                i32::from(blocks[luma_block][luma]),
                i32::from(blocks[1][chroma]),
                i32::from(blocks[0][chroma]),
            )
        }

        /// The halfwords a colour macroblock queues.
        pub fn macroblock_halfwords(
            blocks: &[[i16; DSIZE2]; BLOCKS_PER_MACROBLOCK],
            depth: u32,
            signed: bool,
            stp: bool,
        ) -> Vec<u16> {
            let mut out = Vec::new();
            let pixels = (0..16).flat_map(|y| (0..16).map(move |x| (x, y)));
            if depth == 2 {
                let mut bytes = [0u8; 16 * 16 * 3];
                for ((x, y), dst) in pixels.zip(bytes.chunks_exact_mut(3)) {
                    let rgb = macroblock_pixel(blocks, x, y);
                    for (byte, component) in dst.iter_mut().zip(rgb) {
                        *byte = output_byte(component, signed);
                    }
                }
                for pair in bytes.chunks_exact(2) {
                    out.push(u16::from(pair[0]) | (u16::from(pair[1]) << 8));
                }
            } else {
                let bit15 = if stp { 0x8000 } else { 0 };
                for (x, y) in pixels {
                    let [r, g, b] = macroblock_pixel(blocks, x, y)
                        .map(|component| output_field15(component, signed));
                    out.push(r | (g << 5) | (b << 10) | bit15);
                }
            }
            out
        }

        /// Everything a decode command's stream turns into: the halfwords of
        /// every complete macroblock (or mono block) in order.
        pub fn decode_stream(m: &Mdec, stream: &[u16]) -> Vec<u16> {
            let mut rl: VecDeque<u16> = stream.iter().copied().collect();
            let depth = (m.reg0 >> MDEC0_DEPTH_SHIFT) & MDEC0_DEPTH_MASK;
            let signed = m.reg0 & MDEC0_SIGNED != 0;
            let stp = m.reg0 & MDEC0_STP != 0;
            let mut out = Vec::new();
            loop {
                if depth <= 1 {
                    let mut block = [0i16; DSIZE2];
                    if !decode_block(&mut rl, &mut block, &m.quant_y) {
                        break;
                    }
                    idct(&m.scale_table, &mut block);
                    let add = if signed { 0 } else { 0x80 };
                    let mut mono = [0u8; DSIZE2];
                    for (dst, &sample) in mono.iter_mut().zip(block.iter()) {
                        *dst = (sign_extend_9(i32::from(sample)).clamp(-128, 127) + add) as u8;
                    }
                    if depth == 0 {
                        for samples in mono.chunks_exact(4) {
                            let nibble = |sample: u8| ((u16::from(sample) + 8) >> 4).min(15);
                            out.push(
                                nibble(samples[0])
                                    | (nibble(samples[1]) << 4)
                                    | (nibble(samples[2]) << 8)
                                    | (nibble(samples[3]) << 12),
                            );
                        }
                    } else {
                        for samples in mono.chunks_exact(2) {
                            out.push(u16::from(samples[0]) | (u16::from(samples[1]) << 8));
                        }
                    }
                } else {
                    let mut blocks = [[0i16; DSIZE2]; BLOCKS_PER_MACROBLOCK];
                    let mut complete = true;
                    for (index, block) in blocks.iter_mut().enumerate() {
                        let quant = if index < 2 { &m.quant_uv } else { &m.quant_y };
                        if !decode_block(&mut rl, block, quant) {
                            complete = false;
                            break;
                        }
                        idct(&m.scale_table, block);
                    }
                    if !complete {
                        break;
                    }
                    out.extend(macroblock_halfwords(&blocks, depth, signed, stp));
                }
            }
            out
        }
    }

    /// Small deterministic generator for the comparison tests.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 32) as u32
        }

        fn below(&mut self, bound: u32) -> u32 {
            self.next() % bound
        }
    }

    /// A random run-length stream: blocks with a quantisation scale, a DC
    /// term and AC terms of every run length, padding words between them,
    /// and a cut-off tail now and then.
    fn random_stream(rng: &mut Rng, blocks: usize) -> Vec<u16> {
        let mut stream = Vec::new();
        for _ in 0..blocks {
            let padding = rng.below(3) as usize;
            stream.extend(std::iter::repeat_n(MDEC_END_OF_DATA, padding));
            let q_scale = match rng.below(6) {
                0 => 0,
                1 => 63,
                _ => rng.below(64) as u16,
            };
            // Both ends of the value range come up often.
            let value = |rng: &mut Rng| match rng.below(8) {
                0 => 0x1ffu16,
                1 => 0x200,
                2 => 0x3ff,
                _ => rng.below(0x400) as u16,
            };
            stream.push((q_scale << 10) | value(rng));
            let sparse = rng.below(3) != 0;
            let mut index = 0usize;
            while index < DSIZE2 - 1 {
                let run = if sparse && rng.below(4) != 0 {
                    rng.below(20) as u16
                } else {
                    rng.below(3) as u16
                };
                let word = (run << 10) | value(rng);
                // A word that would be the end-of-data marker is not data.
                if word == MDEC_END_OF_DATA {
                    continue;
                }
                stream.push(word);
                index += usize::from(run) + 1;
            }
            if rng.below(2) == 0 {
                stream.push(MDEC_END_OF_DATA);
            }
        }
        if rng.below(3) == 0 {
            let keep = rng.below(stream.len() as u32 + 1) as usize;
            stream.truncate(keep);
        }
        stream
    }

    /// A decoder with random tables and a decode command for `format`
    /// (command bits 28..25: depth, signed, bit 15), plus the halfwords it
    /// was fed.
    fn random_decoder(rng: &mut Rng, format: u32, blocks: usize) -> (Mdec, Vec<u16>) {
        let mut m = Mdec::new();
        m.write32(MDEC_CTRL_STAT, 0x6000_0000);
        m.write32(MDEC_CMD_DATA, 0x4000_0001);
        let quant: Vec<u32> = (0..32)
            .map(|_| match rng.below(4) {
                0 => 0,
                1 => 0xffff_ffff,
                _ => rng.next(),
            })
            .collect();
        m.dma_write_in(&quant);
        m.write32(MDEC_CMD_DATA, 0x6000_0000);
        let scale: Vec<u32> = match rng.below(4) {
            // Largest and smallest weights: the widest sums.
            0 => vec![0x7fff_7fff; 32],
            1 => vec![0x8000_8000; 32],
            2 => (0..32).map(|_| rng.next() & 0xe000_e000).collect(),
            _ => (0..32).map(|_| rng.next()).collect(),
        };
        m.dma_write_in(&scale);
        let stream = random_stream(rng, blocks);
        let mut words: Vec<u32> = stream
            .chunks(2)
            .map(|pair| {
                u32::from(pair[0]) | (u32::from(*pair.get(1).unwrap_or(&MDEC_END_OF_DATA)) << 16)
            })
            .collect();
        if stream.len() % 2 == 1 {
            // The pad word is part of the stream the decoder sees.
            let last = words.len() - 1;
            words[last] |= u32::from(MDEC_END_OF_DATA) << 16;
        }
        let fed: Vec<u16> = words
            .iter()
            .flat_map(|&w| [w as u16, (w >> 16) as u16])
            .collect();
        m.write32(
            MDEC_CMD_DATA,
            0x2000_0000 | (format << 25) | (words.len() as u32 & 0xffff),
        );
        m.dma_write_in(&words);
        (m, fed)
    }

    #[test]
    fn idct_matches_the_wide_reference_on_random_blocks() {
        let mut rng = Rng(7);
        for round in 0..30_000 {
            let mut scale = [0i16; DSIZE2];
            for weight in &mut scale {
                *weight = match round % 5 {
                    0 => 0x7fff,
                    1 => -0x8000,
                    _ => rng.next() as i16,
                } >> 3;
            }
            let mut block = [0i16; DSIZE2];
            let density = rng.below(65) as usize;
            for _ in 0..density {
                let at = rng.below(64) as usize;
                block[at] = match rng.below(6) {
                    0 => 0x3fff,
                    1 => -0x4000,
                    _ => (rng.next() as i16) >> 1,
                }
                .clamp(-0x4000, 0x3fff);
            }
            let mut expected = block;
            reference::idct(&scale, &mut expected);
            let columns = scale_columns(&scale);
            let mut fast = block;
            idct(&columns, &mut fast);
            assert_eq!(fast, expected, "round {round}");
            let mut scalar = block;
            idct_scalar(&columns, &mut scalar);
            assert_eq!(scalar, expected, "round {round} (scalar)");
        }
    }

    #[test]
    fn colour_conversion_matches_the_per_pixel_reference_for_every_input() {
        // Every luma against every chroma pair: a macroblock holds all 256
        // luma values, so 65 536 of them cover every (y, cb, cr).
        let mut blocks = [[0i16; DSIZE2]; BLOCKS_PER_MACROBLOCK];
        for (index, sample) in (0..256).map(|v| v as i16 - 128).enumerate() {
            let block = 2 + (index / 128) * 2 + (index % 16) / 8;
            let at = ((index / 16) % 8) * 8 + index % 8;
            blocks[block][at] = sample;
        }
        for cb in -128i16..128 {
            for cr in -128i16..128 {
                blocks[0] = [cr; DSIZE2];
                blocks[1] = [cb; DSIZE2];
                for (signed, depth) in [(false, 2), (true, 2), (false, 3), (true, 3)] {
                    let stp = cb & 1 != 0;
                    let expected = reference::macroblock_halfwords(&blocks, depth, signed, stp);
                    let got: Vec<u16> = if depth == 2 {
                        macroblock_bytes24(&blocks, signed)
                            .chunks_exact(2)
                            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                            .collect()
                    } else {
                        macroblock_pixels15(&blocks, signed, if stp { 0x8000 } else { 0 }).to_vec()
                    };
                    assert_eq!(
                        got, expected,
                        "cb {cb} cr {cr} signed {signed} depth {depth}"
                    );
                }
            }
        }
    }

    #[test]
    fn random_streams_decode_to_the_reference_words() {
        let mut rng = Rng(0x5eed);
        let mut decoded_words = 0;
        for round in 0..400 {
            // Depths 4, 8, 24 and 15 bit, signed or not, bit 15 or not.
            let format = rng.below(16);
            let blocks = 1 + rng.below(40) as usize;
            let (mut m, fed) = random_decoder(&mut rng, format, blocks);
            let mut expected = reference::decode_stream(&m, &fed);
            // Reading past the end of the data yields zeros.
            let total = expected.len() / 2 + 1 + rng.below(100) as usize;
            expected.resize(total * 2, 0);
            let expected_words: Vec<u32> = expected
                .chunks_exact(2)
                .map(|pair| u32::from(pair[0]) | (u32::from(pair[1]) << 16))
                .collect();

            // The same decoder state again, restored from a save state, for the
            // byte-wise read.
            let snapshot = postcard::to_allocvec(&m).expect("serialise");
            let mut by_bytes: Mdec = postcard::from_bytes(&snapshot).expect("restore");
            decoded_words += expected_words.iter().filter(|&&word| word != 0).count();

            let mut got = vec![0u32; total];
            let mut at = 0;
            while at < got.len() {
                let chunk = (1 + rng.below(200) as usize).min(got.len() - at);
                m.dma_read_out(&mut got[at..at + chunk]);
                at += chunk;
            }
            assert_eq!(got, expected_words, "round {round} format {format:#x}");

            let mut bytes = vec![0u8; total * 4];
            let mut at = 0;
            while at < bytes.len() {
                let chunk = ((1 + rng.below(200) as usize) * 4).min(bytes.len() - at);
                by_bytes.dma_read_out_bytes(&mut bytes[at..at + chunk]);
                at += chunk;
            }
            let from_bytes: Vec<u32> = bytes
                .chunks_exact(4)
                .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
                .collect();
            assert_eq!(from_bytes, expected_words, "round {round} (bytes)");
        }
        // The streams must have produced pixels, not just agreed on zeros.
        assert!(
            decoded_words > 100_000,
            "only {decoded_words} words decoded"
        );
    }

    #[test]
    fn a_new_matrix_upload_takes_effect_on_the_next_decode() {
        let mut rng = Rng(99);
        let (mut m, fed) = random_decoder(&mut rng, 0b0100, 4);
        let mut out = vec![0u32; 2000];
        m.dma_read_out(&mut out);
        // Replace the matrix and decode a second frame with it.
        m.write32(MDEC_CMD_DATA, 0x6000_0000);
        m.dma_write_in(&[0x1234_5678u32; 32]);
        let words: Vec<u32> = fed
            .chunks_exact(2)
            .map(|pair| u32::from(pair[0]) | (u32::from(pair[1]) << 16))
            .collect();
        m.write32(
            MDEC_CMD_DATA,
            0x2000_0000 | (0b0100 << 25) | words.len() as u32,
        );
        m.dma_write_in(&words);
        let expected = reference::decode_stream(&m, &fed);
        let mut got = vec![0u32; expected.len() / 2];
        m.dma_read_out(&mut got);
        let got: Vec<u16> = got
            .iter()
            .flat_map(|&w| [w as u16, (w >> 16) as u16])
            .collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn halfword_queue_serialises_like_the_deque_it_replaced() {
        let mut queue = HalfQueue::default();
        queue.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
        queue.consume(2);
        queue.push(7);
        let deque: std::collections::VecDeque<u16> = [3, 4, 5, 6, 7].into();
        assert_eq!(
            postcard::to_allocvec(&queue).expect("queue"),
            postcard::to_allocvec(&deque).expect("deque")
        );
        let back: HalfQueue =
            postcard::from_bytes(&postcard::to_allocvec(&deque).expect("deque")).expect("decode");
        assert_eq!(back.as_slice(), &[3, 4, 5, 6, 7]);
    }

    #[test]
    fn halfword_queue_reuses_its_buffer_once_drained() {
        let mut queue = HalfQueue::default();
        queue.extend_from_slice(&[1; 8]);
        queue.consume(8);
        assert!(queue.is_empty());
        assert_eq!(queue.head, 0);
        queue.extend_from_slice(&vec![2; HalfQueue::COMPACT_AT * 2]);
        queue.consume(HalfQueue::COMPACT_AT * 2 - 3);
        queue.push(9);
        assert_eq!(queue.as_slice(), &[2, 2, 2, 9]);
        assert_eq!(queue.len(), 4);
    }

    #[test]
    fn contains_covers_both_registers() {
        assert!(Mdec::contains(0x1f80_1820));
        assert!(Mdec::contains(0x1f80_1823));
        assert!(Mdec::contains(0x1f80_1824));
        assert!(Mdec::contains(0x1f80_1827));
        assert!(!Mdec::contains(0x1f80_181c));
        assert!(!Mdec::contains(0x1f80_1828));
    }

    #[test]
    fn fresh_status_matches_silicon_idle() {
        let mut m = Mdec::new();
        let stat = m.read32(MDEC_CTRL_STAT);
        assert_eq!(stat, 0x8004_0000);
    }

    #[test]
    fn data_port_read_returns_latched_command_register() {
        let mut m = Mdec::new();
        assert_eq!(m.read32(MDEC_CMD_DATA), 0);
        m.write32(MDEC_CMD_DATA, 0x3200_0540);
        assert_eq!(m.read32(MDEC_CMD_DATA), 0x3200_0540);
    }

    #[test]
    fn control_write_reset_bit_clears_state() {
        let mut m = Mdec::new();
        m.write32(MDEC_CTRL_STAT, 0x6000_0000);
        assert!(m.dma_in_enabled());
        assert!(m.dma_out_enabled());
        m.write32(MDEC_CTRL_STAT, 0x8000_0000);
        assert!(!m.dma_in_enabled());
        assert!(!m.dma_out_enabled());
    }

    #[test]
    fn control_write_latches_dma_enables() {
        let mut m = Mdec::new();
        m.write32(MDEC_CTRL_STAT, 0x4000_0000);
        assert!(m.dma_in_enabled());
        assert!(!m.dma_out_enabled());
        m.write32(MDEC_CTRL_STAT, 0x2000_0000);
        assert!(!m.dma_in_enabled());
        assert!(m.dma_out_enabled());
    }

    #[test]
    fn decode_dma_status_exposes_dynamic_requests_and_format() {
        let mut m = Mdec::new();
        m.write32(MDEC_CTRL_STAT, 0x6000_0000);
        m.write32(MDEC_CMD_DATA, 0x3200_0006);
        m.dma_write_in(&[0xfe00_0000; 6]);

        let stat = m.read32(MDEC_CTRL_STAT);
        assert_eq!(stat & MDEC1_BUSY, MDEC1_BUSY);
        assert_eq!(stat & MDEC1_STP, MDEC1_STP);
        assert_eq!(stat & MDEC1_DMA_IN_REQ, MDEC1_DMA_IN_REQ);
        assert_eq!(stat & MDEC1_DMA_OUT_REQ, MDEC1_DMA_OUT_REQ);
        assert_eq!(stat & MDEC1_OUTPUT_DEPTH_MASK, 2 << 25);
        assert_eq!(stat & MDEC1_EMPTY, 0);
    }

    #[test]
    fn data_write_tallies_commands_vs_parameters() {
        let mut m = Mdec::new();
        // Quant table upload command -- 32 param words expected.
        m.write32(MDEC_CMD_DATA, 0x4000_0001);
        assert_eq!(m.commands_seen(), 1);
        // DMA0 carries the payload after the command register is latched.
        m.dma_write_in(&[0xdead_beef]);
        assert_eq!(m.params_seen(), 1);
    }

    #[test]
    fn quant_upload_command_marks_busy_then_clears() {
        let mut m = Mdec::new();
        // Command 2 (colour quant upload), 32 parameter words expected.
        m.write32(MDEC_CMD_DATA, 0x4000_0001);
        assert_eq!(m.read32(MDEC_CTRL_STAT) & MDEC1_BUSY, MDEC1_BUSY);
        // Deliver 32 words through DMA0.
        m.dma_write_in(&[0; 32]);
        assert_eq!(m.read32(MDEC_CTRL_STAT) & MDEC1_BUSY, 0);
    }

    #[test]
    fn direct_fifo_quant_and_scale_uploads_complete() {
        let mut m = Mdec::new();
        m.write32(MDEC_CMD_DATA, 0x4000_0001);
        for _ in 0..32 {
            m.write32(MDEC_CMD_DATA, 0x1010_1010);
        }
        assert_eq!(m.read32(MDEC_CTRL_STAT) & MDEC1_BUSY, 0);
        assert!(m.quant_y.iter().all(|&value| value == 0x10));
        assert!(m.quant_uv.iter().all(|&value| value == 0x10));

        m.write32(MDEC_CMD_DATA, 0x6000_0000);
        for _ in 0..32 {
            m.write32(MDEC_CMD_DATA, 0);
        }
        assert_eq!(m.read32(MDEC_CTRL_STAT) & MDEC1_BUSY, 0);
    }

    #[test]
    fn decode_command_emits_empty_output_on_sentinel_only_stream() {
        let mut m = Mdec::new();
        // Upload identity quant tables so decode produces well-defined
        // (but all-zero) output.
        m.write32(MDEC_CMD_DATA, 0x4000_0001);
        m.dma_write_in(&[0x01_01_01_01; 32]);
        // Issue decode command -- tiny parameter count.
        m.write32(MDEC_CMD_DATA, 0x3000_0001);
        // Feed a single word holding two END-of-data sentinels.
        let sentinel = MDEC_END_OF_DATA as u32;
        m.dma_write_in(&[sentinel | (sentinel << 16)]);
        // Padding-only input cannot produce a block, so the command
        // returns to idle and the data-out FIFO remains empty.
        assert_eq!(m.read32(MDEC_CTRL_STAT) & MDEC1_BUSY, 0);
        assert_eq!(m.read32(MDEC_CTRL_STAT) & MDEC1_EMPTY, MDEC1_EMPTY);
        let mut out = [0u32; 1];
        m.dma_read_out(&mut out);
        assert_eq!(out[0], 0);
        assert!(!m.complete_dma_out());
        assert_eq!(m.read32(MDEC_CTRL_STAT) & MDEC1_BUSY, 0);
    }

    #[test]
    fn idct_applies_the_matrix_in_both_passes_with_rounding() {
        // A diagonal matrix of 0.5 (0x4000 in 1.15 fixed point) halves the
        // block once per pass.
        let mut scale = [0i16; DSIZE2];
        for i in 0..DSIZE {
            scale[i * DSIZE + i] = 0x4000;
        }
        let mut block = [0i16; DSIZE2];
        block[0] = 400;
        block[DSIZE + 1] = 6;
        idct(&scale_columns(&scale), &mut block);
        assert_eq!(block[0], 100);
        // 6 / 4 = 1.5 rounds up to 2.
        assert_eq!(block[DSIZE + 1], 2);
        assert_eq!(block.iter().filter(|&&v| v != 0).count(), 2);
    }

    #[test]
    fn rle_run_and_val_extract_correctly() {
        // Top 6 bits = 0b101010 = 42 → run.
        // Low 10 bits = 0x123 signed = 0x123 (positive, 291).
        let word = 0b1010_1000_0001_0010_u16; // run=42, value=0x12 (hmm)
                                              // Let me pick a cleaner example:
                                              // run = 5 (top 6 bits = 000101), val = 0x1F (low 10 bits = 00_0001_1111 = 31).
        let word2 = (5u16 << 10) | 0x001f;
        assert_eq!(rle_run(word2), 5);
        assert_eq!(rle_val(word2), 31);
        // Negative value: low 10 bits = 0x3FF = -1 signed.
        let word3 = 0x03ff;
        assert_eq!(rle_val(word3), -1);
        // Unused variable so compiler is happy.
        let _ = rle_run(word);
    }

    fn bytes(y: i32, cb: i32, cr: i32) -> [u8; 3] {
        use reference::{output_byte, yuv_to_rgb};
        yuv_to_rgb(y, cb, cr).map(|component| output_byte(component, false))
    }

    #[test]
    fn grey_pixels_keep_their_luma_in_every_component() {
        for y in [-128, -1, 0, 1, 127] {
            assert_eq!(bytes(y, 0, 0), [(y + 128) as u8; 3]);
        }
    }

    #[test]
    fn chroma_moves_the_components_by_the_documented_weights() {
        // Cr = 64: red +1.402 * 64 = 89.7, green -0.7143 * 64 = -45.7.
        assert_eq!(bytes(0, 0, 64), [128 + 90, 128 - 46, 128]);
        // Cb = 64: blue +1.772 * 64 = 113.4, green -0.3437 * 64 = -22.0.
        assert_eq!(bytes(0, 64, 0), [128, 128 - 22, 128 + 113]);
    }

    #[test]
    fn components_saturate_to_the_signed_byte_range() {
        assert_eq!(bytes(127, 0, 127), [255, 128 + 36, 255]);
        // Green is the one component the opposite chroma pulls back up:
        // -128 + 0.3437 * 128 + 0.7143 * 128 = 7.4.
        assert_eq!(bytes(-128, -128, -128), [0, 128 + 7, 0]);
    }

    #[test]
    fn signed_output_keeps_the_twos_complement_byte() {
        use reference::{output_byte, yuv_to_rgb};
        let rgb = yuv_to_rgb(-1, 0, 0);
        assert_eq!(output_byte(rgb[0], true), 0xff);
        assert_eq!(output_byte(yuv_to_rgb(5, 0, 0)[0], true), 5);
    }

    #[test]
    fn fifteen_bit_fields_round_to_the_nearest_multiple_of_eight() {
        use reference::{output_field15, yuv_to_rgb};
        let field = |y: i32| output_field15(yuv_to_rgb(y, 0, 0)[0], false);
        // Byte values 0..=3 round to 0, 4..=11 to 1, and the top saturates.
        assert_eq!(field(-128), 0);
        assert_eq!(field(-125), 0); // byte 3
        assert_eq!(field(-124), 1); // byte 4
        assert_eq!(field(-117), 1); // byte 11
        assert_eq!(field(-116), 2); // byte 12
        assert_eq!(field(127), 31);
        assert_eq!(field(123), 31); // byte 251: (251 + 4) / 8 = 31
    }

    #[test]
    fn macroblocks_counter_increments_on_successful_decode() {
        let mut m = Mdec::new();
        // Identity quant tables.
        m.write32(MDEC_CMD_DATA, 0x4000_0001);
        m.dma_write_in(&[0x01_01_01_01; 32]);
        // Six DC-only blocks make one black macroblock.
        m.write32(MDEC_CMD_DATA, 0x3000_0006);
        let sentinel = MDEC_END_OF_DATA as u32;
        // Need a DC coefficient + sentinel per block x 6 blocks.
        // With all-zero DC + immediate sentinel per block, we produce
        // a single macroblock of all-black output.
        m.dma_write_in(&[sentinel << 16; 6]);
        let mut out = [0u32; 1];
        m.dma_read_out(&mut out);
        assert_eq!(m.macroblocks_decoded(), 1);
    }

    #[test]
    fn mono4_decode_packs_nibbles_and_announces_output_before_dma1() {
        let mut m = Mdec::new();
        m.write32(MDEC_CTRL_STAT, 0x6000_0000);
        m.write32(MDEC_CMD_DATA, 0x4000_0001);
        m.dma_write_in(&[0x0101_0101; 32]);

        m.write32(MDEC_CMD_DATA, 0x2000_0001);
        m.dma_write_in(&[0xfe00_0000]);

        let status = m.read32(MDEC_CTRL_STAT);
        assert_eq!(status & MDEC1_EMPTY, 0);
        assert_eq!(status & MDEC1_DMA_OUT_REQ, MDEC1_DMA_OUT_REQ);
        assert_eq!(status & MDEC1_OUTPUT_DEPTH_MASK, 0);
        let mut output = [0u32; 8];
        m.dma_read_out(&mut output);
        assert_eq!(output, [0x8888_8888; 8]);
    }

    #[test]
    fn mono8_decode_packs_bytes() {
        let mut m = Mdec::new();
        m.write32(MDEC_CMD_DATA, 0x4000_0001);
        m.dma_write_in(&[0x0101_0101; 32]);

        m.write32(MDEC_CMD_DATA, 0x2800_0001);
        m.dma_write_in(&[0xfe00_0000]);

        assert_eq!(m.read32(MDEC_CTRL_STAT) & MDEC1_OUTPUT_DEPTH_MASK, 1 << 25);
        let mut output = [0u32; 16];
        m.dma_read_out(&mut output);
        assert_eq!(output, [0x8080_8080; 16]);
    }

    #[test]
    fn rle_block_decode_skips_leading_padding_sentinels() {
        let rl = [MDEC_END_OF_DATA, MDEC_END_OF_DATA, 0x0010, MDEC_END_OF_DATA];
        let mut block = [0i16; DSIZE2];
        let quant = [1u8; DSIZE2];
        let mut pos = 0;

        assert!(decode_block(&rl, &mut pos, &mut block, &quant));
        assert_eq!(pos, rl.len());
    }

    #[test]
    fn rle_block_decode_returns_false_for_padding_only_stream() {
        let rl = [MDEC_END_OF_DATA, MDEC_END_OF_DATA];
        let mut block = [0i16; DSIZE2];
        let quant = [1u8; DSIZE2];
        let mut pos = 0;

        assert!(!decode_block(&rl, &mut pos, &mut block, &quant));
        assert_eq!(pos, rl.len());
    }

    #[test]
    fn full_block_end_marker_at_slice_boundary_does_not_end_the_frame() {
        let mut m = Mdec::new();
        m.write32(MDEC_CMD_DATA, 0x4000_0001);
        m.dma_write_in(&[0x01_01_01_01; 32]);
        // Macroblock 1: blocks 0..4 are DC-only; its last block (Y4) fills
        // all 64 coefficients and still carries the encoder's FE00.
        // Macroblock 2 follows.
        let mut hw: Vec<u16> = Vec::new();
        for _ in 0..5 {
            hw.extend([0x0400, MDEC_END_OF_DATA]);
        }
        hw.push(0x0400);
        hw.extend(std::iter::repeat_n(0x0001u16, 63));
        hw.push(MDEC_END_OF_DATA);
        for _ in 0..6 {
            hw.extend([0x0400, MDEC_END_OF_DATA]);
        }
        while !hw.len().is_multiple_of(64) {
            hw.push(MDEC_END_OF_DATA);
        }
        let words: Vec<u32> = hw
            .chunks_exact(2)
            .map(|p| p[0] as u32 | (p[1] as u32) << 16)
            .collect();
        m.write32(MDEC_CMD_DATA, 0x3800_0000 | words.len() as u32);
        m.dma_write_in(&words);
        // One 15bpp macroblock is 128 words; read the first as its own slice.
        let mut out = [0u32; 128];
        m.dma_read_out(&mut out);
        assert!(
            !m.complete_dma_out(),
            "frame ended with a macroblock still queued"
        );
        m.dma_read_out(&mut out);
        assert_eq!(m.macroblocks_decoded(), 2);
        assert!(m.complete_dma_out());
    }

    #[test]
    fn rle_block_completes_at_coefficient_63_without_end_marker() {
        let mut encoded = vec![0x0400];
        encoded.extend(std::iter::repeat_n(0u16, 63));
        let mut block = [0i16; DSIZE2];
        let quant = [1u8; DSIZE2];
        let mut pos = 0;

        assert!(decode_block(&encoded, &mut pos, &mut block, &quant));
        assert_eq!(pos, encoded.len());
    }

    #[test]
    fn reset_from_busy_reads_busy_until_it_completes() {
        let mut m = Mdec::new();
        m.write32_at(MDEC_CMD_DATA, 0x4000_0001, 0);
        m.write32_at(MDEC_CMD_DATA, 0, 1);
        m.write32_at(MDEC_CTRL_STAT, 0x8000_0000, 100);
        assert_eq!(m.read32_at(MDEC_CTRL_STAT, 113), 0xa004_0000);
        assert_eq!(
            m.read32_at(MDEC_CTRL_STAT, 100 + RESET_BUSY_CYCLES - 1),
            0xa004_0000
        );
        assert_eq!(
            m.read32_at(MDEC_CTRL_STAT, 100 + RESET_BUSY_CYCLES),
            0x8004_0000
        );
    }

    #[test]
    fn reset_from_idle_reads_idle() {
        let mut m = Mdec::new();
        m.write32_at(MDEC_CTRL_STAT, 0x8000_0000, 100);
        assert_eq!(m.read32_at(MDEC_CTRL_STAT, 101), 0x8004_0000);
        assert_eq!(m.read32_at(MDEC_CTRL_STAT, 200), 0x8004_0000);
    }

    #[test]
    fn control_write_right_behind_a_reset_is_lost_and_later_one_holds() {
        // Enable written 2 cycles behind the reset: lost, reads back, long reset.
        let mut m = Mdec::new();
        m.write32_at(MDEC_CTRL_STAT, 0x8000_0000, 100);
        m.write32_at(MDEC_CTRL_STAT, 0x6000_0000, 102);
        assert_eq!(m.read32_at(MDEC_CTRL_STAT, 115), 0x6000_0000);
        assert_eq!(
            m.read32_at(MDEC_CTRL_STAT, 102 + RESET_BUSY_CYCLES),
            0x8004_0000
        );
        assert!(!m.dma_in_enabled());
        m.write32_at(MDEC_CMD_DATA, 0x6000_0000, 200);
        assert!(!m.dma_in_request());
        assert_eq!(m.read32_at(MDEC_CTRL_STAT, 201), 0xa004_001f);

        // The v1.25 driver (7 cycles behind), even from busy: kept.
        let mut m = Mdec::new();
        m.write32_at(MDEC_CMD_DATA, 0x4000_0001, 0);
        m.write32_at(MDEC_CTRL_STAT, 0x8000_0000, 100);
        m.write32_at(MDEC_CTRL_STAT, 0x6000_0000, 107);
        assert!(m.dma_in_enabled());
        m.write32_at(MDEC_CMD_DATA, 0x4000_0001, 134);
        assert!(m.dma_in_request());
    }

    #[test]
    fn status_counts_parameter_words_left() {
        let mut m = Mdec::new();
        m.write32(MDEC_CTRL_STAT, 0x8000_0000);
        assert_eq!(m.read32(MDEC_CTRL_STAT) & 0xffff, 0);
        m.write32(MDEC_CTRL_STAT, 0x6000_0000);
        m.write32(MDEC_CMD_DATA, 0x4000_0001);
        assert_eq!(m.read32(MDEC_CTRL_STAT), 0xb004_001f);
        m.write32(MDEC_CMD_DATA, 0);
        assert_eq!(m.read32(MDEC_CTRL_STAT) & 0xffff, 0x1e);
        m.dma_write_in(&[0; 31]);
        assert_eq!(m.read32(MDEC_CTRL_STAT), 0x8004_ffff);
    }

    #[test]
    fn cpu_fed_decode_drains_every_macroblock() {
        // Silicon: a frame fed and read back by the CPU comes out whole.
        let mut m = Mdec::new();
        m.write32(MDEC_CMD_DATA, 0x4000_0001);
        m.dma_write_in(&[0x0101_0101; 32]);
        let words = [0xfe00_0010u32; 12]; // two DC-only macroblocks
        m.write32(MDEC_CMD_DATA, 0x3800_0000 | words.len() as u32);
        for word in words {
            m.write32(MDEC_CMD_DATA, word);
        }
        let mut drained = 0;
        while m.read32(MDEC_CTRL_STAT) & MDEC1_EMPTY == 0 {
            m.read32(MDEC_CMD_DATA);
            drained += 1;
        }
        assert_eq!(drained, 2 * 128);
        assert_eq!(m.macroblocks_decoded(), 2);
    }

    #[test]
    fn state_reports_transitions() {
        let mut m = Mdec::new();
        assert_eq!(m.state(), MdecState::Idle);
        m.write32(MDEC_CMD_DATA, 0x3000_0006);
        m.dma_write_in(&[0xfe00_0000; 6]);
        assert_eq!(m.state(), MdecState::DecodeReady);
        let mut out = [0u32; 1];
        m.dma_read_out(&mut out);
        assert_eq!(m.state(), MdecState::DecodeReady);
        let mut rest = [0u32; 191];
        m.dma_read_out(&mut rest);
        m.complete_dma_out();
        assert_eq!(m.state(), MdecState::Idle);
    }
}
