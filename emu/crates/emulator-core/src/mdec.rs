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
//! See `LICENSE` and `docs/license-audit.md`.

// ===============================================================
//  Register addresses + command constants.
// ===============================================================

/// Base address of the MDEC MMIO port.
pub const MDEC_BASE: u32 = 0x1F80_1820;
/// Command / parameter register (writes issue commands or deliver
/// parameter words; reads return queued output pixels).
pub const MDEC_CMD_DATA: u32 = 0x1F80_1820;
/// Status register (read) / control register (write).
pub const MDEC_CTRL_STAT: u32 = 0x1F80_1824;

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
const MDEC1_FIFO: u32 = 0xC000_0000;
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
const MDEC_END_OF_DATA: u16 = 0xFE00;

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
    /// Buffered RLE halfwords received via DMA0 since the decode
    /// command was issued. Drained during decode_macroblocks.
    rl_queue: std::collections::VecDeque<u16>,
    /// Decoded pixel output queue -- ready for DMA1 or data-port reads.
    /// All four output depths are packed into little-endian halfwords.
    out_queue: std::collections::VecDeque<u16>,
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
            rl_queue: std::collections::VecDeque::new(),
            out_queue: std::collections::VecDeque::new(),
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
        match phys & 0x1F80_1FFF {
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
        let value = if phys & 0x1F80_1FFF == MDEC_CTRL_STAT && self.resetting(now) {
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
        if phys & 0x1F80_1FFF == MDEC_CTRL_STAT {
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
        match phys & 0x1F80_1FFF {
            MDEC_CMD_DATA => self.command_write(value),
            MDEC_CTRL_STAT => self.control_write(value),
            _ => {}
        }
    }

    /// Called by the bus for DMA channel 0 transfers -- CPU→MDEC.
    /// Each word becomes two halfwords in little-endian order.
    pub fn dma_write_in(&mut self, words: &[u32]) {
        self.params_seen = self.params_seen.saturating_add(words.len() as u64);
        self.reg1 |= MDEC1_STP;

        match self.command_code() {
            1 => {
                self.reg1 |= MDEC1_BUSY;
                // The words DMA0 delivers are the command's parameters: once
                // they are in, the next MDEC0 write is a new command again.
                self.expected_param_words =
                    self.expected_param_words.saturating_sub(words.len() as u32);
                self.rl_queue.clear();
                self.out_queue.clear();
                for &w in words {
                    self.rl_queue.push_back(w as u16);
                    self.rl_queue.push_back((w >> 16) as u16);
                }
                // Silicon exposes data-out readiness before software starts
                // DMA1. Public ps1-tests polls bit 31 between DMA0 and DMA1.
                self.decode_until_output_words(1);
                if !self.can_continue_decode() && self.out_queue.is_empty() {
                    self.reg1 &= !MDEC1_BUSY;
                }
            }
            2 => {
                self.absorb_quant_upload(words);
                self.expected_param_words = 0;
                self.reg1 &= !MDEC1_BUSY;
            }
            3 => {
                self.absorb_scale_upload(words);
                self.expected_param_words = 0;
                self.reg1 &= !MDEC1_BUSY;
            }
            _ => {}
        }
    }

    /// Called by the bus for DMA channel 1 transfers -- MDEC→CPU.
    /// Fills `out` with decoded pixel words.
    pub fn dma_read_out(&mut self, out: &mut [u32]) {
        for slot in out {
            if self.out_queue.len() < 2 {
                self.decode_until_output_words(2);
            }
            *slot = self.pop_output_word();
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
            && self.rl_queue.iter().all(|&word| word == MDEC_END_OF_DATA)
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
        self.rl_queue.front().copied()
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
        status &= !0xFFFF;
        if !self.fresh_from_reset {
            status |= self.expected_param_words.wrapping_sub(1) & 0xFFFF;
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
                    self.rl_queue.push_back(value as u16);
                    self.rl_queue.push_back((value >> 16) as u16);
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
            1 => value & 0xFFFF,
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
        let lo = self.out_queue.pop_front().unwrap_or(0) as u32;
        let hi = self.out_queue.pop_front().unwrap_or(0) as u32;
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

        // Stream order is Cr, Cb, Y1, Y2, Y3, Y4; the two chroma blocks use
        // the chrominance table and the four luma blocks the luminance one.
        let mut blocks = [[0i16; DSIZE2]; BLOCKS_PER_MACROBLOCK];
        for (index, block) in blocks.iter_mut().enumerate() {
            let quant = if index < 2 {
                &self.quant_uv
            } else {
                &self.quant_y
            };
            if !decode_block(&mut self.rl_queue, block, quant) {
                return false;
            }
            idct(&self.scale_table, block);
        }
        self.emit_macroblock_output(&blocks);
        self.macroblocks_decoded = self.macroblocks_decoded.saturating_add(1);
        true
    }

    fn decode_one_mono_block(&mut self) -> bool {
        let mut block = [0i16; DSIZE2];
        if !decode_block(&mut self.rl_queue, &mut block, &self.quant_y) {
            return false;
        }
        idct(&self.scale_table, &mut block);

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
                for samples in mono.chunks_exact(4) {
                    let nibble = |sample: u8| ((u16::from(sample) + 8) >> 4).min(15);
                    self.out_queue.push_back(
                        nibble(samples[0])
                            | (nibble(samples[1]) << 4)
                            | (nibble(samples[2]) << 8)
                            | (nibble(samples[3]) << 12),
                    );
                }
            }
            1 => {
                for samples in mono.chunks_exact(2) {
                    self.out_queue
                        .push_back(u16::from(samples[0]) | (u16::from(samples[1]) << 8));
                }
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
        let pixels = (0..16).flat_map(|y| (0..16).map(move |x| (x, y)));
        if self.output_depth() == 2 {
            let mut bytes = [0u8; 16 * 16 * 3];
            for ((x, y), out) in pixels.zip(bytes.chunks_exact_mut(3)) {
                let rgb = macroblock_pixel(blocks, x, y);
                for (dst, component) in out.iter_mut().zip(rgb) {
                    *dst = output_byte(component, signed);
                }
            }
            for pair in bytes.chunks_exact(2) {
                self.out_queue
                    .push_back(u16::from(pair[0]) | (u16::from(pair[1]) << 8));
            }
        } else {
            let bit15 = if self.reg0 & MDEC0_STP != 0 {
                0x8000
            } else {
                0
            };
            for (x, y) in pixels {
                let [r, g, b] = macroblock_pixel(blocks, x, y)
                    .map(|component| output_field15(component, signed));
                self.out_queue.push_back(r | (g << 5) | (b << 10) | bit15);
            }
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

/// Read one block from the RLE stream into the fixed-point coefficients the
/// IDCT matrix consumes: the DC term and each AC term are dequantised with
/// the uploaded table (and the block's quantisation scale for AC terms),
/// rounded and saturated to 15 bits, and placed in matrix order. Returns
/// `false` when the stream ends before the block does. The intermediate
/// rounding is observable on silicon (ps1-tests' mono and frame programs),
/// so each step is kept exactly.
fn decode_block(
    rl: &mut std::collections::VecDeque<u16>,
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
    let q_scale = (head >> 10) & 0x3F;
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
    block[ZIG_ZAG_MATRIX[0]] = coefficient.clamp(-0x4000, 0x3FFF) as i16;

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
            block[ZIG_ZAG_MATRIX[index]] = coefficient.clamp(-0x4000, 0x3FFF) as i16;
        }
    }
    true
}

/// Two-pass 8x8 inverse DCT as the hardware performs it: each pass multiplies
/// by the uploaded matrix, adds half an LSB and drops 15 fractional bits; the
/// second pass saturates its result to signed 9 bits and then to -128..=127.
/// The first pass stores its result transposed, which is observable at one
/// LSB when the two roundings differ.
fn idct(scale_table: &[i16; DSIZE2], block: &mut [i16; DSIZE2]) {
    let mut temp = [0i16; DSIZE2];
    for column in 0..DSIZE {
        for x in 0..DSIZE {
            let mut sum = 0i64;
            for u in 0..DSIZE {
                sum += i64::from(block[column * DSIZE + u]) * i64::from(scale_table[x * DSIZE + u]);
            }
            temp[x * DSIZE + column] = ((sum + 0x4000) >> 15) as i16;
        }
    }
    for column in 0..DSIZE {
        for x in 0..DSIZE {
            let mut sum = 0i64;
            for u in 0..DSIZE {
                sum += i64::from(temp[column * DSIZE + u]) * i64::from(scale_table[x * DSIZE + u]);
            }
            let rounded = ((sum + 0x4000) >> 15) as i32;
            block[column * DSIZE + x] = rounded.clamp(-128, 127) as i16;
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
/// The output range of one component, in the same fixed point.
const CSC_MIN: i32 = -128 << CSC_SHIFT;
const CSC_MAX: i32 = 127 << CSC_SHIFT;

/// Convert one pixel's luma and chroma (each in -128..=127) to red, green
/// and blue in 16.16 fixed point, saturated to -128.0..=127.0.
#[inline]
fn yuv_to_rgb(y: i32, cb: i32, cr: i32) -> [i32; 3] {
    let y = y << CSC_SHIFT;
    [
        y + CSC_CR_TO_R * cr,
        y - CSC_CB_TO_G * cb - CSC_CR_TO_G * cr,
        y + CSC_CB_TO_B * cb,
    ]
    .map(|component| component.clamp(CSC_MIN, CSC_MAX))
}

/// The 24-bit output byte for a component: rounded to the nearest integer,
/// then offset into 0..=255 unless the command asked for signed output, in
/// which case the two's-complement byte is delivered as is.
#[inline]
fn output_byte(component: i32, signed: bool) -> u8 {
    let rounded = (component + (1 << (CSC_SHIFT - 1))) >> CSC_SHIFT;
    (if signed { rounded } else { rounded + 128 }) as u8
}

/// The 5-bit field of a 15-bit output pixel for a component: the unsigned
/// byte value is rounded to the nearest multiple of eight (ties upward,
/// working from the unrounded component), saturating at 31. Signed output
/// reduces the two's-complement byte the same way.
#[inline]
fn output_field15(component: i32, signed: bool) -> u16 {
    if signed {
        (u16::from(output_byte(component, true)) + 4) >> 3
    } else {
        // (value + 128 + 4) / 8, floored, from 16.16 fixed point.
        let biased = i64::from(component) + (132i64 << CSC_SHIFT);
        ((biased >> (CSC_SHIFT + 3)) as u16).min(31)
    }
}

/// Pixel `(x, y)` of a 16x16 macroblock from its six decoded blocks (in
/// stream order: Cr, Cb, Y1..Y4). Chroma is shared by each 2x2 pixel group;
/// luma comes from the quadrant block the pixel falls in.
#[inline]
fn macroblock_pixel(
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

// ===============================================================
//  Tests.
// ===============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_covers_both_registers() {
        assert!(Mdec::contains(0x1F80_1820));
        assert!(Mdec::contains(0x1F80_1823));
        assert!(Mdec::contains(0x1F80_1824));
        assert!(Mdec::contains(0x1F80_1827));
        assert!(!Mdec::contains(0x1F80_181C));
        assert!(!Mdec::contains(0x1F80_1828));
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
        m.dma_write_in(&[0xFE00_0000; 6]);

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
        m.dma_write_in(&[0xDEAD_BEEF]);
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
        idct(&scale, &mut block);
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
        let word2 = (5u16 << 10) | 0x001F;
        assert_eq!(rle_run(word2), 5);
        assert_eq!(rle_val(word2), 31);
        // Negative value: low 10 bits = 0x3FF = -1 signed.
        let word3 = 0x03FF;
        assert_eq!(rle_val(word3), -1);
        // Unused variable so compiler is happy.
        let _ = rle_run(word);
    }

    fn bytes(y: i32, cb: i32, cr: i32) -> [u8; 3] {
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
        let rgb = yuv_to_rgb(-1, 0, 0);
        assert_eq!(output_byte(rgb[0], true), 0xFF);
        assert_eq!(output_byte(yuv_to_rgb(5, 0, 0)[0], true), 5);
    }

    #[test]
    fn fifteen_bit_fields_round_to_the_nearest_multiple_of_eight() {
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
        m.dma_write_in(&[0xFE00_0000]);

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
        m.dma_write_in(&[0xFE00_0000]);

        assert_eq!(m.read32(MDEC_CTRL_STAT) & MDEC1_OUTPUT_DEPTH_MASK, 1 << 25);
        let mut output = [0u32; 16];
        m.dma_read_out(&mut output);
        assert_eq!(output, [0x8080_8080; 16]);
    }

    #[test]
    fn rle_block_decode_skips_leading_padding_sentinels() {
        let mut rl = std::collections::VecDeque::from([
            MDEC_END_OF_DATA,
            MDEC_END_OF_DATA,
            0x0010,
            MDEC_END_OF_DATA,
        ]);
        let mut block = [0i16; DSIZE2];
        let quant = [1u8; DSIZE2];

        assert!(decode_block(&mut rl, &mut block, &quant));
        assert!(rl.is_empty());
    }

    #[test]
    fn rle_block_decode_returns_false_for_padding_only_stream() {
        let mut rl = std::collections::VecDeque::from([MDEC_END_OF_DATA, MDEC_END_OF_DATA]);
        let mut block = [0i16; DSIZE2];
        let quant = [1u8; DSIZE2];

        assert!(!decode_block(&mut rl, &mut block, &quant));
        assert!(rl.is_empty());
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
        let mut rl = std::collections::VecDeque::from(encoded);
        let mut block = [0i16; DSIZE2];
        let quant = [1u8; DSIZE2];

        assert!(decode_block(&mut rl, &mut block, &quant));
        assert!(rl.is_empty());
    }

    #[test]
    fn reset_from_busy_reads_busy_until_it_completes() {
        let mut m = Mdec::new();
        m.write32_at(MDEC_CMD_DATA, 0x4000_0001, 0);
        m.write32_at(MDEC_CMD_DATA, 0, 1);
        m.write32_at(MDEC_CTRL_STAT, 0x8000_0000, 100);
        assert_eq!(m.read32_at(MDEC_CTRL_STAT, 113), 0xA004_0000);
        assert_eq!(
            m.read32_at(MDEC_CTRL_STAT, 100 + RESET_BUSY_CYCLES - 1),
            0xA004_0000
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
        assert_eq!(m.read32_at(MDEC_CTRL_STAT, 201), 0xA004_001F);

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
        assert_eq!(m.read32(MDEC_CTRL_STAT) & 0xFFFF, 0);
        m.write32(MDEC_CTRL_STAT, 0x6000_0000);
        m.write32(MDEC_CMD_DATA, 0x4000_0001);
        assert_eq!(m.read32(MDEC_CTRL_STAT), 0xB004_001F);
        m.write32(MDEC_CMD_DATA, 0);
        assert_eq!(m.read32(MDEC_CTRL_STAT) & 0xFFFF, 0x1E);
        m.dma_write_in(&[0; 31]);
        assert_eq!(m.read32(MDEC_CTRL_STAT), 0x8004_FFFF);
    }

    #[test]
    fn cpu_fed_decode_drains_every_macroblock() {
        // Silicon: a frame fed and read back by the CPU comes out whole.
        let mut m = Mdec::new();
        m.write32(MDEC_CMD_DATA, 0x4000_0001);
        m.dma_write_in(&[0x0101_0101; 32]);
        let words = [0xFE00_0010u32; 12]; // two DC-only macroblocks
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
        m.dma_write_in(&[0xFE00_0000; 6]);
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
