//! MIPS R3000A CPU.
//!
//! Instruction set coverage grows one opcode at a time, each added
//! alongside a parity assertion against PCSX-Redux. The decoder itself
//! is intentionally a flat match on the primary opcode field -- we'll
//! refactor to a jump table only if a profiler says to.
//!
//! ## Provenance
//!
//! Portions of this module are parity-matched against, and in places
//! derived from, PCSX-Redux (<https://github.com/grumpycoders/pcsx-redux>),
//! Copyright (C) the PCSX-Redux authors, GPL-2.0-or-later. The MIPS
//! R3000A instruction set is standard; the inline `Redux` references mark
//! where edge-case and timing behaviour is matched to Redux. PSoXide is
//! released under GPL-2.0-or-later in part to honor this lineage; see
//! `LICENSE` and `docs/license-audit.md`.

use psx_hw::memory;
use psx_trace::InstructionRecord;
use thiserror::Error;

use crate::bus::{AccessWidth, Bus};
use crate::freelook::{self, FreelookState};
use psx_gte_core::Gte;

mod branch;
mod icache;
mod timing;

use branch::branch_target;
use icache::InstructionCache;
use timing::cycle_cost;

const CACHE_CONTROL_TAG: u32 = 1 << 2;
const CACHE_CONTROL_IBLKSZ_SHIFT: u32 = 8;
const CACHE_CONTROL_IBLKSZ_MASK: u32 = 3 << CACHE_CONTROL_IBLKSZ_SHIFT;
const CACHE_CONTROL_IS1: u32 = 1 << 11;
/// "No streaming": the core waits for a whole line fill before executing any
/// of it. Clear in the BIOS value. hwtest v1.22 record `0xE9` sets it and a
/// cold sweep goes from 9 clocks a line to 13.
const CACHE_CONTROL_NOSTR: u32 = 1 << 17;
/// COP0 Status.CU2: permit GTE register transfers and commands.
const COP0_STATUS_CU2: u32 = 1 << 30;
/// Retail BIOS state after `FlushCache` returns: scratchpad and I-cache
/// enabled, four-word I-cache refills, and the normal bus-control bits.
const CACHE_CONTROL_BIOS_NORMAL: u32 = 0x0001_E988;

/// Cycles after `MTC2` to LZCS (data reg 30) before the recomputed LZCR
/// (data reg 31) is readable. A read inside this window returns the prior
/// count -- the off-by-one observed on real hardware for a back-to-back
/// `mtc2 lzcs; mfc2 lzcr`.
/// Superseded measurement: a 2026-07-15 SCPH-9902 capture read this as one
/// intervening instruction being sufficient. The 2026-08-07 v1.17 capture on
/// the suite's reference console measures the window one instruction wider:
/// conformance 0x79 (one nop between the write and the read) returns the
/// PRIOR count 8, while 0x7A-0x7D (two or more nops) return the fresh 31.
const LZCR_RESULT_LATENCY: u64 = 3;
/// Short result window for commands that write MAC0.
const MAC0_RESULT_LATENCY: u64 = 2;

/// Errors raised during instruction execution.
#[derive(Error, Debug, PartialEq, Eq)]
pub enum ExecutionError {
    /// Decoder encountered a primary opcode we haven't implemented yet.
    #[error("unimplemented primary opcode {opcode:#04x} at pc={pc:#010x} (instr={instr:#010x})")]
    Unimplemented {
        /// Primary opcode field (bits 31..=26).
        opcode: u8,
        /// PC at which the offending instruction was fetched.
        pc: u32,
        /// Raw 32-bit instruction word.
        instr: u32,
    },

    /// Decoder encountered a SPECIAL funct value we haven't implemented yet.
    #[error("unimplemented SPECIAL funct {funct:#04x} at pc={pc:#010x} (instr={instr:#010x})")]
    UnimplementedSpecial {
        /// Function field (bits 5..=0) for primary opcode 0.
        funct: u8,
        /// PC at which the offending instruction was fetched.
        pc: u32,
        /// Raw 32-bit instruction word.
        instr: u32,
    },

    /// Strict HLE mode (see [`Bus::set_hle_strict`]) reached a BIOS
    /// function the HLE does not implement. The CPU stops at the dispatch
    /// vector before the call has any effect.
    #[error("unimplemented HLE BIOS call {table}({func:02X}h) {name} (ra={ra:#010x})")]
    HleUnimplemented {
        /// Dispatch table letter, `A`, `B` or `C`.
        table: char,
        /// Function number from `$t1`.
        func: u8,
        /// Conventional function name, or `"?"`.
        name: &'static str,
        /// Caller's return address.
        ra: u32,
    },
}

/// Lightweight return value of `Cpu::execute_one`. Carries just
/// the per-instruction trace payload that differs between the
/// HLE-BIOS shortcut path (`pc` is the BIOS-call entry, `instr` is 0)
/// and the normal interpreter path (`pc` is the pre-execution PC,
/// `instr` is the fetched word). Lets `step_traced` build a full
/// `InstructionRecord` without `step` paying for the 256-byte COP2
/// snapshot or the 128-byte GPRs copy.
struct ExecutedInstruction {
    record_pc: u32,
    record_instr: u32,
}

/// Emulator-owned instruction-cache counters.
///
/// These counters observe timing already charged to the emulated CPU. They do
/// not participate in guest state and are reset when a save state is loaded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InstructionCacheProfileSnapshot {
    /// Instruction fetches that filled one or more cache words.
    pub refill_events: u64,
    /// Cache words fetched across all refill events.
    pub refill_words: u64,
    /// CPU stall cycles already charged for those refills.
    pub refill_stall_cycles: u64,
}

/// Why an instruction-cache fetch had to refill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstructionCacheMissKind {
    /// The selected direct-mapped set contained a different physical tag.
    Tag,
    /// The tag matched, but the requested word's valid bit was clear.
    InvalidWord,
}

/// One exact instruction-cache refill observed by the emulator.
///
/// This is diagnostic metadata only. It is excluded from save states and does
/// not affect guest-visible state or timing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InstructionCacheRefillEvent {
    /// Virtual PC whose fetch triggered the refill.
    pub fetch_pc: u32,
    /// Direct-mapped cache set, in the range 0..=255.
    pub cache_set: u8,
    /// Incoming physical 16-byte cache-line address.
    pub incoming_line: u32,
    /// Incoming physical tag bits.
    pub incoming_tag: u32,
    /// Previous physical line reconstructed from the victim tag and set.
    pub victim_line: u32,
    /// Previous physical tag bits.
    pub victim_tag: u32,
    /// Victim per-word valid mask before replacement/refill.
    pub victim_valid_mask: u8,
    /// Whether this was a tag replacement or an invalid-word refill.
    pub miss_kind: InstructionCacheMissKind,
    /// Number of words fetched from the instruction bus.
    pub fill_words: u8,
    /// CPU stall cycles already charged for the refill.
    pub stall_cycles: u32,
}

/// Emulator-owned breakdown of CPU cycles charged while retiring guest code.
///
/// The buckets are observational only: enabling them does not alter guest
/// state or the emulated cycle counter. `stack_ram_load_stall_cycles` is a
/// subset of `ram_load_stall_cycles`, so callers must not add it to
/// [`Self::total_profiled_cycles`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuCycleProfileSnapshot {
    /// One-cycle issue cost charged for ordinary retired instructions.
    pub issue_cycles: u64,
    /// Main-RAM load stalls, including refresh waits and unaligned merge work.
    pub ram_load_stall_cycles: u64,
    /// Main-RAM load stalls whose addressing base register was `$sp`.
    pub stack_ram_load_stall_cycles: u64,
    /// Main-RAM store stalls, including read/modify/write store forms.
    pub ram_store_stall_cycles: u64,
    /// Stalls charged by MMIO or expansion-bus data accesses.
    pub mmio_stall_cycles: u64,
    /// I-cache refill stalls already charged by the instruction fetch path.
    pub icache_refill_stall_cycles: u64,
    /// Stalls from uncached instruction fetches.
    pub uncached_fetch_stall_cycles: u64,
    /// Interlock stalls before issuing a GTE command while the GTE is busy.
    pub gte_busy_stall_cycles: u64,
    /// Interlock stalls on `MFHI`/`MFLO` while multiply/divide is busy.
    pub muldiv_interlock_stall_cycles: u64,
    /// CPU-charged cycles not covered by the named buckets above.
    pub other_stall_cycles: u64,
}

impl CpuCycleProfileSnapshot {
    /// Sum of the disjoint cycle buckets in this snapshot.
    pub fn total_profiled_cycles(self) -> u64 {
        self.issue_cycles
            .saturating_add(self.ram_load_stall_cycles)
            .saturating_add(self.ram_store_stall_cycles)
            .saturating_add(self.mmio_stall_cycles)
            .saturating_add(self.icache_refill_stall_cycles)
            .saturating_add(self.uncached_fetch_stall_cycles)
            .saturating_add(self.gte_busy_stall_cycles)
            .saturating_add(self.muldiv_interlock_stall_cycles)
            .saturating_add(self.other_stall_cycles)
    }

    /// Saturating per-field difference from an earlier snapshot.
    pub fn delta_since(self, earlier: Self) -> Self {
        Self {
            issue_cycles: self.issue_cycles.saturating_sub(earlier.issue_cycles),
            ram_load_stall_cycles: self
                .ram_load_stall_cycles
                .saturating_sub(earlier.ram_load_stall_cycles),
            stack_ram_load_stall_cycles: self
                .stack_ram_load_stall_cycles
                .saturating_sub(earlier.stack_ram_load_stall_cycles),
            ram_store_stall_cycles: self
                .ram_store_stall_cycles
                .saturating_sub(earlier.ram_store_stall_cycles),
            mmio_stall_cycles: self
                .mmio_stall_cycles
                .saturating_sub(earlier.mmio_stall_cycles),
            icache_refill_stall_cycles: self
                .icache_refill_stall_cycles
                .saturating_sub(earlier.icache_refill_stall_cycles),
            uncached_fetch_stall_cycles: self
                .uncached_fetch_stall_cycles
                .saturating_sub(earlier.uncached_fetch_stall_cycles),
            gte_busy_stall_cycles: self
                .gte_busy_stall_cycles
                .saturating_sub(earlier.gte_busy_stall_cycles),
            muldiv_interlock_stall_cycles: self
                .muldiv_interlock_stall_cycles
                .saturating_sub(earlier.muldiv_interlock_stall_cycles),
            other_stall_cycles: self
                .other_stall_cycles
                .saturating_sub(earlier.other_stall_cycles),
        }
    }

    /// Saturating per-field sum with another snapshot.
    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            issue_cycles: self.issue_cycles.saturating_add(other.issue_cycles),
            ram_load_stall_cycles: self
                .ram_load_stall_cycles
                .saturating_add(other.ram_load_stall_cycles),
            stack_ram_load_stall_cycles: self
                .stack_ram_load_stall_cycles
                .saturating_add(other.stack_ram_load_stall_cycles),
            ram_store_stall_cycles: self
                .ram_store_stall_cycles
                .saturating_add(other.ram_store_stall_cycles),
            mmio_stall_cycles: self
                .mmio_stall_cycles
                .saturating_add(other.mmio_stall_cycles),
            icache_refill_stall_cycles: self
                .icache_refill_stall_cycles
                .saturating_add(other.icache_refill_stall_cycles),
            uncached_fetch_stall_cycles: self
                .uncached_fetch_stall_cycles
                .saturating_add(other.uncached_fetch_stall_cycles),
            gte_busy_stall_cycles: self
                .gte_busy_stall_cycles
                .saturating_add(other.gte_busy_stall_cycles),
            muldiv_interlock_stall_cycles: self
                .muldiv_interlock_stall_cycles
                .saturating_add(other.muldiv_interlock_stall_cycles),
            other_stall_cycles: self
                .other_stall_cycles
                .saturating_add(other.other_stall_cycles),
        }
    }
}

/// Where the CPU waited rather than worked, plus its main-RAM data traffic.
///
/// Collected alongside [`CpuCycleProfileSnapshot`] (same enable flag) and
/// just as observational: it never changes guest state or timing.
///
/// A wait loop is a short backward branch (at most
/// [`WAIT_LOOP_MAX_SPAN`] bytes) taken again to the same target without any
/// store in between, other than stores through `$sp`: the shape of
/// `while (!flag) {}` polling, including Sony's libetc `VSync`, which counts
/// a volatile timeout local down on the stack every iteration. The first
/// iteration arms the detector, every later one is charged here, split by
/// what the loop reads. It is a heuristic: a loop that does real work
/// without storing outside its stack frame (a checksum over memory, say) is
/// counted as waiting too.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuWaitProfileSnapshot {
    /// Wait loops that read an I/O register (GPUSTAT, DMA, CD, timers, IRQ).
    pub hardware_poll: CpuCycleProfileSnapshot,
    /// Wait loops that only read memory (vblank counters, event flags).
    pub memory_poll: CpuCycleProfileSnapshot,
    /// Cycles an HLE kernel call spent retrying a wait (WaitEvent, DrawSync).
    pub hle_wait_cycles: u64,
    /// Main-RAM data loads retired while profiling.
    pub ram_loads: u64,
    /// Main-RAM data stores retired while profiling.
    pub ram_stores: u64,
}

impl CpuWaitProfileSnapshot {
    /// Every cycle charged to waiting: both poll kinds plus HLE waits.
    pub fn total_wait_cycles(self) -> u64 {
        self.hardware_poll
            .total_profiled_cycles()
            .saturating_add(self.memory_poll.total_profiled_cycles())
            .saturating_add(self.hle_wait_cycles)
    }
}

/// Longest loop body, in bytes from the branch target to the delay slot,
/// the wait-loop detector treats as polling.
pub const WAIT_LOOP_MAX_SPAN: u32 = 128;

/// Per-iteration state of the wait-loop detector.
#[derive(Clone, Copy, Debug, Default)]
struct WaitLoopTracker {
    armed: bool,
    target: u32,
    stored: bool,
    polled_io: bool,
    start: CpuCycleProfileSnapshot,
}

/// Emulator-owned exact dynamic instruction-class counts.
///
/// All fields are observational and excluded from save states. Memory access
/// widths and regions are classified from the pre-execution register state,
/// matching the effective address used by the instruction itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InstructionClassProfileSnapshot {
    /// Retired instructions observed while profiling was enabled.
    pub instructions: u64,
    /// Zero-word NOP instructions.
    pub nops: u64,
    /// Instructions executed in a branch or jump delay slot.
    pub delay_slot_instructions: u64,
    /// Zero-word NOPs executed in a delay slot.
    pub delay_slot_nops: u64,
    /// Loads executed in a delay slot.
    pub delay_slot_loads: u64,
    /// Stores executed in a delay slot.
    pub delay_slot_stores: u64,
    /// Control-flow instructions executed in a delay slot.
    pub delay_slot_control_flow: u64,
    /// Byte-width loads (`LB`/`LBU`).
    pub byte_loads: u64,
    /// Halfword-width loads (`LH`/`LHU`).
    pub halfword_loads: u64,
    /// Aligned word loads, including `LWC2`.
    pub word_loads: u64,
    /// Unaligned merge loads (`LWL`/`LWR`).
    pub unaligned_loads: u64,
    /// Byte-width stores (`SB`).
    pub byte_stores: u64,
    /// Halfword-width stores (`SH`).
    pub halfword_stores: u64,
    /// Aligned word stores, including `SWC2`.
    pub word_stores: u64,
    /// Unaligned merge stores (`SWL`/`SWR`).
    pub unaligned_stores: u64,
    /// Data accesses targeting mirrored main RAM.
    pub ram_accesses: u64,
    /// Data accesses targeting the mapped scratchpad alias.
    pub scratchpad_accesses: u64,
    /// Data accesses targeting peripheral/expansion MMIO.
    pub mmio_accesses: u64,
    /// Data accesses outside the preceding regions.
    pub other_accesses: u64,
    /// Loads whose effective-address base register is `$sp`.
    pub sp_relative_loads: u64,
    /// Stores whose effective-address base register is `$sp`.
    pub sp_relative_stores: u64,
    /// `LUI` instructions.
    pub lui: u64,
    /// Non-linking direct `J` instructions.
    pub direct_jumps: u64,
    /// Direct linking `JAL` instructions.
    pub jal: u64,
    /// Register-indirect linking `JALR` instructions.
    pub jalr: u64,
    /// Conditional branch instructions, including REGIMM forms.
    pub conditional_branches: u64,
    /// Signed and unsigned multiply instructions.
    pub multiply: u64,
    /// Signed and unsigned divide instructions.
    pub divide: u64,
    /// COP2 register transfer instructions.
    pub gte_register_transfers: u64,
    /// COP2 GTE command instructions.
    pub gte_commands: u64,
}

impl InstructionClassProfileSnapshot {
    /// Saturating per-field difference from an earlier snapshot.
    pub fn delta_since(self, earlier: Self) -> Self {
        macro_rules! delta {
            ($field:ident) => {
                self.$field.saturating_sub(earlier.$field)
            };
        }
        Self {
            instructions: delta!(instructions),
            nops: delta!(nops),
            delay_slot_instructions: delta!(delay_slot_instructions),
            delay_slot_nops: delta!(delay_slot_nops),
            delay_slot_loads: delta!(delay_slot_loads),
            delay_slot_stores: delta!(delay_slot_stores),
            delay_slot_control_flow: delta!(delay_slot_control_flow),
            byte_loads: delta!(byte_loads),
            halfword_loads: delta!(halfword_loads),
            word_loads: delta!(word_loads),
            unaligned_loads: delta!(unaligned_loads),
            byte_stores: delta!(byte_stores),
            halfword_stores: delta!(halfword_stores),
            word_stores: delta!(word_stores),
            unaligned_stores: delta!(unaligned_stores),
            ram_accesses: delta!(ram_accesses),
            scratchpad_accesses: delta!(scratchpad_accesses),
            mmio_accesses: delta!(mmio_accesses),
            other_accesses: delta!(other_accesses),
            sp_relative_loads: delta!(sp_relative_loads),
            sp_relative_stores: delta!(sp_relative_stores),
            lui: delta!(lui),
            direct_jumps: delta!(direct_jumps),
            jal: delta!(jal),
            jalr: delta!(jalr),
            conditional_branches: delta!(conditional_branches),
            multiply: delta!(multiply),
            divide: delta!(divide),
            gte_register_transfers: delta!(gte_register_transfers),
            gte_commands: delta!(gte_commands),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProfiledDataAccess {
    RamLoad { stack_relative: bool },
    RamStore,
    Mmio,
    Other,
}

/// How far behind a RAM load the core is, and which register it fills.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
struct LoadShadow {
    position: u8,
    register: u8,
}

/// MIPS R3000A CPU state.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Cpu {
    pc: u32,
    gprs: [u32; 32],
    /// COP0 (System Control) registers: SR, Cause, EPC, BadVaddr, etc.
    /// Most of these are untouched by early BIOS init; the important
    /// one early on is `SR` (index 12) which gates interrupts and
    /// cache isolation.
    cop0: [u32; 32],
    /// CPU-internal cache-control register at `0xFFFE_0130` (BCC).
    /// It lives here rather than on [`Bus`] because isolated cache
    /// operations and instruction fetch both terminate inside the CPU.
    cache_control: u32,
    /// 4 KiB direct-mapped instruction cache, including tag RAM,
    /// per-word valid bits, and code RAM. This is architectural state:
    /// stale cached code must survive save/load just as it does execution.
    instruction_cache: InstructionCache,
    /// Monotonically increasing step counter. We use "instructions
    /// retired" as our tick metric; cycle-accurate timing lands when
    /// the scheduler does.
    tick: u64,
    /// Branch-delay slot machinery. A branch instruction sets this to
    /// `Some(target)`; the *next* instruction executes as the delay
    /// slot, and *after* it retires PC jumps to `target` instead of
    /// the usual `pc + 4`.
    pending_pc: Option<u32>,
    /// One-slot load-delay machinery. `LW` (and friends) stage their
    /// result here instead of committing immediately; the commit
    /// happens at the end of the *next* instruction's `step`, so the
    /// delay-slot instruction observes the old register value -- which
    /// is what the R3000A hardware does.
    pending_load: Option<(u8, u32)>,
    /// Load staged at the start of `step` and held during `execute`.
    /// Separate from `pending_load` so `set_gpr` can look at it and
    /// squash a same-register writeback when a non-load in the delay
    /// slot writes the same target -- R3000 load-delay semantics.
    committing_load: Option<(u8, u32)>,
    /// Bus cycle at which the in-flight GTE command's result becomes
    /// available. Reading a GTE result register (MFC2/CFC2/SWC2) or issuing
    /// another GTE command before this point stalls the CPU, matching the
    /// hardware load-on-result behaviour; interleaving independent work
    /// between an op and its result read hides the latency. The emulator
    /// computes GTE results eagerly, so this models timing only.
    gte_busy_until: u64,
    /// A RAM load is still on the bus and the instructions behind it may be
    /// running under it. See [`Cpu::hides_in_load_shadow`].
    #[serde(default)]
    load_shadow: Option<LoadShadow>,
    /// Hardware GTE result-read latency. Unlike MAC1-3/SXY/SZ, MAC0 (data
    /// reg 24) and LZCR (data reg 31) are not always readable the instant
    /// their producing operation issues. The eager GTE core computes the
    /// result immediately, so these fields preserve what silicon exposes
    /// during the short settling window.
    gte_mac0_ready_at: u64,
    gte_mac0_stale: u32,
    gte_lzcr_ready_at: u64,
    gte_lzcr_stale: u32,
    /// Tick of the most recent MTC2/CTC2.
    gte_last_write_tick: u64,
    /// NCLIP samples SXY2 in two different pipeline phases. When MTC2 SXY2
    /// immediately precedes NCLIP, the early `+SX1*SY2 + SX2*SY0` products
    /// can see the previous SXY2 while the later negative products see the
    /// current register. An active RTPT result pipeline lets packed SXY writes
    /// forward the new Y into that early phase; without it (or with spaced
    /// writes), the previous Y remains visible. This history/cadence rule
    /// reproduces the burned-EXE instruction shapes measured on SCPH-9902.
    gte_sxy2_write_tick: u64,
    gte_prev_sxy2_x: i16,
    gte_prev_sxy2_y: i16,
    /// Provenance of the live and just-overwritten SXY2 latches. RTPS/RTPT
    /// output can forward through an immediate CPU SXY2 overwrite into
    /// NCLIP's early product; a prior CPU-written SXY2 cannot.
    #[serde(default)]
    gte_sxy2_from_transform: bool,
    #[serde(default)]
    gte_prev_sxy2_from_transform: bool,
    gte_sxy_write_tick: [u64; 3],
    /// RTPT leaves the SXY result pipeline active across following NCLIPs.
    /// In that state, a non-packed full SXY rewrite cannot forward SXY2.y to
    /// NCLIP's first product. Consecutive NCLIPs retain the state; any other
    /// GTE command drains/replaces it.
    gte_nclip_rtpt_history: bool,
    /// CPU tick of the NCLIP/RTPT operation that most recently fed that
    /// result pipeline. The latch survives tight command sequences but not
    /// unrelated game/test work thousands of instructions later.
    gte_nclip_history_tick: u64,
    /// OP's first MAC row can consume the previous IR3 when the command
    /// immediately follows MTC2 IR3. The burned test's predecessor left
    /// IR3=0x0567, producing MAC1=-1074 exactly; the later rows see the new
    /// IR3=0x0600.
    gte_ir3_write_tick: u64,
    gte_prev_ir3: u32,
    gte_last_mtc2_ir3: u32,
    /// Most recent CPU write to MAC0..MAC3. An immediately following GTE
    /// command can reach the same result write port while MTC2 still owns it;
    /// the CPU write wins that one component (observed as OP MAC3 remaining
    /// zero while MAC1/MAC2 complete normally).
    gte_mac_write_tick: u64,
    gte_mac_write_reg: u8,
    gte_mac_write_value: u32,
    /// Diagnostic: bypass the MAC0/LZCR stale-read model entirely.
    /// Seeded from an env var at [`Cpu::new`] -- excluded from save
    /// states (loading doesn't re-run `Cpu::new`, so this and its two
    /// siblings below always come back to their type default, i.e.
    /// hazard modelling off, regardless of the process's env at save
    /// time; re-set the env var and start a fresh emulator instance
    /// to resume a hazard-repro session).
    #[serde(skip)]
    gte_read_latency_bypass: bool,
    /// HWB-010 hazard model: a V0 MVMVA or RTPS issued within
    /// `gte_v0x_window` ticks of an MTC2 VXY0 write computes its first
    /// transform row with the PREVIOUS V0.x (the GTE's sequential row
    /// pipeline runs MAC1 first;
    /// the write commits between the MAC1 and MAC2 windows). Measured on
    /// silicon with six exact arithmetic confirmations across two live
    /// hardware captures, HWB-009 and HWB-010. On hardware
    /// the slip is INTERMITTENT (gated by external bus traffic the
    /// emulator does not model), so a deterministic always-fire is
    /// strictly worse than silicon for well-spaced code: env-gated OFF
    /// by default. `PSOXIDE_GTE_V0X_STALE=1` enables (deterministic
    /// fire inside the window -- the reproduction mode);
    /// `PSOXIDE_GTE_V0X_WINDOW=N` tunes the window (default 2 = the
    /// engine's mtc2-xy; mtc2-z; cop2 hot-path spacing).
    #[serde(skip)]
    gte_v0x_hazard: bool,
    #[serde(skip)]
    gte_v0x_window: u64,
    /// Tick of the most recent MTC2 to VXY0 (data reg 0) and the V0.x
    /// value it overwrote (what a hazarded MAC1 phase reads).
    gte_v0xy_write_tick: u64,
    gte_prev_v0x: i16,
    /// Bus cycle at which the multiply/divide unit's HI/LO result becomes
    /// available. MFHI/MFLO stall to this point (the R3000A HI/LO interlock),
    /// and interleaving work between a MULT/DIV and its result read hides the
    /// latency. Results are computed eagerly, so this models timing only.
    hilo_busy_until: u64,
    hi: u32,
    lo: u32,
    /// When a SYSCALL/BREAK/exception fires, the post-retire PC goes
    /// here instead of `pc + 4` or a pending branch target. The value
    /// is the exception vector (0x8000_0080 or 0xBFC0_0180 depending on
    /// the BEV bit in SR).
    pending_exception_pc: Option<u32>,
    /// A side-loaded EXE entered its guest-installed unresolved-exception
    /// callback through the synthetic HLE kernel frame.
    hle_exception_active: bool,
    /// Per-ExcCode (0..=31) count of exception entries. Diagnostic only.
    /// Excluded from save states.
    #[serde(skip)]
    exception_counts: [u64; 32],
    /// Count of `step()` calls where `bus.external_interrupt_pending()`
    /// was true -- answers "did the IRQ line ever go high from the
    /// CPU's point of view?". Diagnostic. Excluded from save states.
    #[serde(skip)]
    irq_line_high_steps: u64,
    /// Count of `step()` calls where `should_take_interrupt()` was
    /// true -- answers "did we reach the threshold that enters an IRQ
    /// exception?". Excluded from save states.
    #[serde(skip)]
    should_take_interrupt_steps: u64,
    /// Address of a GTE command about to execute, recorded at the start of
    /// the instruction before it when no interrupt was pending then. An
    /// interrupt found pending when that command starts was raised during
    /// the instruction before it, the case where silicon runs the command
    /// and takes the interrupt with EPC still on it. Transient: a save
    /// state lands between instructions, where the next step re-arms it.
    #[serde(skip)]
    gte_irq_watch: Option<u32>,
    /// The last instruction was a branch or jump, taken or not, so the next
    /// one sits in its delay slot. `pending_pc` only records a taken
    /// branch's target; a fault in the delay slot of a branch that was not
    /// taken still reports Cause.BD and EPC on the branch.
    branch_delay_next: bool,
    /// The instruction now executing sits in a branch delay slot. Read by
    /// the fault paths that are not handed `in_delay_slot`. Only meaningful
    /// inside `step`, so never saved.
    #[serde(skip)]
    executing_in_branch_delay: bool,
    /// Emulator-owned instruction-cache refill profile. Excluded from save
    /// states because it is diagnostic history, not emulated hardware state.
    #[serde(skip)]
    instruction_cache_profile: InstructionCacheProfileSnapshot,
    /// Opt-in exact refill-event capture. Disabled during ordinary play.
    #[serde(skip)]
    instruction_cache_event_profile_enabled: bool,
    /// Most recent refill, drained by an emulator-owned profiler after each
    /// step. At most one instruction fetch occurs per interpreter step.
    #[serde(skip)]
    last_instruction_cache_refill: Option<InstructionCacheRefillEvent>,
    /// Opt-in CPU cycle attribution. Kept off on the normal interpreter path
    /// so profiling cannot become a host-performance tax for regular play.
    #[serde(skip)]
    cpu_cycle_profile_enabled: bool,
    /// Emulator-owned CPU cycle attribution history. Diagnostic only.
    #[serde(skip)]
    cpu_cycle_profile: CpuCycleProfileSnapshot,
    /// Wait-loop and RAM-traffic attribution, enabled with the cycle profile.
    #[serde(skip)]
    cpu_wait_profile: CpuWaitProfileSnapshot,
    #[serde(skip)]
    wait_loop: WaitLoopTracker,
    /// Opt-in exact instruction-class profiler.
    #[serde(skip)]
    instruction_class_profile_enabled: bool,
    /// Dynamic instruction-class totals. Diagnostic only.
    #[serde(skip)]
    instruction_class_profile: InstructionClassProfileSnapshot,
    /// COP2 -- Geometry Transformation Engine. Holds 32 data + 32
    /// control registers and dispatches the GTE function set.
    cop2: Gte,
    /// Optional debug freelook camera delta injected into the GTE view
    /// transform for RTPS/RTPT. Off by default; set by the frontend.
    /// Host-side debug camera hack, not emulated game state -- excluded
    /// from save states, always comes back disabled on load (the
    /// frontend re-applies it every frame from its own UI state anyway).
    #[serde(skip)]
    freelook: FreelookState,
    /// Depth of nested exception handlers. Incremented on every
    /// exception entry (IRQ, syscall, break) and decremented on
    /// every `RFE`. `in_isr()` returns `true` iff this is > 0.
    /// Counted as depth (not boolean) so that nested RFEs don't
    /// spuriously clear the flag while we're still inside the
    /// outer handler -- critical for the parity harness's
    /// aggregation of clean IRQ entries, which must continue
    /// across nested syscalls/IRQs inside the handler body.
    isr_depth: u32,
    /// Latched on the *clean* (depth 0 → 1) entry: `true` iff the
    /// outermost exception was an `Interrupt` (cause=0). Stays set
    /// until `isr_depth` returns to 0 via the final RFE. Mirrors
    /// the `!m_wasInISR && cause == 0` condition in Redux's
    /// `debug.cc:235` early-return that hides the IRQ-handler body
    /// from the recorded trace. Syscall-entered spans clear this to
    /// false and stay that way until the outermost RFE.
    clean_irq_entry: bool,
}

/// Every MIPS I branch and jump: REGIMM, J, JAL, BEQ, BNE, BLEZ, BGTZ, and
/// SPECIAL JR/JALR. The instruction after any of them is a delay slot.
fn is_branch_or_jump(instr: u32) -> bool {
    match instr >> 26 {
        0x01..=0x07 => true,
        0x00 => matches!(instr & 0x3F, 0x08 | 0x09),
        _ => false,
    }
}

impl Cpu {
    /// Construct a CPU in its reset state.
    ///
    /// PC is seated at the KSEG1 BIOS reset vector (`0xBFC0_0000`) so
    /// the first fetch goes through the uncached path, matching
    /// hardware behaviour at power-on.
    pub fn new() -> Self {
        Self {
            pc: memory::bios::RESET_VECTOR,
            gprs: [0; 32],
            cop0: [0; 32],
            cache_control: 0,
            instruction_cache: InstructionCache::default(),
            tick: 0,
            pending_pc: None,
            pending_load: None,
            committing_load: None,
            gte_busy_until: 0,
            load_shadow: None,
            gte_mac0_ready_at: 0,
            gte_mac0_stale: 0,
            gte_lzcr_ready_at: 0,
            gte_lzcr_stale: 0,
            gte_last_write_tick: 0,
            gte_sxy2_write_tick: 0,
            gte_prev_sxy2_x: 0,
            gte_prev_sxy2_y: 0,
            gte_sxy2_from_transform: false,
            gte_prev_sxy2_from_transform: false,
            gte_sxy_write_tick: [0; 3],
            gte_nclip_rtpt_history: false,
            gte_nclip_history_tick: 0,
            gte_ir3_write_tick: 0,
            gte_prev_ir3: 0,
            gte_last_mtc2_ir3: 0,
            gte_mac_write_tick: 0,
            gte_mac_write_reg: u8::MAX,
            gte_mac_write_value: 0,
            // Default ON with the SILICON-MEASURED window (settle sweeps,
            // 2026-06-10): MAC0/LZCR are stale only for the immediately-next
            // instruction; one intervening instruction settles them. The
            // earlier guessed windows broke commercial culling; the measured
            // tick-1 window leaves libgte reads (>= +2 instructions) live.
            // PSOXIDE_GTE_READ_LATENCY=0 disables for A/B experiments.
            gte_read_latency_bypass: std::env::var("PSOXIDE_GTE_READ_LATENCY")
                .map(|v| v == "0")
                .unwrap_or(false),
            gte_v0x_hazard: std::env::var("PSOXIDE_GTE_V0X_STALE")
                .map(|v| v == "1")
                .unwrap_or(false),
            gte_v0x_window: std::env::var("PSOXIDE_GTE_V0X_WINDOW")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2),
            gte_v0xy_write_tick: u64::MAX / 2,
            gte_prev_v0x: 0,
            hilo_busy_until: 0,
            hi: 0,
            lo: 0,
            pending_exception_pc: None,
            hle_exception_active: false,
            exception_counts: [0; 32],
            irq_line_high_steps: 0,
            should_take_interrupt_steps: 0,
            gte_irq_watch: None,
            branch_delay_next: false,
            executing_in_branch_delay: false,
            instruction_cache_profile: InstructionCacheProfileSnapshot::default(),
            instruction_cache_event_profile_enabled: false,
            last_instruction_cache_refill: None,
            cpu_cycle_profile_enabled: false,
            cpu_cycle_profile: CpuCycleProfileSnapshot::default(),
            cpu_wait_profile: CpuWaitProfileSnapshot::default(),
            wait_loop: WaitLoopTracker::default(),
            instruction_class_profile_enabled: false,
            instruction_class_profile: InstructionClassProfileSnapshot::default(),
            cop2: Gte::new(),
            freelook: FreelookState::default(),
            isr_depth: 0,
            clean_irq_entry: false,
        }
    }

    /// Set the debug freelook camera delta (see [`FreelookState`]). The
    /// frontend calls this once per frame; `FreelookState::default()`
    /// (disabled) turns it off.
    pub fn set_freelook(&mut self, freelook: FreelookState) {
        self.freelook = freelook;
    }

    /// `true` while the CPU is inside any exception handler (at any
    /// nesting depth). Mirrors Redux's `m_inISR`. Diagnostic.
    #[inline]
    pub fn in_isr(&self) -> bool {
        self.isr_depth > 0
    }

    /// `true` iff we're still inside the span of a *clean* IRQ
    /// entry -- i.e. the outermost handler on the exception stack
    /// was entered via `Interrupt` (cause=0) from user mode. Stays
    /// set across nested exceptions until the outermost RFE. The
    /// parity harness uses this to decide whether to aggregate
    /// handler-body steps into the pre-IRQ record, matching
    /// Redux's `debug.cc:235` early-return which hides the trace
    /// body of clean IRQ entries.
    #[inline]
    pub fn in_irq_handler(&self) -> bool {
        self.clean_irq_entry
    }

    /// COP2 (GTE) state. Diagnostics / UI surfaces only -- opcode
    /// dispatch goes through the inherent methods directly.
    #[inline]
    pub fn cop2(&self) -> &Gte {
        &self.cop2
    }

    /// Cumulative exception counts, keyed by CAUSE.ExcCode. Diagnostic.
    #[inline]
    pub fn exception_counts(&self) -> &[u64; 32] {
        &self.exception_counts
    }

    /// How many `step()` calls observed the IRQ line high.
    #[inline]
    pub fn irq_line_high_steps(&self) -> u64 {
        self.irq_line_high_steps
    }

    /// How many `step()` calls would have entered an IRQ exception.
    #[inline]
    pub fn should_take_interrupt_steps(&self) -> u64 {
        self.should_take_interrupt_steps
    }

    /// Seed CPU state from a PSX-EXE header so the next `step()`
    /// enters the homebrew at its entry point. Used for
    /// `PSOXIDE_EXE` side-loading that bypasses the BIOS.
    ///
    /// The corresponding payload must already have been copied into
    /// RAM by [`crate::Bus::load_exe_payload`].
    pub fn seed_from_exe(&mut self, initial_pc: u32, initial_gp: u32, initial_sp: Option<u32>) {
        self.pc = initial_pc;
        // The real BIOS Exec path flushes the I-cache before entering a
        // newly loaded executable and leaves BIU_CONFIG in its normal
        // 0x0001_E988 state. Direct side-loading bypasses that code, so
        // reproduce both observable effects here. Leaving the reset value
        // of zero would run every KSEG0 instruction uncached and add six RAM
        // bus stalls per instruction.
        self.instruction_cache.invalidate_all();
        self.cache_control = CACHE_CONTROL_BIOS_NORMAL;
        // The retail shell enables COP2 before launching an executable, and
        // games are known to rely on that inherited state. Direct EXE loading
        // bypasses the shell, so reproduce its launch contract here; otherwise
        // the first GTE transfer raises CpU and falls into an unserviceable
        // low-RAM exception path.
        self.cop0[12] |= COP0_STATUS_CU2;
        self.pending_pc = None;
        self.branch_delay_next = false;
        self.pending_load = None;
        self.committing_load = None;
        self.gte_busy_until = 0;
        self.load_shadow = None;
        self.gte_mac0_ready_at = 0;
        self.gte_mac0_stale = 0;
        self.gte_lzcr_ready_at = 0;
        self.gte_lzcr_stale = 0;
        self.gte_sxy2_write_tick = 0;
        self.gte_prev_sxy2_x = 0;
        self.gte_prev_sxy2_y = 0;
        self.gte_sxy2_from_transform = false;
        self.gte_prev_sxy2_from_transform = false;
        self.gte_sxy_write_tick = [0; 3];
        self.gte_nclip_rtpt_history = false;
        self.gte_nclip_history_tick = 0;
        self.gte_ir3_write_tick = 0;
        self.gte_prev_ir3 = 0;
        self.gte_last_mtc2_ir3 = 0;
        self.gte_mac_write_tick = 0;
        self.gte_mac_write_reg = u8::MAX;
        self.gte_mac_write_value = 0;
        self.hilo_busy_until = 0;
        self.pending_exception_pc = None;
        self.hle_exception_active = false;
        self.set_gpr(28, initial_gp);
        // PSX-EXEs with a zero SP header (common in test homebrew like
        // ps1-tests/amidog) expect the BIOS-default stack; a fresh CPU's
        // reset SP is 0, which wraps the stack into unmapped 0xFFFFFFxx
        // and crashes the side-load instantly.
        let sp = initial_sp.unwrap_or(0x801F_FF00);
        self.set_gpr(29, sp);
        // Frame pointer tracks SP on bare-metal boot so backtraces
        // look sane before main() sets up its own frame.
        self.set_gpr(30, sp);
    }

    /// Seed CPU state from a PSX-EXE header plus BIOS `Exec` arguments.
    ///
    /// Retail disc boot enters through `Exec(header, argc, argv)`,
    /// which moves `argc` into `$a0` and `argv` into `$a1` before
    /// jumping to the EXE entry point. Fast-boot paths that warm the
    /// real BIOS first must set these explicitly so stale BIOS register
    /// values do not leak into game code.
    pub fn seed_from_exe_with_args(
        &mut self,
        initial_pc: u32,
        initial_gp: u32,
        initial_sp: Option<u32>,
        argc: u32,
        argv: u32,
    ) {
        self.seed_from_exe(initial_pc, initial_gp, initial_sp);
        self.set_gpr(4, argc);
        self.set_gpr(5, argv);
    }

    /// Set `$fp` after [`Self::seed_from_exe_with_args`], for the disc boot
    /// case where the BIOS leaves it different from `$sp`.
    pub(crate) fn seed_frame_pointer(&mut self, fp: u32) {
        self.set_gpr(30, fp);
    }

    /// Test hook: mutable register file.
    #[cfg(test)]
    pub(crate) fn gprs_mut_for_test(&mut self) -> &mut [u32; 32] {
        &mut self.gprs
    }

    /// Test hook: set the PC.
    #[cfg(test)]
    pub(crate) fn set_pc_for_test(&mut self, pc: u32) {
        self.pc = pc;
    }

    /// Current program counter.
    #[inline]
    pub fn pc(&self) -> u32 {
        self.pc
    }

    /// Read a general-purpose register. `$0` always reads as zero.
    #[inline]
    pub fn gpr(&self, index: u8) -> u32 {
        self.gprs[(index & 31) as usize]
    }

    /// All 32 general-purpose registers. Useful for UI snapshots.
    #[inline]
    pub fn gprs(&self) -> &[u32; 32] {
        &self.gprs
    }

    /// COP0 register file -- System Control coprocessor state (SR, Cause,
    /// EPC, BadVAddr, …).
    #[inline]
    pub fn cop0(&self) -> &[u32; 32] {
        &self.cop0
    }

    /// HI half of the multiply/divide result register.
    #[inline]
    pub fn hi(&self) -> u32 {
        self.hi
    }

    /// LO half of the multiply/divide result register.
    #[inline]
    pub fn lo(&self) -> u32 {
        self.lo
    }

    /// Retired-instruction counter since reset.
    #[inline]
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// Snapshot emulator-owned instruction-cache refill counters.
    #[inline]
    pub fn instruction_cache_profile(&self) -> InstructionCacheProfileSnapshot {
        self.instruction_cache_profile
    }

    /// Enable or disable exact instruction-cache refill event capture.
    pub fn set_instruction_cache_event_profile_enabled(&mut self, enabled: bool) {
        self.instruction_cache_event_profile_enabled = enabled;
        self.last_instruction_cache_refill = None;
    }

    /// Drain the refill produced by the most recent interpreter step.
    #[inline]
    pub fn take_instruction_cache_refill_event(&mut self) -> Option<InstructionCacheRefillEvent> {
        self.last_instruction_cache_refill.take()
    }

    /// Enable or disable CPU cycle attribution, resetting prior observations.
    pub fn set_cpu_cycle_profile_enabled(&mut self, enabled: bool) {
        self.cpu_cycle_profile_enabled = enabled;
        self.cpu_cycle_profile = CpuCycleProfileSnapshot::default();
        self.cpu_wait_profile = CpuWaitProfileSnapshot::default();
        self.wait_loop = WaitLoopTracker::default();
    }

    /// True while CPU cycle attribution is being collected.
    #[inline]
    pub fn cpu_cycle_profile_enabled(&self) -> bool {
        self.cpu_cycle_profile_enabled
    }

    /// Snapshot the wait-loop and RAM-traffic counters collected with the
    /// cycle profile.
    #[inline]
    pub fn cpu_wait_profile(&self) -> CpuWaitProfileSnapshot {
        self.cpu_wait_profile
    }

    /// Snapshot emulator-owned CPU cycle attribution counters.
    #[inline]
    pub fn cpu_cycle_profile(&self) -> CpuCycleProfileSnapshot {
        self.cpu_cycle_profile
    }

    /// Enable or disable exact instruction-class profiling and reset totals.
    pub fn set_instruction_class_profile_enabled(&mut self, enabled: bool) {
        self.instruction_class_profile_enabled = enabled;
        self.instruction_class_profile = InstructionClassProfileSnapshot::default();
    }

    /// Snapshot dynamic instruction-class totals.
    #[inline]
    pub fn instruction_class_profile(&self) -> InstructionClassProfileSnapshot {
        self.instruction_class_profile
    }

    /// Write a general-purpose register, enforcing the MIPS invariant
    /// that `$0` is hardwired to zero.
    ///
    /// Also implements R3000 load-delay squashing: if a load is about
    /// to commit into the same register this instruction is writing,
    /// the load's writeback is cancelled. The hardware only has one
    /// writeback port per GPR, and if the delay slot non-load-wise
    /// writes to the load's target, its value wins -- the load is lost.
    #[inline]
    fn set_gpr(&mut self, index: u8, value: u32) {
        let i = (index & 31) as usize;
        if i != 0 {
            self.gprs[i] = value;
        }
        if let Some((reg, _)) = &self.committing_load {
            if *reg == index {
                self.committing_load = None;
            }
        }
    }

    /// Fetch the 32-bit instruction word at the current PC without
    /// advancing it. Exposed for diagnostic tools; `step` uses it
    /// internally too. `&mut Bus` because some peripheral reads
    /// (CD-ROM, future DMA) mutate.
    pub fn fetch(&mut self, bus: &mut Bus) -> u32 {
        self.fetch_instruction(self.pc, bus)
    }

    #[inline]
    fn instruction_cache_enabled_at(&self, addr: u32) -> bool {
        // KUSEG (0x0000_0000..0x7FFF_FFFF) and KSEG0
        // (0x8000_0000..0x9FFF_FFFF) are cacheable. KSEG1 is the
        // explicit uncached alias and KSEG2 contains CPU control space.
        addr < 0xA000_0000 && self.cache_control & CACHE_CONTROL_IS1 != 0
    }

    #[inline(always)]
    fn fetch_instruction(&mut self, addr: u32, bus: &mut Bus) -> u32 {
        // Hit fast path: cached region, no fill streaming, no diagnostics.
        // Exactly the general path's hit case (no stall, no fill, the bus
        // told it was a hit), without its bookkeeping.
        if bus.code_stream_idle()
            && self.instruction_cache_enabled_at(addr)
            && !self.instruction_cache_event_profile_enabled
            && !bus.limit(crate::limits::ICACHE)
        {
            if let Some(instruction) = self.instruction_cache.hit(memory::to_physical(addr)) {
                bus.note_cached_fetch(true);
                bus.add_zero_cycles();
                return instruction;
            }
        }
        self.fetch_instruction_slow(addr, bus)
    }

    #[inline(never)]
    fn fetch_instruction_slow(&mut self, addr: u32, bus: &mut Bus) -> u32 {
        if self.instruction_cache_event_profile_enabled {
            self.last_instruction_cache_refill = None;
        }
        let abandon = bus.streaming_fill_wait(memory::to_physical(addr));
        if abandon != 0 {
            if self.cpu_cycle_profile_enabled {
                self.cpu_cycle_profile.icache_refill_stall_cycles = self
                    .cpu_cycle_profile
                    .icache_refill_stall_cycles
                    .saturating_add(abandon as u64);
            }
            bus.add_cycles(abandon);
        }
        if !self.instruction_cache_enabled_at(addr) {
            let stalls = bus.instruction_read_stalls(addr);
            if self.cpu_cycle_profile_enabled {
                self.cpu_cycle_profile.uncached_fetch_stall_cycles = self
                    .cpu_cycle_profile
                    .uncached_fetch_stall_cycles
                    .saturating_add(stalls as u64);
            }
            bus.add_cycles(stalls);
            return bus.read_instruction32(addr);
        }
        let phys = memory::to_physical(addr);
        let iblksz =
            ((self.cache_control & CACHE_CONTROL_IBLKSZ_MASK) >> CACHE_CONTROL_IBLKSZ_SHIFT) as u8;
        if bus.limit(crate::limits::ICACHE) {
            // Limit oracle: every cached fetch hits. The cache still fills
            // (so its contents stay coherent), but no fill touches the bus.
            let (instruction, _, _) = self.instruction_cache.fetch(phys, iblksz, bus, false);
            bus.note_cached_fetch(true);
            return instruction;
        }
        let (instruction, fill, refill) = self.instruction_cache.fetch(
            phys,
            iblksz,
            bus,
            self.instruction_cache_event_profile_enabled,
        );
        bus.note_cached_fetch(fill.words == 0 && abandon == 0);
        let filled_words = fill.words;
        let streaming = self.cache_control & CACHE_CONTROL_NOSTR == 0;
        let stalls = bus.icache_fill_stalls(phys, fill.words, fill.lead_words, streaming);
        if filled_words != 0 {
            self.instruction_cache_profile.refill_events = self
                .instruction_cache_profile
                .refill_events
                .saturating_add(1);
            self.instruction_cache_profile.refill_words = self
                .instruction_cache_profile
                .refill_words
                .saturating_add(filled_words as u64);
            self.instruction_cache_profile.refill_stall_cycles = self
                .instruction_cache_profile
                .refill_stall_cycles
                .saturating_add(stalls as u64);
        }
        if self.cpu_cycle_profile_enabled {
            self.cpu_cycle_profile.icache_refill_stall_cycles = self
                .cpu_cycle_profile
                .icache_refill_stall_cycles
                .saturating_add(stalls as u64);
        }
        if self.instruction_cache_event_profile_enabled {
            self.last_instruction_cache_refill = refill.map(|event| InstructionCacheRefillEvent {
                fetch_pc: addr,
                cache_set: event.set,
                incoming_line: event.incoming_line,
                incoming_tag: event.incoming_tag,
                victim_line: event.victim_line,
                victim_tag: event.victim_tag,
                victim_valid_mask: event.victim_valid_mask,
                miss_kind: if event.tag_miss {
                    InstructionCacheMissKind::Tag
                } else {
                    InstructionCacheMissKind::InvalidWord
                },
                fill_words: event.fill_words,
                stall_cycles: stalls,
            });
        }
        bus.add_cycles(stalls);
        instruction
    }

    #[inline]
    fn instruction_address_is_executable(addr: u32) -> bool {
        if addr >= 0xC000_0000 {
            return false;
        }
        let phys = memory::to_physical(addr);
        phys < memory::ram::MIRROR_END
            || (memory::bios::BASE..memory::bios::BASE + memory::bios::SIZE as u32).contains(&phys)
    }

    #[inline]
    fn profiled_data_access(&self, instr: u32) -> Option<ProfiledDataAccess> {
        let opcode = ((instr >> 26) & 0x3f) as u8;
        let is_load = matches!(
            opcode,
            0x20 | 0x21 | 0x22 | 0x23 | 0x24 | 0x25 | 0x26 | 0x32
        );
        let is_store = matches!(opcode, 0x28 | 0x29 | 0x2a | 0x2b | 0x2e | 0x3a);
        if !is_load && !is_store {
            return None;
        }

        let base = ((instr >> 21) & 0x1f) as u8;
        let offset = (instr as i16) as i32 as u32;
        let address = self.gpr(base).wrapping_add(offset);
        let physical = memory::to_physical(address);
        if physical < memory::ram::MIRROR_END {
            return Some(if is_load {
                ProfiledDataAccess::RamLoad {
                    stack_relative: base == 29,
                }
            } else {
                ProfiledDataAccess::RamStore
            });
        }
        if (memory::scratchpad::BASE..memory::scratchpad::BASE + memory::scratchpad::SIZE as u32)
            .contains(&physical)
        {
            return Some(ProfiledDataAccess::Other);
        }
        if (memory::expansion1::BASE..memory::bios::BASE).contains(&physical) {
            return Some(ProfiledDataAccess::Mmio);
        }
        Some(ProfiledDataAccess::Other)
    }

    fn profile_instruction_class(&mut self, instr: u32, in_delay_slot: bool) {
        let opcode = ((instr >> 26) & 0x3F) as u8;
        let funct = (instr & 0x3F) as u8;
        let base = ((instr >> 21) & 0x1F) as u8;
        let is_load = matches!(opcode, 0x20..=0x26 | 0x32);
        let is_store = matches!(opcode, 0x28 | 0x29 | 0x2A | 0x2B | 0x2E | 0x3A);
        let is_control_flow =
            matches!(opcode, 0x01..=0x07) || (opcode == 0 && matches!(funct, 0x08 | 0x09));
        let profile = &mut self.instruction_class_profile;

        macro_rules! increment {
            ($field:ident) => {
                profile.$field = profile.$field.saturating_add(1)
            };
        }

        increment!(instructions);
        if instr == 0 {
            increment!(nops);
        }
        if in_delay_slot {
            increment!(delay_slot_instructions);
            if instr == 0 {
                increment!(delay_slot_nops);
            }
            if is_load {
                increment!(delay_slot_loads);
            }
            if is_store {
                increment!(delay_slot_stores);
            }
            if is_control_flow {
                increment!(delay_slot_control_flow);
            }
        }

        match opcode {
            0x20 | 0x24 => increment!(byte_loads),
            0x21 | 0x25 => increment!(halfword_loads),
            0x22 | 0x26 => increment!(unaligned_loads),
            0x23 | 0x32 => increment!(word_loads),
            0x28 => increment!(byte_stores),
            0x29 => increment!(halfword_stores),
            0x2A | 0x2E => increment!(unaligned_stores),
            0x2B | 0x3A => increment!(word_stores),
            _ => {}
        }

        if is_load || is_store {
            let offset = (instr as i16) as i32 as u32;
            let address = self.gprs[(base & 31) as usize].wrapping_add(offset);
            let physical = memory::to_physical(address);
            if physical < memory::ram::MIRROR_END {
                increment!(ram_accesses);
            } else if !(0xA000_0000..0xC000_0000).contains(&address)
                && (memory::scratchpad::BASE
                    ..memory::scratchpad::BASE + memory::scratchpad::SIZE as u32)
                    .contains(&physical)
            {
                increment!(scratchpad_accesses);
            } else if (memory::expansion1::BASE..memory::bios::BASE).contains(&physical) {
                increment!(mmio_accesses);
            } else {
                increment!(other_accesses);
            }
            if base == 29 {
                if is_load {
                    increment!(sp_relative_loads);
                } else {
                    increment!(sp_relative_stores);
                }
            }
        }

        match opcode {
            0x02 => increment!(direct_jumps),
            0x03 => increment!(jal),
            0x01 | 0x04..=0x07 => increment!(conditional_branches),
            0x0F => increment!(lui),
            0x12 => {
                if instr & (1 << 25) != 0 {
                    increment!(gte_commands);
                } else {
                    increment!(gte_register_transfers);
                }
            }
            0 => match funct {
                0x09 => increment!(jalr),
                0x18 | 0x19 => increment!(multiply),
                0x1A | 0x1B => increment!(divide),
                _ => {}
            },
            _ => {}
        }
    }

    /// Wait-loop detector and RAM-traffic count; see
    /// [`CpuWaitProfileSnapshot`]. Runs only while the cycle profile is on,
    /// after the instruction retired and `self.pc` holds the next PC.
    fn observe_wait_loop(
        &mut self,
        instr: u32,
        pc: u32,
        taken_branch: Option<u32>,
        access: Option<ProfiledDataAccess>,
    ) {
        let opcode = (instr >> 26) & 0x3f;
        let base = (instr >> 21) & 0x1f;
        if matches!(opcode, 0x28 | 0x29 | 0x2a | 0x2b | 0x2e | 0x3a) && base != 29 {
            self.wait_loop.stored = true;
        }
        match access {
            Some(ProfiledDataAccess::RamLoad { .. }) => {
                self.cpu_wait_profile.ram_loads = self.cpu_wait_profile.ram_loads.saturating_add(1);
            }
            Some(ProfiledDataAccess::RamStore) => {
                self.cpu_wait_profile.ram_stores =
                    self.cpu_wait_profile.ram_stores.saturating_add(1);
            }
            Some(ProfiledDataAccess::Mmio) => self.wait_loop.polled_io = true,
            Some(ProfiledDataAccess::Other) | None => {}
        }
        let Some(target) = taken_branch else {
            return;
        };
        // `pc` is the delay slot; an exception may have redirected past the
        // branch, which ends any loop in progress. Forward branches (an `if`
        // inside the loop body) and calls leave it armed; only the loop's
        // own backward branch closes an iteration.
        if self.pc != target {
            self.wait_loop.armed = false;
            return;
        }
        if target > pc {
            return;
        }
        if pc - target > WAIT_LOOP_MAX_SPAN {
            self.wait_loop.armed = false;
            return;
        }
        let tracker = self.wait_loop;
        if tracker.armed && tracker.target == target && !tracker.stored {
            let spent = self.cpu_cycle_profile.delta_since(tracker.start);
            let bucket = if tracker.polled_io {
                &mut self.cpu_wait_profile.hardware_poll
            } else {
                &mut self.cpu_wait_profile.memory_poll
            };
            *bucket = bucket.saturating_add(spent);
        }
        self.wait_loop = WaitLoopTracker {
            armed: true,
            target,
            stored: false,
            polled_io: false,
            start: self.cpu_cycle_profile,
        };
    }

    #[inline]
    fn record_execution_stalls(
        &mut self,
        instr: u32,
        access: Option<ProfiledDataAccess>,
        stalls: u64,
    ) {
        if stalls == 0 {
            return;
        }
        match access {
            Some(ProfiledDataAccess::RamLoad { stack_relative }) => {
                self.cpu_cycle_profile.ram_load_stall_cycles = self
                    .cpu_cycle_profile
                    .ram_load_stall_cycles
                    .saturating_add(stalls);
                if stack_relative {
                    self.cpu_cycle_profile.stack_ram_load_stall_cycles = self
                        .cpu_cycle_profile
                        .stack_ram_load_stall_cycles
                        .saturating_add(stalls);
                }
            }
            Some(ProfiledDataAccess::RamStore) => {
                self.cpu_cycle_profile.ram_store_stall_cycles = self
                    .cpu_cycle_profile
                    .ram_store_stall_cycles
                    .saturating_add(stalls);
            }
            Some(ProfiledDataAccess::Mmio) => {
                self.cpu_cycle_profile.mmio_stall_cycles = self
                    .cpu_cycle_profile
                    .mmio_stall_cycles
                    .saturating_add(stalls);
            }
            Some(ProfiledDataAccess::Other) => {
                self.cpu_cycle_profile.other_stall_cycles = self
                    .cpu_cycle_profile
                    .other_stall_cycles
                    .saturating_add(stalls);
            }
            None => {
                let opcode = ((instr >> 26) & 0x3f) as u8;
                let special = opcode == 0;
                let function = (instr & 0x3f) as u8;
                if opcode == 0x12 && instr & (1 << 25) != 0 {
                    self.cpu_cycle_profile.gte_busy_stall_cycles = self
                        .cpu_cycle_profile
                        .gte_busy_stall_cycles
                        .saturating_add(stalls);
                } else if special && matches!(function, 0x10 | 0x12) {
                    self.cpu_cycle_profile.muldiv_interlock_stall_cycles = self
                        .cpu_cycle_profile
                        .muldiv_interlock_stall_cycles
                        .saturating_add(stalls);
                } else {
                    self.cpu_cycle_profile.other_stall_cycles = self
                        .cpu_cycle_profile
                        .other_stall_cycles
                        .saturating_add(stalls);
                }
            }
        }
    }

    #[inline]
    fn read_cache_control(&self) -> u32 {
        // Bits 6 and 10 are hardwired zero on retail hardware.
        self.cache_control & !((1 << 6) | (1 << 10))
    }

    #[inline]
    fn write_cache_control(&mut self, value: u32) {
        self.cache_control = value & !((1 << 6) | (1 << 10));
    }

    #[inline]
    fn cache_control_lane(addr: u32) -> Option<u32> {
        addr.checked_sub(memory::cache_control::ADDR)
            .filter(|offset| *offset < 4)
    }

    #[inline]
    fn read_byte(&self, addr: u32, bus: &mut Bus) -> u8 {
        if let Some(offset) = Self::cache_control_lane(addr) {
            return (self.read_cache_control() >> (offset * 8)) as u8;
        }
        bus.read8(addr)
    }

    #[inline]
    fn read_half(&self, addr: u32, bus: &mut Bus) -> u16 {
        if let Some(offset) = Self::cache_control_lane(addr) {
            return (self.read_cache_control() >> (offset * 8)) as u16;
        }
        bus.read16(addr)
    }

    #[inline]
    fn read_word(&self, addr: u32, bus: &mut Bus) -> u32 {
        if addr == memory::cache_control::ADDR {
            return self.read_cache_control();
        }
        if self.cache_isolated() && self.cache_control & CACHE_CONTROL_IS1 != 0 {
            return if self.cache_control & CACHE_CONTROL_TAG != 0 {
                self.instruction_cache.read_tag(memory::to_physical(addr))
            } else {
                self.instruction_cache.read_data(memory::to_physical(addr))
            };
        }
        bus.read32(addr)
    }

    #[inline]
    fn write_word(&mut self, addr: u32, value: u32, bus: &mut Bus) {
        if addr == memory::cache_control::ADDR {
            self.write_cache_control(value);
            return;
        }
        if self.cache_isolated() {
            if self.cache_control & CACHE_CONTROL_IS1 != 0 {
                if self.cache_control & CACHE_CONTROL_TAG != 0 {
                    self.instruction_cache
                        .write_tag(memory::to_physical(addr), value);
                } else {
                    self.instruction_cache
                        .write_data(memory::to_physical(addr), value);
                }
            }
            return;
        }
        bus.cpu_write32(addr, value);
    }

    #[inline]
    fn write_byte(&mut self, addr: u32, value: u32, bus: &mut Bus) {
        if let Some(offset) = Self::cache_control_lane(addr) {
            let shift = offset * 8;
            let merged = (self.read_cache_control() & !(0xFF << shift)) | ((value & 0xFF) << shift);
            self.write_cache_control(merged);
            return;
        }
        if !self.cache_isolated() {
            bus.cpu_write8(addr, value);
        }
    }

    #[inline]
    fn write_half(&mut self, addr: u32, value: u32, bus: &mut Bus) {
        if let Some(offset) = Self::cache_control_lane(addr) {
            let shift = offset * 8;
            let merged =
                (self.read_cache_control() & !(0xFFFF << shift)) | ((value & 0xFFFF) << shift);
            self.write_cache_control(merged);
            return;
        }
        if !self.cache_isolated() {
            bus.cpu_write16(addr, value);
        }
    }

    #[inline]
    fn charge_read(&self, bus: &mut Bus, addr: u32, width: AccessWidth) {
        if !self.cache_isolated() || Self::cache_control_lane(addr).is_some() {
            if Self::limit_free_read(bus, addr) {
                return;
            }
            let stalls = bus.cpu_read_stalls(addr, width);
            if crate::timers::Timers::contains(memory::to_physical(addr)) {
                bus.add_root_counter_read_stalls(addr, stalls);
            } else {
                bus.add_cycles(stalls);
            }
        }
    }

    /// Limit oracles `ram` and `mmio`: a data read that costs only its
    /// issue cycle.
    #[inline]
    fn limit_free_read(bus: &Bus, addr: u32) -> bool {
        let phys = memory::to_physical(addr);
        (bus.limit(crate::limits::RAM) && phys < memory::ram::MIRROR_END)
            || (bus.limit(crate::limits::MMIO) && Self::polled_status_register(phys))
    }

    /// GPUSTAT and the interrupt, DMA and timer registers: what guest wait
    /// loops poll.
    fn polled_status_register(phys: u32) -> bool {
        (0x1F80_1070..=0x1F80_10FF).contains(&phys)
            || crate::timers::Timers::contains(phys)
            || (0x1F80_1814..0x1F80_1818).contains(&phys)
    }

    /// SWL/SWR are emulated as read-merge-write, but main RAM does not see a
    /// read: the memory controller writes the selected byte lanes. hwtest
    /// v1.22 record `0xCD` (64 `swl; swr` pairs) reads 266 cycles on silicon,
    /// the cost of 128 ordinary stores, where charging a load for each read
    /// 902. Other buses keep the load charge until something measures them.
    fn charge_unaligned_store_read(&self, bus: &mut Bus, aligned: u32) {
        if !Self::is_cpu_local_memory(aligned) {
            self.charge_read(bus, aligned, AccessWidth::Word);
        }
    }

    /// Main RAM or scratchpad, through any segment.
    fn is_cpu_local_memory(addr: u32) -> bool {
        let phys = memory::to_physical(addr);
        phys < memory::ram::MIRROR_END
            || (memory::scratchpad::BASE
                ..memory::scratchpad::BASE + memory::scratchpad::SIZE as u32)
                .contains(&phys)
    }

    /// Lane-steering cycles for a partial LWL/LWR. They were fitted to the
    /// unaligned SPU word probe (38.94 cycles for the pair on a 16-bit bus).
    /// Main RAM shows none: hwtest v1.22 record `0xCC` (64 `lwl; lwr` pairs)
    /// reads 901 cycles on silicon, two plain seven-cycle loads each.
    fn unaligned_lane_cycles(addr: u32, cycles: u32) -> u32 {
        if Self::is_cpu_local_memory(addr) {
            0
        } else {
            cycles
        }
    }

    /// Read only the byte lanes consumed by LWL. The external bus does not
    /// blindly perform a 32-bit transaction for a partial-word opcode: this
    /// is observable on the SPU's 16-bit register bus. Partial forms have one
    /// extra lane-steering cycle beyond the selected byte/half/word access.
    fn read_lwl_lanes(&self, addr: u32, bus: &mut Bus) -> u32 {
        let aligned = addr & !3;
        match addr & 3 {
            0 => {
                self.charge_read(bus, aligned, AccessWidth::Byte);
                bus.add_cycles(Self::unaligned_lane_cycles(aligned, 1));
                bus.read8(aligned) as u32
            }
            1 => {
                self.charge_read(bus, aligned, AccessWidth::Half);
                bus.add_cycles(Self::unaligned_lane_cycles(aligned, 1));
                bus.read16(aligned) as u32
            }
            2 => {
                self.charge_read(bus, aligned, AccessWidth::Word);
                bus.add_cycles(Self::unaligned_lane_cycles(aligned, 1));
                bus.read16(aligned) as u32 | ((bus.read8(aligned + 2) as u32) << 16)
            }
            _ => {
                self.charge_read(bus, aligned, AccessWidth::Word);
                bus.read32(aligned)
            }
        }
    }

    /// Mirror of [`Self::read_lwl_lanes`] for LWR. Returned bytes retain
    /// their aligned-word bit positions so the architectural merge formula
    /// below remains identical to the R3000 truth table.
    fn read_lwr_lanes(&self, addr: u32, bus: &mut Bus) -> u32 {
        let aligned = addr & !3;
        match addr & 3 {
            0 => {
                self.charge_read(bus, aligned, AccessWidth::Word);
                bus.read32(aligned)
            }
            1 => {
                self.charge_read(bus, aligned, AccessWidth::Word);
                // The R3000's right-merge path has a two-cycle lane-steering
                // penalty. Together with LWL's one cycle this reproduces the
                // 38.94-cycle unaligned SPU word measured on silicon.
                bus.add_cycles(Self::unaligned_lane_cycles(aligned, 2));
                ((bus.read8(aligned + 1) as u32) << 8) | ((bus.read16(aligned + 2) as u32) << 16)
            }
            2 => {
                self.charge_read(bus, aligned + 2, AccessWidth::Half);
                bus.add_cycles(Self::unaligned_lane_cycles(aligned, 2));
                (bus.read16(aligned + 2) as u32) << 16
            }
            _ => {
                self.charge_read(bus, aligned + 3, AccessWidth::Byte);
                bus.add_cycles(Self::unaligned_lane_cycles(aligned, 2));
                (bus.read8(aligned + 3) as u32) << 24
            }
        }
    }

    /// Execute one instruction. Production hot path -- does NOT
    /// allocate an `InstructionRecord` (404 bytes returned by value
    /// per call accounted for ~10% of frontend frame time before
    /// this gate). Use [`Cpu::step_traced`] when you actually need
    /// the post-retirement register snapshot (parity tests, the
    /// debug-UI single-step button).
    #[inline]
    pub fn step(&mut self, bus: &mut Bus) -> Result<(), ExecutionError> {
        self.execute_one(bus).map(|_| ())
    }

    /// Execute up to `max_steps` instructions, stopping after the first one
    /// for which `stop_after` returns `true`, and return how many ran. The
    /// same as `step` in a loop with that check after each instruction, but
    /// one call for the whole run, so the interpreter's setup and teardown
    /// are paid once rather than per instruction. Stops early, with the
    /// count so far, on an execution error.
    #[inline]
    pub fn run(
        &mut self,
        bus: &mut Bus,
        max_steps: u64,
        mut stop_after: impl FnMut(&Bus) -> bool,
    ) -> (u64, Result<(), ExecutionError>) {
        let mut steps = 0;
        while steps < max_steps {
            if let Err(error) = self.execute_one(bus) {
                return (steps, Err(error));
            }
            steps += 1;
            if stop_after(bus) {
                break;
            }
        }
        (steps, Ok(()))
    }

    /// Execute one instruction and return a full register snapshot
    /// after retirement. Allocates 404 bytes per call.
    #[inline]
    pub fn step_traced(&mut self, bus: &mut Bus) -> Result<InstructionRecord, ExecutionError> {
        let outcome = self.execute_one(bus)?;
        let (cop2_data, cop2_ctl) = self.snapshot_cop2();
        Ok(InstructionRecord {
            // Trace records report bus cycles (same unit Redux's
            // `m_regs.cycle` uses). `self.tick` keeps counting
            // retired instructions for diagnostics.
            tick: bus.cycles(),
            pc: outcome.record_pc,
            instr: outcome.record_instr,
            gprs: self.gprs,
            cop2_data,
            cop2_ctl,
        })
    }

    /// Execute one instruction. Returns the data needed to build an
    /// `InstructionRecord` if the caller wants one -- `pc` and `instr`
    /// at retirement (these differ between the HLE-BIOS shortcut
    /// and the normal interpreter path). Both `step` and
    /// `step_traced` go through here, so the interpreter logic
    /// stays in one place.
    #[inline(always)]
    fn execute_one(&mut self, bus: &mut Bus) -> Result<ExecutedInstruction, ExecutionError> {
        if !bus.limits.tracks_pc() {
            return self.execute_one_inner(bus);
        }
        // Limit oracles with code ranges: free ranges freeze the clock for
        // the instruction; wait and profile ranges count what it charged.
        let pc = self.pc;
        bus.limits.begin_instruction(pc);
        let cycles_before = bus.cycles();
        let skipped_before = bus.limits.skipped_cycles;
        let profile_before = self.cpu_cycle_profile;
        let outcome = self.execute_one_outlined(bus);
        let charged = bus.cycles().saturating_sub(cycles_before);
        let skipped = bus.limits.skipped_cycles - skipped_before;
        if bus.limits.end_instruction(pc, charged, skipped) && self.cpu_cycle_profile_enabled {
            let d = self.cpu_cycle_profile.delta_since(profile_before);
            bus.limits.add_wait_stalls([
                d.icache_refill_stall_cycles,
                d.ram_load_stall_cycles,
                d.ram_store_stall_cycles,
                d.mmio_stall_cycles,
                d.gte_busy_stall_cycles,
                d.muldiv_interlock_stall_cycles,
            ]);
        }
        outcome
    }

    /// [`Self::execute_one_inner`] kept out of line, for the limit-oracle
    /// path, so only the hot loops carry an inlined copy.
    #[inline(never)]
    fn execute_one_outlined(
        &mut self,
        bus: &mut Bus,
    ) -> Result<ExecutedInstruction, ExecutionError> {
        self.execute_one_inner(bus)
    }

    #[inline(always)]
    fn execute_one_inner(&mut self, bus: &mut Bus) -> Result<ExecutedInstruction, ExecutionError> {
        // Diagnostic only -- track how many steps the IRQ pin was high.
        // We deliberately do NOT mirror the pin into `cop0[13].IP[2]`:
        // PCSX-Redux's CAUSE register is only written at exception
        // entry, never live-updated, so software `mfc0 v0, $13` reads
        // see only what the last exception stored. Mirroring the live
        // pin would surface as IP[2]=1 in syscall handlers' CAUSE
        // reads (e.g. step 19258368) and break parity.
        if bus.external_interrupt_pending() {
            self.irq_line_high_steps = self.irq_line_high_steps.saturating_add(1);
        }

        // HLE BIOS: when enabled (only for side-loaded EXEs, never
        // for parity), intercept jumps into the BIOS dispatcher
        // addresses 0xA0 / 0xB0 / 0xC0. The caller's trampoline has
        // already loaded `$t1` with the function number and parked
        // the return address in `$ra`.
        // Both HLE hooks live in the kernel's first 64 KiB (the A0/B0/C0
        // vectors, the exception-return stub, and trap words written there);
        // `hle_bios::dispatch` answers `None` everywhere else.
        if bus.hle_bios_enabled && memory::to_physical(self.pc) < 0x1_0000 {
            if self.hle_exception_active
                && memory::to_physical(self.pc)
                    == memory::to_physical(crate::hle_bios::EXCEPTION_RETURN_STUB)
            {
                let record_pc = self.pc;
                self.finish_hle_exception(bus);
                bus.tick(2);
                if self.cpu_cycle_profile_enabled {
                    self.cpu_cycle_profile.issue_cycles =
                        self.cpu_cycle_profile.issue_cycles.saturating_add(2);
                }
                self.tick += 1;
                return Ok(ExecutedInstruction {
                    record_pc,
                    record_instr: 0,
                });
            }
            let ra = self.gpr(31);
            let hle_cycles_before = bus.cycles();
            if let Some(out) = crate::hle_bios::dispatch(self.pc, bus, &mut self.gprs) {
                if out.outcome == crate::hle_bios::Outcome::Unimplemented && bus.hle_strict() {
                    return Err(ExecutionError::HleUnimplemented {
                        table: out.table.letter(),
                        func: out.func,
                        name: crate::hle_bios::function_name(out.table, out.func),
                        ra,
                    });
                }
                // A(44h) FlushCache normally executes the BIOS's isolated
                // tag-clear loop, and kernel-patch counterpatches rewrite
                // guest code. HLE skips the loop, so preserve the
                // architectural result here.
                if out.flush_icache {
                    self.instruction_cache.invalidate_all();
                }
                if let Some(v0) = out.v0 {
                    self.set_gpr(2, v0);
                }
                self.pc = out.next_pc;
                self.pending_pc = None;
                self.branch_delay_next = false;
                self.pending_load = None;
                self.committing_load = None;
                bus.tick(out.cycles);
                if self.cpu_cycle_profile_enabled {
                    self.cpu_cycle_profile.issue_cycles =
                        self.cpu_cycle_profile.issue_cycles.saturating_add(2);
                    self.cpu_cycle_profile.other_stall_cycles =
                        self.cpu_cycle_profile.other_stall_cycles.saturating_add(
                            bus.cycles()
                                .saturating_sub(hle_cycles_before.saturating_add(2)),
                        );
                }
                self.tick += 1;
                let record_pc = self.pc;
                // A waiting HLE call (WaitEvent, gpu_sync) is retried from
                // the same PC; take pending interrupts between attempts, as
                // the retail wait loops do, or nothing could end the wait.
                if out.retry {
                    if self.cpu_cycle_profile_enabled {
                        self.cpu_wait_profile.hle_wait_cycles = self
                            .cpu_wait_profile
                            .hle_wait_cycles
                            .saturating_add(bus.cycles().saturating_sub(hle_cycles_before));
                    }
                    bus.drain_scheduler_events_post_op();
                    if self.should_take_interrupt(bus) {
                        self.enter_exception(ExceptionCode::Interrupt, self.pc, false);
                        self.pc = self
                            .pending_exception_pc
                            .take()
                            .expect("enter_exception staged a vector");
                    }
                }
                return Ok(ExecutedInstruction {
                    record_pc,
                    record_instr: 0,
                });
            }
        }

        let pc_before = self.pc;
        let gte_irq_taken_after = self.gte_irq_hazard(pc_before, bus);
        if pc_before & 3 != 0 || !Self::instruction_address_is_executable(pc_before) {
            // The PS1 scratchpad is the repurposed data cache and cannot
            // supply instructions. Reject it (and every other unmapped
            // instruction address) before consulting the I-cache, otherwise
            // an emulator-only scratchpad code kernel can appear to work.
            let in_delay_slot = self.pending_pc.is_some() || self.branch_delay_next;
            self.branch_delay_next = false;
            self.cop0[8] = pc_before;
            let code = if pc_before & 3 != 0 {
                ExceptionCode::AddressErrorLoad
            } else {
                ExceptionCode::InstructionBusError
            };
            self.enter_exception(code, pc_before, in_delay_slot);
            self.stage_hle_unresolved_exception(bus);
            self.pending_pc = None;
            self.branch_delay_next = false;
            if let Some((reg, value)) = self.pending_load.take() {
                let index = (reg & 31) as usize;
                if index != 0 {
                    self.gprs[index] = value;
                }
            }
            self.committing_load = None;
            if self.instruction_cache_event_profile_enabled {
                self.last_instruction_cache_refill = None;
            }
            self.pc = self
                .pending_exception_pc
                .take()
                .expect("instruction fetch fault staged an exception vector");
            bus.tick(2);
            return Ok(ExecutedInstruction {
                record_pc: pc_before,
                record_instr: 0,
            });
        }
        let instr = self.fetch_instruction(pc_before, bus);

        // BIAS charged BEFORE the opcode runs -- matches Redux's
        // `m_regs.cycle += BIAS` at psxinterpreter.cc:1631, which is
        // *ahead* of the opcode dispatch. Any MMIO reads the opcode
        // issues (Timer counters, GPUSTAT, CDROM status) therefore
        // observe the POST-BIAS cycle. Placing the tick after the
        // opcode would have them observe the pre-BIAS cycle, drifting
        // Timer 1's counter behind Redux's by ~2 cycles per memory
        // access -- which showed as a 34-count offset at step
        // 19,472,447's Timer 1 read.
        let issue_cycles = if self.hides_in_load_shadow(instr, bus) {
            0
        } else {
            cycle_cost(instr)
        };
        bus.tick(issue_cycles);
        if self.cpu_cycle_profile_enabled {
            self.cpu_cycle_profile.issue_cycles = self
                .cpu_cycle_profile
                .issue_cycles
                .saturating_add(issue_cycles as u64);
        }

        // If the *previous* instruction was a branch, the current
        // instruction is its delay slot -- after retiring, PC goes to
        // the branch target instead of the usual `pc + 4`.
        let branch_after_this = self.pending_pc.take();
        let in_delay_slot = branch_after_this.is_some();
        // Architectural delay slot, taken branch or not: what Cause.BD and
        // EPC report for a fault here. `in_delay_slot` keeps meaning "a
        // taken branch redirects after this", which also gates the
        // Redux-style interrupt check below.
        let in_branch_delay = std::mem::take(&mut self.branch_delay_next) || in_delay_slot;
        self.executing_in_branch_delay = in_branch_delay;

        if self.instruction_class_profile_enabled {
            self.profile_instruction_class(instr, in_delay_slot);
        }

        // The load delay queued by the *previous* instruction is held
        // in `committing_load` for the duration of `execute`. The
        // delay slot itself sees the pre-load value; any `set_gpr`
        // in execute that targets the load's register will squash
        // the commit (R3000 writeback-port collision -- the
        // non-load write wins). Any new load this instruction issues
        // goes into `pending_load` and fires on the *next* call.
        self.committing_load = self.pending_load.take();

        let profiled_access = self
            .cpu_cycle_profile_enabled
            .then(|| self.profiled_data_access(instr))
            .flatten();
        let execute_cycles_before = bus.cycles();
        self.execute(instr, pc_before, in_branch_delay, bus)?;
        self.executing_in_branch_delay = false;
        self.branch_delay_next = is_branch_or_jump(instr) && self.pending_exception_pc.is_none();
        if bus.take_ram_load_from_cached_code() {
            // Every load opcode names its destination in rt.
            self.load_shadow = Some(LoadShadow {
                position: 0,
                register: ((instr >> 16) & 0x1F) as u8,
            });
        }
        if self.cpu_cycle_profile_enabled {
            self.record_execution_stalls(
                instr,
                profiled_access,
                bus.cycles().saturating_sub(execute_cycles_before),
            );
        }

        if let Some((reg, value)) = self.committing_load.take() {
            let i = (reg & 31) as usize;
            if i != 0 {
                self.gprs[i] = value;
            }
        }

        // Exception takes priority: it cancels any pending branch and
        // redirects PC to the exception vector.
        self.pc = if let Some(exc_pc) = self.pending_exception_pc.take() {
            exc_pc
        } else {
            match branch_after_this {
                Some(target) => target,
                None => self.pc.wrapping_add(4),
            }
        };
        if self.cpu_cycle_profile_enabled {
            self.observe_wait_loop(instr, pc_before, branch_after_this, profiled_access);
        }

        // Hardware-IRQ check, end-of-step. Mirrors Redux's `branchTest`,
        // which only runs when the just-retired instruction was a delay
        // slot (`if (m_inDelaySlot) ... branchTest()`). Doing it after
        // the instruction (rather than before the next) means the trace
        // record still shows the regular instruction at this step, and
        // the interrupt-vector PC shows up at the *next* step -- exactly
        // how Redux's trace reads.
        //
        // But BEFORE that IRQ check: drain scheduler events against
        // the POST-opcode cycle count. Otherwise peripheral events
        // whose deadline falls during this instruction's memory-
        // access cycles (BIAS passed them, but `add_cycles` did not)
        // don't raise their IRQ bit in time for this step's check,
        // and the IRQ only dispatches at the NEXT delay slot -- a
        // consistent 5-6 instruction delay that compounds into the
        // Crash 900M -6% drift. Redux's `branchTest` calls
        // `counters->update()` inline for the same reason.
        if in_delay_slot {
            self.apply_redux_bios_kernel_call_intercept();
            bus.drain_scheduler_events_post_op();
        }
        if gte_irq_taken_after && self.pending_exception_pc.is_none() {
            // The GTE command has run; EPC stays on it, so a handler that
            // returns to EPC runs it a second time (hwtest v1.24 0xC9).
            self.should_take_interrupt_steps = self.should_take_interrupt_steps.saturating_add(1);
            self.enter_exception(ExceptionCode::Interrupt, pc_before, false);
            self.pc = self
                .pending_exception_pc
                .take()
                .expect("enter_exception staged a vector");
        } else if in_delay_slot && self.should_take_interrupt(bus) {
            self.should_take_interrupt_steps = self.should_take_interrupt_steps.saturating_add(1);
            // Redux passes `bd=0` to `exception(0x400, 0)`: the IRQ
            // is taken cleanly between instructions, not in a delay
            // slot of its own.
            self.enter_exception(ExceptionCode::Interrupt, self.pc, false);
            self.pc = self
                .pending_exception_pc
                .take()
                .expect("enter_exception staged a vector");
        }

        self.tick += 1;
        Ok(ExecutedInstruction {
            record_pc: pc_before,
            record_instr: instr,
        })
    }

    /// PCSX-Redux runs a small kernel-call intercept immediately after
    /// a branch-delay slot lands on the BIOS A0/B0 trampolines, before
    /// its debug trace record is emitted. It mostly mirrors TTY output
    /// to the host, but A(03h)/B(35h) `write(fd=1, ptr, size)` also
    /// stores `size` in `$v0`. Keeping that visible side effect here
    /// preserves lockstep parity while still letting the real BIOS code
    /// run on the following instructions.
    fn apply_redux_bios_kernel_call_intercept(&mut self) {
        let base = (self.pc >> 20) & 0x0FFC;
        if !matches!(base, 0x000 | 0x800 | 0xA00) {
            return;
        }
        let pc = self.pc & ((memory::ram::SIZE as u32) - 1);
        let call = self.gpr(9) & 0xFF;
        let is_stdout_write = (pc == 0xA0 && call == 0x03) || (pc == 0xB0 && call == 0x35);
        if is_stdout_write && self.gpr(4) == 1 {
            self.set_gpr(2, self.gpr(6));
        }
    }

    /// Snapshot all 64 GTE registers using the software-visible
    /// `MFC2`/`CFC2` accessors so the recorded values match what
    /// Redux's `regs.CP2D.r` / `regs.CP2C.r` expose. Pure read; no
    /// side effects.
    #[cfg(feature = "trace-cop2")]
    fn snapshot_cop2(&self) -> ([u32; 32], [u32; 32]) {
        let mut data = [0u32; 32];
        let mut ctl = [0u32; 32];
        for i in 0..32u8 {
            data[i as usize] = self.cop2.read_data(i);
            ctl[i as usize] = self.cop2.read_control(i);
        }
        (data, ctl)
    }

    #[cfg(not(feature = "trace-cop2"))]
    #[inline(always)]
    fn snapshot_cop2(&self) -> ([u32; 32], [u32; 32]) {
        ([0u32; 32], [0u32; 32])
    }

    /// Decode and execute a single instruction. Does not advance PC
    /// (the caller is responsible, to keep branch-delay handling
    /// localised to the branch opcodes that will add it later).
    ///
    /// `in_delay_slot` is `true` when the current instruction sits in
    /// the delay slot of a taken branch -- the SYSCALL/BREAK handlers
    /// need it to set the BD bit in `CAUSE` correctly.
    fn execute(
        &mut self,
        instr: u32,
        pc: u32,
        in_delay_slot: bool,
        bus: &mut Bus,
    ) -> Result<(), ExecutionError> {
        let opcode = ((instr >> 26) & 0x3F) as u8;
        match opcode {
            0x00 => self.dispatch_special(instr, pc, in_delay_slot, bus),
            0x01 => self.dispatch_regimm(instr, pc),
            0x02 => self.op_j(instr, pc),
            0x03 => self.op_jal(instr, pc),
            0x04 => self.op_beq(instr, pc),
            0x05 => self.op_bne(instr, pc),
            0x06 => self.op_blez(instr, pc),
            0x07 => self.op_bgtz(instr, pc),
            0x08 => self.op_addi(instr),
            0x09 => self.op_addiu(instr),
            0x0A => self.op_slti(instr),
            0x0B => self.op_sltiu(instr),
            0x0C => self.op_andi(instr),
            0x0D => self.op_ori(instr),
            0x0E => self.op_xori(instr),
            0x0F => self.op_lui(instr),
            0x10 => {
                if self.cop_instruction_usable(0) {
                    self.dispatch_cop0(instr)
                } else {
                    self.raise_coprocessor_unusable(0, pc, in_delay_slot, bus)
                }
            }
            0x11 => self.dispatch_absent_coprocessor(1, pc, in_delay_slot, bus),
            0x12 => {
                if self.cop_instruction_usable(2) {
                    self.dispatch_cop2(instr, bus)
                } else {
                    self.raise_coprocessor_unusable(2, pc, in_delay_slot, bus)
                }
            }
            0x13 => self.dispatch_absent_coprocessor(3, pc, in_delay_slot, bus),
            0x20 => self.op_lb(instr, bus),
            0x21 => self.op_lh(instr, bus),
            0x22 => self.op_lwl(instr, bus),
            0x23 => self.op_lw(instr, bus),
            0x24 => self.op_lbu(instr, bus),
            0x25 => self.op_lhu(instr, bus),
            0x26 => self.op_lwr(instr, bus),
            0x28 => self.op_sb(instr, bus),
            0x29 => self.op_sh(instr, bus),
            0x2A => self.op_swl(instr, bus),
            0x2B => self.op_sw(instr, bus),
            0x2E => self.op_swr(instr, bus),
            0x30 => self.dispatch_absent_cop_memory(0, pc, in_delay_slot, bus),
            0x31 => self.dispatch_absent_cop_memory(1, pc, in_delay_slot, bus),
            0x32 => {
                if self.cop_memory_usable(2) {
                    self.op_lwc2(instr, bus)
                } else {
                    self.raise_coprocessor_unusable(2, pc, in_delay_slot, bus)
                }
            }
            0x33 => self.dispatch_absent_cop_memory(3, pc, in_delay_slot, bus),
            0x38 => self.dispatch_absent_cop_memory(0, pc, in_delay_slot, bus),
            0x39 => self.dispatch_absent_cop_memory(1, pc, in_delay_slot, bus),
            0x3A => {
                if self.cop_memory_usable(2) {
                    self.op_swc2(instr, bus)
                } else {
                    self.raise_coprocessor_unusable(2, pc, in_delay_slot, bus)
                }
            }
            0x3B => self.dispatch_absent_cop_memory(3, pc, in_delay_slot, bus),
            _ => Err(ExecutionError::Unimplemented { opcode, pc, instr }),
        }
    }

    /// COP0 instructions remain available in kernel mode even when SR.CU0 is
    /// clear. COP1/2/3 instructions are gated directly by their CU bit.
    #[inline]
    fn cop_instruction_usable(&self, cop: u8) -> bool {
        let cu = self.cop0[12] & (1 << (28 + cop as u32)) != 0;
        if cop == 0 {
            let user_mode = self.cop0[12] & (1 << 1) != 0;
            !user_mode || cu
        } else {
            cu
        }
    }

    /// The LWCz/SWCz primary opcodes use SR.CUz even for COP0 while the CPU
    /// is in kernel mode. This distinction is covered by the public
    /// `cpu/cop` silicon suite (`MFC0` works with CU0 clear; `SWC0` traps).
    #[inline]
    fn cop_memory_usable(&self, cop: u8) -> bool {
        self.cop0[12] & (1 << (28 + cop as u32)) != 0
    }

    /// The PS1 has no COP1 or COP3. With the corresponding CU bit enabled,
    /// their instruction encodings are accepted as inert operations rather
    /// than Reserved Instruction traps. With CU clear they raise CpU and
    /// report the selected coprocessor in CAUSE.CE.
    fn dispatch_absent_coprocessor(
        &mut self,
        cop: u8,
        pc: u32,
        in_delay_slot: bool,
        bus: &mut Bus,
    ) -> Result<(), ExecutionError> {
        if self.cop_instruction_usable(cop) {
            Ok(())
        } else {
            self.raise_coprocessor_unusable(cop, pc, in_delay_slot, bus)
        }
    }

    /// LWC/SWC for an absent coprocessor has no data-path side effect when
    /// enabled, but still obeys the CU gate. COP0 uses this same behavior for
    /// the otherwise unsupported LWC0/SWC0 encodings.
    fn dispatch_absent_cop_memory(
        &mut self,
        cop: u8,
        pc: u32,
        in_delay_slot: bool,
        bus: &mut Bus,
    ) -> Result<(), ExecutionError> {
        if self.cop_memory_usable(cop) {
            Ok(())
        } else {
            self.raise_coprocessor_unusable(cop, pc, in_delay_slot, bus)
        }
    }

    fn raise_coprocessor_unusable(
        &mut self,
        cop: u8,
        pc: u32,
        in_delay_slot: bool,
        bus: &mut Bus,
    ) -> Result<(), ExecutionError> {
        self.enter_exception(ExceptionCode::CoprocessorUnusable, pc, in_delay_slot);
        self.cop0[13] |= ((cop as u32) & 3) << 28;
        self.stage_hle_unresolved_exception(bus);
        Ok(())
    }

    /// Dispatch table for COP0 instructions (primary opcode `0x10`).
    /// The sub-operation lives in bits 25..=21.
    fn dispatch_cop0(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let cop_op = ((instr >> 21) & 0x1F) as u8;
        match cop_op {
            0x00 => self.op_mfc0(instr),
            0x04 => self.op_mtc0(instr),
            0x10 => self.op_rfe(),
            // The R3000A accepts the unused COP0 operation fields as inert
            // encodings. `cpu/cop::testCop0InvalidOpcode` observes no trap.
            _ => Ok(()),
        }
    }

    /// Dispatch table for COP2 (GTE) instructions (primary opcode
    /// `0x12`). Bit 25 selects: when clear, the upper 5 bits of bits
    /// 25..=21 pick MFC2/CFC2/MTC2/CTC2; when set, the bottom 25 bits
    /// encode a GTE function (RTPS, NCLIP, MVMVA, …).
    fn dispatch_cop2(&mut self, instr: u32, bus: &mut Bus) -> Result<(), ExecutionError> {
        if instr & (1 << 25) != 0 {
            // The GTE is not pipelined: issuing a command while a previous one
            // is still in flight stalls until it completes.
            self.gte_sync(bus);
            // MAC0 has result-read latency: snapshot the now-settled prior
            // MAC0 so a too-soon read returns it (see gte_mac0_* docs).
            self.gte_mac0_stale = self.cop2.read_data(24);
            // FREELOOK (debug): compose the camera delta onto the view
            // transform for RTPS/RTPT only; restored right after the op.
            let fl = self.freelook;
            let freelook_saved = freelook::apply_for_op(&mut self.cop2, &fl, instr);
            // HWB-010 hazard: a transform reading V0 inside the MTC2 VXY0
            // commit window computes its first row with the previous V0.x.
            // MVMVA is console-confirmed; RTPS is included in this env-gated
            // diagnostic path to reproduce the SDK projection schedule.
            let opcode = instr & 0x3F;
            if self.gte_v0x_hazard
                && (opcode == 0x01 || (opcode == 0x12 && (instr >> 15) & 3 == 0))
                && self.tick.wrapping_sub(self.gte_v0xy_write_tick) <= self.gte_v0x_window
            {
                self.cop2.execute_with_stale_v0x(instr, self.gte_prev_v0x);
            } else if (instr & 0x3F) == 0x0C && self.tick.wrapping_sub(self.gte_ir3_write_tick) <= 1
            {
                self.cop2
                    .execute_op_with_stale_ir3(instr, self.gte_prev_ir3 as i16);
            } else {
                self.cop2.execute(instr);
            }
            if let Some(saved) = freelook_saved {
                freelook::restore(&mut self.cop2, &saved);
            }
            // `command_cycles` is the documented internal execution time.
            // The result becomes available on the following CPU cycle: the
            // PAL command-throughput sweep is one cycle per interlock slower
            // than a deadline ending on the final internal cycle.
            self.gte_busy_until = bus.cycles() + Gte::command_cycles(instr) as u64 + 1;
            let recent_cop2_write = self.tick.wrapping_sub(self.gte_last_write_tick) <= 2;
            if (instr & 0x3F) == 0x06 && self.tick.wrapping_sub(self.gte_sxy2_write_tick) <= 2 {
                // NCLIP's two positive SXY2 products are its early phase.
                // SXY2.x is not forwarded from the immediately preceding
                // MTC2. SXY2.y is forwarded only by the packed (two-cycle)
                // SXY0/SXY1/SXY2 write cadence emitted by the hot path, and
                // only while an RTPT result pipeline remains active.
                let sxy0 = self.cop2.read_data(12);
                let sxy1 = self.cop2.read_data(13);
                let sxy2 = self.cop2.read_data(14);
                let x0 = sxy0 as u16 as i16 as i64;
                let y0 = (sxy0 >> 16) as u16 as i16 as i64;
                let y1 = (sxy1 >> 16) as u16 as i16 as i64;
                let x1 = sxy1 as u16 as i16 as i64;
                let y2 = (sxy2 >> 16) as u16 as i16 as i64;
                let x2 = sxy2 as u16 as i16 as i64;
                let old_x2 = self.gte_prev_sxy2_x as i64;
                let sxy01_gap = self.gte_sxy_write_tick[1].wrapping_sub(self.gte_sxy_write_tick[0]);
                let sxy12_gap = self.gte_sxy_write_tick[2].wrapping_sub(self.gte_sxy_write_tick[1]);
                let history_active = self.gte_nclip_rtpt_history
                    && self.tick.wrapping_sub(self.gte_nclip_history_tick) <= 4096;
                let spaced_sxy_triplet = history_active && sxy01_gap == 3 && sxy12_gap == 3;
                let early_y2 = if spaced_sxy_triplet
                    || (!history_active && !self.gte_prev_sxy2_from_transform)
                {
                    self.gte_prev_sxy2_y as i64
                } else {
                    y2
                };
                let mac0 = x0 * y1 + x1 * early_y2 + old_x2 * y0 - x0 * y2 - x1 * y0 - x2 * y1;
                self.cop2.write_data(24, mac0 as u32);
                self.gte_mac0_stale = mac0 as u32;
                self.gte_mac0_ready_at = self.tick + MAC0_RESULT_LATENCY;
            } else if recent_cop2_write {
                self.gte_mac0_ready_at = self.tick + MAC0_RESULT_LATENCY;
            } else {
                self.gte_mac0_ready_at = self.tick; // result is immediately readable
            }
            if (24..=27).contains(&self.gte_mac_write_reg)
                && self.tick.wrapping_sub(self.gte_mac_write_tick) <= 1
            {
                self.cop2
                    .write_data(self.gte_mac_write_reg, self.gte_mac_write_value);
                if self.gte_mac_write_reg == 24 {
                    self.gte_mac0_stale = self.gte_mac_write_value;
                }
            }
            match instr & 0x3F {
                // KNOWN GAP (conformance 0x8b): silicon computes the full
                // cross in BOTH phases of the controlled scene-C replica,
                // while this model's phases classify differently (spaced
                // first-materialization vs packed register-reuse cadence)
                // and 0x8b fails. Restricting establishment to RTPT fixes
                // 0x8b's invariant but breaks the small-value settle cases
                // 0x74-0x78 through the same substitution arms; no simple
                // write-side-effect model covers both (see the SXY dump
                // notes in hardware-tests). The real fix is modeling the
                // positive-winding anomaly itself.
                0x30 | 0x06 => {
                    self.gte_nclip_rtpt_history = true;
                    self.gte_nclip_history_tick = self.tick;
                }
                _ => self.gte_nclip_rtpt_history = false,
            }
            if matches!(instr & 0x3F, 0x01 | 0x30) {
                self.gte_sxy2_from_transform = true;
            }
            return Ok(());
        }
        let cop_op = ((instr >> 21) & 0x1F) as u8;
        match cop_op {
            // MFC2/CFC2 wait for a running command. hwtest v1.23 records
            // 0x130-0x134 on a launch PAL console: RTPS, a read, then 20 nops
            // costs 37 clocks a turn whichever of SXY2, MAC0, MAC1 or IR1 is
            // read, against 22 when the read comes after RTPS has finished:
            // the read waits exactly as long as another command would.
            // What the read then RETURNS is a separate matter: MAC0 and LZCR
            // can still be stale, by instruction count, as the conformance
            // cases pinned on silicon require (`gte_read_data_latency`).
            0x00 => {
                self.gte_sync(bus);
                self.op_mfc2(instr, bus)
            }
            0x02 => {
                self.gte_sync(bus);
                self.op_cfc2(instr, bus)
            }
            0x04 => self.op_mtc2(instr, bus),
            0x06 => self.op_ctc2(instr, bus),
            // Unassigned COP2 move/control fields are inert on silicon.
            _ => Ok(()),
        }
    }

    /// Load scheduling (LSI's LDSCH): the wait of a RAM load overlaps the
    /// instructions behind it, as long as they need neither the bus nor the
    /// value being loaded. Returns whether `instr` costs nothing for it.
    ///
    /// hwtest v1.23 records 0x125-0x128 with v1.22's 0x74/0xC8/0xCE, 64 turns
    /// of a load followed by k `addiu` on another register, clocks a turn:
    /// 7, 8, 9, 9, 9, 9 (k = 6), 11 (k = 8). The first two instructions behind
    /// a load pay in full; the next four are free; after that the wait is
    /// over. The pcsx-redux load-timings test describes the same shape.
    fn hides_in_load_shadow(&mut self, instr: u32, bus: &Bus) -> bool {
        let Some(shadow) = self.load_shadow.as_mut() else {
            return false;
        };
        shadow.position += 1;
        let opcode = instr >> 26;
        let touches_bus = opcode >= 0x20;
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        // rt is a destination in the immediate formats, so this errs towards
        // ending the shadow early, never towards hiding a dependent read.
        let reads_loaded = shadow.register != 0 && (rs == shadow.register || rt == shadow.register);
        if touches_bus || reads_loaded || !bus.last_fetch_was_a_cache_hit() || shadow.position > 6 {
            self.load_shadow = None;
            return false;
        }
        shadow.position > 2
    }

    /// Stall the CPU until any in-flight GTE command has completed. Called
    /// before reading a GTE result register (MFC2/CFC2/SWC2) or issuing a new
    /// command, so the modelled frametime charges the GTE latency the CPU
    /// would actually wait on -- and rewards interleaving independent work
    /// between an op and its result read.
    #[inline]
    fn gte_sync(&mut self, bus: &mut Bus) {
        if bus.limit(crate::limits::GTE) {
            return;
        }
        stall_to(bus, self.gte_busy_until);
    }

    /// `MFC2 rt, rd` -- move from COP2 data register `rd` into GPR
    /// `rt`. Like LW, this respects the one-slot load delay so the
    /// next instruction sees the *old* register value.
    fn op_mfc2(&mut self, instr: u32, bus: &Bus) -> Result<(), ExecutionError> {
        let rt = ((instr >> 16) & 0x1F) as u8;
        let rd = ((instr >> 11) & 0x1F) as u8;
        let value = self.gte_read_data_latency(bus, rd);
        if rt != 0 {
            self.pending_load = Some((rt, value));
        }
        Ok(())
    }

    /// Read a GTE data register, applying the MAC0 / LZCR result-read
    /// latency. Every other register reads the live (eagerly computed)
    /// value, matching hardware where MAC1-3 / SXY / SZ settle in time
    /// for a back-to-back read but MAC0 / LZCR do not.
    fn gte_read_data_latency(&self, _bus: &Bus, rd: u8) -> u32 {
        // PSOXIDE_TRACE_STALE=1: log the first stale-serving reads (PC, reg,
        // tick distance) to identify exactly which game code trips the model.
        #[allow(clippy::collapsible_if)]
        if crate::env_flag!("PSOXIDE_TRACE_STALE") {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let stale = match rd {
                24 => self.tick < self.gte_mac0_ready_at,
                31 => self.tick < self.gte_lzcr_ready_at,
                _ => false,
            };
            if stale && N.fetch_add(1, Ordering::Relaxed) < 24 {
                let ready = if rd == 24 {
                    self.gte_mac0_ready_at
                } else {
                    self.gte_lzcr_ready_at
                };
                eprintln!(
                    "[stale] pc={:#010x} reg={} tick={} ready_at={} (dist {})",
                    self.pc,
                    rd,
                    self.tick,
                    ready,
                    ready - self.tick
                );
            }
        }
        // Diagnostic bypass: PSOXIDE_NO_GTE_READ_LATENCY=1 returns live
        // values (pre-latency-model behavior) to A/B the model against
        // commercial 3D culling (missing-model investigations).
        if self.gte_read_latency_bypass {
            return self.cop2.read_data(rd);
        }
        match rd {
            24 if self.tick < self.gte_mac0_ready_at => self.gte_mac0_stale,
            31 if self.tick < self.gte_lzcr_ready_at => self.gte_lzcr_stale,
            _ => self.cop2.read_data(rd),
        }
    }

    /// `CFC2 rt, rd` -- same as MFC2 but reads a control register.
    fn op_cfc2(&mut self, instr: u32, _bus: &Bus) -> Result<(), ExecutionError> {
        let rt = ((instr >> 16) & 0x1F) as u8;
        let rd = ((instr >> 11) & 0x1F) as u8;
        let value = self.cop2.read_control(rd);
        if rt != 0 {
            self.pending_load = Some((rt, value));
        }
        Ok(())
    }

    /// `MTC2 rt, rd` -- move from GPR `rt` to COP2 data register `rd`.
    /// Coprocessor writes commit immediately (no delay slot). Writing
    /// LZCS (reg 30) recomputes LZCR (reg 31), which has the same
    /// result-read latency as MAC0: a back-to-back LZCR read returns the
    /// prior count (the off-by-one seen on real hardware).
    fn op_mtc2(&mut self, instr: u32, _bus: &Bus) -> Result<(), ExecutionError> {
        let rt = ((instr >> 16) & 0x1F) as u8;
        let rd = ((instr >> 11) & 0x1F) as u8;
        self.gte_last_write_tick = self.tick;
        if rd == 30 {
            self.gte_lzcr_stale = self.cop2.read_data(31);
            self.gte_lzcr_ready_at = self.tick + LZCR_RESULT_LATENCY;
        }
        if rd == 0 {
            // VXY0 write: remember the V0.x being overwritten and when.
            // An MVMVA issued inside the commit window computes its MAC1
            // phase with this stale value (HWB-010 hazard model).
            self.gte_prev_v0x = self.cop2.read_data(0) as u16 as i16;
            self.gte_v0xy_write_tick = self.tick;
        }
        if (12..=14).contains(&rd) {
            self.gte_sxy_write_tick[(rd - 12) as usize] = self.tick;
        }
        if rd == 14 {
            let previous = self.cop2.read_data(14);
            self.gte_prev_sxy2_x = previous as u16 as i16;
            self.gte_prev_sxy2_y = (previous >> 16) as u16 as i16;
            self.gte_prev_sxy2_from_transform = self.gte_sxy2_from_transform;
            self.gte_sxy2_from_transform = false;
            self.gte_sxy2_write_tick = self.tick;
        }
        if rd == 11 {
            // The forwarding latch retains the previous CPU-supplied IR3,
            // not an intervening command's architectural IR3 result. In the
            // burned sequence SQR changes visible IR3 to 466, but OP's first
            // row still consumes the earlier MTC2 value 0x0567.
            self.gte_prev_ir3 = self.gte_last_mtc2_ir3;
            self.gte_last_mtc2_ir3 = self.gpr(rt);
            self.gte_ir3_write_tick = self.tick;
        }
        if (24..=27).contains(&rd) {
            self.gte_mac_write_tick = self.tick;
            self.gte_mac_write_reg = rd;
            self.gte_mac_write_value = self.gpr(rt);
        }
        self.cop2.write_data(rd, self.gpr(rt));
        Ok(())
    }

    /// `CTC2 rt, rd` -- same as MTC2 but writes a control register.
    /// Writes commit immediately: the staged-settle and drop-during-exec
    /// CTC2 hazard models that once lived here were refuted on silicon:
    /// every RT settle/drop sweep case passed.
    fn op_ctc2(&mut self, instr: u32, _bus: &Bus) -> Result<(), ExecutionError> {
        let rt = ((instr >> 16) & 0x1F) as u8;
        let rd = ((instr >> 11) & 0x1F) as u8;
        self.gte_last_write_tick = self.tick;
        self.cop2.write_control(rd, self.gpr(rt));
        Ok(())
    }

    /// `LWC2 rt, offset(rs)` -- load 32-bit word from memory into COP2
    /// data register `rt`. No GPR is touched, so no load-delay slot.
    fn op_lwc2(&mut self, instr: u32, bus: &mut Bus) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let offset = (instr as i16) as i32 as u32;
        let addr = self.gpr(rs).wrapping_add(offset);
        self.charge_read(bus, addr, AccessWidth::Word);
        let value = self.read_word(addr, bus);
        self.cop2.write_data(rt, value);
        Ok(())
    }

    /// `SWC2 rt, offset(rs)` -- store COP2 data register `rt` to memory.
    fn op_swc2(&mut self, instr: u32, bus: &mut Bus) -> Result<(), ExecutionError> {
        if self.cache_isolated() {
            return Ok(());
        }
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let offset = (instr as i16) as i32 as u32;
        let addr = self.gpr(rs).wrapping_add(offset);
        // SWC2 stores a GTE register, so it reads a result -- subject to the
        // same MAC0/LZCR read latency as MFC2 (no stall on real hardware).
        let value = self.gte_read_data_latency(bus, rt);
        bus.cpu_write32(addr, value);
        Ok(())
    }

    fn dispatch_regimm(&mut self, instr: u32, pc: u32) -> Result<(), ExecutionError> {
        let rt = ((instr >> 16) & 0x1F) as u8;
        match rt {
            0x00 => self.op_bltz(instr, pc),
            0x01 => self.op_bgez(instr, pc),
            0x10 => self.op_bltzal(instr, pc),
            0x11 => self.op_bgezal(instr, pc),
            _ => Err(ExecutionError::Unimplemented {
                opcode: 0x01,
                pc,
                instr,
            }),
        }
    }

    /// `MTC0 rt, rd` -- move from CPU GPR `rt` to COP0 register `rd`.
    fn op_mtc0(&mut self, instr: u32) -> Result<(), ExecutionError> {
        // Opt-in trace: catch anything setting SR.BEV (bit 22) mid-run --
        // exceptions then vector through the ROM handler and fall off the
        // mapped BIOS, which presents as a wild pc at 0xbfc80000.
        if crate::env_flag!("PSOXIDE_TRACE_EXC") {
            let rd = (instr >> 11) & 31;
            let rt = ((instr >> 16) & 31) as u8;
            let v = self.gpr(rt);
            if rd == 12 && v & (1 << 22) != 0 {
                eprintln!(
                    "[cpu] MTC0 SR with BEV set: value=0x{v:08x} pc=0x{:08x}",
                    self.pc()
                );
            }
        }

        let rt = ((instr >> 16) & 0x1F) as u8;
        let rd = ((instr >> 11) & 0x1F) as usize;
        self.cop0[rd] = self.gpr(rt);
        Ok(())
    }

    /// Dispatch table for primary-opcode `SPECIAL` (0x00), selected by
    /// the 6-bit function field in bits 5..=0.
    fn dispatch_special(
        &mut self,
        instr: u32,
        pc: u32,
        in_delay_slot: bool,
        bus: &mut Bus,
    ) -> Result<(), ExecutionError> {
        let funct = (instr & 0x3F) as u8;
        match funct {
            0x00 => self.op_sll(instr),
            0x02 => self.op_srl(instr),
            0x03 => self.op_sra(instr),
            0x04 => self.op_sllv(instr),
            0x06 => self.op_srlv(instr),
            0x07 => self.op_srav(instr),
            0x08 => self.op_jr(instr, pc),
            0x09 => self.op_jalr(instr, pc),
            0x0C => self.op_syscall(pc, in_delay_slot),
            0x0D => self.op_break(pc, in_delay_slot),
            // MFHI/MFLO read the multiply/divide unit; stall until an
            // in-flight MULT/DIV has retired (the R3000A HI/LO interlock).
            0x10 => {
                if !bus.limit(crate::limits::MULDIV) {
                    hilo_stall_to(bus, self.hilo_busy_until);
                }
                self.op_mfhi(instr)
            }
            0x11 => self.op_mthi(instr),
            0x12 => {
                if !bus.limit(crate::limits::MULDIV) {
                    hilo_stall_to(bus, self.hilo_busy_until);
                }
                self.op_mflo(instr)
            }
            0x13 => self.op_mtlo(instr),
            // MULT/MULTU/DIV/DIVU run asynchronously: charge their latency so
            // a dependent MFHI/MFLO stalls and interleaved work hides it.
            0x18 => {
                let cycles = mult_cycles(self.gpr(((instr >> 21) & 0x1F) as u8), true);
                let r = self.op_mult(instr);
                self.hilo_busy_until = bus.cycles() + cycles as u64 + 1;
                r
            }
            0x19 => {
                let cycles = mult_cycles(self.gpr(((instr >> 21) & 0x1F) as u8), false);
                let r = self.op_multu(instr);
                self.hilo_busy_until = bus.cycles() + cycles as u64 + 1;
                r
            }
            0x1A => {
                let r = self.op_div(instr);
                self.hilo_busy_until = bus.cycles() + DIV_CYCLES as u64 + 1;
                r
            }
            0x1B => {
                let r = self.op_divu(instr);
                self.hilo_busy_until = bus.cycles() + DIV_CYCLES as u64 + 1;
                r
            }
            0x20 => self.op_add(instr),
            0x21 => self.op_addu(instr),
            0x22 => self.op_sub(instr),
            0x23 => self.op_subu(instr),
            0x24 => self.op_and(instr),
            0x25 => self.op_or(instr),
            0x26 => self.op_xor(instr),
            0x27 => self.op_nor(instr),
            0x2A => self.op_slt(instr),
            0x2B => self.op_sltu(instr),
            _ => Err(ExecutionError::UnimplementedSpecial { funct, pc, instr }),
        }
    }

    /// `LUI rt, imm16` -- load upper immediate: `rt = imm16 << 16`.
    fn op_lui(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rt = ((instr >> 16) & 0x1F) as u8;
        let imm = instr & 0xFFFF;
        self.set_gpr(rt, imm << 16);
        Ok(())
    }

    /// `ADDI rt, rs, imm16` -- add sign-extended immediate, signed.
    ///
    /// Differs from `ADDIU` in one place: on signed overflow, the
    /// destination register is left unchanged and a 12 (Overflow)
    /// exception fires. Games occasionally rely on the trap for
    /// range-check idioms -- treating `ADDI` as `ADDIU` means those
    /// games silently run past what should have been a clamped value.
    fn op_addi(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let imm = (instr as i16) as i32;
        let a = self.gpr(rs) as i32;
        match a.checked_add(imm) {
            Some(sum) => {
                self.set_gpr(rt, sum as u32);
                Ok(())
            }
            None => {
                // Signed overflow -- destination unchanged, raise
                // CAUSE.ExcCode = 12 (Overflow).
                let in_delay_slot = self.executing_in_branch_delay;
                self.enter_exception(ExceptionCode::Overflow, self.pc, in_delay_slot);
                Ok(())
            }
        }
    }

    /// `ADDIU rt, rs, imm16` -- add sign-extended immediate, no overflow trap:
    /// `rt = rs + sign_extend(imm16)`.
    ///
    /// Despite the "U" in the name, both operands are interpreted with
    /// the same bit pattern; the difference from `ADDI` is only that
    /// arithmetic overflow does not raise an exception.
    fn op_addiu(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let imm = (instr as i16) as i32 as u32;
        self.set_gpr(rt, self.gpr(rs).wrapping_add(imm));
        Ok(())
    }

    /// `ORI rt, rs, imm16` -- bitwise OR with zero-extended immediate:
    /// `rt = rs | imm16`.
    fn op_ori(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let imm = instr & 0xFFFF;
        self.set_gpr(rt, self.gpr(rs) | imm);
        Ok(())
    }

    /// `OR rd, rs, rt` -- bitwise OR of two registers: `rd = rs | rt`.
    fn op_or(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let rd = ((instr >> 11) & 0x1F) as u8;
        self.set_gpr(rd, self.gpr(rs) | self.gpr(rt));
        Ok(())
    }

    /// `ADDU rd, rs, rt` -- add unsigned (no overflow trap): `rd = rs + rt`.
    fn op_addu(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let rd = ((instr >> 11) & 0x1F) as u8;
        self.set_gpr(rd, self.gpr(rs).wrapping_add(self.gpr(rt)));
        Ok(())
    }

    /// `SLT rd, rs, rt` -- set-less-than, signed: `rd = (rs < rt) ? 1 : 0`.
    fn op_slt(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let rd = ((instr >> 11) & 0x1F) as u8;
        let lhs = self.gpr(rs) as i32;
        let rhs = self.gpr(rt) as i32;
        self.set_gpr(rd, (lhs < rhs) as u32);
        Ok(())
    }

    /// `SLTU rd, rs, rt` -- set-less-than, unsigned.
    fn op_sltu(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let rd = ((instr >> 11) & 0x1F) as u8;
        self.set_gpr(rd, (self.gpr(rs) < self.gpr(rt)) as u32);
        Ok(())
    }

    /// `SLL rd, rt, sa` -- shift left logical by `sa` bits.
    /// When `rd = rt = sa = 0`, the whole encoding is `0x0000_0000`,
    /// which is the canonical `NOP`.
    fn op_sll(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rt = ((instr >> 16) & 0x1F) as u8;
        let rd = ((instr >> 11) & 0x1F) as u8;
        let sa = (instr >> 6) & 0x1F;
        self.set_gpr(rd, self.gpr(rt) << sa);
        Ok(())
    }

    /// `J target` -- unconditional jump. The 26-bit `target` is left-shifted
    /// by 2 and merged with the top 4 bits of the delay-slot's PC
    /// (`pc + 4`) to form the absolute destination.
    ///
    /// The delay slot (the instruction immediately after the jump)
    /// executes before PC actually lands at the target -- this happens
    /// via [`Cpu::step`]'s `pending_pc` handling.
    fn op_j(&mut self, instr: u32, pc: u32) -> Result<(), ExecutionError> {
        let target_field = instr & 0x03FF_FFFF;
        let delay_slot_pc = pc.wrapping_add(4);
        let target = (delay_slot_pc & 0xF000_0000) | (target_field << 2);
        self.pending_pc = Some(target);
        Ok(())
    }

    /// `BEQ rs, rt, offset` -- branch (delay-slotted) if `rs == rt`.
    /// Target = `(pc + 4) + (sign_extend(offset) << 2)`.
    fn op_beq(&mut self, instr: u32, pc: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        if self.gpr(rs) == self.gpr(rt) {
            self.pending_pc = Some(branch_target(pc, instr));
        }
        Ok(())
    }

    /// `BNE rs, rt, offset` -- branch (delay-slotted) if `rs != rt`.
    fn op_bne(&mut self, instr: u32, pc: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        if self.gpr(rs) != self.gpr(rt) {
            self.pending_pc = Some(branch_target(pc, instr));
        }
        Ok(())
    }

    /// `LW rt, offset(rs)` -- load word: `rt = mem[rs + sign_ext(offset)]`.
    ///
    /// The R3000A has a one-slot load delay: the loaded value lands in
    /// `rt` at the end of the *next* instruction, not this one. We
    /// stage the load in `pending_load`; [`Cpu::step`] commits it
    /// after the following instruction executes.
    fn op_lw(&mut self, instr: u32, bus: &mut Bus) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let offset = (instr as i16) as i32 as u32;
        let addr = self.gpr(rs).wrapping_add(offset);
        // Alignment traps happen before the memory transaction.
        if addr & 3 != 0 {
            self.raise_address_error(ExceptionCode::AddressErrorLoad, addr, bus);
            return Ok(());
        }
        self.charge_read(bus, addr, AccessWidth::Word);
        let value = self.read_word(addr, bus);
        // Loads to $zero are no-ops; never queue a commit to it.
        if rt != 0 {
            self.pending_load = Some((rt, value));
        }
        Ok(())
    }

    /// `SW rt, offset(rs)` -- store word: `mem[rs + sign_ext(offset)] = rt`.
    ///
    /// If COP0 Status Register bit 16 (IsC -- isolate cache) is set, the
    /// store is redirected to instruction-cache tag or code RAM according
    /// to BCC.TAG. The BIOS uses word stores in this mode to invalidate all
    /// tags and then clear the cache's code RAM without touching main RAM.
    fn op_sw(&mut self, instr: u32, bus: &mut Bus) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let offset = (instr as i16) as i32 as u32;
        let addr = self.gpr(rs).wrapping_add(offset);
        // Alignment trap before the store reaches memory.
        if addr & 3 != 0 {
            self.raise_address_error(ExceptionCode::AddressErrorStore, addr, bus);
            return Ok(());
        }
        self.write_word(addr, self.gpr(rt), bus);
        Ok(())
    }

    fn op_mfc0(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rt = ((instr >> 16) & 0x1F) as u8;
        let rd = ((instr >> 11) & 0x1F) as usize;
        if rt != 0 {
            self.pending_load = Some((rt, self.cop0[rd]));
        }
        Ok(())
    }

    fn op_rfe(&mut self) -> Result<(), ExecutionError> {
        // Mirrors Redux's `psxRFE` (psxinterpreter.cc:779): restore
        // the previous KU/IE pair by shifting SR[5:0] right by two.
        //
        // Exception-depth bookkeeping: decrement the handler-depth
        // counter. When it reaches zero we've exited the outermost
        // handler and can clear the `clean_irq_entry` latch -- the
        // parity harness's aggregation then stops silently
        // absorbing steps and starts recording user-code
        // instructions again.
        let sr = self.cop0[12];
        let restored = (sr & !0b1111) | ((sr >> 2) & 0b1111);
        self.cop0[12] = restored;
        self.isr_depth = self.isr_depth.saturating_sub(1);
        if self.isr_depth == 0 {
            self.clean_irq_entry = false;
        }
        Ok(())
    }

    fn op_jal(&mut self, instr: u32, pc: u32) -> Result<(), ExecutionError> {
        let target_field = instr & 0x03FF_FFFF;
        let delay_slot_pc = pc.wrapping_add(4);
        let target = (delay_slot_pc & 0xF000_0000) | (target_field << 2);
        self.set_gpr(31, delay_slot_pc.wrapping_add(4));
        self.pending_pc = Some(target);
        Ok(())
    }

    fn op_jr(&mut self, instr: u32, _pc: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        self.pending_pc = Some(self.gpr(rs));
        Ok(())
    }

    fn op_jalr(&mut self, instr: u32, pc: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rd = ((instr >> 11) & 0x1F) as u8;
        let target = self.gpr(rs);
        self.set_gpr(rd, pc.wrapping_add(8));
        self.pending_pc = Some(target);
        Ok(())
    }

    fn op_blez(&mut self, instr: u32, pc: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        if (self.gpr(rs) as i32) <= 0 {
            self.pending_pc = Some(branch_target(pc, instr));
        }
        Ok(())
    }

    fn op_bgtz(&mut self, instr: u32, pc: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        if (self.gpr(rs) as i32) > 0 {
            self.pending_pc = Some(branch_target(pc, instr));
        }
        Ok(())
    }

    fn op_bltz(&mut self, instr: u32, pc: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        if (self.gpr(rs) as i32) < 0 {
            self.pending_pc = Some(branch_target(pc, instr));
        }
        Ok(())
    }

    fn op_bgez(&mut self, instr: u32, pc: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        if (self.gpr(rs) as i32) >= 0 {
            self.pending_pc = Some(branch_target(pc, instr));
        }
        Ok(())
    }

    fn op_bltzal(&mut self, instr: u32, pc: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        self.set_gpr(31, pc.wrapping_add(8));
        if (self.gpr(rs) as i32) < 0 {
            self.pending_pc = Some(branch_target(pc, instr));
        }
        Ok(())
    }

    fn op_bgezal(&mut self, instr: u32, pc: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        self.set_gpr(31, pc.wrapping_add(8));
        if (self.gpr(rs) as i32) >= 0 {
            self.pending_pc = Some(branch_target(pc, instr));
        }
        Ok(())
    }

    fn op_slti(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let imm = (instr as i16) as i32;
        self.set_gpr(rt, ((self.gpr(rs) as i32) < imm) as u32);
        Ok(())
    }

    fn op_sltiu(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let imm = (instr as i16) as i32 as u32;
        self.set_gpr(rt, (self.gpr(rs) < imm) as u32);
        Ok(())
    }

    fn op_andi(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let imm = instr & 0xFFFF;
        self.set_gpr(rt, self.gpr(rs) & imm);
        Ok(())
    }

    fn op_xori(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let imm = instr & 0xFFFF;
        self.set_gpr(rt, self.gpr(rs) ^ imm);
        Ok(())
    }

    fn op_lb(&mut self, instr: u32, bus: &mut Bus) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let offset = (instr as i16) as i32 as u32;
        let addr = self.gpr(rs).wrapping_add(offset);
        self.charge_read(bus, addr, AccessWidth::Byte);
        let value = self.read_byte(addr, bus) as i8 as i32 as u32;
        if rt != 0 {
            self.pending_load = Some((rt, value));
        }
        Ok(())
    }

    fn op_lbu(&mut self, instr: u32, bus: &mut Bus) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let offset = (instr as i16) as i32 as u32;
        let addr = self.gpr(rs).wrapping_add(offset);
        self.charge_read(bus, addr, AccessWidth::Byte);
        let value = self.read_byte(addr, bus) as u32;
        if rt != 0 {
            self.pending_load = Some((rt, value));
        }
        Ok(())
    }

    fn op_lh(&mut self, instr: u32, bus: &mut Bus) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let offset = (instr as i16) as i32 as u32;
        let addr = self.gpr(rs).wrapping_add(offset);
        if addr & 1 != 0 {
            self.raise_address_error(ExceptionCode::AddressErrorLoad, addr, bus);
            return Ok(());
        }
        self.charge_read(bus, addr, AccessWidth::Half);
        let value = self.read_half(addr, bus) as i16 as i32 as u32;
        if rt != 0 {
            self.pending_load = Some((rt, value));
        }
        Ok(())
    }

    fn op_lhu(&mut self, instr: u32, bus: &mut Bus) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let offset = (instr as i16) as i32 as u32;
        let addr = self.gpr(rs).wrapping_add(offset);
        if addr & 1 != 0 {
            self.raise_address_error(ExceptionCode::AddressErrorLoad, addr, bus);
            return Ok(());
        }
        self.charge_read(bus, addr, AccessWidth::Half);
        let value = self.read_half(addr, bus) as u32;
        if rt != 0 {
            self.pending_load = Some((rt, value));
        }
        Ok(())
    }

    fn op_sb(&mut self, instr: u32, bus: &mut Bus) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let offset = (instr as i16) as i32 as u32;
        let addr = self.gpr(rs).wrapping_add(offset);
        self.write_byte(addr, self.gpr(rt), bus);
        Ok(())
    }

    fn op_sh(&mut self, instr: u32, bus: &mut Bus) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let offset = (instr as i16) as i32 as u32;
        let addr = self.gpr(rs).wrapping_add(offset);
        if addr & 1 != 0 {
            self.raise_address_error(ExceptionCode::AddressErrorStore, addr, bus);
            return Ok(());
        }
        self.write_half(addr, self.gpr(rt), bus);
        Ok(())
    }

    /// `LWL rt, offset(rs)` -- Load Word Left. Together with `LWR`,
    /// loads a 32-bit word that may be unaligned. LWL reads the word
    /// containing `addr` and merges its high-order bytes into `rt`
    /// from the top down.
    ///
    /// Unaligned memcpy sequence: `LWL rt, 3(rs)` + `LWR rt, 0(rs)`
    /// loads 4 bytes from `rs..rs+4` regardless of alignment.
    ///
    /// Also: LWL/LWR preserve bytes in the destination they don't
    /// overwrite, so the delay-slot value of `rt` matters -- the
    /// previous instruction's committing load gets **merged**, not
    /// squashed (the opposite of non-load writes). Redux's model
    /// peeks at the staged `rt` via the pending-load slot.
    fn op_lwl(&mut self, instr: u32, bus: &mut Bus) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let offset = (instr as i16) as i32 as u32;
        let addr = self.gpr(rs).wrapping_add(offset);
        let word = self.read_lwl_lanes(addr, bus);
        // If there's a pending-commit load for the same register, see
        // its staged value instead of the current register file --
        // that's what matches hardware's LWL-LWR-merge convention.
        let current = self.staged_gpr(rt);
        // PSX-SPX + Redux `LWL_SHIFT`/`LWL_MASK` tables:
        //   (addr & 3): 0 → shift=24 mask=0x00FFFFFF
        //               1 → shift=16 mask=0x0000FFFF
        //               2 → shift=8  mask=0x000000FF
        //               3 → shift=0  mask=0x00000000
        // Visually for Mem=1234, Reg=abcd:
        //   addr=0 → 4bcd   (low byte of mem goes to rt's MSB)
        //   addr=1 → 34cd
        //   addr=2 → 234d
        //   addr=3 → 1234   (full 4-byte load)
        let shift = (3 - (addr & 3)) * 8;
        let mask = !(0xFFFF_FFFFu32 << shift);
        let merged = (current & mask) | (word << shift);
        if rt != 0 {
            self.pending_load = Some((rt, merged));
        }
        Ok(())
    }

    /// `LWR rt, offset(rs)` -- Load Word Right. Mirror of LWL; merges
    /// the low-order bytes of the word containing `addr` into `rt`.
    fn op_lwr(&mut self, instr: u32, bus: &mut Bus) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let offset = (instr as i16) as i32 as u32;
        let addr = self.gpr(rs).wrapping_add(offset);
        // LWL/LWR targeting the same register are architecturally
        // interlocked: LWR sees the staged LWL result and the pair pays one
        // merge cycle. This is why the public SPU unaligned-word probe lands
        // at ~39 cycles rather than two independent full-word transactions.
        if self
            .committing_load
            .is_some_and(|(pending_reg, _)| pending_reg == rt)
        {
            bus.add_cycles(Self::unaligned_lane_cycles(addr, 1));
        }
        let word = self.read_lwr_lanes(addr, bus);
        let current = self.staged_gpr(rt);
        let shift = (addr & 3) * 8;
        let mask = 0xFFFF_FFFFu32 >> (24 - shift);
        let merged = (current & !(0xFFFF_FFFFu32 >> shift)) | (word >> shift);
        let _ = mask; // the mask form above is equivalent to the >> chain used
        if rt != 0 {
            self.pending_load = Some((rt, merged));
        }
        Ok(())
    }

    /// `SWL rt, offset(rs)` -- Store Word Left. Mirror of LWL on the
    /// store side. Writes `rt`'s high bytes into the word containing
    /// `addr`, preserving the lower bytes of the memory word.
    fn op_swl(&mut self, instr: u32, bus: &mut Bus) -> Result<(), ExecutionError> {
        if self.cache_isolated() {
            return Ok(());
        }
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let offset = (instr as i16) as i32 as u32;
        let addr = self.gpr(rs).wrapping_add(offset);
        let aligned = addr & !3;
        self.charge_unaligned_store_read(bus, aligned);
        let mem = bus.read32(aligned);
        let reg = self.gpr(rt);
        let shift = (addr & 3) * 8;
        let merged = (mem & !(0xFFFF_FFFFu32 >> (24 - shift))) | (reg >> (24 - shift));
        // Charged as the ordinary store it is on the bus.
        bus.cpu_write32(aligned, merged);
        Ok(())
    }

    /// `SWR rt, offset(rs)` -- Store Word Right. Mirror of LWR on the
    /// store side.
    fn op_swr(&mut self, instr: u32, bus: &mut Bus) -> Result<(), ExecutionError> {
        if self.cache_isolated() {
            return Ok(());
        }
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let offset = (instr as i16) as i32 as u32;
        let addr = self.gpr(rs).wrapping_add(offset);
        let aligned = addr & !3;
        self.charge_unaligned_store_read(bus, aligned);
        let mem = bus.read32(aligned);
        let reg = self.gpr(rt);
        let shift = (addr & 3) * 8;
        let merged = (mem & !(0xFFFF_FFFFu32 << shift)) | (reg << shift);
        // Charged as the ordinary store it is on the bus.
        bus.cpu_write32(aligned, merged);
        Ok(())
    }

    /// Return the register value that would be seen by an LWL/LWR
    /// merge: prefer the staged (`committing_load`) value if one is
    /// pending for this register, else the current register file.
    /// Matches R3000 hardware behaviour where LWL and LWR merge with
    /// a load delay they share with each other.
    fn staged_gpr(&self, index: u8) -> u32 {
        if let Some((reg, value)) = &self.committing_load {
            if *reg == index {
                return *value;
            }
        }
        self.gpr(index)
    }

    fn op_srl(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rt = ((instr >> 16) & 0x1F) as u8;
        let rd = ((instr >> 11) & 0x1F) as u8;
        let sa = (instr >> 6) & 0x1F;
        self.set_gpr(rd, self.gpr(rt) >> sa);
        Ok(())
    }

    fn op_sra(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rt = ((instr >> 16) & 0x1F) as u8;
        let rd = ((instr >> 11) & 0x1F) as u8;
        let sa = (instr >> 6) & 0x1F;
        self.set_gpr(rd, ((self.gpr(rt) as i32) >> sa) as u32);
        Ok(())
    }

    fn op_sllv(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let rd = ((instr >> 11) & 0x1F) as u8;
        self.set_gpr(rd, self.gpr(rt) << (self.gpr(rs) & 0x1F));
        Ok(())
    }

    fn op_srlv(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let rd = ((instr >> 11) & 0x1F) as u8;
        self.set_gpr(rd, self.gpr(rt) >> (self.gpr(rs) & 0x1F));
        Ok(())
    }

    fn op_srav(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let rd = ((instr >> 11) & 0x1F) as u8;
        self.set_gpr(rd, ((self.gpr(rt) as i32) >> (self.gpr(rs) & 0x1F)) as u32);
        Ok(())
    }

    /// `ADD rd, rs, rt` -- signed add. Raises Overflow (code 12) on
    /// signed overflow; destination unchanged. `ADDU` is the wrap-
    /// silently variant.
    fn op_add(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let rd = ((instr >> 11) & 0x1F) as u8;
        let a = self.gpr(rs) as i32;
        let b = self.gpr(rt) as i32;
        match a.checked_add(b) {
            Some(sum) => {
                self.set_gpr(rd, sum as u32);
                Ok(())
            }
            None => {
                let in_delay_slot = self.executing_in_branch_delay;
                self.enter_exception(ExceptionCode::Overflow, self.pc, in_delay_slot);
                Ok(())
            }
        }
    }

    /// `SUB rd, rs, rt` -- signed subtract. Raises Overflow (code 12)
    /// on signed overflow; destination unchanged. `SUBU` wraps.
    fn op_sub(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let rd = ((instr >> 11) & 0x1F) as u8;
        let a = self.gpr(rs) as i32;
        let b = self.gpr(rt) as i32;
        match a.checked_sub(b) {
            Some(diff) => {
                self.set_gpr(rd, diff as u32);
                Ok(())
            }
            None => {
                let in_delay_slot = self.executing_in_branch_delay;
                self.enter_exception(ExceptionCode::Overflow, self.pc, in_delay_slot);
                Ok(())
            }
        }
    }

    fn op_subu(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let rd = ((instr >> 11) & 0x1F) as u8;
        self.set_gpr(rd, self.gpr(rs).wrapping_sub(self.gpr(rt)));
        Ok(())
    }

    fn op_and(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let rd = ((instr >> 11) & 0x1F) as u8;
        self.set_gpr(rd, self.gpr(rs) & self.gpr(rt));
        Ok(())
    }

    fn op_xor(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let rd = ((instr >> 11) & 0x1F) as u8;
        self.set_gpr(rd, self.gpr(rs) ^ self.gpr(rt));
        Ok(())
    }

    fn op_nor(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let rd = ((instr >> 11) & 0x1F) as u8;
        self.set_gpr(rd, !(self.gpr(rs) | self.gpr(rt)));
        Ok(())
    }

    fn op_mfhi(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rd = ((instr >> 11) & 0x1F) as u8;
        let hi = self.hi;
        self.set_gpr(rd, hi);
        Ok(())
    }

    fn op_mthi(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        self.hi = self.gpr(rs);
        Ok(())
    }

    fn op_mflo(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rd = ((instr >> 11) & 0x1F) as u8;
        let lo = self.lo;
        self.set_gpr(rd, lo);
        Ok(())
    }

    fn op_mtlo(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        self.lo = self.gpr(rs);
        Ok(())
    }

    fn op_mult(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let result = (self.gpr(rs) as i32 as i64) * (self.gpr(rt) as i32 as i64);
        self.hi = (result >> 32) as u32;
        self.lo = result as u32;
        Ok(())
    }

    fn op_multu(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let result = (self.gpr(rs) as u64) * (self.gpr(rt) as u64);
        self.hi = (result >> 32) as u32;
        self.lo = result as u32;
        Ok(())
    }

    fn op_div(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let n = self.gpr(rs) as i32;
        let d = self.gpr(rt) as i32;
        if d == 0 {
            self.hi = n as u32;
            self.lo = if n < 0 { 1 } else { u32::MAX };
        } else if n == i32::MIN && d == -1 {
            self.hi = 0;
            self.lo = i32::MIN as u32;
        } else {
            self.hi = (n % d) as u32;
            self.lo = (n / d) as u32;
        }
        Ok(())
    }

    fn op_divu(&mut self, instr: u32) -> Result<(), ExecutionError> {
        let rs = ((instr >> 21) & 0x1F) as u8;
        let rt = ((instr >> 16) & 0x1F) as u8;
        let n = self.gpr(rs);
        let d = self.gpr(rt);
        if d == 0 {
            self.hi = n;
            self.lo = u32::MAX;
        } else {
            self.hi = n % d;
            self.lo = n / d;
        }
        Ok(())
    }

    /// COP0 SR bit 16 (IsC). When set, D-cache is isolated from memory
    /// and loads/stores don't reach RAM.
    #[inline]
    fn cache_isolated(&self) -> bool {
        self.cop0[12] & (1 << 16) != 0
    }

    /// Interrupts against GTE commands, as a console behaves (hwtest v1.24
    /// cases 0xC8-0xCB, psx-spx "Interrupts vs GTE Commands"): an interrupt
    /// raised during the instruction before a GTE command is taken after
    /// the command has executed, with EPC pointing at the command. A handler
    /// that returns to EPC therefore runs it twice; the BIOS handler and
    /// psx-rt step EPC over it. This is DuckStation's model.
    ///
    /// Interrupts are otherwise dispatched at branch boundaries (Redux's
    /// `branchTest`), so this samples the line at the two boundaries around
    /// the instruction before each GTE command: armed at the start of that
    /// instruction when nothing is pending, and returning `true` (take the
    /// interrupt after this step) when the command starts with one pending.
    /// An interrupt already pending before would have been taken earlier on
    /// silicon and keeps the deferral in [`Cpu::should_take_interrupt`].
    /// A delay-slot command is excluded: EPC would hold the branch.
    #[inline(always)]
    fn gte_irq_hazard(&mut self, pc: u32, bus: &mut Bus) -> bool {
        const IRQ_ENABLED: u32 = 0x401;
        let watched = self.gte_irq_watch.take() == Some(pc);
        if self.cop0[12] & IRQ_ENABLED != IRQ_ENABLED {
            return false;
        }
        let next = self.pending_pc.unwrap_or(pc.wrapping_add(4));
        let next_is_gte = bus
            .peek_instruction(next)
            .is_some_and(|word| word & 0xFE00_0000 == 0x4A00_0000);
        if !watched && !next_is_gte {
            return false;
        }
        self.gte_irq_sample(watched, next_is_gte.then_some(next), bus)
    }

    /// The slow half of [`Cpu::gte_irq_hazard`], reached only around GTE
    /// commands.
    #[inline(never)]
    fn gte_irq_sample(&mut self, watched: bool, next_gte: Option<u32>, bus: &mut Bus) -> bool {
        bus.drain_scheduler_events_post_op();
        let pending = bus.external_interrupt_pending();
        if watched && self.pending_pc.is_none() && !self.branch_delay_next && pending {
            return true;
        }
        if !pending {
            self.gte_irq_watch = next_gte;
        }
        false
    }

    /// `true` when the CPU should take an interrupt exception right
    /// now. Mirrors PCSX-Redux's `branchTest`:
    ///   `(I_STAT & I_MASK) && ((SR & 0x401) == 0x401)`
    /// -- i.e. some hardware source is both pending and enabled,
    /// SR.IM[2] (hardware-IRQ mask, bit 10) is on, and SR.IEc (global
    /// interrupt enable, bit 0) is on. Software interrupts on IP[0..1]
    /// would also raise via this path on real hardware; the BIOS
    /// doesn't use them, so we don't model that.
    fn should_take_interrupt(&self, bus: &mut Bus) -> bool {
        let sr = self.cop0[12];
        if !(bus.external_interrupt_pending() && (sr & 0x401) == 0x401) {
            return false;
        }
        // R3000A hardware bug -- "interrupts vs GTE commands". If the
        // next instruction about to execute is a GTE cofun (COP2
        // function instruction, opcode 0100_10 with bit 25 set --
        // i.e. top byte masked with 0xFE equals 0x4A), taking the
        // IRQ here gets the GTE instruction executed anyway but
        // also parks EPC pointing at it, so the ISR's return
        // advances PC past the GTE op -- effectively losing it.
        //
        // Reference: `psx-spx`'s "Interrupts vs GTE Commands"
        // section; Redux mirrors the fix at `r3000a.cc:411`.
        //
        // `peek_instruction` is side-effect-free; it returns `None`
        // for non-code addresses, which we treat as "not a GTE
        // cofun" (games never execute from MMIO).
        if let Some(next) = bus.peek_instruction(self.pc) {
            if (next & 0xFE00_0000) == 0x4A00_0000 {
                return false;
            }
        }
        true
    }

    /// `SYSCALL` -- raise a syscall exception (CAUSE.ExcCode = 8). The
    /// kernel's exception handler services it (critical sections, thread
    /// switches); with the HLE BIOS that handler is guest code in RAM too.
    fn op_syscall(&mut self, pc: u32, in_delay_slot: bool) -> Result<(), ExecutionError> {
        self.enter_exception(ExceptionCode::Syscall, pc, in_delay_slot);
        Ok(())
    }

    /// `BREAK` -- raise a breakpoint exception (CAUSE.ExcCode = 9). Not
    /// hit during normal BIOS boot but cheap to add alongside SYSCALL
    /// since they share the exception-entry plumbing.
    fn op_break(&mut self, pc: u32, in_delay_slot: bool) -> Result<(), ExecutionError> {
        self.enter_exception(ExceptionCode::Break, pc, in_delay_slot);
        Ok(())
    }

    /// Shared exception-entry sequence. Mutates COP0 registers and
    /// stages the exception-vector PC for [`Cpu::step`] to apply.
    ///
    /// - **CAUSE**: *overwrite* with `(ExcCode << 2) | (BD bit) | (IP[2]
    ///   for Interrupt)`. Mirrors PCSX-Redux's `m_regs.CP0.n.Cause = code`
    ///   in `R3000Acpu::exception` -- Redux blows the whole register
    ///   away on every exception, including IP bits, so software-side
    ///   `mfc0 v0, $13` reads only ever see what the most recent
    ///   exception parked there. We have to mirror this exactly: if we
    ///   preserved IP[2] (the natural real-hardware behaviour) BIOS
    ///   syscall handlers would observe `CAUSE = 0x420` while Redux
    ///   sees `0x20`, breaking GPR parity.
    /// - **SR**: push the 3-level KU/IE stack -- bits `SR[5:0]` become
    ///   `(SR[3:0] << 2)`, with the new current pair (bits 1..0)
    ///   entering kernel-mode / interrupts-disabled.
    /// - **Vector**: `0xBFC0_0180` when `SR.BEV` (bit 22) is set (the
    ///   post-reset default the BIOS boots in), else `0x8000_0080`.
    fn enter_exception(&mut self, code: ExceptionCode, pc: u32, in_delay_slot: bool) {
        let code_bits = (code as u32) & 0x1F;
        self.exception_counts[code_bits as usize] =
            self.exception_counts[code_bits as usize].saturating_add(1);
        // Opt-in fault tracing for guest debugging: every non-IRQ exception
        // with its cause, EPC, and delay-slot flag.
        if !matches!(code, ExceptionCode::Interrupt) && crate::env_flag!("PSOXIDE_TRACE_EXC") {
            eprintln!(
                "[cpu] exception code {} at pc=0x{:08x} bd={}",
                code_bits, pc, in_delay_slot as u32
            );
        }

        let mut cause = code_bits << 2;
        if matches!(code, ExceptionCode::Interrupt) {
            cause |= 1 << 10;
        }
        // Latch `clean_irq_entry` only on the outermost entry (depth
        // 0 → 1) and only if that entry was an IRQ. Nested
        // exceptions inside an IRQ handler don't flip the latch --
        // the parity harness keeps aggregating through them until
        // the outermost RFE brings the depth back to zero.
        // Matches Redux's `m_wasInISR` which is snapshotted at
        // `startStepping()` and governs the `debug.cc:235`
        // early-return for the whole stepIn span.
        if self.isr_depth == 0 {
            self.clean_irq_entry = matches!(code, ExceptionCode::Interrupt);
        }
        self.isr_depth = self.isr_depth.saturating_add(1);
        if in_delay_slot {
            cause |= 1 << 31;
        }
        self.cop0[13] = cause;

        self.cop0[14] = if in_delay_slot {
            pc.wrapping_sub(4)
        } else {
            pc
        };

        let sr = self.cop0[12];
        self.cop0[12] = (sr & !0x3F) | ((sr & 0x0F) << 2);

        let vector = if sr & (1 << 22) != 0 {
            0xBFC0_0180
        } else {
            0x8000_0080
        };
        self.pending_exception_pc = Some(vector);
    }

    /// Raise an AdEL or AdES address-error exception. Stores the
    /// offending virtual address in COP0 BadVaddr (cop0[8]) and
    /// hands off to [`Cpu::enter_exception`] with the appropriate
    /// code. The delay-slot flag comes from `executing_in_branch_delay`,
    /// which `step` sets for the instruction it is executing.
    fn raise_address_error(&mut self, code: ExceptionCode, addr: u32, bus: &mut Bus) {
        let in_delay_slot = self.executing_in_branch_delay;
        self.cop0[8] = addr;
        self.enter_exception(code, self.pc, in_delay_slot);
        self.stage_hle_unresolved_exception(bus);
    }

    /// Route a side-loaded EXE's address error through the unresolved-handler
    /// slot A(40h) of the RAM A0 table, when the guest has replaced it. The
    /// interrupted frame goes to the current thread's TCB (`[[0x108]]`),
    /// whose layout matches psn00bsdk's `Thread::registers`, so existing
    /// homebrew handlers can inspect CAUSE, change the saved return PC, and
    /// return.
    fn stage_hle_unresolved_exception(&mut self, bus: &mut Bus) {
        if !bus.hle_bios_enabled || self.hle_exception_active {
            return;
        }
        let handler = crate::hle_kernel::peek32(bus, crate::hle_bios::UNRESOLVED_HANDLER_PTR);
        let tcb = crate::hle_kernel::current_tcb(bus);
        if handler == 0 || handler == crate::hle_kernel::stub_addr(0, 0x40) || tcb == 0 {
            return;
        }
        use crate::hle_bios::{TCB_CAUSE, TCB_HI, TCB_LO, TCB_REGISTERS, TCB_RETURN_PC, TCB_SR};
        use crate::hle_kernel::poke32;
        for (index, value) in self.gprs.iter().copied().enumerate() {
            poke32(bus, tcb + TCB_REGISTERS + index as u32 * 4, value);
        }
        poke32(bus, tcb + TCB_RETURN_PC, self.cop0[14]);
        poke32(bus, tcb + TCB_HI, self.hi);
        poke32(bus, tcb + TCB_LO, self.lo);
        poke32(bus, tcb + TCB_SR, self.cop0[12]);
        poke32(bus, tcb + TCB_CAUSE, self.cop0[13]);

        self.gprs[31] = crate::hle_bios::EXCEPTION_RETURN_STUB;
        self.pending_exception_pc = Some(handler);
        self.pending_pc = None;
        self.branch_delay_next = false;
        self.pending_load = None;
        self.hle_exception_active = true;
    }

    fn finish_hle_exception(&mut self, bus: &mut Bus) {
        use crate::hle_bios::{TCB_CAUSE, TCB_HI, TCB_LO, TCB_REGISTERS, TCB_RETURN_PC, TCB_SR};
        use crate::hle_kernel::peek32;
        let tcb = crate::hle_kernel::current_tcb(bus);
        for index in 0..32 {
            self.gprs[index] = peek32(bus, tcb + TCB_REGISTERS + index as u32 * 4);
        }
        self.gprs[0] = 0;
        self.hi = peek32(bus, tcb + TCB_HI);
        self.lo = peek32(bus, tcb + TCB_LO);
        self.cop0[13] = peek32(bus, tcb + TCB_CAUSE);
        let saved_sr = peek32(bus, tcb + TCB_SR);
        self.cop0[12] = (saved_sr & !0x0F) | ((saved_sr >> 2) & 0x0F);
        self.pc = peek32(bus, tcb + TCB_RETURN_PC);
        self.pending_pc = None;
        self.branch_delay_next = false;
        self.pending_load = None;
        self.committing_load = None;
        self.pending_exception_pc = None;
        self.hle_exception_active = false;
        self.isr_depth = self.isr_depth.saturating_sub(1);
        if self.isr_depth == 0 {
            self.clean_irq_entry = false;
        }
    }
}

/// R3000A iterative divide latency, in CPU cycles. DIV and DIVU take the
/// same fixed time regardless of operands.
const DIV_CYCLES: u32 = 36;

/// Stall the CPU forward to `deadline` (a [`Bus::cycles`] value) if it has
/// not reached it yet. Shared by the GTE and multiply/divide result
/// interlocks: both compute their result eagerly, and this charges the cycles
/// the CPU would spend waiting for the unit to retire before reading it.
#[inline]
fn stall_to(bus: &mut Bus, deadline: u64) {
    let now = bus.cycles();
    if now < deadline {
        bus.add_cycles((deadline - now) as u32);
    }
}

/// The HI/LO interlock. As [`stall_to`], except that a read arriving one
/// cycle before the unit retires does not stall.
///
/// Measured on a launch PAL console with the hwtest v1.22 gap sweep (records
/// `0x79`-`0x84`, 16 repeats of `multu; k nops; mflo`): with k = m - 1 nops
/// behind a multiply of latency m the pair costs m + 1 cycles, one fewer than
/// with no nops at all, at all three bands (110/126, 158/174, 222/238), and
/// from k = m on the cost is k + 2 as modelled. So the result is readable one
/// cycle sooner than the stalled path suggests, and taking the stall costs a
/// cycle to come out of. Only k = 0 and k = m - 1 are measured below m; the
/// shape between them is assumed flat, as it was before.
#[inline]
fn hilo_stall_to(bus: &mut Bus, deadline: u64) {
    let now = bus.cycles();
    if now + 1 < deadline {
        bus.add_cycles((deadline - now) as u32);
    }
}

/// R3000A multiply latency in CPU cycles. The multiply/divide unit retires
/// faster for small multipliers: 6 cycles when the `rs` operand's magnitude
/// fits in 11 bits, 9 within 20 bits, otherwise the full 13. `signed` selects
/// the MULT (sign-magnitude) versus MULTU (raw) interpretation of `rs`.
#[inline]
fn mult_cycles(rs: u32, signed: bool) -> u32 {
    let magnitude = if signed && (rs as i32) < 0 { !rs } else { rs };
    if magnitude < 0x800 {
        6
    } else if magnitude < 0x10_0000 {
        9
    } else {
        13
    }
}

/// Cycle cost per instruction -- matches PCSX-Redux's simple-interpreter
/// `BIAS = 2` (every instruction adds 2 to its cycle counter before
/// any opcode-specific accounting). Some opcodes on real hardware cost
/// more (MULT ≈ 7–13, DIV ≈ 36, memory stalls by region) and Redux
/// models a handful of those in its accurate mode; when our parity
/// probes reveal a divergence where the extra cycles matter, specific
/// MIPS R3000 exception codes (CAUSE.ExcCode). Only the ones we
/// actively raise are listed; the rest arrive as they're implemented.
#[repr(u8)]
#[derive(Copy, Clone)]
enum ExceptionCode {
    /// External interrupt -- asserted by the IRQ controller. See
    /// [`Cpu::should_take_interrupt`] for the gating logic.
    Interrupt = 0,
    /// AdEL -- load-side address error. Raised by `LH`/`LHU` on a
    /// halfword-misaligned address and by `LW` on a word-misaligned
    /// one. Real BIOS code occasionally relies on this trap to
    /// reject malformed pointers; silent "succeeds with garbage"
    /// is the worst possible failure mode.
    AddressErrorLoad = 4,
    /// AdES -- store-side address error. Raised by `SH`/`SW` for
    /// the equivalent misalignment cases.
    AddressErrorStore = 5,
    /// IBE -- instruction fetch from a non-executable physical region. The
    /// PS1 scratchpad is data-only, so attempting to run a scratch-resident
    /// kernel enters this exception rather than reading its bytes as code.
    InstructionBusError = 6,
    Syscall = 8,
    Break = 9,
    /// CpU -- selected coprocessor is unavailable. CAUSE.CE is filled by
    /// [`Cpu::raise_coprocessor_unusable`] after the common entry sequence.
    CoprocessorUnusable = 11,
    /// Integer arithmetic overflow -- raised by `ADD`, `ADDI`, and
    /// `SUB` when the signed result doesn't fit in 32 bits. `ADDU`,
    /// `ADDIU`, and `SUBU` are the silently-wrapping variants.
    Overflow = 12,
}

impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_bios_with_first_word(word: u32) -> Vec<u8> {
        synthetic_bios_with_words(&[word])
    }

    fn synthetic_bios_with_words(words: &[u32]) -> Vec<u8> {
        let mut bios = vec![0u8; memory::bios::SIZE];
        for (i, word) in words.iter().enumerate() {
            let offset = i * 4;
            bios[offset..offset + 4].copy_from_slice(&word.to_le_bytes());
        }
        bios
    }

    #[test]
    fn reset_state_points_at_bios_reset_vector() {
        let cpu = Cpu::new();
        assert_eq!(cpu.pc(), 0xBFC0_0000);
    }

    #[test]
    fn reset_state_has_zeroed_registers() {
        let cpu = Cpu::new();
        for i in 0..32 {
            assert_eq!(cpu.gpr(i), 0);
        }
    }

    #[test]
    fn cpu_cycle_profile_attributes_stack_loads_without_changing_cycles() {
        let mut cpu = Cpu::new();
        let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
        cpu.pc = 0x8000_1000;
        cpu.cache_control = CACHE_CONTROL_BIOS_NORMAL;
        cpu.gprs[29] = 0x8000_2000;
        bus.write32(0x8000_1000, 0x8fa8_0000); // lw $t0, 0($sp)
        bus.write32(0x8000_2000, 0x1234_5678);
        cpu.set_cpu_cycle_profile_enabled(true);

        let cycles_before = bus.cycles();
        cpu.step(&mut bus).unwrap();
        let profile = cpu.cpu_cycle_profile();

        assert_eq!(profile.issue_cycles, 1);
        assert!(profile.icache_refill_stall_cycles > 0);
        assert!(profile.ram_load_stall_cycles > 0);
        assert_eq!(
            profile.stack_ram_load_stall_cycles,
            profile.ram_load_stall_cycles
        );
        assert_eq!(
            profile.total_profiled_cycles(),
            bus.cycles().saturating_sub(cycles_before)
        );
    }

    #[test]
    fn cpu_cycle_profile_attributes_muldiv_interlocks() {
        let mut cpu = Cpu::new();
        let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
        cpu.pc = 0x8000_1000;
        cpu.cache_control = CACHE_CONTROL_BIOS_NORMAL;
        cpu.gprs[8] = 7;
        cpu.gprs[9] = 9;
        bus.write32(0x8000_1000, (8 << 21) | (9 << 16) | 0x18); // mult $t0, $t1
        bus.write32(0x8000_1004, (10 << 11) | 0x12); // mflo $t2
        cpu.set_cpu_cycle_profile_enabled(true);

        cpu.step(&mut bus).unwrap();
        cpu.step(&mut bus).unwrap();
        let profile = cpu.cpu_cycle_profile();

        assert_eq!(profile.issue_cycles, 2);
        assert!(profile.muldiv_interlock_stall_cycles > 0);
        assert_eq!(cpu.gpr(10), 63);
    }

    #[test]
    fn fetch_returns_first_bios_word() {
        // Real stock BIOSes (SCPH1001 / 5500 / 5501 / 5502) all begin with
        // `lui $t0, 0x0013` = 0x3C08_0013 as part of cache-control init.
        let mut bus = Bus::new(synthetic_bios_with_first_word(0x3C08_0013)).unwrap();
        let mut cpu = Cpu::new();
        assert_eq!(cpu.fetch(&mut bus), 0x3C08_0013);
    }

    #[test]
    fn cached_aliases_share_stale_code_while_kseg1_bypasses() {
        let mut cpu = Cpu::new();
        let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
        bus.write32(0x0000_1000, 0x1111_1111);
        cpu.cache_control = CACHE_CONTROL_IS1 | (3 << CACHE_CONTROL_IBLKSZ_SHIFT);

        cpu.pc = 0x8000_1000;
        assert_eq!(cpu.fetch(&mut bus), 0x1111_1111);
        bus.write32(0xA000_1000, 0x2222_2222);

        cpu.pc = 0x0000_1000;
        assert_eq!(cpu.fetch(&mut bus), 0x1111_1111);
        cpu.pc = 0xA000_1000;
        assert_eq!(cpu.fetch(&mut bus), 0x2222_2222);
    }

    #[test]
    fn isolated_tag_clear_invalidates_a_cached_line() {
        let mut cpu = Cpu::new();
        let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
        bus.write32(0x0000_2000, 0x3333_3333);
        cpu.cache_control = CACHE_CONTROL_IS1 | (3 << CACHE_CONTROL_IBLKSZ_SHIFT);
        cpu.pc = 0x8000_2000;
        assert_eq!(cpu.fetch(&mut bus), 0x3333_3333);

        bus.write32(0xA000_2000, 0x4444_4444);
        cpu.cop0[12] |= 1 << 16;
        cpu.cache_control |= CACHE_CONTROL_TAG;
        cpu.write_word(0x8000_2000, 0, &mut bus);
        cpu.cop0[12] &= !(1 << 16);
        cpu.cache_control &= !CACHE_CONTROL_TAG;

        assert_eq!(cpu.fetch(&mut bus), 0x4444_4444);
    }

    #[test]
    fn cache_control_word_access_latches_and_reads_hardwired_bits() {
        let mut cpu = Cpu::new();
        let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
        cpu.write_word(memory::cache_control::ADDR, u32::MAX, &mut bus);
        assert_eq!(
            cpu.read_word(memory::cache_control::ADDR, &mut bus),
            !((1 << 6) | (1 << 10))
        );
    }

    #[test]
    fn seed_from_exe_invalidates_stale_instruction_cache() {
        let mut cpu = Cpu::new();
        let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
        bus.write32(0x3000, 0x5555_5555);
        cpu.cache_control = CACHE_CONTROL_IS1 | (3 << CACHE_CONTROL_IBLKSZ_SHIFT);
        cpu.pc = 0x8000_3000;
        assert_eq!(cpu.fetch(&mut bus), 0x5555_5555);
        bus.write32(0xA000_3000, 0x6666_6666);

        cpu.seed_from_exe(0x8000_3000, 0, None);
        assert_eq!(cpu.read_cache_control(), CACHE_CONTROL_BIOS_NORMAL);
        assert_eq!(cpu.fetch(&mut bus), 0x6666_6666);
    }

    #[test]
    fn seed_from_exe_enables_cop2_like_the_bios_shell() {
        let mut cpu = Cpu::new();
        cpu.cop0[12] = 0x401;

        cpu.seed_from_exe(0x8000_3000, 0, None);

        assert_eq!(cpu.cop0[12], 0x4000_0401);
    }

    #[test]
    fn nclip_uses_previous_sxy2_x_in_early_pipeline_stage() {
        let mut cpu = Cpu::new();
        let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
        // Scene A, immediately following scene C. Hardware returned 0x7674:
        // current terms plus SY0 * (previous SX2 - current SX1).
        cpu.cop2.write_data(12, 0x006e_0095);
        cpu.cop2.write_data(13, 0xffe2_0094);
        cpu.cop2.write_data(14, 0xffd2_0194); // previous scene-C SXY2
        cpu.gte_nclip_rtpt_history = true;
        cpu.gte_nclip_history_tick = 0;
        cpu.gte_sxy_write_tick[0] = 6;
        cpu.gte_sxy_write_tick[1] = 8;
        cpu.gprs[8] = 0xffde_00dc; // new scene-A SXY2
        cpu.tick = 10;
        cpu.op_mtc2(0x4888_7000, &bus).unwrap();
        cpu.tick = 12;

        cpu.dispatch_cop2(0x4a00_0006, &mut bus).unwrap();

        assert_eq!(cpu.cop2.read_data(24), 0x0000_7674);
    }

    #[test]
    fn nclip_sxy_write_cadence_controls_early_y_forwarding() {
        let run = |write_gap: u64| {
            let mut cpu = Cpu::new();
            let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
            // RTPT-E's settled SXY2 is (-36, -152). Scene A overwrites all
            // three SXY registers before NCLIP using either of the two exact
            // instruction cadences found in the burned executable.
            cpu.cop2.write_data(14, 0xff68_ffdc);
            cpu.gte_nclip_rtpt_history = true;
            for (index, (rd, value)) in [
                (12u8, 0x006e_0095),
                (13u8, 0xffe2_0094),
                (14u8, 0xffde_00dc),
            ]
            .into_iter()
            .enumerate()
            {
                cpu.gprs[8] = value;
                cpu.tick = 10 + index as u64 * write_gap;
                cpu.op_mtc2(0x4888_0000 | (u32::from(rd) << 11), &bus)
                    .unwrap();
            }
            cpu.tick += 1;
            cpu.dispatch_cop2(0x4a00_0006, &mut bus).unwrap();
            cpu.cop2.read_data(24)
        };

        // Packed writes forward the new Y but not X; spaced writes retain
        // both components for the early positive products.
        assert_eq!(run(2), 0xffff_b964);
        assert_eq!(run(3), 0xffff_752c);
    }

    #[test]
    fn nclip_without_rtpt_history_uses_previous_sxy2_y() {
        let mut cpu = Cpu::new();
        let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
        cpu.cop2.write_data(14, 0); // previous SXY2.y
        for (index, (rd, value)) in [
            (12u8, 0x0000_0000),
            (13u8, 0x0000_000a),
            (14u8, 0x000a_0000),
        ]
        .into_iter()
        .enumerate()
        {
            cpu.gprs[8] = value;
            cpu.tick = 10 + index as u64 * 2;
            cpu.op_mtc2(0x4888_0000 | (u32::from(rd) << 11), &bus)
                .unwrap();
        }
        cpu.tick += 1;
        cpu.dispatch_cop2(0x4a00_0006, &mut bus).unwrap();

        assert_eq!(cpu.cop2.read_data(24), 0);
    }

    #[test]
    fn nclip_forwards_y_from_overwritten_transform_sxy2() {
        let mut cpu = Cpu::new();
        let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
        cpu.cop2.write_data(14, 0x007c_001a);
        cpu.gte_sxy2_from_transform = true;
        for (index, (rd, value)) in [
            (12u8, 0x0000_0000),
            (13u8, 0x0000_000a),
            (14u8, 0x000a_0000),
        ]
        .into_iter()
        .enumerate()
        {
            cpu.gprs[8] = value;
            cpu.tick = 10 + index as u64 * 2;
            cpu.op_mtc2(0x4888_0000 | (u32::from(rd) << 11), &bus)
                .unwrap();
        }
        cpu.tick += 1;
        cpu.dispatch_cop2(0x4a00_0006, &mut bus).unwrap();

        assert_eq!(cpu.cop2.read_data(24), 100);
    }

    #[test]
    fn lzcr_is_stale_until_two_instructions_after_the_write() {
        // 2026-08-07 console capture (conformance 0x79 vs 0x7A-0x7D): one
        // intervening instruction still reads the prior count; two settle.
        let mut cpu = Cpu::new();
        let bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
        cpu.cop2.write_data(30, 0x00ff_ffff); // old LZCR = 8
        cpu.gprs[8] = 1; // new LZCR = 31
        cpu.tick = 10;
        cpu.op_mtc2(0x4888_f000, &bus).unwrap();

        cpu.tick = 11;
        assert_eq!(cpu.gte_read_data_latency(&bus, 31), 8);
        cpu.tick = 12;
        assert_eq!(cpu.gte_read_data_latency(&bus, 31), 8);
        cpu.tick = 13;
        assert_eq!(cpu.gte_read_data_latency(&bus, 31), 31);
    }

    #[test]
    fn immediate_mtc2_mac_write_wins_same_component_command_writeback() {
        let mut cpu = Cpu::new();
        let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
        cpu.cop2.write_control(0, 0x1000);
        cpu.cop2.write_control(2, 0x2000);
        cpu.cop2.write_control(4, 0x3000);
        cpu.cop2.write_data(9, 0x0400);
        cpu.cop2.write_data(10, 0x0500);
        cpu.cop2.write_data(11, 0x0600);
        cpu.gprs[8] = 0;
        cpu.tick = 10;
        cpu.op_mtc2(0x4888_d800, &bus).unwrap(); // MTC2 $t0, MAC3
        cpu.tick = 11;

        cpu.dispatch_cop2(0x4a08_000c, &mut bus).unwrap(); // OP sf=1

        assert_eq!(cpu.cop2.read_data(25), 0xffff_fd00);
        assert_eq!(cpu.cop2.read_data(26), 0x0000_0600);
        assert_eq!(cpu.cop2.read_data(27), 0x0000_0000);
    }

    #[test]
    fn immediate_mtc2_ir3_reaches_op_after_first_mac_row() {
        let mut cpu = Cpu::new();
        let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
        cpu.cop2.write_control(0, 0x1000);
        cpu.cop2.write_control(2, 0x2000);
        cpu.cop2.write_control(4, 0x3000);
        cpu.cop2.write_data(9, 0x0400);
        cpu.cop2.write_data(10, 0x0500);
        cpu.cop2.write_data(11, 0x01d2); // architectural SQR result = 466
        cpu.gte_last_mtc2_ir3 = 0x0567; // prior CPU-written SQR input
        cpu.gprs[8] = 0x0600;
        cpu.tick = 10;
        cpu.op_mtc2(0x4888_5800, &bus).unwrap(); // MTC2 $t0, IR3
        cpu.tick = 11;

        cpu.dispatch_cop2(0x4a08_000c, &mut bus).unwrap();

        assert_eq!(cpu.cop2.read_data(25), 0xffff_fbce);
        assert_eq!(cpu.cop2.read_data(26), 0x0000_0600);
        assert_eq!(cpu.cop2.read_data(27), 0xffff_fd00);
    }

    #[test]
    fn hle_flush_cache_invalidates_stale_code() {
        let mut cpu = Cpu::new();
        let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
        bus.enable_hle_bios();
        bus.write32(0x2_4000, 0x7777_7777);
        cpu.cache_control = CACHE_CONTROL_IS1 | (3 << CACHE_CONTROL_IBLKSZ_SHIFT);
        cpu.pc = 0x8002_4000;
        assert_eq!(cpu.fetch(&mut bus), 0x7777_7777);
        bus.write32(0xA002_4000, 0x8888_8888);

        cpu.pc = 0xA0;
        cpu.gprs[9] = 0x44;
        cpu.gprs[31] = 0x8002_4000;
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.pc(), 0x8002_4000);
        assert_eq!(cpu.fetch(&mut bus), 0x8888_8888);
    }

    #[test]
    fn strict_hle_stops_at_an_unimplemented_call_without_side_effects() {
        let mut cpu = Cpu::new();
        let mut bus = Bus::new_without_bios();
        bus.enable_hle_bios();
        bus.set_hle_strict(true);
        cpu.pc = 0xA0;
        cpu.gprs[2] = 0x1234_5678;
        cpu.gprs[9] = 0x3A; // abort
        cpu.gprs[31] = 0x8001_0000;
        let cycles = bus.cycles();
        let err = cpu.step(&mut bus).unwrap_err();
        assert_eq!(
            err,
            ExecutionError::HleUnimplemented {
                table: 'A',
                func: 0x3A,
                name: "abort",
                ra: 0x8001_0000,
            }
        );
        assert_eq!(cpu.pc(), 0xA0);
        assert_eq!(cpu.gprs[2], 0x1234_5678);
        assert_eq!(bus.cycles(), cycles);

        // Without strict mode the same call returns 0 and resumes at $ra.
        bus.set_hle_strict(false);
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.pc(), 0x8001_0000);
        assert_eq!(cpu.gprs[2], 0);
    }

    #[test]
    fn step_executes_lui_and_advances_pc() {
        // lui $t0, 0x0013 → $t0 = 0x0013_0000, PC += 4
        let mut bus = Bus::new(synthetic_bios_with_first_word(0x3C08_0013)).unwrap();
        let mut cpu = Cpu::new();
        let record = cpu.step_traced(&mut bus).expect("lui decodes");

        assert_eq!(record.pc, 0xBFC0_0000);
        assert_eq!(record.instr, 0x3C08_0013);
        assert_eq!(record.gprs[8], 0x0013_0000); // $t0
                                                 // Reset-vector execution is uncached: 32 BIOS word stalls plus
                                                 // the instruction's one issue cycle.
        assert_eq!(record.tick, 33);
        assert_eq!(cpu.pc(), 0xBFC0_0004);
    }

    #[test]
    fn lui_to_r0_is_silently_discarded() {
        // lui $0, 0xDEAD -- writing to $0 must leave it at zero.
        // opcode=0x0F, rt=0, imm=0xDEAD → 0x3C00_DEAD
        let mut bus = Bus::new(synthetic_bios_with_first_word(0x3C00_DEAD)).unwrap();
        let mut cpu = Cpu::new();
        let record = cpu.step_traced(&mut bus).expect("lui to r0 decodes");
        assert_eq!(record.gprs[0], 0);
    }

    #[test]
    fn disabled_cop1_raises_coprocessor_unusable_with_ce() {
        // COP1 is absent and SR.CU1 starts clear.
        let mut bus = Bus::new(synthetic_bios_with_first_word(0x4400_0000)).unwrap();
        let mut cpu = Cpu::new();
        cpu.step(&mut bus)
            .expect("CpU is an architectural exception");
        assert_eq!((cpu.cop0[13] >> 2) & 0x1F, 11);
        assert_eq!((cpu.cop0[13] >> 28) & 3, 1);
        assert_eq!(cpu.cop0[14], 0xBFC0_0000);
        assert_eq!(cpu.pc(), 0x8000_0080);
    }

    #[test]
    fn enabled_absent_coprocessors_and_invalid_fields_are_inert() {
        for (word, cu_bit) in [
            (0x4400_0000, 29), // COP1 valid-looking operation
            (0x4BE0_0000, 30), // COP2 operation field 0x1f
            (0x4C00_0000, 31), // COP3 valid-looking operation
        ] {
            let mut bus = Bus::new(synthetic_bios_with_first_word(word)).unwrap();
            let mut cpu = Cpu::new();
            cpu.cop0[12] |= 1 << cu_bit;
            cpu.step(&mut bus).expect("enabled encoding is inert");
            assert_eq!(cpu.pc(), 0xBFC0_0004);
            assert_eq!((cpu.cop0[13] >> 2) & 0x1F, 0);
        }

        // COP0 is usable in kernel mode even with CU0 clear, including its
        // otherwise unassigned operation fields.
        let mut bus = Bus::new(synthetic_bios_with_first_word(0x43E0_0000)).unwrap();
        let mut cpu = Cpu::new();
        cpu.step(&mut bus)
            .expect("kernel COP0 invalid field is inert");
        assert_eq!(cpu.pc(), 0xBFC0_0004);
    }

    #[test]
    fn coprocessor_memory_opcodes_obey_cu_bits() {
        // SWC0 $1,0($zero) traps with CU0 clear even in kernel mode.
        let mut bus = Bus::new(synthetic_bios_with_first_word(0xE001_0000)).unwrap();
        let mut cpu = Cpu::new();
        cpu.step(&mut bus).expect("disabled SWC0 raises CpU");
        assert_eq!((cpu.cop0[13] >> 2) & 0x1F, 11);
        assert_eq!((cpu.cop0[13] >> 28) & 3, 0);

        // Enabling CU0 accepts the same unsupported data-path operation.
        let mut bus = Bus::new(synthetic_bios_with_first_word(0xE001_0000)).unwrap();
        let mut cpu = Cpu::new();
        cpu.cop0[12] |= 1 << 28;
        cpu.step(&mut bus).expect("enabled SWC0 is inert");
        assert_eq!(cpu.pc(), 0xBFC0_0004);

        // SWC3 reports CE=3 when CU3 is clear.
        let mut bus = Bus::new(synthetic_bios_with_first_word(0xEC01_0000)).unwrap();
        let mut cpu = Cpu::new();
        cpu.step(&mut bus).expect("disabled SWC3 raises CpU");
        assert_eq!((cpu.cop0[13] >> 2) & 0x1F, 11);
        assert_eq!((cpu.cop0[13] >> 28) & 3, 3);
    }

    #[test]
    fn nop_advances_pc_without_side_effects() {
        // SLL $0, $0, 0 encodes a NOP.
        let mut bus = Bus::new(synthetic_bios_with_first_word(0x0000_0000)).unwrap();
        let mut cpu = Cpu::new();
        let record = cpu.step_traced(&mut bus).expect("nop decodes");
        assert_eq!(cpu.pc(), 0xBFC0_0004);
        assert!(record.gprs.iter().all(|&v| v == 0));
    }

    #[test]
    fn ori_zero_extends_immediate() {
        // ori $t0, $t1, 0xABCD; opcode=0x0D, rs=9, rt=8, imm=0xABCD
        // Encoding: (0x0D << 26) | (9 << 21) | (8 << 16) | 0xABCD = 0x352 8ABCD
        let mut bus = Bus::new(synthetic_bios_with_first_word(0x3528_ABCD)).unwrap();
        let mut cpu = Cpu::new();
        cpu.gprs[9] = 0xFFFF_0000; // $t1 = 0xFFFF0000
        let record = cpu.step_traced(&mut bus).expect("ori decodes");
        assert_eq!(record.gprs[8], 0xFFFF_ABCD);
    }

    #[test]
    fn load_delay_squashed_by_same_reg_addiu() {
        // Regression test for the 12.7M-step parity divergence:
        //   lw    $t1, 0($a0)      # stages load delay
        //   addiu $t1, $zero, 1    # delay slot writes $t1 non-load-wise
        //   nop                    # reveals committed $t1
        // R3000 semantics: addiu's write squashes the load's writeback,
        // so after the three instructions $t1 = 1, not the loaded word.
        let mut bios = vec![0u8; memory::bios::SIZE];
        // lw $t1, 0($a0): opcode=0x23, rs=4 ($a0), rt=9 ($t1), offset=0
        bios[0..4].copy_from_slice(&0x8C89_0000u32.to_le_bytes());
        // addiu $t1, $zero, 1: opcode=0x09, rs=0, rt=9, imm=1
        bios[4..8].copy_from_slice(&0x2409_0001u32.to_le_bytes());
        // nop
        bios[8..12].copy_from_slice(&0u32.to_le_bytes());

        let mut bus = Bus::new(bios).unwrap();
        let mut cpu = Cpu::new();
        // $a0 = 0xBFC0_0000 (some RAM-ish address -- LW reads whatever's
        // there, which for this test is the BIOS itself / zeroes).
        // Actual loaded value doesn't matter; what matters is that
        // ADDIU's write survives.
        cpu.gprs[4] = 0xBFC0_0000;

        cpu.step(&mut bus).expect("lw");
        cpu.step(&mut bus).expect("addiu in delay slot");
        let record = cpu.step_traced(&mut bus).expect("nop reveals state");

        assert_eq!(record.gprs[9], 1, "addiu must survive LW's delay");
    }

    #[test]
    fn redux_bios_write_intercept_updates_v0_on_trampoline_delay_slot() {
        let mut bus = Bus::new(synthetic_bios_with_words(&[
            0x0140_0008, // jr $t2
            0x2409_0035, // addiu $t1,$zero,0x35 (B0 write)
        ]))
        .unwrap();
        let mut cpu = Cpu::new();
        cpu.gprs[4] = 1; // stdout
        cpu.gprs[6] = 0x20; // size
        cpu.gprs[10] = 0x0000_00B0; // BIOS B table trampoline

        cpu.step(&mut bus).expect("jr decodes");
        let record = cpu.step_traced(&mut bus).expect("delay slot decodes");

        assert_eq!(record.pc, 0xBFC0_0004);
        assert_eq!(cpu.pc(), 0x0000_00B0);
        assert_eq!(record.gprs[2], 0x20);
    }

    #[test]
    fn addi_traps_on_signed_overflow() {
        // $t0 = 0x7FFFFFFF (i32::MAX). ADDI $t1, $t0, 1 overflows.
        // Post-step: $t1 should be unchanged (not 0x80000000), and
        // CAUSE.ExcCode should read 12 (Overflow).
        let mut bios = vec![0u8; memory::bios::SIZE];
        // lui $t0, 0x7FFF
        bios[0..4].copy_from_slice(&0x3C08_7FFFu32.to_le_bytes());
        // ori $t0, $t0, 0xFFFF → $t0 = 0x7FFFFFFF
        bios[4..8].copy_from_slice(&0x3508_FFFFu32.to_le_bytes());
        // addi $t1, $t0, 1 → overflow
        bios[8..12].copy_from_slice(&0x2109_0001u32.to_le_bytes());

        let mut bus = Bus::new(bios).unwrap();
        let mut cpu = Cpu::new();
        cpu.step(&mut bus).expect("lui");
        cpu.step(&mut bus).expect("ori");
        let exc_count_before = cpu.exception_counts()[12];
        cpu.step(&mut bus).expect("addi does not bubble an Err");
        assert_eq!(cpu.gprs[9], 0, "t1 must remain unchanged on overflow");
        assert_eq!(
            cpu.exception_counts()[12],
            exc_count_before + 1,
            "Overflow (12) exception must have fired"
        );
        let cause = cpu.cop0[13];
        assert_eq!((cause >> 2) & 0x1F, 12, "CAUSE.ExcCode = 12 after trap");
    }

    #[test]
    fn addi_negative_overflow_traps() {
        // $t0 = 0x80000000 (i32::MIN). ADDI $t1, $t0, -1 overflows.
        let mut bios = vec![0u8; memory::bios::SIZE];
        // lui $t0, 0x8000
        bios[0..4].copy_from_slice(&0x3C08_8000u32.to_le_bytes());
        // addi $t1, $t0, -1 (imm = 0xFFFF)
        bios[4..8].copy_from_slice(&0x2109_FFFFu32.to_le_bytes());

        let mut bus = Bus::new(bios).unwrap();
        let mut cpu = Cpu::new();
        cpu.step(&mut bus).expect("lui");
        cpu.step(&mut bus).expect("addi");
        assert_eq!(cpu.gprs[9], 0, "t1 unchanged on negative overflow");
        assert_eq!(cpu.exception_counts()[12], 1);
    }

    #[test]
    fn addi_no_overflow_writes_destination() {
        // Edge: exactly at the boundary (i32::MAX - 1) + 1 = i32::MAX.
        // No overflow; $t1 should receive the result.
        let mut bios = vec![0u8; memory::bios::SIZE];
        // lui $t0, 0x7FFF
        bios[0..4].copy_from_slice(&0x3C08_7FFFu32.to_le_bytes());
        // ori $t0, $t0, 0xFFFE → 0x7FFFFFFE
        bios[4..8].copy_from_slice(&0x3508_FFFEu32.to_le_bytes());
        // addi $t1, $t0, 1 → 0x7FFFFFFF (no overflow)
        bios[8..12].copy_from_slice(&0x2109_0001u32.to_le_bytes());

        let mut bus = Bus::new(bios).unwrap();
        let mut cpu = Cpu::new();
        cpu.step(&mut bus).expect("lui");
        cpu.step(&mut bus).expect("ori");
        cpu.step(&mut bus).expect("addi");
        assert_eq!(cpu.gprs[9], 0x7FFF_FFFF);
        assert_eq!(cpu.exception_counts()[12], 0);
    }

    #[test]
    fn add_traps_on_signed_overflow() {
        // $t0 = 0x7FFFFFFF, $t1 = 1. ADD $t2, $t0, $t1 overflows.
        let mut bios = vec![0u8; memory::bios::SIZE];
        // lui $t0, 0x7FFF
        bios[0..4].copy_from_slice(&0x3C08_7FFFu32.to_le_bytes());
        // ori $t0, $t0, 0xFFFF
        bios[4..8].copy_from_slice(&0x3508_FFFFu32.to_le_bytes());
        // ori $t1, $zero, 1
        bios[8..12].copy_from_slice(&0x3409_0001u32.to_le_bytes());
        // add $t2, $t0, $t1 -- special=0, rs=8, rt=9, rd=10, funct=0x20
        let add = (8u32 << 21) | (9u32 << 16) | (10u32 << 11) | 0x20u32;
        bios[12..16].copy_from_slice(&add.to_le_bytes());

        let mut bus = Bus::new(bios).unwrap();
        let mut cpu = Cpu::new();
        cpu.step(&mut bus).expect("lui");
        cpu.step(&mut bus).expect("ori t0");
        cpu.step(&mut bus).expect("ori t1");
        cpu.step(&mut bus).expect("add");
        assert_eq!(cpu.gprs[10], 0, "t2 unchanged on overflow");
        assert_eq!(cpu.exception_counts()[12], 1);
    }

    #[test]
    fn sub_traps_on_signed_overflow() {
        // $t0 = 0x80000000 (i32::MIN), $t1 = 1. SUB $t2, $t0, $t1 =
        // i32::MIN - 1, which overflows.
        let mut bios = vec![0u8; memory::bios::SIZE];
        // lui $t0, 0x8000
        bios[0..4].copy_from_slice(&0x3C08_8000u32.to_le_bytes());
        // ori $t1, $zero, 1
        bios[4..8].copy_from_slice(&0x3409_0001u32.to_le_bytes());
        // sub $t2, $t0, $t1 -- funct=0x22
        let sub = (8u32 << 21) | (9u32 << 16) | (10u32 << 11) | 0x22u32;
        bios[8..12].copy_from_slice(&sub.to_le_bytes());

        let mut bus = Bus::new(bios).unwrap();
        let mut cpu = Cpu::new();
        cpu.step(&mut bus).expect("lui");
        cpu.step(&mut bus).expect("ori t1");
        cpu.step(&mut bus).expect("sub");
        assert_eq!(cpu.gprs[10], 0, "t2 unchanged on overflow");
        assert_eq!(cpu.exception_counts()[12], 1);
    }

    #[test]
    fn sub_no_overflow_writes_destination() {
        // 10 - 3 = 7 -- ordinary subtract, no trap.
        let mut bios = vec![0u8; memory::bios::SIZE];
        // ori $t0, $zero, 10
        bios[0..4].copy_from_slice(&0x3408_000Au32.to_le_bytes());
        // ori $t1, $zero, 3
        bios[4..8].copy_from_slice(&0x3409_0003u32.to_le_bytes());
        // sub $t2, $t0, $t1
        let sub = (8u32 << 21) | (9u32 << 16) | (10u32 << 11) | 0x22u32;
        bios[8..12].copy_from_slice(&sub.to_le_bytes());

        let mut bus = Bus::new(bios).unwrap();
        let mut cpu = Cpu::new();
        cpu.step(&mut bus).expect("ori t0");
        cpu.step(&mut bus).expect("ori t1");
        cpu.step(&mut bus).expect("sub");
        assert_eq!(cpu.gprs[10], 7);
        assert_eq!(cpu.exception_counts()[12], 0);
    }

    #[test]
    fn should_take_interrupt_skips_gte_cofun_next() {
        // Arrange a bus where IRQ is pending + enabled, and the next
        // instruction at PC is a GTE cofun (top byte 0x4A). The
        // hardware-bug workaround says: don't fire the IRQ.
        let mut bus = Bus::new(synthetic_bios_with_first_word(0x4A00_0001)).unwrap();
        let mut cpu = Cpu::new();
        // Set SR IEc (bit 0) + IM2 (bit 10) so IRQ is unmasked.
        cpu.cop0[12] = 0x401;
        // Raise a hardware IRQ and set the mask so it's pending.
        bus.irq_mut().raise(crate::irq::IrqSource::VBlank);
        bus.irq_mut().write_mask(0x1);
        // PC points at the cofun at the BIOS reset vector.
        assert_eq!(cpu.pc(), 0xBFC0_0000);
        // With a GTE cofun at PC, the workaround should refuse to fire.
        assert!(!cpu.should_take_interrupt(&mut bus));
        // Now change the word to something non-cofun -- same opcode
        // area but bit 25 clear (MFC2): top byte becomes 0x48 which
        // doesn't match the mask. The IRQ should fire.
        bus = Bus::new(synthetic_bios_with_first_word(0x4800_0000)).unwrap();
        bus.irq_mut().raise(crate::irq::IrqSource::VBlank);
        bus.irq_mut().write_mask(0x1);
        assert!(cpu.should_take_interrupt(&mut bus));
    }

    /// A GTE command at `0x8000_1004` behind a nop, interrupts enabled,
    /// vector in RAM. Returns the CPU after `before_gte` has run on it.
    fn gte_irq_fixture(before_gte: impl FnOnce(&mut Bus)) -> (Cpu, Bus) {
        let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
        bus.write32(0x8000_1000, 0); // nop
        bus.write32(0x8000_1004, 0x4A18_0001); // RTPS
        bus.write32(0x8000_1008, 0);
        let mut cpu = Cpu::new();
        cpu.pc = 0x8000_1000;
        cpu.cop0[12] = 0x4000_0401; // CU2, IM2, IEc; BEV clear
        bus.irq_mut().write_mask(0x1);
        // SXY FIFO sentinels: each RTPS shifts SXY1 into SXY0.
        cpu.cop2.write_data(12, 0x5A5A_0001);
        cpu.cop2.write_data(13, 0x5A5A_0002);
        cpu.cop2.write_data(14, 0x5A5A_0003);
        cpu.step(&mut bus).unwrap(); // the nop, which arms the watch
        before_gte(&mut bus);
        cpu.step(&mut bus).unwrap(); // the RTPS
        (cpu, bus)
    }

    #[test]
    fn interrupt_raised_before_a_gte_command_is_taken_after_it_with_epc_on_it() {
        let (cpu, _) = gte_irq_fixture(|bus| bus.irq_mut().raise(crate::irq::IrqSource::VBlank));
        assert_eq!(cpu.cop0[14], 0x8000_1004, "EPC stays on the GTE command");
        assert_eq!(cpu.pc(), 0x8000_0080);
        assert_eq!(cpu.cop2.read_data(12), 0x5A5A_0002, "the command ran once");
    }

    #[test]
    fn gte_command_without_a_new_interrupt_runs_normally() {
        let (cpu, _) = gte_irq_fixture(|_| {});
        assert_eq!(cpu.pc(), 0x8000_1008);
        assert_eq!(cpu.cop2.read_data(12), 0x5A5A_0002);
    }

    /// Run `code` from `0x8000_1000` for `steps` instructions with `$t0`
    /// = 0x7FFF_FFFF and `$t1` = 1; returns (Cause, EPC).
    fn fault_fixture(code: &[u32], steps: usize) -> (u32, u32) {
        let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
        for (i, word) in code.iter().enumerate() {
            bus.write32(0x8000_1000 + 4 * i as u32, *word);
        }
        let mut cpu = Cpu::new();
        cpu.pc = 0x8000_1000;
        cpu.cop0[12] = 0;
        cpu.set_gpr(8, 0x7FFF_FFFF);
        cpu.set_gpr(9, 1);
        for _ in 0..steps {
            cpu.step(&mut bus).unwrap();
        }
        assert_eq!(cpu.pc(), 0x8000_0080, "the fault was taken");
        (cpu.cop0[13], cpu.cop0[14])
    }

    const ADD_OVERFLOW: u32 = 0x0109_5020; // add $t2, $t0, $t1
    const BD: u32 = 1 << 31;

    #[test]
    fn overflow_outside_a_delay_slot_reports_its_own_address() {
        let (cause, epc) = fault_fixture(&[ADD_OVERFLOW], 1);
        assert_eq!((cause >> 2) & 0x1F, 12);
        assert_eq!(cause & BD, 0);
        assert_eq!(epc, 0x8000_1000);
    }

    #[test]
    fn overflow_in_a_not_taken_branch_delay_slot_reports_the_branch() {
        // bne $zero, $zero (never taken); add in its delay slot.
        let (cause, epc) = fault_fixture(&[0x1400_0004, ADD_OVERFLOW], 2);
        assert_eq!((cause >> 2) & 0x1F, 12);
        assert_ne!(cause & BD, 0);
        assert_eq!(epc, 0x8000_1000);
    }

    #[test]
    fn load_address_error_in_a_taken_branch_delay_slot_reports_the_branch() {
        // beq $zero, $zero (always taken); lw $t2, 1($zero) in its slot.
        let (cause, epc) = fault_fixture(&[0x1000_0004, 0x8C0A_0001], 2);
        assert_eq!((cause >> 2) & 0x1F, 4);
        assert_ne!(cause & BD, 0);
        assert_eq!(epc, 0x8000_1000);
    }

    /// Truth-table regression for LWL / LWR / SWL / SWR unaligned
    /// ops. Matches PSX-SPX + PCSX-Redux's `LWL_SHIFT` / `LWL_MASK`
    /// / `LWR_*` / `SWL_*` / `SWR_*` tables exactly. LWL was
    /// previously inverted (shift = (addr & 3) * 8 instead of
    /// (3 - (addr & 3)) * 8), which corrupted every unaligned
    /// word load -- the root cause of an observed commercial stack
    /// corruption after the Sony logo, where the game iterates
    /// strings via lwl/lwr pairs, and one of those overwrote the
    /// saved $ra.
    #[test]
    fn lwl_truth_table_matches_redux() {
        // Redux's canonical tables from r3000a.h:
        //   LWL_MASK  = {0x00FFFFFF, 0x0000FFFF, 0x000000FF, 0x00000000}
        //   LWL_SHIFT = {24, 16, 8, 0}
        // Result formula: rt = (rt & mask) | (mem << shift)
        // For Mem = 0x12345678 (bytes 78 56 34 12 in LE memory),
        // Reg = 0xAABBCCDD:
        //   addr&3=0 → (rt & 0x00FFFFFF) | (mem << 24) = 0x78BBCCDD
        //   addr&3=1 → (rt & 0x0000FFFF) | (mem << 16) = 0x5678CCDD
        //   addr&3=2 → (rt & 0x000000FF) | (mem << 8)  = 0x345678DD
        //   addr&3=3 → (rt & 0x00000000) | (mem << 0)  = 0x12345678
        let mem = 0x1234_5678u32;
        let reg = 0xAABB_CCDDu32;
        let expected = [0x78BB_CCDDu32, 0x5678_CCDD, 0x3456_78DD, 0x1234_5678];
        for aw in 0..=3u32 {
            let shift = (3 - (aw & 3)) * 8;
            let mask = !(0xFFFF_FFFFu32 << shift);
            let result = (reg & mask) | (mem << shift);
            assert_eq!(
                result, expected[aw as usize],
                "LWL addr&3={aw}: got 0x{result:08x}, want 0x{:08x}",
                expected[aw as usize]
            );
        }
    }

    #[test]
    fn lwr_truth_table_matches_redux() {
        // LWR_MASK  = {0x00000000, 0xFF000000, 0xFFFF0000, 0xFFFFFF00}
        // LWR_SHIFT = {0, 8, 16, 24}
        // For Mem = 0x12345678, Reg = 0xAABBCCDD:
        //   addr&3=0 → 0x12345678
        //   addr&3=1 → 0xAA123456
        //   addr&3=2 → 0xAABB1234
        //   addr&3=3 → 0xAABBCC12
        let mem = 0x1234_5678u32;
        let reg = 0xAABB_CCDDu32;
        let expected = [0x1234_5678u32, 0xAA12_3456, 0xAABB_1234, 0xAABB_CC12];
        for aw in 0..=3u32 {
            let shift = (aw & 3) * 8;
            let mask = !(0xFFFF_FFFFu32 >> shift);
            let result = (reg & mask) | (mem >> shift);
            assert_eq!(
                result, expected[aw as usize],
                "LWR addr&3={aw}: got 0x{result:08x}, want 0x{:08x}",
                expected[aw as usize]
            );
        }
    }

    #[test]
    fn partial_word_bus_reads_select_lanes_and_charge_lane_steering() {
        let cpu = Cpu::new();
        let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
        bus.write32(0x1000, 0x1234_5678);

        // Keep this lane-steering test away from the DRAM refresh slot at
        // cycle zero. Refresh contention has its own bus-timing coverage.
        let settle = (100 - bus.cycles()) as u32;
        bus.add_cycles(settle);

        // Main RAM selects lanes for nothing: every form is the six RAM
        // stalls. hwtest v1.22 record 0xCC reads 901 clocks for 64 lwl/lwr
        // pairs on silicon, two plain loads each. The steering cycles belong
        // to the 16-bit SPU bus they were fitted on.
        let start = bus.cycles();
        assert_eq!(cpu.read_lwl_lanes(0x1001, &mut bus), 0x0000_5678);
        assert_eq!(bus.cycles() - start, 6);

        let start = bus.cycles();
        assert_eq!(cpu.read_lwr_lanes(0x1002, &mut bus), 0x1234_0000);
        assert_eq!(bus.cycles() - start, 6);

        let start = bus.cycles();
        assert_eq!(cpu.read_lwl_lanes(0x1003, &mut bus), 0x1234_5678);
        assert_eq!(bus.cycles() - start, 6);

        assert_eq!(Cpu::unaligned_lane_cycles(0x1F80_1DAA, 2), 2);
        assert_eq!(Cpu::unaligned_lane_cycles(0x8000_1000, 2), 0);
        assert_eq!(Cpu::unaligned_lane_cycles(0x1F80_0000, 2), 0);
    }

    /// Cycles for `program` run warm from KSEG0, the way the hwtest warm
    /// harness runs a block: once to fill the I-cache, then timed.
    fn warm_cycles(cpu: &mut Cpu, bus: &mut Bus, program: &[u32]) -> u64 {
        const BASE: u32 = 0x8000_1000;
        cpu.cache_control = CACHE_CONTROL_BIOS_NORMAL;
        for (index, word) in program.iter().enumerate() {
            bus.write32(BASE + index as u32 * 4, *word);
        }
        let mut elapsed = 0;
        for _pass in 0..2 {
            // Clear of the DRAM refresh slot, which has its own coverage.
            let settle = 600 - (bus.cycles() % 515);
            bus.add_cycles(settle as u32 % 515);
            cpu.pc = BASE;
            let start = bus.cycles();
            for _ in program {
                cpu.step(bus).unwrap();
            }
            elapsed = bus.cycles() - start;
        }
        elapsed
    }

    const MULTU_T0_T1: u32 = (8 << 21) | (9 << 16) | 0x19;
    const MFLO_T2: u32 = (10 << 11) | 0x12;

    fn multu_gap_program(gap: usize) -> [u32; 18] {
        let mut program = [0u32; 18];
        program[0] = MULTU_T0_T1;
        program[1 + gap] = MFLO_T2;
        program
    }

    fn limit_oracles(mask: u32, free: &str, wait: &str) -> crate::limits::LimitOracles {
        use crate::limits::{LimitOracles, RangeSet};
        LimitOracles::new(
            mask,
            0,
            RangeSet::parse(free).unwrap(),
            RangeSet::parse(wait).unwrap(),
            RangeSet::default(),
        )
    }

    /// `warm_cycles` on a fresh machine, with `limits` installed.
    fn warm_cycles_with(limits: crate::limits::LimitOracles, program: &[u32]) -> u64 {
        let mut cpu = Cpu::new();
        let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
        bus.set_limit_oracles(limits);
        cpu.gprs[8] = 0x8000_4000;
        cpu.gprs[9] = 0x0001_0041;
        cpu.cop0[12] |= 1 << 30; // COP2 usable
        warm_cycles(&mut cpu, &mut bus, program)
    }

    #[test]
    fn muldiv_oracle_removes_the_hilo_interlock() {
        use crate::limits::MULDIV;
        let program = multu_gap_program(0);
        // rs = 0x8000_4000: the full 13-cycle multiply.
        assert_eq!(
            warm_cycles_with(limit_oracles(0, "", ""), &program[..2]),
            15
        );
        assert_eq!(
            warm_cycles_with(limit_oracles(MULDIV, "", ""), &program[..2]),
            2
        );
    }

    #[test]
    fn gte_oracle_removes_the_command_interlock() {
        use crate::limits::GTE;
        const RTPS: u32 = 0x4A08_0001;
        const MFC2_T2_SXY2: u32 = 0x480A_7000;
        let mut program = std::vec![RTPS, MFC2_T2_SXY2];
        program.extend(std::iter::repeat_n(0, 20));
        assert_eq!(warm_cycles_with(limit_oracles(0, "", ""), &program), 37);
        assert_eq!(warm_cycles_with(limit_oracles(GTE, "", ""), &program), 22);
    }

    #[test]
    fn ram_oracle_makes_loads_and_stores_cost_their_issue_cycle() {
        use crate::limits::RAM;
        let loads = [LW_T1_T0; 8];
        let stores = [SW_ZERO_T0; 8];
        assert_eq!(warm_cycles_with(limit_oracles(0, "", ""), &loads), 56);
        assert_eq!(warm_cycles_with(limit_oracles(RAM, "", ""), &loads), 8);
        assert_eq!(warm_cycles_with(limit_oracles(0, "", ""), &stores), 12);
        assert_eq!(warm_cycles_with(limit_oracles(RAM, "", ""), &stores), 8);
    }

    #[test]
    fn mmio_oracle_makes_status_polls_cost_one_cycle() {
        use crate::limits::MMIO;
        let lw_gpustat = (0x23 << 26) | (8 << 21) | (9 << 16);
        let run = |mask| {
            let mut cpu = Cpu::new();
            let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
            bus.set_limit_oracles(limit_oracles(mask, "", ""));
            let program = [lw_gpustat; 4];
            cpu.gprs[8] = 0x1F80_1814;
            warm_cycles(&mut cpu, &mut bus, &program)
        };
        assert!(run(0) > 4);
        assert_eq!(run(MMIO), 4);
    }

    #[test]
    fn icache_oracle_makes_a_cold_run_cost_its_issue_cycles() {
        use crate::limits::ICACHE;
        let cold = |mask| {
            let mut cpu = Cpu::new();
            let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
            bus.set_limit_oracles(limit_oracles(mask, "", ""));
            cpu.cache_control = CACHE_CONTROL_BIOS_NORMAL;
            for index in 0..8u32 {
                bus.write32(0x8000_1000 + index * 4, ADDIU_T2);
            }
            bus.add_cycles(100);
            cpu.pc = 0x8000_1000;
            let start = bus.cycles();
            for _ in 0..8 {
                cpu.step(&mut bus).unwrap();
            }
            bus.cycles() - start
        };
        assert!(cold(0) > 8);
        assert_eq!(cold(ICACHE), 8);
    }

    #[test]
    fn free_ranges_take_no_time_and_wait_ranges_are_counted() {
        let program = [ADDIU_T2; 8];
        // The program sits at 0x80001000; free its first four words.
        let mut cpu = Cpu::new();
        let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
        bus.set_limit_oracles(limit_oracles(
            0,
            "80001000 80001010 f",
            "80001010 80001020 w",
        ));
        let total = warm_cycles(&mut cpu, &mut bus, &program);
        assert_eq!(total, 4);
        // Both passes: the cold one pays refills inside the wait range too.
        let waited = bus.limits.wait_cycles;
        assert!(waited > 8, "{waited}");
        assert!(bus.limits.skipped_cycles >= 8);
        assert_eq!(bus.limits.free_instructions, 8);
        assert_eq!(bus.limits.thawed_accesses, 0);
        assert_eq!(bus.limits.wait_stalls, [0; 6]); // no cycle profile, no split
                                                    // Counting alone changes nothing.
        let counted = warm_cycles_with(limit_oracles(0, "", "80001000 80001020 w"), &program);
        assert_eq!(
            counted,
            warm_cycles_with(limit_oracles(0, "", ""), &program)
        );
    }

    #[test]
    fn a_free_range_still_pays_for_hardware_reads() {
        // Four GPUSTAT reads in a free range: the issue cycles are free, the
        // MMIO wait states are not.
        let lw_gpustat = (0x23 << 26) | (8 << 21) | (9 << 16);
        let run = |free: &str| {
            let mut cpu = Cpu::new();
            let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
            bus.set_limit_oracles(limit_oracles(0, free, ""));
            cpu.gprs[8] = 0x1F80_1814;
            let cycles = warm_cycles(&mut cpu, &mut bus, &[lw_gpustat; 4]);
            (cycles, bus.limits.thawed_accesses)
        };
        let (plain, _) = run("");
        let (free, thawed) = run("80001000 80001010 f");
        assert_eq!(thawed, 8);
        assert_eq!(free, plain - 4);
    }

    #[test]
    fn wait_ranges_split_their_stalls_when_the_cycle_profile_is_on() {
        let mut cpu = Cpu::new();
        let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
        bus.set_limit_oracles(limit_oracles(0, "", "80001000 80001020 w"));
        cpu.set_cpu_cycle_profile_enabled(true);
        cpu.gprs[8] = 0x8000_4000;
        warm_cycles(&mut cpu, &mut bus, &[LW_T1_T0; 8]);
        let [_, ram_load, ram_store, mmio, gte, muldiv] = bus.limits.wait_stalls;
        assert!(ram_load > 0);
        assert_eq!((ram_store, mmio, gte, muldiv), (0, 0, 0, 0));
        assert_eq!(
            bus.limits.wait_stalls.iter().sum::<u64>() + cpu.cpu_cycle_profile().issue_cycles,
            bus.limits.wait_cycles
        );
    }

    #[test]
    fn a_multiply_read_one_cycle_early_does_not_stall() {
        // hwtest v1.22 gap sweep on a launch PAL console, per repeat of
        // `multu; k nops; mflo` with a small rs (latency 6): 8 clocks at
        // k = 0, 7 at k = 5, 8 at k = 6, 9 at k = 7.
        for (gap, clocks) in [(0usize, 8u64), (5, 7), (6, 8), (7, 9)] {
            let mut cpu = Cpu::new();
            let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
            cpu.gprs[8] = 0x7FF;
            cpu.gprs[9] = 0x0001_0041;
            let program = multu_gap_program(gap);
            let total = warm_cycles(&mut cpu, &mut bus, &program[..gap + 2]);
            assert_eq!(total, clocks, "gap {gap}");
        }
    }

    #[test]
    fn unaligned_ram_stores_cost_two_ordinary_stores() {
        // hwtest v1.22 record 0xCD: 64 `swl; swr` pairs in 266 clocks.
        let mut cpu = Cpu::new();
        let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
        cpu.gprs[8] = 0x8000_2001;
        let swl = (0x2A << 26) | (8 << 21) | (9 << 16) | 3;
        let swr = (0x2E << 26) | (8 << 21) | (9 << 16);
        let pair = warm_cycles(&mut cpu, &mut bus, &[swl, swr]);
        let sw = (0x2B << 26) | (8 << 21) | (9 << 16) | 3;
        let mut cpu = Cpu::new();
        let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
        cpu.gprs[8] = 0x8000_2001;
        let plain = warm_cycles(&mut cpu, &mut bus, &[sw, sw]);
        assert_eq!(pair, plain);
    }

    const ADDIU_T2: u32 = (0x09 << 26) | (10 << 21) | (10 << 16) | 1;
    const SW_ZERO_T0: u32 = (0x2B << 26) | (8 << 21);
    const LW_T1_T0: u32 = (0x23 << 26) | (8 << 21) | (9 << 16);

    /// `turns` x (`access`, then `behind` independent instructions), warm.
    fn access_then(access: u32, behind: usize, turns: usize) -> u64 {
        let mut program = std::vec::Vec::new();
        for _ in 0..turns {
            program.push(access);
            program.extend(std::iter::repeat_n(ADDIU_T2, behind));
        }
        let mut cpu = Cpu::new();
        let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
        cpu.gprs[8] = 0x8000_4000;
        warm_cycles(&mut cpu, &mut bus, &program)
    }

    #[test]
    fn the_write_buffer_hides_a_store_with_work_behind_it() {
        // hwtest v1.23 0x129/0x12A, v1.22 0x1F/0x76: clocks a turn.
        assert_eq!(access_then(SW_ZERO_T0, 1, 16), 16 * 2);
        assert_eq!(access_then(SW_ZERO_T0, 2, 16), 16 * 3);
        assert_eq!(access_then(SW_ZERO_T0, 3, 16), 16 * 4);
        // Back to back: four slots, then one store per completed write.
        // 0x12D has a burst of eight at twelve clocks.
        assert_eq!(access_then(SW_ZERO_T0, 0, 8), 12);
        assert_eq!(access_then(SW_ZERO_T0, 0, 4), 4);
    }

    #[test]
    fn a_ram_load_hides_the_third_to_sixth_instruction_behind_it() {
        // hwtest v1.22/v1.23 0xC8, 0x74, 0x125-0x128, 0xCE: clocks a turn.
        for (behind, clocks) in [
            (0u64, 7u64),
            (1, 8),
            (2, 9),
            (3, 9),
            (4, 9),
            (6, 9),
            (8, 11),
        ] {
            let turns = 8;
            let total = access_then(LW_T1_T0, behind as usize, turns);
            assert_eq!(total, clocks * turns as u64, "{behind} behind");
        }
    }

    #[test]
    fn an_instruction_that_reads_the_loaded_register_is_not_hidden() {
        // `lw t1; addiu x3; addu t3,t1,t1` : the fourth instruction would hide,
        // but it needs the value.
        let addu_t3_t1_t1 = (9 << 21) | (9 << 16) | (11 << 11) | 0x21;
        let dependent = [LW_T1_T0, ADDIU_T2, ADDIU_T2, addu_t3_t1_t1, ADDIU_T2];
        let independent = [LW_T1_T0, ADDIU_T2, ADDIU_T2, ADDIU_T2, ADDIU_T2];
        // A fresh machine each: the I-cache would keep serving the first
        // program to the second.
        let run = |program: &[u32]| {
            let mut cpu = Cpu::new();
            let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
            cpu.gprs[8] = 0x8000_4000;
            warm_cycles(&mut cpu, &mut bus, program)
        };
        let with_use = run(&dependent);
        let without = run(&independent);
        assert_eq!(without, 9);
        assert_eq!(with_use, 11);
    }

    #[test]
    fn a_coprocessor_read_waits_for_the_running_command() {
        // hwtest v1.23 0x130-0x134: RTPS; mfc2; 20 nops is 37 clocks a turn,
        // and 22 (plus the 16 nops) when the read comes after RTPS is done.
        const RTPS: u32 = 0x4A08_0001;
        const MFC2_T2_SXY2: u32 = 0x480A_7000;
        let turn = |nops_before: usize| {
            let mut program = std::vec![RTPS];
            program.extend(std::iter::repeat_n(0, nops_before));
            program.push(MFC2_T2_SXY2);
            program.extend(std::iter::repeat_n(0, 20));
            let mut cpu = Cpu::new();
            let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
            cpu.cop0[12] |= 1 << 30; // COP2 usable
            warm_cycles(&mut cpu, &mut bus, &program)
        };
        assert_eq!(turn(0), 37);
        assert_eq!(turn(16), 22 + 16);
    }

    #[test]
    fn a_load_waits_for_a_streaming_line_fill_and_pays_ram_size_bit_7() {
        // hwtest v1.22 records 0x3C/0x3D: a cold sweep of `lw; nop; lw; nop`
        // lines costs 24 clocks a line with RAM_SIZE bit 7 set and 23 clear
        // (6242 and 5958 for 256 lines, less the refresh slots).
        for (ram_size, clocks) in [(0x0000_0B88u32, 24u64), (0x0000_0B08, 23)] {
            let mut cpu = Cpu::new();
            let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
            bus.write32(0x1F80_1060, ram_size);
            cpu.cache_control = CACHE_CONTROL_BIOS_NORMAL;
            cpu.gprs[25] = 0x8000_4000;
            let lw = (0x23 << 26) | (25 << 21) | (3 << 16);
            for (index, word) in [lw, 0, lw, 0].into_iter().enumerate() {
                bus.write32(0x8000_1000 + index as u32 * 4, word);
            }
            bus.add_cycles(100);
            cpu.pc = 0x8000_1000;
            let start = bus.cycles();
            for _ in 0..4 {
                cpu.step(&mut bus).unwrap();
            }
            assert_eq!(bus.cycles() - start, clocks, "RAM_SIZE {ram_size:#x}");
        }
    }

    #[test]
    fn cache_control_nostr_makes_a_line_fill_blocking() {
        // hwtest v1.22 records 0xE1/0xE9: a cold sweep of nops costs 9 clocks
        // a line streaming and 13 with NOSTR set.
        for (cache_control, clocks) in [
            (CACHE_CONTROL_BIOS_NORMAL, 9u64),
            (CACHE_CONTROL_BIOS_NORMAL | CACHE_CONTROL_NOSTR, 13),
        ] {
            let mut cpu = Cpu::new();
            let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
            cpu.cache_control = cache_control;
            bus.add_cycles(100);
            cpu.pc = 0x8000_1000;
            let start = bus.cycles();
            for _ in 0..4 {
                cpu.step(&mut bus).unwrap();
            }
            assert_eq!(bus.cycles() - start, clocks);
        }
    }

    #[test]
    fn jumping_out_of_a_streaming_fill_waits_for_it_and_restarts() {
        // A cold `j; nop` leaf against the same leaf warm, entered at word 0,
        // 1 and 2 of its line. Silicon, both consoles: 8, 7 and 5 clocks
        // (records 0x42-0x44 read 26/25/23 against 0x45's 18), and 8.2 a call
        // in the alias ping-pong (0x8C/0x8D).
        let j_far = (0x02 << 26) | ((0x8000_3040u32 & 0x0FFF_FFFF) >> 2);
        for (entry_word, extra) in [(0u32, 8u64), (1, 7), (2, 5)] {
            let mut cpu = Cpu::new();
            let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
            cpu.cache_control = CACHE_CONTROL_BIOS_NORMAL;
            let leaf = 0x8000_1000 + entry_word * 4;
            bus.write32(leaf, j_far);
            // Make the landing line resident, so only the leaf's line can
            // miss. (0x3040, not 0x3000: that shares the leaf's cache index.)
            cpu.pc = 0x8000_3040;
            for _ in 0..4 {
                cpu.step(&mut bus).unwrap();
            }
            let mut clocks = [0u64; 2];
            for pass in &mut clocks {
                bus.add_cycles(100);
                cpu.pc = leaf;
                let start = bus.cycles();
                for _ in 0..3 {
                    cpu.step(&mut bus).unwrap();
                }
                *pass = bus.cycles() - start;
            }
            assert_eq!(clocks[1], 3, "entry word {entry_word}");
            assert_eq!(clocks[0] - clocks[1], extra, "entry word {entry_word}");
        }
    }

    #[test]
    fn a_multiply_does_not_hide_behind_a_front_loaded_refill() {
        // `multu` as the last word of a line, `mflo` as the first of the next,
        // cold. The missed word lands two clocks after the miss, so the read
        // is three clocks behind the multiply and still stalls: 1 + 2 + 1 + 4
        // for a latency of 6. Charging the whole fill before the first word
        // would have hidden the multiply inside it.
        let mut cpu = Cpu::new();
        let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
        cpu.cache_control = CACHE_CONTROL_BIOS_NORMAL;
        cpu.gprs[8] = 0x7FF;
        cpu.gprs[9] = 0x0001_0041;
        bus.write32(0x8000_100C, MULTU_T0_T1);
        bus.write32(0x8000_1010, MFLO_T2);
        cpu.pc = 0x8000_1000;
        for _ in 0..3 {
            cpu.step(&mut bus).unwrap();
        }
        bus.add_cycles(100);
        let start = bus.cycles();
        cpu.step(&mut bus).unwrap();
        cpu.step(&mut bus).unwrap();
        assert_eq!(bus.cycles() - start, 8);
    }

    #[test]
    fn code_run_through_kseg1_costs_six_clocks_an_instruction() {
        // hwtest v1.22 record 0x1E: 128 nops through KSEG1 in 784 clocks.
        let mut cpu = Cpu::new();
        let mut bus = Bus::new(synthetic_bios_with_first_word(0)).unwrap();
        cpu.cache_control = CACHE_CONTROL_BIOS_NORMAL;
        bus.add_cycles(100);
        cpu.pc = 0xA000_1000;
        let start = bus.cycles();
        for _ in 0..8 {
            cpu.step(&mut bus).unwrap();
        }
        assert_eq!(bus.cycles() - start, 48);
    }

    #[test]
    fn swl_truth_table_matches_redux() {
        // SWL_MASK  = {0xFFFFFF00, 0xFFFF0000, 0xFF000000, 0x00000000}
        // SWL_SHIFT = {24, 16, 8, 0}  (applied as reg >> shift)
        // For Mem = 0xAABBCCDD, Reg = 0x12345678:
        //   addr&3=0 → (AABBCCDD & FFFFFF00) | (12345678 >> 24) = 0xAABBCC12
        //   addr&3=1 → (AABBCCDD & FFFF0000) | (12345678 >> 16) = 0xAABB1234
        //   addr&3=2 → (AABBCCDD & FF000000) | (12345678 >> 8)  = 0xAA123456
        //   addr&3=3 → 0x12345678
        let mem = 0xAABB_CCDDu32;
        let reg = 0x1234_5678u32;
        let expected = [0xAABB_CC12u32, 0xAABB_1234, 0xAA12_3456, 0x1234_5678];
        for aw in 0..=3u32 {
            let shift = (aw & 3) * 8;
            let mask = !(0xFFFF_FFFFu32 >> (24 - shift));
            let result = (mem & mask) | (reg >> (24 - shift));
            assert_eq!(
                result, expected[aw as usize],
                "SWL addr&3={aw}: got 0x{result:08x}, want 0x{:08x}",
                expected[aw as usize]
            );
        }
    }

    #[test]
    fn swr_truth_table_matches_redux() {
        // SWR_MASK  = {0x00000000, 0x000000FF, 0x0000FFFF, 0x00FFFFFF}
        // SWR_SHIFT = {0, 8, 16, 24}  (applied as reg << shift)
        // For Mem = 0xAABBCCDD, Reg = 0x12345678:
        //   addr&3=0 → 0x12345678
        //   addr&3=1 → 0x345678DD
        //   addr&3=2 → 0x5678CCDD
        //   addr&3=3 → 0x78BBCCDD
        let mem = 0xAABB_CCDDu32;
        let reg = 0x1234_5678u32;
        let expected = [0x1234_5678u32, 0x3456_78DD, 0x5678_CCDD, 0x78BB_CCDD];
        for aw in 0..=3u32 {
            let shift = (aw & 3) * 8;
            let mask = !(0xFFFF_FFFFu32 << shift);
            let result = (mem & mask) | (reg << shift);
            assert_eq!(
                result, expected[aw as usize],
                "SWR addr&3={aw}: got 0x{result:08x}, want 0x{:08x}",
                expected[aw as usize]
            );
        }
    }

    /// Step a single load/store at the BIOS reset vector with `$a0`
    /// (rs=4) preset to `base`. Returns the post-step CPU + bus so
    /// callers can inspect COP0 and cycle counters. The opcode word
    /// must use rs=4 so this preset is the effective base register.
    fn step_one_load_store(opword: u32, base: u32) -> (Cpu, Bus) {
        let mut bus = Bus::new(synthetic_bios_with_first_word(opword)).unwrap();
        let mut cpu = Cpu::new();
        cpu.gprs[4] = base;
        cpu.step(&mut bus).expect("op decodes");
        (cpu, bus)
    }

    fn assert_address_error(cpu: &Cpu, expected_code: u32, expected_bad: u32, lw_pc: u32) {
        // CAUSE.ExcCode lives in bits 6..2.
        let exc_code = (cpu.cop0[13] >> 2) & 0x1F;
        assert_eq!(exc_code, expected_code, "ExcCode mismatch");
        assert_eq!(cpu.cop0[8], expected_bad, "BadVaddr mismatch");
        assert_eq!(cpu.cop0[14], lw_pc, "EPC mismatch");
        // SR.BEV is 0 at reset (Cpu::new) -- and Redux's r3000a.cc
        // reset value `0x10900000` also leaves bit 22 clear despite
        // a misleading "BEV = 1" comment. Both sides therefore land
        // on the non-BEV vector for traps fired before SR gets
        // explicitly configured.
        assert_eq!(cpu.pc(), 0x8000_0080, "vector mismatch");
    }

    #[test]
    fn op_lw_misaligned_addr_raises_adel() {
        // LW $t1, 1($a0); base aligned so the +1 offset is what
        // lands the address on a non-word boundary.
        let (cpu, _bus) = step_one_load_store(0x8C89_0001, 0xBFC0_0000);
        assert_address_error(&cpu, 4, 0xBFC0_0001, 0xBFC0_0000);
        // No load delay should have been queued -- the trap fires
        // before the bus access. After one extra step the destination
        // register must still be untouched.
        assert_eq!(cpu.gprs[9], 0, "rt must remain unchanged on AdEL");
    }

    #[test]
    fn op_lh_misaligned_addr_raises_adel() {
        // LH $t1, 1($a0): odd halfword address.
        let (cpu, _bus) = step_one_load_store(0x8489_0001, 0xBFC0_0000);
        assert_address_error(&cpu, 4, 0xBFC0_0001, 0xBFC0_0000);
    }

    #[test]
    fn op_lhu_misaligned_addr_raises_adel() {
        // LHU $t1, 1($a0): odd halfword address.
        let (cpu, _bus) = step_one_load_store(0x9489_0001, 0xBFC0_0000);
        assert_address_error(&cpu, 4, 0xBFC0_0001, 0xBFC0_0000);
    }

    #[test]
    fn op_sw_misaligned_addr_raises_ades() {
        // SW $t1, 1($a0): word write to odd address. Use scratch RAM
        // (0x0000_0000 .. 2 MiB) as the base so a non-trapping SW
        // would actually land somewhere -- proves the trap fired
        // before the bus write rather than just being silently no-op.
        let (cpu, _bus) = step_one_load_store(0xAC89_0001, 0x0000_0000);
        assert_address_error(&cpu, 5, 0x0000_0001, 0xBFC0_0000);
    }

    #[test]
    fn op_sh_misaligned_addr_raises_ades() {
        let (cpu, _bus) = step_one_load_store(0xA489_0001, 0x0000_0000);
        assert_address_error(&cpu, 5, 0x0000_0001, 0xBFC0_0000);
    }

    #[test]
    fn hle_unresolved_exception_uses_guest_thread_frame_and_resumes() {
        let mut bus = Bus::new_without_bios();
        bus.enable_hle_bios();
        bus.write32(crate::hle_bios::UNRESOLVED_HANDLER_PTR, 0x8001_2340);
        let mut cpu = Cpu::new();
        cpu.pc = 0x8001_0000;
        cpu.gprs[5] = 0xCAFE_BABE;

        cpu.raise_address_error(ExceptionCode::AddressErrorStore, 0x1F80_104A, &mut bus);
        assert_eq!(cpu.pending_exception_pc, Some(0x8001_2340));
        assert!(cpu.hle_exception_active);
        // The frame lands in the current thread's TCB, found via [[0x108]].
        let tcb = crate::hle_kernel::current_tcb(&bus);
        assert_ne!(tcb, 0);
        use crate::hle_bios::{TCB_CAUSE, TCB_REGISTERS, TCB_RETURN_PC};
        assert_eq!(bus.read32(tcb + TCB_REGISTERS + 5 * 4), 0xCAFE_BABE);
        assert_eq!(bus.read32(tcb + TCB_RETURN_PC), 0x8001_0000);
        assert_eq!(
            (bus.read32(tcb + TCB_CAUSE) >> 2) & 0x1F,
            ExceptionCode::AddressErrorStore as u32
        );

        // The guest handler skips the faulting instruction before returning.
        bus.write32(tcb + TCB_RETURN_PC, 0x8001_0004);
        cpu.gprs[5] = 0;
        cpu.finish_hle_exception(&mut bus);
        assert_eq!(cpu.pc, 0x8001_0004);
        assert_eq!(cpu.gprs[5], 0xCAFE_BABE);
        assert!(!cpu.hle_exception_active);
        assert_eq!(cpu.isr_depth, 0);
    }

    #[test]
    fn hle_unresolved_exception_ignores_the_default_a40_slot() {
        let mut bus = Bus::new_without_bios();
        bus.enable_hle_bios();
        let mut cpu = Cpu::new();
        cpu.pc = 0x8001_0000;
        cpu.raise_address_error(ExceptionCode::AddressErrorStore, 0x1F80_104A, &mut bus);
        assert!(!cpu.hle_exception_active);
        assert_eq!(cpu.pending_exception_pc, Some(0x8000_0080));
    }

    #[test]
    fn hle_table_entries_dispatch_to_traps_or_guest_code() {
        let mut bus = Bus::new_without_bios();
        bus.enable_hle_bios();
        let mut cpu = Cpu::new();
        // Direct call of the RAM table entry for A(1Bh) strlen.
        bus.write32(0x8002_0000, u32::from_le_bytes(*b"abc\0"));
        let entry = bus.read32(crate::hle_kernel::A0_TABLE + 4 * 0x1B);
        cpu.pc = entry;
        cpu.gprs[4] = 0x8002_0000;
        cpu.gprs[31] = 0x8001_0000;
        cpu.step(&mut bus).unwrap();
        assert_eq!((cpu.pc(), cpu.gprs[2]), (0x8001_0000, 3));

        // A guest replacement of an entry is jumped to by the vector.
        bus.write32(crate::hle_kernel::A0_TABLE + 4 * 0x1B, 0x8003_0000);
        cpu.pc = 0xA0;
        cpu.gprs[9] = 0x1B;
        cpu.gprs[2] = 0x55;
        cpu.step(&mut bus).unwrap();
        assert_eq!(
            (cpu.pc(), cpu.gprs[2], cpu.gprs[31]),
            (0x8003_0000, 0x55, 0x8001_0000)
        );
    }

    #[test]
    fn hle_longjmp_restores_callee_saved_registers_and_returns_value() {
        let mut bus = Bus::new_without_bios();
        bus.enable_hle_bios();
        let mut cpu = Cpu::new();
        for r in [16usize, 23, 28, 29, 30] {
            cpu.gprs[r] = 0x1000 + r as u32;
        }
        cpu.pc = 0xA0;
        cpu.gprs[9] = 0x13;
        cpu.gprs[4] = 0x8002_0000;
        cpu.gprs[31] = 0x8001_0040;
        cpu.step(&mut bus).unwrap();
        assert_eq!((cpu.pc(), cpu.gprs[2]), (0x8001_0040, 0));

        for r in [16usize, 23, 28, 29, 30] {
            cpu.gprs[r] = 0;
        }
        cpu.pc = 0xA0;
        cpu.gprs[9] = 0x14;
        cpu.gprs[4] = 0x8002_0000;
        cpu.gprs[5] = 0;
        cpu.gprs[31] = 0x8005_0000;
        cpu.step(&mut bus).unwrap();
        assert_eq!(
            (cpu.pc(), cpu.gprs[2]),
            (0x8001_0040, 0),
            "0 is not bumped to 1"
        );
        for r in [16usize, 23, 28, 29, 30] {
            assert_eq!(cpu.gprs[r], 0x1000 + r as u32);
        }
    }

    /// Run the CPU until `pc` reaches `stop` (or a step budget ends).
    fn run_until(cpu: &mut Cpu, bus: &mut Bus, stop: u32, budget: u32) {
        for _ in 0..budget {
            if cpu.pc == stop {
                return;
            }
            cpu.step(bus).unwrap();
        }
        panic!("pc {:#010x} never reached {stop:#010x}", cpu.pc);
    }

    fn put_code(bus: &mut Bus, base: u32, words: &[u32]) {
        for (i, w) in words.iter().enumerate() {
            bus.write32(base + 4 * i as u32, *w);
        }
    }

    #[test]
    fn hle_critical_section_syscalls_go_through_the_guest_exception_handler() {
        let mut bus = Bus::new_without_bios();
        bus.enable_hle_bios();
        let mut cpu = Cpu::new();
        cpu.cop0[12] = 0x401;
        // li a0,1; syscall; nop; li a0,2; syscall; nop; b .; nop
        put_code(
            &mut bus,
            0x8001_0000,
            &[0x2404_0001, 0x0C, 0, 0x2404_0002, 0x0C, 0, 0x1000_FFFF, 0],
        );
        cpu.pc = 0x8001_0000;
        run_until(&mut cpu, &mut bus, 0x8001_000C, 2000);
        assert_eq!(cpu.cop0[12] & 0x401, 0, "EnterCriticalSection");
        assert_eq!(cpu.gprs[2], 1, "IRQs were enabled");
        assert_eq!(cpu.exception_counts()[ExceptionCode::Syscall as usize], 1);
        run_until(&mut cpu, &mut bus, 0x8001_0018, 2000);
        assert_eq!(cpu.cop0[12] & 0x401, 0x401, "ExitCriticalSection");
    }

    #[test]
    fn hle_vblank_irq_delivers_the_root_counter_event_and_returns() {
        let mut bus = Bus::new_without_bios();
        bus.enable_hle_bios();
        let mut cpu = Cpu::new();
        let ex = crate::hle_exceptions::code();
        // Open and enable a mark-ready event for F2000003h/2 (VBlank).
        let ev = crate::hle_exceptions::open_event(&mut bus, 0xF200_0003, 2, 0x2000, 0);
        crate::hle_exceptions::set_event_enabled(&mut bus, ev, true);
        bus.write32(0x1F80_1074, 1); // I_MASK: VBlank
                                     // Spin with IRQs enabled until the event is ready.
        put_code(&mut bus, 0x8001_0000, &[0x1000_FFFF, 0]);
        cpu.pc = 0x8001_0000;
        cpu.gprs[5] = 0xCAFE_BABE;
        cpu.cop0[12] = 0x401;
        let mut delivered = false;
        for _ in 0..3_000_000 {
            cpu.step(&mut bus).unwrap();
            if crate::hle_exceptions::test_event(&mut bus, ev) {
                delivered = true;
                break;
            }
        }
        assert!(delivered, "VBlank event delivered");
        // Back in the loop through ReturnFromException, with the frame
        // restored and the IRQ acknowledged.
        let mut left_through_rfe = false;
        for _ in 0..5000 {
            if cpu.pc == 0x8001_0000 {
                break;
            }
            left_through_rfe |= cpu.pc == ex.return_from_exception;
            cpu.step(&mut bus).unwrap();
        }
        assert!(left_through_rfe);
        assert_eq!(cpu.pc, 0x8001_0000);
        assert_eq!(cpu.gprs[5], 0xCAFE_BABE);
        assert_eq!(cpu.cop0[12] & 0x401, 0x401);
        assert_eq!(bus.read32(0x1F80_1070) & 1, 0, "VBlank acknowledged");
    }

    #[test]
    fn aligned_lw_does_not_raise() {
        // Sanity: aligned access leaves COP0 untouched.
        let (cpu, _bus) = step_one_load_store(0x8C89_0000, 0xBFC0_0000);
        assert_eq!(cpu.cop0[8], 0, "BadVaddr must not be touched");
        assert_eq!(cpu.cop0[13], 0, "Cause must not be touched");
        assert_eq!(cpu.pc(), 0xBFC0_0004, "should advance, not vector");
    }

    #[test]
    fn misaligned_lw_does_not_charge_memory_cycle() {
        // A trapping LW bills the uncached BIOS instruction fetch and its
        // issue cycle, but no data-read stalls because alignment is checked
        // before the memory transaction.
        let (_cpu, bus) = step_one_load_store(0x8C89_0001, 0xBFC0_0000);
        assert_eq!(bus.cycles(), 32 + cycle_cost(0) as u64);
    }

    #[test]
    fn scratchpad_instruction_fetch_raises_ibe_before_icache_lookup() {
        let mut bus = Bus::new_without_bios();
        let mut cpu = Cpu::new();
        cpu.pc = 0x1F80_0000;
        cpu.cache_control = CACHE_CONTROL_BIOS_NORMAL;

        // Seed the matching cache line with a valid NOP. The architectural
        // executable-region check must win even over an apparent cache hit.
        cpu.instruction_cache.write_data(cpu.pc, 0);
        cpu.instruction_cache.write_tag(cpu.pc, 0xF);

        cpu.step(&mut bus).expect("IBE enters the exception vector");

        assert_eq!((cpu.cop0[13] >> 2) & 0x1F, 6);
        assert_eq!(cpu.cop0[8], 0x1F80_0000);
        assert_eq!(cpu.cop0[14], 0x1F80_0000);
        assert_eq!(cpu.pc(), 0x8000_0080);
        assert_eq!(cpu.exception_counts()[6], 1);
    }

    #[test]
    fn instruction_fetch_fault_in_delay_slot_sets_bd_and_branch_epc() {
        let mut bus = Bus::new_without_bios();
        // Last executable word of the 8 MiB RAM mirror: J 0x80001000.
        // Its sequential delay-slot address is 0x80800000, just outside the
        // mirror and therefore raises IBE before the pending jump commits.
        let jump = 0x0800_0000 | ((0x8000_1000u32 >> 2) & 0x03FF_FFFF);
        bus.write32(0x001F_FFFC, jump);
        let mut cpu = Cpu::new();
        cpu.pc = 0x807F_FFFC;
        cpu.cache_control = CACHE_CONTROL_BIOS_NORMAL;

        cpu.step(&mut bus).expect("jump");
        assert_eq!(cpu.pc(), 0x8080_0000);
        cpu.step(&mut bus).expect("delay-slot IBE");

        assert_eq!((cpu.cop0[13] >> 2) & 0x1F, 6);
        assert_ne!(cpu.cop0[13] & (1 << 31), 0, "CAUSE.BD");
        assert_eq!(cpu.cop0[8], 0x8080_0000);
        assert_eq!(cpu.cop0[14], 0x807F_FFFC);
        assert_eq!(cpu.pc(), 0x8000_0080);
    }

    #[test]
    fn exact_icache_refill_event_reports_victim_and_miss_kind() {
        let mut bus = Bus::new_without_bios();
        bus.write32(0x0000_1000, 0);
        bus.write32(0x0000_2000, 0);
        let mut cpu = Cpu::new();
        cpu.cache_control = CACHE_CONTROL_BIOS_NORMAL;
        cpu.set_instruction_cache_event_profile_enabled(true);

        cpu.pc = 0x8000_1000;
        cpu.step(&mut bus).expect("first cached NOP");
        let first = cpu
            .take_instruction_cache_refill_event()
            .expect("first tag refill");
        assert_eq!(first.fetch_pc, 0x8000_1000);
        assert_eq!(first.cache_set, 0);
        assert_eq!(first.incoming_line, 0x0000_1000);
        assert_eq!(first.victim_valid_mask, 0);
        assert_eq!(first.miss_kind, InstructionCacheMissKind::Tag);
        assert_eq!(first.fill_words, 4);

        cpu.pc = 0x8000_2000;
        cpu.step(&mut bus).expect("same-set replacement NOP");
        let second = cpu
            .take_instruction_cache_refill_event()
            .expect("replacement refill");
        assert_eq!(second.incoming_line, 0x0000_2000);
        assert_eq!(second.victim_line, 0x0000_1000);
        assert_eq!(second.victim_valid_mask, 0xF);
        assert_eq!(second.miss_kind, InstructionCacheMissKind::Tag);
        assert!(second.stall_cycles > 0);
    }

    #[test]
    fn instruction_class_profile_counts_delay_width_region_and_units() {
        let mut cpu = Cpu::new();
        cpu.gprs[29] = memory::scratchpad::BASE;

        // LB $t0, 0($sp), followed by a delay-slot NOP observation.
        let lb_sp = (0x20u32 << 26) | (29u32 << 21) | (8u32 << 16);
        cpu.profile_instruction_class(lb_sp, false);
        cpu.profile_instruction_class(0, true);
        // One GTE command, one LUI, one JALR and one MULT.
        cpu.profile_instruction_class(0x4A00_0001, false);
        cpu.profile_instruction_class(0x3C08_1234, false);
        cpu.profile_instruction_class((8u32 << 21) | (31u32 << 11) | 0x09, false);
        cpu.profile_instruction_class((8u32 << 21) | (9u32 << 16) | 0x18, false);

        let profile = cpu.instruction_class_profile();
        assert_eq!(profile.instructions, 6);
        assert_eq!(profile.byte_loads, 1);
        assert_eq!(profile.scratchpad_accesses, 1);
        assert_eq!(profile.sp_relative_loads, 1);
        assert_eq!(profile.nops, 1);
        assert_eq!(profile.delay_slot_instructions, 1);
        assert_eq!(profile.delay_slot_nops, 1);
        assert_eq!(profile.gte_commands, 1);
        assert_eq!(profile.lui, 1);
        assert_eq!(profile.jalr, 1);
        assert_eq!(profile.multiply, 1);
    }
}
