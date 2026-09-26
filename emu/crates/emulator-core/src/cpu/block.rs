//! Decoded-block cache: the interpreter's front end, shared with the
//! recompiler.
//!
//! A block is straight-line code read out of the instruction cache: up to
//! [`MAX_BLOCK_OPS`] words starting at some PC, ending after the delay slot
//! of the first branch, after a terminator (an instruction that can change
//! the status register or always traps), or before the first word that is
//! not a cache hit. Each word is decoded once into a [`DecodedOp`].
//!
//! A block stays valid while the I-cache lines it was read from are
//! unchanged ([`InstructionCache::generation`]): every word is then still a
//! hit with the same value, so executing from the block is executing what
//! the fetch would have returned. Main RAM is not consulted: like the
//! hardware, the cache keeps serving stale code after RAM changes.
//!
//! Entering a block requires the state in which a fetch is a plain cache
//! hit with nothing to account ([`Cpu::block_entry_ok`]); the per-op
//! executor in `cpu.rs` then runs each op through the interpreter's own
//! back half ([`Cpu::execute_fetched`]).
//!
//! [`InstructionCache::generation`]: super::icache::InstructionCache::generation

use super::*;
use crate::bus::RamStamp;

/// Longest block, in instructions.
pub const MAX_BLOCK_OPS: usize = 64;
/// I-cache lines a block can span (64 words from any alignment).
const MAX_LINES: usize = MAX_BLOCK_OPS / 4 + 1;
/// Main RAM size; blocks are indexed by physical word within one mirror.
const RAM_BYTES: u32 = memory::ram::SIZE as u32;

/// How an instruction may sit in a block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum OpClass {
    /// Register arithmetic that cannot trap: reads and writes GPRs only
    /// (shifts, ADDU/SUBU, logic, SLT*, ADDIU, SLTI*, ANDI/ORI/XORI, LUI).
    Alu,
    /// Branch or jump; the next op is its delay slot and ends the block.
    Branch,
    /// Load from memory (LB, LH, LWL, LW, LBU, LHU, LWR, LWC2).
    Load,
    /// Store to memory (SB, SH, SWL, SW, SWR, SWC2).
    Store,
    /// Everything else that keeps running the block: trapping adds
    /// (ADD/ADDI/SUB), multiply/divide and HI/LO moves, MFC0, GTE register
    /// moves (MFC2/CFC2/MTC2/CTC2).
    Other,
    /// A GTE command. The interrupt-versus-GTE hazard is sampled around it.
    GteCommand,
    /// Ends the block after it runs: SYSCALL, BREAK, COP0 other than MFC0
    /// (MTC0 and RFE change the status register), absent coprocessors.
    Terminator,
}

/// Flag bits of [`DecodedOp::flags`].
pub mod op_flags {
    /// The op sits in the delay slot of the block's branch.
    pub const DELAY_SLOT: u8 = 1 << 0;
    /// The op is the last of its block.
    pub const LAST: u8 = 1 << 1;
    /// Primary opcode `>= 0x20`: the op uses the data bus (loads, stores,
    /// coprocessor memory ops). Ends a load shadow.
    pub const TOUCHES_BUS: u8 = 1 << 2;
    /// The batch executor ([`Cpu::run_fast`]) may run the op: register
    /// arithmetic, branches, CPU loads and stores, and the [`OpClass::Other`]
    /// ops outside delay slots. GTE commands and terminators never.
    ///
    /// [`Cpu::run_fast`]: super::Cpu
    pub const BATCH: u8 = 1 << 3;
    /// The next op in the block is a GTE command (from the cached words;
    /// holds for RAM too while the block's RAM check does).
    pub const NEXT_GTE: u8 = 1 << 4;
}

/// One decoded instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct DecodedOp {
    /// The instruction word, as the I-cache holds it.
    pub word: u32,
    /// What kind of instruction it is.
    pub class: OpClass,
    /// [`op_flags`] bits.
    pub flags: u8,
    /// `rs` field.
    pub rs: u8,
    /// `rt` field.
    pub rt: u8,
    /// Operation within its class, for a one-level dispatch: the SPECIAL
    /// function field plus `0x40` for primary opcode 0, else the primary
    /// opcode.
    pub handler: u8,
}

/// Classify one instruction word for block building. `None`: the word
/// cannot be executed from a block (an encoding the interpreter rejects);
/// the block stops before it and the interpreter reports the error.
pub fn classify(word: u32) -> Option<OpClass> {
    let op = word >> 26;
    let rs = (word >> 21) & 0x1F;
    let rt = (word >> 16) & 0x1F;
    Some(match op {
        0x00 => match word & 0x3F {
            0x00 | 0x02 | 0x03 | 0x04 | 0x06 | 0x07 => OpClass::Alu,
            0x08 | 0x09 => OpClass::Branch,
            0x0C | 0x0D => OpClass::Terminator,
            0x10..=0x13 | 0x18..=0x1B | 0x20 | 0x22 => OpClass::Other,
            0x21 | 0x23..=0x27 | 0x2A | 0x2B => OpClass::Alu,
            _ => return None,
        },
        0x01 => match rt {
            0x00 | 0x01 | 0x10 | 0x11 => OpClass::Branch,
            _ => return None,
        },
        0x02..=0x07 => OpClass::Branch,
        0x08 => OpClass::Other,
        0x09..=0x0F => OpClass::Alu,
        0x10 if rs == 0 => OpClass::Other,
        0x10 => OpClass::Terminator,
        0x12 if word & 0xFE00_0000 == 0x4A00_0000 => OpClass::GteCommand,
        0x12 => OpClass::Other,
        0x11 | 0x13 => OpClass::Terminator,
        0x20..=0x26 | 0x32 => OpClass::Load,
        0x28..=0x2B | 0x2E | 0x3A => OpClass::Store,
        0x30 | 0x31 | 0x33 | 0x38 | 0x39 | 0x3B => OpClass::Terminator,
        _ => return None,
    })
}

/// One block: decoded ops plus the I-cache lines they were read from.
pub struct Block {
    /// Virtual address of the first op.
    pub vaddr: u32,
    /// The ops, in order.
    pub ops: Vec<DecodedOp>,
    /// Opaque handle for a second tier (the recompiler's code for this
    /// block); 0 for none. Cleared whenever the block is decoded again;
    /// kept when revalidation finds the same words.
    pub native: usize,
    /// Times the block was entered. Diagnostic, for tiering decisions.
    pub hits: u32,
    /// Main RAM held exactly the block's words when last checked, and the
    /// RAM pages have not been written since ([`Cpu::block_ram_matches`]).
    ram_matches: bool,
    /// The RAM word after the last op is a GTE command (as last checked).
    after_is_gte: bool,
    ram_stamp: RamStamp,
    /// `(line base physical address, generation)` of every I-cache line
    /// the ops were read from.
    lines: [(u32, u32); MAX_LINES],
    line_count: u8,
    /// I-cache epoch at which the lines were last found unchanged.
    checked_epoch: u64,
}

impl Block {
    /// The RAM word after the last op is a GTE command, as of the last RAM
    /// check ([`Cpu::block_ram_matches`]).
    pub fn after_is_gte(&self) -> bool {
        self.after_is_gte
    }

    /// Whether every I-cache line the block was read from is unchanged.
    ///
    /// A line that changed may have been refilled with the same code (an
    /// eviction and a later miss on the same words): then every word is
    /// compared against the cache and, when all are still hits with the
    /// same value, the block is kept with the new generations instead of
    /// being decoded again.
    #[inline(always)]
    fn current(&mut self, cache: &InstructionCache) -> bool {
        let epoch = cache.epoch();
        if self.checked_epoch == epoch {
            return true;
        }
        let current = self.lines[..self.line_count as usize]
            .iter()
            .all(|&(phys, generation)| cache.generation(phys) == generation);
        if current || self.revalidate(cache) {
            self.checked_epoch = epoch;
            return true;
        }
        false
    }

    #[inline(never)]
    fn revalidate(&mut self, cache: &InstructionCache) -> bool {
        let base = memory::to_physical(self.vaddr);
        let end = base.wrapping_add(4 * self.ops.len() as u32);
        // Only the words in lines that changed can differ.
        for &(line, generation) in &self.lines[..self.line_count as usize] {
            if cache.generation(line) == generation {
                continue;
            }
            let mut phys = line.max(base);
            while phys < (line + 0x10).min(end) {
                let op = &self.ops[(phys.wrapping_sub(base) / 4) as usize];
                if cache.hit(phys) != Some(op.word) {
                    return false;
                }
                phys += 4;
            }
        }
        for (phys, generation) in &mut self.lines[..self.line_count as usize] {
            *generation = cache.generation(*phys);
        }
        true
    }
}

/// Where a running block is: the next op to execute and the PC it is
/// expected at. Any other PC (a taken branch, an exception, a debugger
/// write) means the block was left.
#[derive(Clone, Copy, Default)]
pub(super) struct Cursor {
    /// Block index plus one; zero when outside any block.
    pub block: u32,
    /// Index of the next op.
    pub op: u32,
    /// PC of the next op.
    pub pc: u32,
    /// Cache-control register at entry. A store that changes it (the
    /// I-cache enable) leaves the block.
    pub cache_control: u32,
}

/// Decoded blocks, indexed by the physical word they start at.
#[derive(Default)]
pub struct BlockCache {
    /// Per RAM word: block index plus one, zero for none. Allocated on
    /// first use (zeroed pages cost nothing until touched).
    slots: Vec<u32>,
    blocks: Vec<Block>,
    /// Blocks built (or rebuilt) since creation. Diagnostic.
    pub built: u64,
}

impl BlockCache {
    /// The block at `index`.
    #[inline(always)]
    pub fn block(&self, index: u32) -> &Block {
        &self.blocks[index as usize]
    }

    /// Drop every block.
    pub fn clear(&mut self) {
        self.slots = Vec::new();
        self.blocks.clear();
    }

    /// Number of blocks held.
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Whether no block is held.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

/// Access for a second tier over the block cache (the recompiler). Hidden
/// from the docs: an interface between two parts of the emulator, not an
/// API.
impl Cpu {
    /// The current block starting at `pc`, found, validated against the
    /// I-cache and built as needed (see the module docs); `None` when no
    /// block can start there. Does not enter it.
    #[doc(hidden)]
    pub fn block_find(&mut self, bus: &Bus, pc: u32) -> Option<u32> {
        let cursor = self.cursor;
        let found = self.enter_block(bus, pc).then(|| self.cursor.block - 1);
        self.cursor = cursor;
        found
    }

    /// Block `index` (from [`Cpu::block_find`]).
    #[doc(hidden)]
    pub fn block_get(&self, index: u32) -> &Block {
        self.blocks.block(index)
    }

    /// Set block `index`'s second-tier handle (see [`Block::native`]).
    #[doc(hidden)]
    pub fn block_set_native(&mut self, index: u32, native: usize) {
        self.blocks.blocks[index as usize].native = native;
    }

    /// See [`Cpu::block_ram_matches`].
    #[doc(hidden)]
    pub fn block_ram_check(&mut self, bus: &Bus, index: u32) -> bool {
        self.block_ram_matches(bus, index as usize)
    }
}

impl Cpu {
    /// Whether a fetch at the current PC would be a cache hit with nothing
    /// but a streaming line fill to account ([`Cpu::block_fetch`]), the
    /// precondition for running from a block: the block cache is on, no
    /// profiler wants per-fetch data, the PC is not in a branch delay slot,
    /// and the cache is enabled and not isolated. (No limit oracle is
    /// configured either; callers check that.)
    #[inline(always)]
    pub(super) fn block_entry_ok(&self) -> bool {
        self.block_cache_enabled
            && !self.cpu_cycle_profile_enabled
            && !self.instruction_class_profile_enabled
            && !self.instruction_cache_event_profile_enabled
            && self.pending_pc.is_none()
            && !self.branch_delay_next
            && self.cop0[12] & (1 << 16) == 0
            && self.cache_control & CACHE_CONTROL_IS1 != 0
    }

    /// The fetch of a block op at `pc`: a cache hit, which only has
    /// something to account while a line fill is still streaming in
    /// ([`Bus::streaming_fill_wait`]). Exactly what `fetch_instruction`
    /// does for a hit with no profiler or limit oracle.
    #[inline(always)]
    pub(super) fn block_fetch(bus: &mut Bus, pc: u32) {
        if bus.code_stream_idle() {
            bus.note_cached_fetch(true);
        } else {
            Self::block_fetch_streaming(bus, pc);
        }
        bus.add_zero_cycles();
    }

    #[cold]
    #[inline(never)]
    fn block_fetch_streaming(bus: &mut Bus, pc: u32) {
        let abandon = bus.streaming_fill_wait(memory::to_physical(pc));
        if abandon != 0 {
            bus.add_cycles(abandon);
        }
        bus.note_cached_fetch(abandon == 0);
    }

    /// A batched step's fetch while a line fill is still streaming in: the
    /// wait reads and moves the clock. `false`, having only settled the
    /// clock, when the wait would leave the quiet span: the step is then
    /// left to the interpreter.
    #[cold]
    #[inline(never)]
    fn batch_fetch_streaming(bus: &mut Bus, pc: u32, issue: &mut u64, limit: u64) -> bool {
        bus.advance_quiet(*issue);
        *issue = 0;
        let wait = bus.streaming_fill_wait_peek(memory::to_physical(pc));
        if bus.cycles() + u64::from(wait) + 1 >= limit {
            return false;
        }
        Self::block_fetch_streaming(bus, pc);
        bus.add_zero_cycles();
        true
    }

    /// Find (building if needed) a current block starting at `pc` and point
    /// the cursor at its first op. `false` when no block can start there.
    #[inline(never)]
    pub(super) fn enter_block(&mut self, bus: &Bus, pc: u32) -> bool {
        if pc & 3 != 0 || pc >= 0xA000_0000 {
            return false;
        }
        let phys = memory::to_physical(pc);
        if phys >= memory::ram::MIRROR_END || (bus.hle_bios_enabled && phys < 0x1_0000) {
            return false;
        }
        let slot = ((phys % RAM_BYTES) >> 2) as usize;
        if self.blocks.slots.is_empty() {
            self.blocks.slots = vec![0; (RAM_BYTES / 4) as usize];
        }
        let id = self.blocks.slots[slot];
        let index = if id != 0 && {
            let block = &mut self.blocks.blocks[id as usize - 1];
            block.vaddr == pc && block.current(&self.instruction_cache)
        } {
            id - 1
        } else {
            // A first word that is not a hit (evicted, not yet refilled)
            // cannot start a block now; keep whatever the slot holds for
            // when the line comes back, rather than decoding nothing.
            if self.instruction_cache.hit(phys).is_none() {
                return false;
            }
            let reuse = (id != 0).then(|| id - 1);
            match self.build_block(bus, pc, reuse) {
                Some(index) => {
                    self.blocks.slots[slot] = index + 1;
                    index
                }
                None => return false,
            }
        };
        let block = &mut self.blocks.blocks[index as usize];
        block.hits = block.hits.wrapping_add(1);
        self.cursor = Cursor {
            block: index + 1,
            op: 0,
            pc,
            cache_control: self.cache_control,
        };
        true
    }

    /// Whether main RAM holds exactly block `index`'s words, from a stamp
    /// of the RAM pages taken when last compared (compared again only after
    /// a write to those pages). While it does, the GTE interrupt hazard's
    /// look at "the next instruction in RAM" can use the decoded words
    /// ([`op_flags::NEXT_GTE`], [`Block::after_is_gte`]).
    #[inline]
    pub(super) fn block_ram_matches(&mut self, bus: &Bus, index: usize) -> bool {
        let block = &mut self.blocks.blocks[index];
        let offset = (memory::to_physical(block.vaddr) % RAM_BYTES) as usize;
        let stamp = bus.ram_stamp(offset, 4 * block.ops.len() + 4);
        debug_assert!(
            stamp != block.ram_stamp
                || block.ram_matches
                    == block
                        .ops
                        .iter()
                        .enumerate()
                        .all(|(i, op)| bus.ram_word(offset + 4 * i) == op.word),
            "RAM written without a page count at block {:08x}",
            block.vaddr
        );
        if stamp != block.ram_stamp {
            block.ram_matches = block
                .ops
                .iter()
                .enumerate()
                .all(|(i, op)| bus.ram_word(offset + 4 * i) == op.word);
            let after = block.vaddr.wrapping_add(4 * block.ops.len() as u32);
            block.after_is_gte = bus.peek_is_gte_command(after);
            block.ram_stamp = stamp;
        }
        block.ram_matches
    }

    /// Decode a block at `pc` from the I-cache into slot `reuse` (or a new
    /// slot). `None` when the word at `pc` is not a hit or not executable
    /// from a block.
    fn build_block(&mut self, bus: &Bus, pc: u32, reuse: Option<u32>) -> Option<u32> {
        let hle = bus.hle_bios_enabled;
        let base = memory::to_physical(pc);
        let mut block = match reuse {
            Some(index) => {
                let mut old = std::mem::replace(
                    &mut self.blocks.blocks[index as usize],
                    Block {
                        vaddr: 0,
                        ops: Vec::new(),
                        lines: [(0, 0); MAX_LINES],
                        line_count: 0,
                        checked_epoch: 0,
                        native: 0,
                        hits: 0,
                        ram_matches: false,
                        after_is_gte: false,
                        ram_stamp: RamStamp::default(),
                    },
                );
                old.ops.clear();
                old.line_count = 0;
                old.native = 0;
                old.hits = 0;
                old.ram_stamp = RamStamp::default();
                old
            }
            None => Block {
                vaddr: 0,
                ops: Vec::with_capacity(16),
                lines: [(0, 0); MAX_LINES],
                line_count: 0,
                checked_epoch: 0,
                native: 0,
                hits: 0,
                ram_matches: false,
                after_is_gte: false,
                ram_stamp: RamStamp::default(),
            },
        };
        block.vaddr = pc;
        // The word at `i`, when a fetch there is a cache hit inside the same
        // RAM mirror and outside the HLE kernel area.
        let word_at = |cpu: &Cpu, i: usize| -> Option<u32> {
            let phys = base.wrapping_add(4 * i as u32);
            if phys / RAM_BYTES != base / RAM_BYTES || (hle && phys < 0x1_0000) {
                return None;
            }
            cpu.instruction_cache.hit(phys)
        };
        while block.ops.len() < MAX_BLOCK_OPS {
            let i = block.ops.len();
            let Some(word) = word_at(self, i) else { break };
            let Some(class) = classify(word) else { break };
            match class {
                OpClass::Branch => {
                    // A branch needs its delay slot in the same block, and a
                    // branch in a delay slot is left to the interpreter.
                    if i + 1 >= MAX_BLOCK_OPS {
                        break;
                    }
                    let Some(slot) = word_at(self, i + 1) else {
                        break;
                    };
                    let Some(slot_class) = classify(slot) else {
                        break;
                    };
                    if slot_class == OpClass::Branch {
                        break;
                    }
                    block.ops.push(decoded(word, class, 0));
                    block
                        .ops
                        .push(decoded(slot, slot_class, op_flags::DELAY_SLOT));
                    break;
                }
                OpClass::Terminator => {
                    block.ops.push(decoded(word, class, 0));
                    break;
                }
                _ => block.ops.push(decoded(word, class, 0)),
            }
        }
        if block.ops.is_empty() {
            // Keep the slot for a later build, matching no PC meanwhile.
            block.vaddr = 1;
            if let Some(index) = reuse {
                self.blocks.blocks[index as usize] = block;
            }
            return None;
        }
        if let Some(last) = block.ops.last_mut() {
            last.flags |= op_flags::LAST;
        }
        for i in 1..block.ops.len() {
            if block.ops[i].class == OpClass::GteCommand {
                block.ops[i - 1].flags |= op_flags::NEXT_GTE;
            }
        }
        let first_line = base & !0xF;
        let last_line = base.wrapping_add(4 * (block.ops.len() as u32 - 1)) & !0xF;
        let mut line = first_line;
        loop {
            block.lines[block.line_count as usize] =
                (line, self.instruction_cache.generation(line));
            block.line_count += 1;
            if line == last_line {
                break;
            }
            line += 0x10;
        }
        block.checked_epoch = self.instruction_cache.epoch();
        self.blocks.built += 1;
        Some(match reuse {
            Some(index) => {
                self.blocks.blocks[index as usize] = block;
                index
            }
            None => {
                self.blocks.blocks.push(block);
                self.blocks.blocks.len() as u32 - 1
            }
        })
    }
}

impl Cpu {
    /// Run from decoded blocks for as long as nothing but the CPU, RAM, the
    /// scratchpad and the clock can change, and return how many
    /// instructions retired (zero when the op at the PC cannot start such a
    /// stretch; the caller then steps once).
    ///
    /// This is the per-instruction interpreter with the bookkeeping that
    /// provably does nothing inside the stretch done once, chaining from
    /// block to block across branches:
    ///
    /// * every step's tick leaves the clock below [`Bus::quiet_limit`] (the
    ///   next scheduler event or SPU sample, and the end of any GPU span in
    ///   which advancing the clock only decays credit) and below
    ///   `until_cycle`, so a tick's drain and the SPU catch-up at the start
    ///   of a step do nothing, and clock advances add up.
    ///   Issue cycles are added up and applied before anything reads the
    ///   clock (a memory access, a multiply, the branch-boundary work);
    /// * loads and stores go to main RAM or the scratchpad only (checked
    ///   per access; else the stretch stops before the op), which touch no
    ///   device, interrupt or event;
    /// * the interrupt line sampled at the start of each step can only
    ///   change at the branch-boundary work, so it is sampled once per
    ///   stretch between boundaries and counted per step;
    /// * the GTE interrupt hazard is checked per step as in the
    ///   interpreter, and the stretch stops before any step where it would
    ///   sample;
    /// * after a taken branch's delay slot, the branch-boundary work
    ///   (kernel-call intercept, scheduler and CD drain, interrupt check)
    ///   runs exactly as in the interpreter; an interrupt ends the stretch.
    ///
    /// Callers must not need to look at the machine between these steps.
    pub(super) fn run_fast(&mut self, bus: &mut Bus, budget: u64, until_cycle: u64) -> u64 {
        if self.cursor.block == 0 || self.cursor.pc != self.pc {
            self.cursor.block = 0;
            if !(self.block_entry_ok() && self.enter_block(bus, self.pc)) {
                return 0;
            }
        }
        const IRQ_ENABLED: u32 = 0x401;
        let mut done = 0u64;
        // Steps since the interrupt line was last counted.
        let mut uncounted = 0u64;
        // Issue cycles not yet applied to the bus clock.
        let mut issue = 0u64;
        // The fetch flags need setting for a plain cache hit.
        let mut refetch = true;
        let mut limit = bus.quiet_limit().min(until_cycle);
        // The GTE hazard's watch: only the hazard sample sets it, and the
        // batch stops before any step that would sample, so the first step
        // takes it and it stays clear. A watch on that first step with
        // interrupts on means the step samples: leave it to the interpreter.
        if let Some(watch) = self.gte_irq_watch {
            if watch == self.pc && self.cop0[12] & IRQ_ENABLED == IRQ_ENABLED {
                return 0;
            }
            self.gte_irq_watch = None;
        }
        'blocks: loop {
            let irq_enabled = self.cop0[12] & IRQ_ENABLED == IRQ_ENABLED;
            let index = (self.cursor.block - 1) as usize;
            // The hazard reads the next word from the block while RAM
            // matches it; a store in the batch ends that for this block.
            let mut ram_ok = irq_enabled && self.block_ram_matches(bus, index);
            let after_is_gte = self.blocks.blocks[index].after_is_gte;
            let ops = self.blocks.blocks[index].ops.as_ptr();
            loop {
                let pc = self.pc;
                // SAFETY: the cursor indexes an op of this block (it stops at
                // the op flagged LAST), and nothing below touches the block
                // cache until the loop leaves this block.
                let op = unsafe { *ops.add(self.cursor.op as usize) };
                let delay = op.flags & op_flags::DELAY_SLOT != 0;
                if op.flags & op_flags::BATCH == 0
                    || done >= budget
                    || bus.cycles() + issue + 1 >= limit
                {
                    break 'blocks;
                }
                if irq_enabled {
                    let next_gte = if delay {
                        bus.peek_is_gte_command(self.pending_pc.unwrap_or(pc.wrapping_add(4)))
                    } else if ram_ok {
                        if op.flags & op_flags::LAST != 0 {
                            after_is_gte
                        } else {
                            op.flags & op_flags::NEXT_GTE != 0
                        }
                    } else {
                        bus.peek_is_gte_command(pc.wrapping_add(4))
                    };
                    if next_gte {
                        break 'blocks;
                    }
                }
                let memory = matches!(op.class, OpClass::Load | OpClass::Store);
                let mut addr = 0;
                // A device access (I/O, BIOS, expansion) may change anything
                // a caller looks at: run it, then end the batch.
                let mut device = false;
                if memory {
                    addr = self.gpr(op.rs).wrapping_add((op.word as i16) as i32 as u32);
                    match access_kind(op.word, addr) {
                        Access::Quiet => {}
                        Access::Device => device = true,
                        Access::Unsafe => break 'blocks,
                    }
                }
                // -- the step, as `execute_one_inner` + `execute_fetched` --
                if !bus.code_stream_idle() {
                    if !Self::batch_fetch_streaming(bus, pc, &mut issue, limit) {
                        break 'blocks;
                    }
                    refetch = true;
                } else if refetch {
                    bus.note_cached_fetch(true);
                    refetch = false;
                }
                issue += if self.load_shadow.is_some() && self.hides_in_load_shadow(op.word, bus) {
                    0
                } else {
                    u64::from(cycle_cost(op.word))
                };
                let branch_after_this = if delay { self.pending_pc.take() } else { None };
                self.branch_delay_next = false;
                self.executing_in_branch_delay = delay;
                let loading = self.pending_load.is_some();
                if loading {
                    self.committing_load = self.pending_load.take();
                }
                match op.class {
                    OpClass::Alu | OpClass::Branch => self.execute_register_op(op, pc),
                    OpClass::Load | OpClass::Store => {
                        bus.advance_quiet(issue);
                        issue = 0;
                        self.execute_memory_op(op, addr, bus);
                        if op.class == OpClass::Store {
                            ram_ok = false;
                        }
                    }
                    _ => {
                        // Multiply/divide, HI/LO and GTE moves read the clock.
                        bus.advance_quiet(issue);
                        issue = 0;
                        self.pc = pc;
                        let _ = self.execute(op.word, pc, false, bus);
                    }
                }
                self.executing_in_branch_delay = false;
                if memory && bus.take_ram_load_from_cached_code() {
                    self.load_shadow = Some(LoadShadow {
                        position: 0,
                        register: op.rt,
                    });
                }
                if loading {
                    if let Some((reg, value)) = self.committing_load.take() {
                        let i = (reg & 31) as usize;
                        if i != 0 {
                            self.gprs[i] = value;
                        }
                    }
                }
                self.tick += 1;
                done += 1;
                uncounted += 1;
                if let Some(vector) = self.pending_exception_pc.take() {
                    // An `Other` op trapped (overflow, coprocessor
                    // unusable); never in a delay slot. Entering the
                    // exception left the block.
                    self.pc = vector;
                    break 'blocks;
                }
                self.branch_delay_next = op.class == OpClass::Branch;
                if let Some(target) = branch_after_this {
                    // Taken branch: the branch-boundary work reads the clock
                    // and may raise interrupts; settle everything first.
                    self.pc = target;
                    bus.advance_quiet(issue);
                    issue = 0;
                    if bus.external_interrupt_pending_quiet(uncounted) {
                        self.irq_line_high_steps =
                            self.irq_line_high_steps.saturating_add(uncounted);
                    }
                    uncounted = 0;
                    self.cursor.block = 0;
                    self.apply_redux_bios_kernel_call_intercept();
                    if !bus.post_op_quiet() {
                        bus.drain_scheduler_events_post_op();
                        limit = bus.quiet_limit().min(until_cycle);
                    }
                    if self.should_take_interrupt(bus) {
                        self.should_take_interrupt_steps =
                            self.should_take_interrupt_steps.saturating_add(1);
                        self.enter_exception(ExceptionCode::Interrupt, self.pc, false);
                        self.pc = self
                            .pending_exception_pc
                            .take()
                            .expect("enter_exception staged a vector");
                        break 'blocks;
                    }
                    if device {
                        break 'blocks;
                    }
                    if !self.enter_block(bus, self.pc) {
                        break 'blocks;
                    }
                    continue 'blocks;
                }
                self.pc = pc.wrapping_add(4);
                if op.flags & op_flags::LAST != 0 {
                    self.cursor.block = 0;
                    if device {
                        break 'blocks;
                    }
                    if !self.enter_block(bus, self.pc) {
                        break 'blocks;
                    }
                    continue 'blocks;
                }
                self.cursor.op += 1;
                self.cursor.pc = self.pc;
                if device {
                    break 'blocks;
                }
            }
        }
        bus.advance_quiet(issue);
        if uncounted != 0 && bus.external_interrupt_pending_quiet(uncounted) {
            self.irq_line_high_steps = self.irq_line_high_steps.saturating_add(uncounted);
        }
        done
    }

    /// Execute a CPU load or store (primary opcodes `0x20..=0x2E`) to `addr`
    /// (already checked by [`access_kind`]): plain loads and word stores to
    /// main RAM directly (see [`Bus::cpu_ram_load`], [`Bus::cpu_ram_store32`]),
    /// everything else through its `op_*` function.
    #[inline(always)]
    fn execute_memory_op(&mut self, op: DecodedOp, addr: u32, bus: &mut Bus) {
        let instr = op.word;
        if memory::to_physical(addr) < memory::ram::MIRROR_END {
            let value = match instr >> 26 {
                0x20 => Some(bus.cpu_ram_load(addr, 1) as u8 as i8 as i32 as u32),
                0x24 => Some(bus.cpu_ram_load(addr, 1)),
                0x21 => Some(bus.cpu_ram_load(addr, 2) as u16 as i16 as i32 as u32),
                0x25 => Some(bus.cpu_ram_load(addr, 2)),
                0x23 => Some(bus.cpu_ram_load(addr, 4)),
                _ => None,
            };
            if let Some(value) = value {
                if op.rt != 0 {
                    self.pending_load = Some((op.rt, value));
                }
                return;
            }
            if instr >> 26 == 0x2B {
                bus.cpu_ram_store32(addr, self.gpr(op.rt));
                return;
            }
        }
        let _ = match instr >> 26 {
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
            _ => unreachable!("not a CPU load or store: {instr:08x}"),
        };
    }

    /// Execute an [`OpClass::Alu`] or [`OpClass::Branch`] word: the same
    /// `op_*` functions [`Cpu::execute`] dispatches to, none of which can
    /// fail or touch the bus.
    #[inline(always)]
    fn execute_register_op(&mut self, op: DecodedOp, pc: u32) {
        let instr = op.word;
        let _ = match op.handler {
            0x40 => self.op_sll(instr),
            0x42 => self.op_srl(instr),
            0x43 => self.op_sra(instr),
            0x44 => self.op_sllv(instr),
            0x46 => self.op_srlv(instr),
            0x47 => self.op_srav(instr),
            0x48 => self.op_jr(instr, pc),
            0x49 => self.op_jalr(instr, pc),
            0x61 => self.op_addu(instr),
            0x63 => self.op_subu(instr),
            0x64 => self.op_and(instr),
            0x65 => self.op_or(instr),
            0x66 => self.op_xor(instr),
            0x67 => self.op_nor(instr),
            0x6A => self.op_slt(instr),
            0x6B => self.op_sltu(instr),
            0x01 => self.dispatch_regimm(instr, pc),
            0x02 => self.op_j(instr, pc),
            0x03 => self.op_jal(instr, pc),
            0x04 => self.op_beq(instr, pc),
            0x05 => self.op_bne(instr, pc),
            0x06 => self.op_blez(instr, pc),
            0x07 => self.op_bgtz(instr, pc),
            0x09 => self.op_addiu(instr),
            0x0A => self.op_slti(instr),
            0x0B => self.op_sltiu(instr),
            0x0C => self.op_andi(instr),
            0x0D => self.op_ori(instr),
            0x0E => self.op_xori(instr),
            0x0F => self.op_lui(instr),
            _ => unreachable!("not a register-only op: {instr:08x}"),
        };
    }
}

/// How a CPU load or store to `addr` fits in a batch.
enum Access {
    /// Main RAM (any segment) or the scratchpad (cached segments): touches
    /// no device, interrupt or event.
    Quiet,
    /// Any other address below KSEG2: a device, the BIOS or an expansion
    /// region. Runs through the full bus path; the batch ends after it.
    Device,
    /// Misaligned (an address error) or KSEG2 (cache control): left to the
    /// interpreter.
    Unsafe,
}

#[inline(always)]
fn access_kind(word: u32, addr: u32) -> Access {
    let aligned = match word >> 26 {
        0x23 | 0x2B => addr & 3 == 0,
        0x21 | 0x25 | 0x29 => addr & 1 == 0,
        _ => true,
    };
    if !aligned || addr >= 0xC000_0000 {
        return Access::Unsafe;
    }
    let phys = memory::to_physical(addr);
    if phys < memory::ram::MIRROR_END
        || (addr < 0xA000_0000
            && (memory::scratchpad::BASE
                ..memory::scratchpad::BASE + memory::scratchpad::SIZE as u32)
                .contains(&phys))
    {
        Access::Quiet
    } else {
        Access::Device
    }
}

fn decoded(word: u32, class: OpClass, flags: u8) -> DecodedOp {
    let bus_flag = if word >> 26 >= 0x20 {
        op_flags::TOUCHES_BUS
    } else {
        0
    };
    let batch = match class {
        OpClass::Alu | OpClass::Branch => true,
        // CPU loads and stores; not LWC2/SWC2, which go through the GTE.
        OpClass::Load | OpClass::Store => word >> 26 <= 0x2E,
        OpClass::Other => flags & op_flags::DELAY_SLOT == 0,
        OpClass::GteCommand | OpClass::Terminator => false,
    };
    let bus_flag = bus_flag | if batch { op_flags::BATCH } else { 0 };
    DecodedOp {
        word,
        class,
        flags: flags | bus_flag,
        rs: ((word >> 21) & 0x1F) as u8,
        rt: ((word >> 16) & 0x1F) as u8,
        handler: if word >> 26 == 0 {
            0x40 | (word & 0x3F) as u8
        } else {
            (word >> 26) as u8
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(op: u32, rs: u32, rt: u32, rd: u32, sa: u32, funct: u32) -> u32 {
        (op << 26) | (rs << 21) | (rt << 16) | (rd << 11) | (sa << 6) | funct
    }

    fn i(op: u32, rs: u32, rt: u32, imm: i32) -> u32 {
        (op << 26) | (rs << 21) | (rt << 16) | (imm as u32 & 0xFFFF)
    }

    fn digest<T: serde::Serialize>(value: &T) -> Vec<u8> {
        postcard::to_allocvec(value).expect("serialise")
    }

    /// A machine running a loop of loads, stores, byte/half accesses,
    /// multiply, a call and return with work in both delay slots, and an
    /// overflow exception whose handler returns with RFE in a delay slot.
    fn machine(blocks: bool) -> (Cpu, Bus) {
        const T0: u32 = 8;
        const T1: u32 = 9;
        const T2: u32 = 10;
        const T3: u32 = 11;
        const T4: u32 = 12;
        const T5: u32 = 13;
        const K0: u32 = 26;
        const RA: u32 = 31;
        let program = [
            i(0x0F, 0, T0, 0x8002),            // 00 lui t0, 0x8002
            i(0x09, 0, T1, 300),               // 04 addiu t1, zero, 300
            i(0x23, T0, T2, 0),                // 08 loop: lw t2, 0(t0)
            r(0, T2, T1, T3, 0, 0x21),         // 0c addu t3, t2, t1
            i(0x2B, T0, T3, 4),                // 10 sw t3, 4(t0)
            i(0x20, T0, T4, 5),                // 14 lb t4, 5(t0)
            i(0x29, T0, T4, 8),                // 18 sh t4, 8(t0)
            r(0, T1, T3, 0, 0, 0x19),          // 1c multu t1, t3
            r(0, 0, 0, T5, 0, 0x12),           // 20 mflo t5
            (0x03 << 26) | (0x0001_0060 >> 2), // 24 jal sub
            r(0, 0, T5, T5, 1, 0x00),          // 28 sll t5, t5, 1 (delay)
            i(0x09, T1, T1, -1),               // 2c addiu t1, t1, -1
            i(0x05, T1, 0, -10),               // 30 bne t1, zero, loop
            i(0x09, T0, T0, 12),               // 34 addiu t0, t0, 12 (delay)
            i(0x0F, 0, T4, 0x7FFF),            // 38 lui t4, 0x7fff
            i(0x0D, T4, T4, 0xFFFF),           // 3c ori t4, t4, 0xffff
            i(0x08, T4, T5, 1),                // 40 addi t5, t4, 1 (overflow)
            (0x02 << 26) | (0x0001_0000 >> 2), // 44 j start
            0,                                 // 48 nop
            0,
            0,
            0,
            0,
            0,
            r(0, T2, T5, T2, 0, 0x21), // 60 sub: addu t2, t2, t5
            r(0, RA, 0, 0, 0, 0x08),   // 64 jr ra
            r(0, T3, T2, T3, 0, 0x26), // 68 xor t3, t3, t2 (delay)
        ];
        let handler = [
            (0x10 << 26) | (K0 << 16) | (14 << 11), // mfc0 k0, epc
            0,                                      // nop (load delay)
            i(0x09, K0, K0, 4),                     // addiu k0, k0, 4
            r(0, K0, 0, 0, 0, 0x08),                // jr k0
            0x4200_0010,                            // rfe (delay)
        ];
        let bytes = |words: &[u32]| {
            words
                .iter()
                .flat_map(|w| w.to_le_bytes())
                .collect::<Vec<_>>()
        };
        let mut bus = Bus::new_without_bios();
        bus.load_exe_payload(0x8001_0000, &bytes(&program));
        bus.load_exe_payload(0x8000_0080, &bytes(&handler));
        let mut cpu = Cpu::new();
        cpu.set_block_cache_enabled(blocks);
        cpu.seed_from_exe(0x8001_0000, 0, Some(0x801F_FF00));
        (cpu, bus)
    }

    /// `Cpu::run` from decoded blocks, in chunks of every size, leaves the
    /// same machine as the plain interpreter stepping one at a time.
    #[test]
    fn batched_runs_match_single_steps() {
        let (mut plain, mut plain_bus) = machine(false);
        for _ in 0..60_000 {
            plain.step(&mut plain_bus).expect("step");
        }
        // The program went round its loop and took the overflow trap.
        assert!(
            plain.exception_counts()[12] >= 5,
            "{:?}",
            plain.exception_counts()
        );
        for chunk in [1u64, 3, 17, 1000, 60_000] {
            let (mut cpu, mut bus) = machine(true);
            let mut done = 0;
            while done < 60_000 {
                let (ran, result) =
                    cpu.run(&mut bus, chunk.min(60_000 - done), u64::MAX, |_| false);
                result.expect("run");
                done += ran;
            }
            assert!(cpu.blocks_built() > 0, "chunk {chunk}: no blocks were used");
            assert_eq!(digest(&cpu), digest(&plain), "CPU differs, chunk {chunk}");
            assert_eq!(
                digest(&bus),
                digest(&plain_bus),
                "bus differs, chunk {chunk}"
            );
        }
    }

    /// `until_cycle` stops on the same instruction as checking the clock
    /// after every step.
    #[test]
    fn run_stops_at_the_cycle_limit() {
        let (mut plain, mut plain_bus) = machine(false);
        let limit = 150_000;
        let mut steps = 0;
        while plain_bus.cycles() < limit {
            plain.step(&mut plain_bus).expect("step");
            steps += 1;
        }
        let (mut cpu, mut bus) = machine(true);
        let (ran, result) = cpu.run(&mut bus, u64::MAX, limit, |_| false);
        result.expect("run");
        assert_eq!(ran, steps);
        assert_eq!(digest(&cpu), digest(&plain));
        assert_eq!(digest(&bus), digest(&plain_bus));
    }

    #[test]
    fn classes_of_common_encodings() {
        assert_eq!(classify(0x0000_0000), Some(OpClass::Alu)); // nop
        assert_eq!(classify(0x27BD_FFE8), Some(OpClass::Alu)); // addiu sp, sp, -24
        assert_eq!(classify(0x2021_0001), Some(OpClass::Other)); // addi at, at, 1
        assert_eq!(classify(0x8FBF_0010), Some(OpClass::Load)); // lw ra, 16(sp)
        assert_eq!(classify(0xAFBF_0010), Some(OpClass::Store)); // sw ra, 16(sp)
        assert_eq!(classify(0x03E0_0008), Some(OpClass::Branch)); // jr ra
        assert_eq!(classify(0x0C00_0000), Some(OpClass::Branch)); // jal
        assert_eq!(classify(0x4A18_0001), Some(OpClass::GteCommand)); // rtps
        assert_eq!(classify(0x4808_0000), Some(OpClass::Other)); // mfc2
        assert_eq!(classify(0x4000_6000), Some(OpClass::Other)); // mfc0
        assert_eq!(classify(0x4084_6000), Some(OpClass::Terminator)); // mtc0 a0, sr
        assert_eq!(classify(0x4200_0010), Some(OpClass::Terminator)); // rfe
        assert_eq!(classify(0x0000_000C), Some(OpClass::Terminator)); // syscall
        assert_eq!(classify(0xFC00_0000), None);
    }
}
