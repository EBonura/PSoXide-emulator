//! Prototype dynamic recompiler for the PSoXide R3000A.
//!
//! [`Jit::step`] is a drop-in replacement for [`Cpu::step`]: it runs one
//! compiled block, or one interpreter step when no block applies, and
//! leaves the machine in exactly the state the interpreter would have
//! reached after the same number of instructions. The design is in
//! `designs/psoxide-jit.md` of the perf workspace; in short:
//!
//! * a block is straight-line code from the I-cache, up to and including a
//!   branch and its delay slot;
//! * register arithmetic and branch decisions run natively (AArch64);
//! * everything with timing or bus semantics (loads, stores, multiply and
//!   divide, COP0/GTE moves) runs through the interpreter's own code via
//!   [`emulator_core::cpu::jit_abi`];
//! * the per-instruction bookkeeping of a run of register-only instructions
//!   (issue cycle, load shadow, fetch flags) is charged ahead of the run,
//!   and only when no scheduler event can fall inside it; otherwise the
//!   block exits and the interpreter steps across the event;
//! * the last instruction of every block goes through the interpreter's
//!   full step, which owns the delay-slot interrupt and event checks.

#![allow(clippy::missing_safety_doc)]

pub mod a64;
mod codebuf;
pub mod decode;
#[doc(hidden)]
pub mod testgen;
pub mod tier;

use a64::{Asm, Cond, ZR};
use codebuf::CodeBuffer;
use decode::{alu_dest, classify, is_store, Alu, Branch, Class};
use emulator_core::cpu::jit_abi::{self, is_gte_command, is_hle_hook};
use emulator_core::cpu::ExecutionError;
use emulator_core::{Bus, Cpu};

/// Longest block, in instructions.
const MAX_BLOCK: usize = 64;
/// Main RAM size; blocks are indexed by physical word modulo this.
const RAM_SIZE: u32 = 2 * 1024 * 1024;
/// End of main RAM and its mirrors.
const RAM_MIRROR_END: u32 = 0x0080_0000;
/// Code buffer size.
const CODE_BYTES: usize = 32 * 1024 * 1024;

// Host registers holding the context during a block.
const X_CPU: u8 = 19;
const X_BUS: u8 = 20;
const X_FRAME: u8 = 21;
pub(crate) const X_GPRS: u8 = 22;

// Frame field offsets (see `Frame`).
const F_PEND_REG: u32 = 0;
const F_PEND_VAL: u32 = 4;
const F_TAKEN: u32 = 8;
const F_TARGET: u32 = 12;

/// Helper result: keep running the block.
const CONTINUE: u32 = 0;
/// Helper result: the CPU state is complete; return to the dispatcher.
const EXIT: u32 = 1;
/// Helper result: the interpreter reported an error (in `Frame::error`).
const ERROR: u32 = 2;
/// Helper result: the block's branch went back to the block's own start
/// and nothing outside the CPU happened; run the block again.
const LOOP: u32 = 3;

/// State shared between generated code and helpers for one block run.
#[repr(C)]
pub struct Frame {
    /// Register of the load in flight (0: none).
    pend_reg: u32,
    /// Value of the load in flight.
    pend_val: u32,
    /// The block's branch was taken.
    taken: u32,
    /// The block's branch target.
    target: u32,
    block: *const Block,
    error: Option<ExecutionError>,
}

impl Frame {
    fn take_pending(&mut self) -> Option<(u8, u32)> {
        let reg = std::mem::take(&mut self.pend_reg);
        (reg != 0).then_some((reg as u8, self.pend_val))
    }

    fn set_pending(&mut self, pending: Option<(u8, u32)>) {
        match pending {
            Some((reg, val)) => {
                self.pend_reg = reg as u32;
                self.pend_val = val;
            }
            None => self.pend_reg = 0,
        }
    }
}

type Entry = unsafe extern "C" fn(*mut Cpu, *mut Bus, *mut Frame, *mut u32) -> u32;

/// How the compiled code runs one instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Native arithmetic or branch, inside an accounted run.
    Native,
    /// Interpreter code for the operation, bookkeeping specialised.
    Plain,
    /// The interpreter's full step (GTE commands and their neighbours).
    Full,
    /// A branch delay slot, through the full step; the block goes on
    /// after it when the branch was not taken.
    Delay,
    /// The block's last instruction (not a delay slot), through the full
    /// step.
    Last,
}

/// One compiled block.
pub struct Block {
    vaddr: u32,
    words: Vec<u32>,
    /// `words` as little-endian bytes, for the RAM comparisons.
    bytes: Vec<u8>,
    /// For each word, the end (exclusive) of the stretch that runs without
    /// passing a branch: up to and including the next delay slot. RAM is
    /// compared one stretch at a time, on entry and when a branch falls
    /// through, so a long block entered for a few words costs a short
    /// check.
    stretch_end: Vec<u16>,
    modes: Vec<Mode>,
    entry: Option<Entry>,
    /// Physical RAM offsets (modulo RAM size) the words occupy.
    lo: u32,
    hi: u32,
    /// The block stopped early because the word at this address was not in
    /// the I-cache yet.
    missing_at: Option<u32>,
    /// I-cache epoch at which every word last matched the cache.
    epoch: u64,
}

/// Whether a block can run as compiled.
enum Check {
    Valid,
    /// Some word is not in the I-cache (or RAM disagrees): let the
    /// interpreter fetch it, keep the block.
    Missing,
    /// The cache holds different code: recompile.
    Changed,
}

/// Counters for reports.
#[derive(Clone, Copy, Debug, Default)]
pub struct JitStats {
    /// Blocks compiled to native code.
    pub compiled: u64,
    /// Candidate blocks too short to compile.
    pub interpret_only: u64,
    /// Native block runs.
    pub runs: u64,
    /// Instructions retired inside native block runs.
    pub native_instructions: u64,
    /// Interpreter steps taken by the dispatcher.
    pub interpreter_steps: u64,
    /// Native runs that made no progress (event due at entry).
    pub no_progress: u64,
    /// Blocks recompiled after their code changed.
    pub invalidated: u64,
    /// Code buffer flushes.
    pub flushes: u64,
    /// Code bytes emitted.
    pub code_bytes: u64,
    /// Guest instructions compiled.
    pub guest_instructions: u64,
    /// Interpreter steps because the CPU state forbade entry (delay slot,
    /// isolated or disabled cache, profiler, limits, fetch stream).
    pub fallback_state: u64,
    /// Interpreter steps outside compilable memory (uncached segment, the
    /// kernel area below 64 KiB, BIOS).
    pub fallback_range: u64,
    /// Interpreter steps because a block word was not in the I-cache or
    /// RAM disagreed with it.
    pub fallback_missing: u64,
    /// Interpreter steps at blocks too short to compile.
    pub fallback_short: u64,
    /// Host nanoseconds spent building and emitting compiled blocks.
    pub compile_ns: u64,
}

/// Install the native tier over the interpreter's block cache: `Cpu::run`
/// then compiles hot blocks and runs them natively. Returns a handle to its
/// counters, or `None` when executable memory cannot be mapped (the
/// interpreter then runs alone). `PSOXIDE_JIT=0` leaves the tier out.
pub fn install_tier(cpu: &mut Cpu) -> Option<std::sync::Arc<std::sync::Mutex<tier::TierStats>>> {
    if std::env::var("PSOXIDE_JIT").is_ok_and(|v| v == "0") {
        return None;
    }
    let (compiler, stats) = tier::TierCompiler::new()?;
    cpu.set_block_compiler(Some(Box::new(compiler)));
    Some(stats)
}

/// The recompiler: block cache plus dispatcher.
pub struct Jit {
    enabled: bool,
    code: CodeBuffer,
    blocks: Vec<Block>,
    /// Physical RAM word -> block index + 1.
    table: Vec<u32>,
    frame: Box<Frame>,
    stats: JitStats,
    /// `PSOXIDE_JIT_PROFILE=1`: interpreter fallback steps per 4 KiB page
    /// of the PC, by reason.
    fallback_pages: Option<std::collections::BTreeMap<(u8, u32), u64>>,
}

fn phys(vaddr: u32) -> u32 {
    vaddr & 0x1FFF_FFFF
}

impl Jit {
    /// A recompiler with an empty cache. `None` when executable memory
    /// cannot be mapped.
    pub fn new() -> Option<Self> {
        Some(Self {
            enabled: true,
            code: CodeBuffer::new(CODE_BYTES)?,
            blocks: Vec::new(),
            table: vec![0; (RAM_SIZE / 4) as usize],
            frame: Box::new(Frame {
                pend_reg: 0,
                pend_val: 0,
                taken: 0,
                target: 0,
                block: std::ptr::null(),
                error: None,
            }),
            stats: JitStats::default(),
            fallback_pages: std::env::var("PSOXIDE_JIT_PROFILE")
                .is_ok_and(|v| v == "1")
                .then(Default::default),
        })
    }

    /// Turn compiled execution on or off; off means every step is the
    /// interpreter's.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Drop every compiled block. Call after replacing the machine state
    /// (loading a save state).
    pub fn reset(&mut self) {
        self.flush();
    }

    /// Counters so far.
    pub fn stats(&self) -> JitStats {
        self.stats
    }

    /// Execute at least one instruction: a compiled block when one applies
    /// at the current PC, otherwise one interpreter step.
    pub fn step(&mut self, cpu: &mut Cpu, bus: &mut Bus) -> Result<(), ExecutionError> {
        if !self.enabled {
            return cpu.step(bus);
        }
        if !(cpu.jit_can_enter(bus) && bus.jit_stream_settled()) {
            self.stats.fallback_state += 1;
        } else if let Some(index) = self.find(cpu, bus, cpu.pc()) {
            let before = cpu.tick();
            let status = self.run_block(index, cpu, bus);
            let retired = cpu.tick() - before;
            self.stats.native_instructions += retired;
            if status == ERROR {
                return Err(self
                    .frame
                    .error
                    .take()
                    .expect("an erroring helper stores its error"));
            }
            if retired != 0 {
                return Ok(());
            }
            self.stats.no_progress += 1;
        }
        self.stats.interpreter_steps += 1;
        if let Some(pages) = self.fallback_pages.as_mut() {
            let state = cpu.jit_debug_state();
            let reason = if state.pending_pc.is_some() || state.branch_delay_next {
                0
            } else if !bus.jit_stream_settled() {
                2
            } else if !cpu.jit_can_enter(bus) {
                3
            } else {
                1
            };
            let key = if std::env::var_os("PSOXIDE_JIT_PROFILE_PC").is_some() {
                cpu.pc()
            } else {
                cpu.pc() >> 12
            };
            *pages.entry((reason, key)).or_default() += 1;
        }
        cpu.step(bus)
    }

    /// Interpreter fallback steps by (reason, PC page), heaviest first:
    /// 0 delay slot, 1 no block, 2 fetch stream, 3 other CPU state. Empty unless
    /// `PSOXIDE_JIT_PROFILE=1`.
    pub fn fallback_profile(&self) -> Vec<((u8, u32), u64)> {
        let mut out: Vec<_> = self
            .fallback_pages
            .iter()
            .flatten()
            .map(|(k, v)| (*k, *v))
            .collect();
        out.sort_by(|a, b| b.1.cmp(&a.1));
        out
    }

    /// Run until something outside the CPU may have happened: stop after
    /// the first instruction whose bus activity reached the next scheduler
    /// event or SPU sample (a frame boundary among them), or after about
    /// `budget` instructions. Between those points the host has nothing to
    /// observe, so this is equivalent to calling [`Jit::step`] (or
    /// [`Cpu::step`]) in a loop and checking after each call.
    pub fn run(&mut self, cpu: &mut Cpu, bus: &mut Bus, budget: u64) -> Result<(), ExecutionError> {
        let start = cpu.tick();
        loop {
            let due = bus.jit_next_due();
            self.step(cpu, bus)?;
            if bus.cycles() >= due || cpu.tick() - start >= budget {
                return Ok(());
            }
        }
    }

    fn run_block(&mut self, index: usize, cpu: &mut Cpu, bus: &mut Bus) -> u32 {
        let block = &self.blocks[index];
        let entry = block.entry.expect("find returns compiled blocks only");
        self.stats.runs += 1;
        // The first fetch retires a settled line-fill stream.
        bus.jit_settle_stream();
        let frame: &mut Frame = &mut self.frame;
        frame.block = block;
        frame.taken = 0;
        frame.target = 0;
        frame.set_pending(cpu.jit_take_pending_load());
        let cpu_ptr: *mut Cpu = cpu;
        let bus_ptr: *mut Bus = bus;
        // SAFETY: the generated code only touches the CPU register file at
        // `gprs_offset` and the frame, and passes the same pointers to the
        // helpers, which rebuild the references one at a time.
        unsafe {
            let gprs = cpu_ptr
                .cast::<u8>()
                .add(jit_abi::gprs_offset())
                .cast::<u32>();
            entry(cpu_ptr, bus_ptr, frame, gprs)
        }
    }

    /// The compiled block for `pc`, compiling or revalidating as needed.
    fn find(&mut self, cpu: &Cpu, bus: &Bus, pc: u32) -> Option<usize> {
        let p = phys(pc);
        if pc & 3 != 0 || pc >= 0xA000_0000 || p >= RAM_MIRROR_END {
            self.stats.fallback_range += 1;
            return None;
        }
        let slot = ((p % RAM_SIZE) >> 2) as usize;
        let id = self.table[slot];
        if id != 0 {
            let index = id as usize - 1;
            let block = &mut self.blocks[index];
            if block.vaddr == pc {
                match Self::check(block, cpu, bus) {
                    Check::Missing => {
                        self.stats.fallback_missing += 1;
                        return None;
                    }
                    Check::Valid => {
                        let grown = block
                            .missing_at
                            .is_some_and(|va| cpu.jit_code_word(bus, va).is_some());
                        if !grown {
                            if block.entry.is_none() {
                                self.stats.fallback_short += 1;
                            }
                            return block.entry.is_some().then_some(index);
                        }
                    }
                    Check::Changed => self.stats.invalidated += 1,
                }
            }
        }
        let block = self.compile(cpu, bus, pc)?;
        if block.words.is_empty() {
            self.stats.fallback_missing += 1;
            return None;
        }
        if block.entry.is_none() {
            self.stats.fallback_short += 1;
        }
        let compiled = block.entry.is_some();
        self.blocks.push(block);
        self.table[slot] = self.blocks.len() as u32;
        compiled.then_some(self.blocks.len() - 1)
    }

    fn check(block: &mut Block, cpu: &Cpu, bus: &Bus) -> Check {
        let epoch = cpu.jit_icache_epoch();
        if block.epoch != epoch {
            for (i, &word) in block.words.iter().enumerate() {
                match cpu.jit_cached_word(block.vaddr + 4 * i as u32) {
                    None => return Check::Missing,
                    Some(w) if w != word => return Check::Changed,
                    Some(_) => {}
                }
            }
            block.epoch = epoch;
        }
        if block.entry.is_some()
            && !bus.jit_ram_matches(
                phys(block.vaddr),
                &block.bytes[..4 * block.stretch_end[0] as usize],
            )
        {
            return Check::Missing;
        }
        Check::Valid
    }

    /// Read a block's words from the I-cache and choose each one's mode.
    /// Returns the words, their classes, which ones are delay slots, and
    /// the address of a word that stopped the block by not being cached.
    fn build(cpu: &Cpu, bus: &Bus, pc: u32) -> (Vec<u32>, Vec<Class>, Vec<bool>, Option<u32>) {
        let mut words = Vec::new();
        let mut classes = Vec::new();
        let base = phys(pc) % RAM_SIZE;
        let word_at = |i: usize| -> Option<u32> {
            let va = pc.wrapping_add(4 * i as u32);
            // Never wrap across the end of a RAM mirror.
            if phys(va) % RAM_SIZE != base + 4 * i as u32 {
                return None;
            }
            cpu.jit_code_word(bus, va)
        };
        let mut delay = Vec::new();
        let mut missing_at = None;
        while words.len() < MAX_BLOCK {
            let i = words.len();
            let Some(word) = word_at(i) else {
                missing_at = Some(pc.wrapping_add(4 * i as u32));
                break;
            };
            let class = if is_hle_hook(pc.wrapping_add(4 * i as u32), word) {
                Class::Reject
            } else {
                classify(word)
            };
            match class {
                Class::Reject => break,
                Class::Terminator => {
                    words.push(word);
                    classes.push(class);
                    delay.push(false);
                    break;
                }
                Class::Branch(branch) => {
                    if i + 1 >= MAX_BLOCK {
                        break;
                    }
                    let Some(slot) = word_at(i + 1) else {
                        missing_at = Some(pc.wrapping_add(4 * (i + 1) as u32));
                        break;
                    };
                    let slot_class = if is_hle_hook(pc.wrapping_add(4 * (i + 1) as u32), slot) {
                        Class::Reject
                    } else {
                        classify(slot)
                    };
                    if matches!(slot_class, Class::Branch(_) | Class::Reject)
                        || is_gte_command(slot)
                    {
                        break;
                    }
                    words.extend([word, slot]);
                    classes.extend([class, slot_class]);
                    delay.extend([false, true]);
                    // Blocks could go on past an untaken conditional branch
                    // (`h_delay` supports it), but that measured slower on
                    // games: with the I-cache changing often, entry checks
                    // grow with block length rather than with the words
                    // actually run. Until stretches are validated lazily,
                    // every branch ends the block.
                    let _ = branch;
                    break;
                }
                _ => {
                    words.push(word);
                    classes.push(class);
                    delay.push(false);
                }
            }
        }
        (words, classes, delay, missing_at)
    }

    fn compile(&mut self, cpu: &Cpu, bus: &Bus, pc: u32) -> Option<Block> {
        let started = std::time::Instant::now();
        let block = self.compile_inner(cpu, bus, pc);
        if block.as_ref().is_some_and(|b| b.entry.is_some()) {
            self.stats.compile_ns += started.elapsed().as_nanos() as u64;
        }
        block
    }

    fn compile_inner(&mut self, cpu: &Cpu, bus: &Bus, pc: u32) -> Option<Block> {
        let (words, classes, delay, missing_at) = Self::build(cpu, bus, pc);
        let epoch = cpu.jit_icache_epoch();
        let lo = phys(pc) % RAM_SIZE;
        let hi = lo + 4 * words.len() as u32;
        if words.len() < 2 {
            self.stats.interpret_only += 1;
            return Some(Block {
                vaddr: pc,
                bytes: Vec::new(),
                stretch_end: Vec::new(),
                words,
                modes: Vec::new(),
                entry: None,
                lo,
                hi,
                missing_at,
                epoch,
            });
        }
        let n = words.len();
        let modes: Vec<Mode> = (0..n)
            .map(|i| {
                if delay[i] {
                    return Mode::Delay;
                }
                if i == n - 1 {
                    return Mode::Last;
                }
                let next_is_gte = is_gte_command(words[i + 1]);
                match classes[i] {
                    Class::Alu(_) | Class::Branch(_) if !next_is_gte => Mode::Native,
                    Class::Helper if !next_is_gte => Mode::Plain,
                    _ => Mode::Full,
                }
            })
            .collect();
        let asm = emit(pc, &words, &classes, &modes);
        let entry = match self.code.install(&asm.code) {
            Some(ptr) => ptr,
            None => {
                self.flush();
                self.code.install(&asm.code)?
            }
        };
        self.stats.compiled += 1;
        self.stats.code_bytes += asm.code.len() as u64 * 4;
        self.stats.guest_instructions += n as u64;
        let mut stretch_end = vec![n as u16; n];
        for i in (0..n).rev() {
            if modes[i] == Mode::Delay {
                stretch_end[i] = i as u16 + 1;
            } else if i + 1 < n {
                stretch_end[i] = stretch_end[i + 1];
            }
        }
        Some(Block {
            vaddr: pc,
            bytes: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
            stretch_end,
            words,
            modes,
            // SAFETY: `entry` points at code emitted for the `Entry` ABI.
            entry: Some(unsafe { std::mem::transmute::<*const u8, Entry>(entry) }),
            lo,
            hi,
            missing_at,
            epoch,
        })
    }

    fn flush(&mut self) {
        self.blocks.clear();
        self.table.iter_mut().for_each(|slot| *slot = 0);
        self.code.reset();
        self.stats.flushes += 1;
    }

    /// Code buffer bytes in use.
    pub fn code_bytes_in_use(&self) -> usize {
        self.code.used()
    }

    /// Per-mode instruction counts over the blocks compiled so far
    /// (native, plain helper, full step, delay slot, other last).
    pub fn mode_census(&self) -> [u64; 5] {
        let mut out = [0u64; 5];
        for block in &self.blocks {
            for mode in &block.modes {
                out[*mode as usize] += 1;
            }
        }
        out
    }

    /// Blocks that end in a branch delay slot.
    pub fn branch_blocks(&self) -> usize {
        self.blocks
            .iter()
            .filter(|b| b.modes.last() == Some(&Mode::Delay))
            .count()
    }
}

// ---------------------------------------------------------------------------
// Helpers called from generated code.

/// Whether a store to `addr` leaves the block's assumptions intact: main RAM
/// outside the block's own words, or the scratchpad.
fn store_keeps_block(addr: u32, block: &Block) -> bool {
    if addr >= 0xC000_0000 {
        return false;
    }
    let p = phys(addr);
    if p < RAM_MIRROR_END {
        let off = p % RAM_SIZE;
        return !(block.lo.saturating_sub(3)..block.hi).contains(&off);
    }
    addr < 0xA000_0000 && (0x1F80_0000..0x1F80_0400).contains(&p)
}

fn store_address(cpu: &Cpu, word: u32) -> u32 {
    let rs = ((word >> 21) & 0x1F) as u8;
    cpu.gpr(rs).wrapping_add((word as i16) as i32 as u32)
}

unsafe extern "C" fn h_account(
    cpu: *mut Cpu,
    bus: *mut Bus,
    frame: *mut Frame,
    start: u32,
    len: u32,
) -> u32 {
    // SAFETY: pointers from `Jit::run`, valid for the block's duration.
    let (cpu, bus, frame) = unsafe { (&mut *cpu, &mut *bus, &mut *frame) };
    // SAFETY: the block outlives its run.
    let block = unsafe { &*frame.block };
    let start = start as usize;
    if cpu.jit_account_run(bus, &block.words[start..start + len as usize]) {
        CONTINUE
    } else {
        cpu.jit_exit(block.vaddr + 4 * start as u32, frame.take_pending());
        EXIT
    }
}

/// Shared tail of the mid-block helpers: continue only when the instruction
/// retired normally, dispatched no event and left the block's code alone.
fn after_mid_block(
    cpu: &mut Cpu,
    bus: &Bus,
    frame: &mut Frame,
    pc: u32,
    due: u64,
    store_ok: bool,
) -> u32 {
    let pending = cpu.jit_take_pending_load();
    if store_ok && cpu.pc() == pc.wrapping_add(4) && bus.cycles() < due {
        frame.set_pending(pending);
        CONTINUE
    } else {
        let next = cpu.pc();
        cpu.jit_exit(next, pending);
        EXIT
    }
}

unsafe extern "C" fn h_plain(
    cpu: *mut Cpu,
    bus: *mut Bus,
    frame: *mut Frame,
    index: u32,
    _: u32,
) -> u32 {
    // SAFETY: as in `h_account`.
    let (cpu, bus, frame) = unsafe { (&mut *cpu, &mut *bus, &mut *frame) };
    let block = unsafe { &*frame.block };
    let pc = block.vaddr + 4 * index;
    let word = block.words[index as usize];
    let store_ok = !is_store(word) || store_keeps_block(store_address(cpu, word), block);
    let due = bus.jit_next_due();
    let pending = frame.take_pending();
    if let Err(error) = cpu.jit_step_plain(bus, pc, word, pending) {
        frame.error = Some(error);
        return ERROR;
    }
    after_mid_block(cpu, bus, frame, pc, due, store_ok)
}

unsafe extern "C" fn h_full(
    cpu: *mut Cpu,
    bus: *mut Bus,
    frame: *mut Frame,
    index: u32,
    _: u32,
) -> u32 {
    // SAFETY: as in `h_account`.
    let (cpu, bus, frame) = unsafe { (&mut *cpu, &mut *bus, &mut *frame) };
    let block = unsafe { &*frame.block };
    let pc = block.vaddr + 4 * index;
    let word = block.words[index as usize];
    let store_ok = !is_store(word) || store_keeps_block(store_address(cpu, word), block);
    let due = bus.jit_next_due();
    let pending = frame.take_pending();
    if let Err(error) = cpu.jit_step_cached(bus, pc, word, None, pending) {
        frame.error = Some(error);
        return ERROR;
    }
    after_mid_block(cpu, bus, frame, pc, due, store_ok)
}

unsafe extern "C" fn h_last(
    cpu: *mut Cpu,
    bus: *mut Bus,
    frame: *mut Frame,
    index: u32,
    _: u32,
) -> u32 {
    // SAFETY: as in `h_account`.
    let (cpu, bus, frame) = unsafe { (&mut *cpu, &mut *bus, &mut *frame) };
    let block = unsafe { &*frame.block };
    let pc = block.vaddr + 4 * index;
    let pending = frame.take_pending();
    match cpu.jit_step_cached(bus, pc, block.words[index as usize], None, pending) {
        Ok(()) => EXIT,
        Err(error) => {
            frame.error = Some(error);
            ERROR
        }
    }
}

/// A branch delay slot. Taken: the step ends with the interpreter's
/// delay-slot drain and interrupt check; the block runs again when the
/// target is its own start and nothing outside the CPU happened, otherwise
/// it exits. Not taken: the block goes on unless this was its last word.
unsafe extern "C" fn h_delay(
    cpu: *mut Cpu,
    bus: *mut Bus,
    frame: *mut Frame,
    index: u32,
    last: u32,
) -> u32 {
    // SAFETY: as in `h_account`.
    let (cpu, bus, frame) = unsafe { (&mut *cpu, &mut *bus, &mut *frame) };
    let block = unsafe { &*frame.block };
    let pc = block.vaddr + 4 * index;
    let word = block.words[index as usize];
    let store_ok = !is_store(word) || store_keeps_block(store_address(cpu, word), block);
    let taken = frame.taken != 0;
    let target = frame.target;
    let due = bus.jit_next_due();
    let pending = frame.take_pending();
    if let Err(error) = cpu.jit_step_cached(bus, pc, word, Some(taken.then_some(target)), pending) {
        frame.error = Some(error);
        return ERROR;
    }
    let quiet = store_ok && bus.cycles() < due;
    if taken {
        if quiet && cpu.pc() == target && target == block.vaddr {
            frame.set_pending(cpu.jit_take_pending_load());
            return LOOP;
        }
        return EXIT;
    }
    if last == 0 && quiet && cpu.pc() == pc.wrapping_add(4) {
        // The next stretch has not been compared with RAM yet.
        let (from, to) = (
            index as usize + 1,
            block.stretch_end[index as usize + 1] as usize,
        );
        if bus.jit_ram_matches(
            phys(block.vaddr) + 4 * from as u32,
            &block.bytes[4 * from..4 * to],
        ) {
            frame.set_pending(cpu.jit_take_pending_load());
            return CONTINUE;
        }
    }
    EXIT
}

// ---------------------------------------------------------------------------
// Code generation.

type Helper = unsafe extern "C" fn(*mut Cpu, *mut Bus, *mut Frame, u32, u32) -> u32;

fn call(a: &mut Asm, helper: Helper, arg3: u32, arg4: u32) {
    a.mov_x(0, X_CPU);
    a.mov_x(1, X_BUS);
    a.mov_x(2, X_FRAME);
    a.mov32(3, arg3);
    a.mov32(4, arg4);
    a.mov64(16, helper as usize as u64);
    a.blr(16);
}

pub(crate) fn load_reg(a: &mut Asm, host: u8, guest: u32) {
    if guest == 0 {
        a.movz_w(host, 0, 0);
    } else {
        a.ldr_w(host, X_GPRS, guest * 4);
    }
}

fn store_reg(a: &mut Asm, host: u8, guest: u32) {
    if guest != 0 {
        a.str_w(host, X_GPRS, guest * 4);
    }
}

pub(crate) fn emit_alu(a: &mut Asm, alu: Alu, word: u32) {
    let rs = (word >> 21) & 0x1F;
    let rt = (word >> 16) & 0x1F;
    let rd = (word >> 11) & 0x1F;
    let sa = (word >> 6) & 0x1F;
    let imm = word & 0xFFFF;
    let simm = (word as i16) as i32 as u32;
    match alu {
        Alu::Sll | Alu::Srl | Alu::Sra => {
            load_reg(a, 10, rt);
            if sa == 0 {
                a.mov_w(11, 10);
            } else {
                match alu {
                    Alu::Sll => a.lsl_w_imm(11, 10, sa),
                    Alu::Srl => a.lsr_w_imm(11, 10, sa),
                    _ => a.asr_w_imm(11, 10, sa),
                }
            }
            store_reg(a, 11, rd);
        }
        Alu::Sllv | Alu::Srlv | Alu::Srav => {
            load_reg(a, 9, rs);
            load_reg(a, 10, rt);
            match alu {
                Alu::Sllv => a.lslv_w(11, 10, 9),
                Alu::Srlv => a.lsrv_w(11, 10, 9),
                _ => a.asrv_w(11, 10, 9),
            }
            store_reg(a, 11, rd);
        }
        Alu::Addu | Alu::Subu | Alu::And | Alu::Or | Alu::Xor | Alu::Nor | Alu::Slt | Alu::Sltu => {
            load_reg(a, 9, rs);
            load_reg(a, 10, rt);
            match alu {
                Alu::Addu => a.add_w(11, 9, 10),
                Alu::Subu => a.sub_w(11, 9, 10),
                Alu::And => a.and_w(11, 9, 10),
                Alu::Or => a.orr_w(11, 9, 10),
                Alu::Xor => a.eor_w(11, 9, 10),
                Alu::Nor => {
                    a.orr_w(11, 9, 10);
                    a.orn_w(11, ZR, 11);
                }
                Alu::Slt => {
                    a.cmp_w(9, 10);
                    a.cset_w(11, Cond::Lt);
                }
                _ => {
                    a.cmp_w(9, 10);
                    a.cset_w(11, Cond::Lo);
                }
            }
            store_reg(a, 11, rd);
        }
        Alu::Addiu | Alu::Slti | Alu::Sltiu => {
            load_reg(a, 9, rs);
            a.mov32(10, simm);
            match alu {
                Alu::Addiu => a.add_w(11, 9, 10),
                Alu::Slti => {
                    a.cmp_w(9, 10);
                    a.cset_w(11, Cond::Lt);
                }
                _ => {
                    a.cmp_w(9, 10);
                    a.cset_w(11, Cond::Lo);
                }
            }
            store_reg(a, 11, rt);
        }
        Alu::Andi | Alu::Ori | Alu::Xori => {
            load_reg(a, 9, rs);
            a.mov32(10, imm);
            match alu {
                Alu::Andi => a.and_w(11, 9, 10),
                Alu::Ori => a.orr_w(11, 9, 10),
                _ => a.eor_w(11, 9, 10),
            }
            store_reg(a, 11, rt);
        }
        Alu::Lui => {
            a.mov32(11, imm << 16);
            store_reg(a, 11, rt);
        }
    }
}

fn emit_branch(a: &mut Asm, branch: Branch, word: u32, pc: u32) {
    emit_branch_decision(a, branch, word, pc);
    a.str_w(11, X_FRAME, F_TAKEN);
    a.str_w(12, X_FRAME, F_TARGET);
}

/// A branch's decision: writes the link register (through the squashing
/// path, like `set_gpr`), and leaves w11 = taken (0/1), w12 = target.
pub(crate) fn emit_branch_decision(a: &mut Asm, branch: Branch, word: u32, pc: u32) {
    let rs = (word >> 21) & 0x1F;
    let rt = (word >> 16) & 0x1F;
    let rd = (word >> 11) & 0x1F;
    let jump_target = (pc.wrapping_add(4) & 0xF000_0000) | ((word & 0x03FF_FFFF) << 2);
    let branch_target = pc
        .wrapping_add(4)
        .wrapping_add((((word as i16) as i32) << 2) as u32);
    let link = pc.wrapping_add(8);
    // w11 = taken (0/1), w12 = target.
    match branch {
        Branch::J | Branch::Jal => {
            if branch == Branch::Jal {
                a.mov32(13, link);
                store_reg(a, 13, 31);
            }
            a.movz_w(11, 1, 0);
            a.mov32(12, jump_target);
        }
        Branch::Jr | Branch::Jalr => {
            load_reg(a, 12, rs);
            if branch == Branch::Jalr {
                a.mov32(13, link);
                store_reg(a, 13, rd);
            }
            a.movz_w(11, 1, 0);
        }
        Branch::Beq | Branch::Bne => {
            load_reg(a, 9, rs);
            load_reg(a, 10, rt);
            a.cmp_w(9, 10);
            a.cset_w(
                11,
                if branch == Branch::Beq {
                    Cond::Eq
                } else {
                    Cond::Ne
                },
            );
            a.mov32(12, branch_target);
        }
        Branch::Blez | Branch::Bgtz | Branch::Bltz | Branch::Bgez => {
            load_reg(a, 9, rs);
            a.cmp_w_imm(9, 0);
            a.cset_w(
                11,
                match branch {
                    Branch::Blez => Cond::Le,
                    Branch::Bgtz => Cond::Gt,
                    Branch::Bltz => Cond::Lt,
                    _ => Cond::Ge,
                },
            );
            a.mov32(12, branch_target);
        }
        Branch::Bltzal | Branch::Bgezal => {
            // The link is written before rs is read, as the interpreter does.
            a.mov32(13, link);
            store_reg(a, 13, 31);
            load_reg(a, 9, rs);
            a.cmp_w_imm(9, 0);
            a.cset_w(
                11,
                if branch == Branch::Bltzal {
                    Cond::Lt
                } else {
                    Cond::Ge
                },
            );
            a.mov32(12, branch_target);
        }
    }
}

/// Commit the load in flight after a native instruction, unless that
/// instruction wrote the same register (`dest`).
fn emit_commit(a: &mut Asm, dest: Option<u8>) {
    a.ldr_w(14, X_FRAME, F_PEND_REG);
    let none = a.cbz_w(14);
    let mut squash = None;
    if let Some(d) = dest.filter(|&d| d != 0) {
        a.cmp_w_imm(14, d as u32);
        squash = Some(a.b_cond(Cond::Eq));
    }
    a.ldr_w(15, X_FRAME, F_PEND_VAL);
    a.str_w_idx4(15, X_GPRS, 14);
    if let Some(fixup) = squash {
        a.bind(fixup);
    }
    a.str_w(ZR, X_FRAME, F_PEND_REG);
    a.bind(none);
}

fn emit(pc: u32, words: &[u32], classes: &[Class], modes: &[Mode]) -> Asm {
    let mut a = Asm::default();
    a.stp_x_pre(29, 30, -48);
    a.mov_fp_sp();
    a.stp_x(X_CPU, X_BUS, 16);
    a.stp_x(X_FRAME, X_GPRS, 32);
    a.mov_x(X_CPU, 0);
    a.mov_x(X_BUS, 1);
    a.mov_x(X_FRAME, 2);
    a.mov_x(X_GPRS, 3);
    let top = a.pos();

    let mut exits = Vec::new();
    let mut maybe_pending = true;
    let n = words.len();
    let mut i = 0;
    while i < n {
        match modes[i] {
            Mode::Native => {
                let mut end = i;
                while end < n && modes[end] == Mode::Native {
                    end += 1;
                }
                call(&mut a, h_account, i as u32, (end - i) as u32);
                exits.push(a.cbnz_w(0));
                for j in i..end {
                    let word = words[j];
                    match classes[j] {
                        Class::Alu(alu) => emit_alu(&mut a, alu, word),
                        Class::Branch(branch) => {
                            emit_branch(&mut a, branch, word, pc + 4 * j as u32)
                        }
                        _ => unreachable!("native mode is ALU or branch"),
                    }
                    if maybe_pending {
                        emit_commit(&mut a, alu_dest(classes[j], word));
                        maybe_pending = false;
                    }
                }
                i = end;
            }
            Mode::Plain | Mode::Full => {
                let helper: Helper = if modes[i] == Mode::Plain {
                    h_plain
                } else {
                    h_full
                };
                call(&mut a, helper, i as u32, 0);
                exits.push(a.cbnz_w(0));
                maybe_pending = true;
                i += 1;
            }
            Mode::Delay => {
                let last = i == n - 1;
                call(&mut a, h_delay, i as u32, last as u32);
                a.cmp_w_imm(0, LOOP);
                let again = a.b_cond(Cond::Eq);
                a.bind_to(again, top);
                if !last {
                    exits.push(a.cbnz_w(0));
                }
                maybe_pending = true;
                i += 1;
            }
            Mode::Last => {
                call(&mut a, h_last, i as u32, 0);
                i += 1;
            }
        }
    }
    for fixup in exits {
        a.bind(fixup);
    }
    a.ldp_x(X_CPU, X_BUS, 16);
    a.ldp_x(X_FRAME, X_GPRS, 32);
    a.ldp_x_post(29, 30, 48);
    a.ret();
    a
}
