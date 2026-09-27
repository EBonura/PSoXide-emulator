//! Interface between the interpreter's batched path and the native tier
//! (`psoxide-jit`).
//!
//! The native tier only specialises the batch loop in `cpu/block.rs`
//! (`Cpu::run_fast`): compiled code runs register arithmetic and branch
//! decisions itself and hands everything with timing or bus semantics to
//! the helpers here, which call the batch's own code (`Cpu::batch_step`,
//! the RAM access paths, the GTE hazard sample, the block-to-block work).
//! [`NativeRun`] is the state compiled code and the helpers share.

use super::*;

/// A COP2 function instruction (a GTE command).
#[doc(hidden)]
#[inline]
pub fn is_gte_command(word: u32) -> bool {
    word & 0xFE00_0000 == 0x4A00_0000
}

/// Interpreter state that lockstep checkers compare and that has no public
/// getter of its own.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JitDebugState {
    /// Branch target waiting for its delay slot.
    pub pending_pc: Option<u32>,
    /// The next instruction sits in a branch delay slot.
    pub branch_delay_next: bool,
    /// Load waiting to commit after the next instruction.
    pub pending_load: Option<(u8, u32)>,
    /// Load-shadow position and register, if one is active.
    pub load_shadow: Option<(u8, u8)>,
    /// GTE interrupt-hazard watch address.
    pub gte_irq_watch: Option<u32>,
    /// HI/LO interlock deadline.
    pub hilo_busy_until: u64,
    /// GTE interlock deadline.
    pub gte_busy_until: u64,
    /// Exception nesting depth.
    pub isr_depth: u32,
    /// Retired-instruction count.
    pub tick: u64,
}

impl Cpu {
    /// State for lockstep checkers.
    #[doc(hidden)]
    pub fn jit_debug_state(&self) -> JitDebugState {
        JitDebugState {
            pending_pc: self.pending_pc,
            branch_delay_next: self.branch_delay_next,
            pending_load: self.pending_load,
            load_shadow: self.load_shadow.map(|s| (s.position, s.register)),
            gte_irq_watch: self.gte_irq_watch,
            hilo_busy_until: self.hilo_busy_until,
            gte_busy_until: self.gte_busy_until,
            isr_depth: self.isr_depth,
            tick: self.tick,
        }
    }
}

/// State a compiled block prefix runs with, shared with its helpers. Laid
/// out for the generated code (offsets in `psoxide-jit`'s tier emitter).
///
/// A compiled prefix is the run of ops from a block's first op that are
/// register arithmetic or CPU loads and stores to main RAM or the
/// scratchpad, stopping before the first op that is anything else, the
/// block's last op, or an op followed by a GTE command. For each op it does
/// exactly what [`Cpu::run_fast`](super::block) does: stop before the op when
/// the batch budget is spent or the clock would reach `limit`; then the
/// issue cycle (inline, following the interpreter's load-shadow rule),
/// the operation, the load-delay commit and squash. Loads and stores run
/// through [`jit_batch_memory`], the interpreter's own batched step.
#[doc(hidden)]
#[repr(C)]
pub struct NativeRun {
    /// The CPU.
    pub cpu: *mut Cpu,
    /// The bus.
    pub bus: *mut Bus,
    /// The CPU's register file.
    pub gprs: *mut u32,
    /// The bus clock.
    pub cycles: *const u64,
    /// The block's decoded ops.
    pub ops: *const crate::cpu::block::DecodedOp,
    /// Address of the block's first op.
    pub vaddr: u32,
    /// Number of ops in the block.
    pub op_count: u32,
    /// Retired-instruction count at entry.
    pub tick0: u64,
    /// Issue cycles not yet applied to the clock (in and out).
    pub issue: u64,
    /// Exclusive clock limit of the quiet span.
    pub limit: u64,
    /// Steps left in the batch budget.
    pub budget_left: u64,
    /// Ops retired so far (out).
    pub ran: u64,
    /// Register of the load in flight, 0 for none (in and out).
    pub pend_reg: u32,
    /// Its value.
    pub pend_val: u32,
    /// Main RAM still holds the block's words (in and out).
    pub ram_ok: u32,
    /// Interrupts are enabled (SR IEc and IM2).
    pub irq_enabled: u32,
    /// A load shadow is active (in and out). The compiled code keeps the
    /// shadow itself (`shadow`, `shadow_pos`, `shadow_reg`) and applies the
    /// interpreter's rule inline; helpers take it over and hand it back.
    pub shadow: u32,
    /// The shadow's position (instructions since its load).
    pub shadow_pos: u32,
    /// The register the shadowing load fills.
    pub shadow_reg: u32,
    /// The block's branch was taken (out; valid once the branch ran).
    pub taken: u32,
    /// The block's branch target (out).
    pub target: u32,
    /// Main RAM's word after the block is a GTE command.
    pub after_is_gte: u32,
    /// How the compiled code ended (out): [`NATIVE_IN_BLOCK`] and friends.
    pub status: u32,
    /// The batch loop's state, for [`jit_chain`].
    pub batch: *mut crate::cpu::block::BatchState,
    /// Main RAM.
    pub ram: *const u8,
    /// The I-cache's change count ([`LinkCell::gen`]).
    pub epoch_ptr: *const u64,
    /// The block cache's link generation ([`LinkCell::gen`]).
    pub link_gen_ptr: *const u64,
    /// Main RAM's page write counts.
    pub pages: *const u32,
    /// The block being run: its index plus one (the cursor's `block`).
    pub block: u32,
    /// A GTE interrupt watch is set (the helper that sets it says so).
    pub watch: u32,
    /// The exit cell [`jit_chain`] fills in when the next block runs
    /// compiled (null for none).
    pub link: *mut LinkCell,
    /// [`Bus::jit_load_state_ptrs`]: fill end, refresh deadline, last RAM
    /// access, data-bus latch.
    pub load_state: [usize; 4],
    /// Cycles compiled code moved the clock by itself whose GPU decay is
    /// still to apply ([`Bus::jit_settle_decay`]).
    pub decay: u64,
    /// `ran` when the current block was entered: `ran - ran0` ops of it
    /// have retired. `ran` counts from the last [`Cpu::native_enter`].
    pub ran0: u64,
    /// The clock below which a branch boundary has nothing to do
    /// ([`Bus::boundary_quiet_until`]).
    pub boundary_until: u64,
    /// Branch boundaries must take the full path (an interrupt is pending,
    /// or SR was written).
    pub slow_boundary: u32,
    /// Clock of the last branch boundary [`jit_chain`] passed quietly and
    /// has not yet recorded in the bus (0: none).
    pub last_boundary: u64,
}

/// Exit: stopped before op `ran` of the current block (the interpreter
/// carries on there; when that op is the delay slot, the branch decision is
/// in `taken`/`target`).
pub const NATIVE_IN_BLOCK: u32 = 0;
/// Exit: [`jit_chain`] ended the batch.
pub const NATIVE_BREAK: u32 = 1;
/// Exit: [`jit_chain`] entered the next block, which runs interpreted.
pub const NATIVE_NEXT_BLOCK: u32 = 2;
/// Exit: an op raised an exception; the PC is at its vector.
pub const NATIVE_EXCEPTION: u32 = 3;
/// Exit: stopped after a device access the batch may not go past
/// ([`Cpu::device_step_quiet`]); the batch ends once that op's step is
/// complete.
pub const NATIVE_STOPPED: u32 = 4;

/// Bytes from a block's entry to its chaining entry: the frame setup that a
/// chained block (reached by a jump from another block's code) skips.
pub const NATIVE_CHAIN_OFFSET: usize = 32;

impl NativeRun {
    /// Fresh state for a native run inside the batch `batch`.
    pub(super) fn new(
        cpu: &mut Cpu,
        bus: &mut Bus,
        batch: &mut crate::cpu::block::BatchState,
    ) -> Self {
        Self {
            cpu: cpu as *mut Cpu,
            bus: bus as *mut Bus,
            gprs: cpu.gprs.as_mut_ptr(),
            cycles: bus.jit_cycles_ptr(),
            ops: std::ptr::null(),
            vaddr: 0,
            op_count: 0,
            tick0: 0,
            issue: 0,
            limit: 0,
            budget_left: 0,
            ran: 0,
            pend_reg: 0,
            pend_val: 0,
            ram_ok: 0,
            irq_enabled: 0,
            shadow: 0,
            shadow_pos: 0,
            shadow_reg: 0,
            taken: 0,
            target: 0,
            after_is_gte: 0,
            status: NATIVE_IN_BLOCK,
            batch,
            ram: bus.jit_ram_ptr(),
            epoch_ptr: cpu.instruction_cache.epoch_ptr(),
            link_gen_ptr: cpu.blocks.link_gen_ptr(),
            pages: bus.jit_ram_pages().0,
            block: 0,
            watch: 0,
            link: std::ptr::null_mut(),
            load_state: bus.jit_load_state_ptrs(),
            decay: 0,
            ran0: 0,
            boundary_until: 0,
            slow_boundary: 1,
            last_boundary: 0,
        }
    }
}

impl Cpu {
    /// Compiled code ran every op of the current block: account them, do
    /// the branch-boundary work after a taken branch (or step past the
    /// block), and enter the next block. `None` when the batch ends there
    /// (an interrupt was taken, or no block starts at the new PC).
    pub(super) fn native_block_done(
        &mut self,
        bus: &mut Bus,
        run: &NativeRun,
        st: &mut crate::cpu::block::BatchState,
    ) -> Option<usize> {
        use crate::cpu::block::op_flags;
        let n = run.op_count as usize;
        // SAFETY: `ops` points at the current block's `op_count` ops.
        let last = unsafe { *run.ops.add(n - 1) };
        self.tick = run.tick0 + run.ran;
        st.done += run.ran;
        st.uncounted += run.ran;
        st.issue = run.issue;
        self.branch_delay_next = false;
        self.executing_in_branch_delay = false;
        if last.flags & op_flags::DELAY_SLOT != 0 && run.taken != 0 {
            if !self.batch_taken_boundary(bus, run.target, st) {
                return None;
            }
        } else {
            self.pc = run.vaddr.wrapping_add(4 * n as u32);
            self.cursor.block = 0;
        }
        if !self.enter_block(bus, self.pc) {
            return None;
        }
        Some((self.cursor.block - 1) as usize)
    }
}

/// End of a block whose ops compiled code all ran: what the batch loop does
/// between blocks (the branch-boundary work after a taken branch, finding
/// and validating the next block), then the next block's chaining entry
/// when it can run natively. Returns 0 to hand back to the batch loop,
/// with `status` saying where things stand.
///
/// # Safety
/// As [`jit_batch_memory`].
#[doc(hidden)]
pub unsafe extern "C" fn jit_chain(run: *mut NativeRun) -> usize {
    // SAFETY: per the contract, the pointers are live and unaliased here.
    let run = unsafe { &mut *run };
    let (cpu, bus, st) = unsafe { (&mut *run.cpu, &mut *run.bus, &mut *run.batch) };
    let link = std::mem::replace(&mut run.link, std::ptr::null_mut());
    if let Some(entry) = cpu.chain_fast(bus, run) {
        cpu.fill_link(bus, run, link, entry);
        return entry + NATIVE_CHAIN_OFFSET;
    }
    run.flush_boundary(bus);
    let Some(index) = cpu.native_block_done(bus, run, st) else {
        run.status = NATIVE_BREAK;
        return 0;
    };
    let irq_enabled = cpu.irq_enabled();
    let ram_ok = irq_enabled && cpu.block_ram_matches(bus, index);
    match cpu.native_ready(bus, index, irq_enabled, ram_ok, st) {
        Some(entry) => {
            cpu.native_enter(bus, run, index, irq_enabled, ram_ok, st);
            cpu.fill_link(bus, run, link, entry);
            entry + NATIVE_CHAIN_OFFSET
        }
        None => {
            run.status = NATIVE_NEXT_BLOCK;
            0
        }
    }
}

/// A compiled block prefix.
#[doc(hidden)]
pub type NativeFn = unsafe extern "C" fn(*mut NativeRun);

/// One exit of a compiled block, linked to the block it last went to
/// (filled in by [`jit_chain`], read by the compiled code). While the
/// I-cache epoch plus the block cache's link generation equal `gen`, the
/// block at `block` still starts at `vaddr` with these ops and is still
/// current and compiled; with interrupts enabled, main RAM still holds its
/// words (for the GTE hazard) while pages `page1` and `page2` still have
/// write counts `count1` and `count2`. The compiled code then does
/// [`Cpu::chain_fast`]'s work itself and jumps to `entry`.
#[doc(hidden)]
#[repr(C)]
#[derive(Default)]
pub struct LinkCell {
    /// The target's code, past the frame setup and register loads
    /// ([`NATIVE_BODY_OFFSET`]); 0 while unlinked.
    pub entry: usize,
    /// See above.
    pub gen: u64,
    /// The target block's ops.
    pub ops: usize,
    /// Its first op's address.
    pub vaddr: u32,
    /// Its index plus one.
    pub block: u32,
    /// Its op count.
    pub op_count: u32,
    /// Main RAM's word after it is a GTE command.
    pub after_is_gte: u32,
    /// First page of its words (and the word after).
    pub page1: u32,
    /// That page's write count.
    pub count1: u32,
    /// Last page of them.
    pub page2: u32,
    /// Its write count.
    pub count2: u32,
}

/// Offsets of [`LinkCell`] fields, for the emitter.
#[doc(hidden)]
pub mod link_cell {
    use super::LinkCell;
    /// `entry`
    pub const ENTRY: usize = core::mem::offset_of!(LinkCell, entry);
    /// `gen`
    pub const GEN: usize = core::mem::offset_of!(LinkCell, gen);
    /// `ops`
    pub const OPS: usize = core::mem::offset_of!(LinkCell, ops);
    /// `vaddr`
    pub const VADDR: usize = core::mem::offset_of!(LinkCell, vaddr);
    /// `block`
    pub const BLOCK: usize = core::mem::offset_of!(LinkCell, block);
    /// `op_count`
    pub const OP_COUNT: usize = core::mem::offset_of!(LinkCell, op_count);
    /// `after_is_gte`
    pub const AFTER_IS_GTE: usize = core::mem::offset_of!(LinkCell, after_is_gte);
    /// `page1`
    pub const PAGE1: usize = core::mem::offset_of!(LinkCell, page1);
    /// `count1`
    pub const COUNT1: usize = core::mem::offset_of!(LinkCell, count1);
    /// `page2`
    pub const PAGE2: usize = core::mem::offset_of!(LinkCell, page2);
    /// `count2`
    pub const COUNT2: usize = core::mem::offset_of!(LinkCell, count2);
}

/// Bytes from a block's entry to its first op's code: the frame setup and
/// the register loads that linked code (already set up) skips.
pub const NATIVE_BODY_OFFSET: usize = 60;

/// Byte offsets of [`NativeRun`] fields, for the emitter.
#[doc(hidden)]
pub mod native_run {
    use super::NativeRun;
    /// `gprs`
    pub const GPRS: usize = core::mem::offset_of!(NativeRun, gprs);
    /// `cycles`
    pub const CYCLES: usize = core::mem::offset_of!(NativeRun, cycles);
    /// `issue`
    pub const ISSUE: usize = core::mem::offset_of!(NativeRun, issue);
    /// `limit`
    pub const LIMIT: usize = core::mem::offset_of!(NativeRun, limit);
    /// `budget_left`
    pub const BUDGET_LEFT: usize = core::mem::offset_of!(NativeRun, budget_left);
    /// `ran`
    pub const RAN: usize = core::mem::offset_of!(NativeRun, ran);
    /// `pend_reg`
    pub const PEND_REG: usize = core::mem::offset_of!(NativeRun, pend_reg);
    /// `pend_val`
    pub const PEND_VAL: usize = core::mem::offset_of!(NativeRun, pend_val);
    /// `ram_ok`
    pub const RAM_OK: usize = core::mem::offset_of!(NativeRun, ram_ok);
    /// `irq_enabled`
    pub const IRQ_ENABLED: usize = core::mem::offset_of!(NativeRun, irq_enabled);
    /// `shadow`
    pub const SHADOW: usize = core::mem::offset_of!(NativeRun, shadow);
    /// `shadow_pos`
    pub const SHADOW_POS: usize = core::mem::offset_of!(NativeRun, shadow_pos);
    /// `shadow_reg`
    pub const SHADOW_REG: usize = core::mem::offset_of!(NativeRun, shadow_reg);
    /// `taken`
    pub const TAKEN: usize = core::mem::offset_of!(NativeRun, taken);
    /// `target`
    pub const TARGET: usize = core::mem::offset_of!(NativeRun, target);
    /// `after_is_gte`
    pub const AFTER_IS_GTE: usize = core::mem::offset_of!(NativeRun, after_is_gte);
    /// `status`
    pub const STATUS: usize = core::mem::offset_of!(NativeRun, status);
    /// `ram`
    pub const RAM: usize = core::mem::offset_of!(NativeRun, ram);
    /// `epoch_ptr`
    pub const EPOCH_PTR: usize = core::mem::offset_of!(NativeRun, epoch_ptr);
    /// `link_gen_ptr`
    pub const LINK_GEN_PTR: usize = core::mem::offset_of!(NativeRun, link_gen_ptr);
    /// `pages`
    pub const PAGES: usize = core::mem::offset_of!(NativeRun, pages);
    /// `block`
    pub const BLOCK: usize = core::mem::offset_of!(NativeRun, block);
    /// `watch`
    pub const WATCH: usize = core::mem::offset_of!(NativeRun, watch);
    /// `link`
    pub const LINK: usize = core::mem::offset_of!(NativeRun, link);
    /// `load_state`
    pub const LOAD_STATE: usize = core::mem::offset_of!(NativeRun, load_state);
    /// `decay`
    pub const DECAY: usize = core::mem::offset_of!(NativeRun, decay);
    /// `ops`
    pub const OPS: usize = core::mem::offset_of!(NativeRun, ops);
    /// `vaddr`
    pub const VADDR: usize = core::mem::offset_of!(NativeRun, vaddr);
    /// `op_count`
    pub const OP_COUNT: usize = core::mem::offset_of!(NativeRun, op_count);
    /// `ran0`
    pub const RAN0: usize = core::mem::offset_of!(NativeRun, ran0);
    /// `boundary_until`
    pub const BOUNDARY_UNTIL: usize = core::mem::offset_of!(NativeRun, boundary_until);
    /// `slow_boundary`
    pub const SLOW_BOUNDARY: usize = core::mem::offset_of!(NativeRun, slow_boundary);
    /// `last_boundary`
    pub const LAST_BOUNDARY: usize = core::mem::offset_of!(NativeRun, last_boundary);
}

/// Helper result: go on with the next op.
pub const NATIVE_CONTINUE: u32 = 0;
/// Helper result: stop; the op was not run (or was the last to run, see
/// the helper).
pub const NATIVE_STOP: u32 = 1;

impl NativeRun {
    /// Record the last quietly passed branch boundary in the bus (see
    /// [`Bus::note_post_op_cycle`]).
    /// Also applies the GPU decay compiled code deferred.
    #[inline(always)]
    pub(super) fn flush_boundary(&mut self, bus: &mut Bus) {
        if self.last_boundary != 0 {
            bus.note_post_op_cycle(self.last_boundary);
            self.last_boundary = 0;
        }
        bus.jit_settle_decay(std::mem::take(&mut self.decay));
    }

    /// The CPU's load shadow as the compiled code keeps it.
    #[inline(always)]
    pub(super) fn shadow_from(&mut self, cpu: &Cpu) {
        match cpu.load_shadow {
            Some(shadow) => {
                self.shadow = 1;
                self.shadow_pos = u32::from(shadow.position);
                self.shadow_reg = u32::from(shadow.register);
            }
            None => self.shadow = 0,
        }
    }

    /// Hand the compiled code's load shadow back to the CPU.
    #[inline(always)]
    pub(super) fn shadow_to(&self, cpu: &mut Cpu) {
        cpu.load_shadow = (self.shadow != 0).then_some(LoadShadow {
            position: self.shadow_pos as u8,
            register: self.shadow_reg as u8,
        });
    }
}

/// Run op `index` of the block, a CPU load or store, as the batch does:
/// stop before it ([`NATIVE_STOP`], nothing changed) when the budget is
/// spent, the clock would reach the limit, or the access is one the batch
/// cannot run (misaligned, KSEG2); otherwise the interpreter's
/// batched step; a device access too, after which the batch goes on only
/// if [`Cpu::device_step_quiet`] allows (else `status` becomes
/// [`NATIVE_STOPPED`] and it returns [`NATIVE_STOP`] with the op run).
/// Returns [`NATIVE_STOP`] after running it too when main RAM may no
/// longer hold the block's words with interrupts enabled (the GTE hazard
/// then needs RAM peeks the compiled code does not do).
///
/// # Safety
/// `run` must be the state of a running compiled prefix and `index` one of
/// its ops.
#[doc(hidden)]
pub unsafe extern "C" fn jit_batch_memory(run: *mut NativeRun, index: u32) -> u32 {
    // SAFETY: per the contract, the pointers are live and unaliased here.
    let run = unsafe { &mut *run };
    let (cpu, bus) = unsafe { (&mut *run.cpu, &mut *run.bus) };
    // The clock may move past the quiet span here, where the GPU's state
    // matters: apply the decay compiled code deferred first.
    bus.jit_settle_decay(std::mem::take(&mut run.decay));
    let op = unsafe { *run.ops.add(index as usize) };
    if run.ran >= run.budget_left || bus.cycles() + run.issue + 1 >= run.limit {
        return NATIVE_STOP;
    }
    let addr = cpu.gpr(op.rs).wrapping_add((op.word as i16) as i32 as u32);
    let device = !crate::cpu::block::quiet_access(op.word, addr);
    if device && !crate::cpu::block::device_access(op.word, addr) {
        return NATIVE_STOP;
    }
    // A device may read the GPU or the timers: hand the deferred decay and
    // the last branch boundary to the bus first.
    let raised = if device {
        run.flush_boundary(bus);
        crate::cpu::block::irq_raise_total(bus)
    } else {
        0
    };
    let pc = run.vaddr.wrapping_add(4 * index);
    cpu.tick = run.tick0 + run.ran;
    cpu.pc = pc;
    cpu.pending_load = (run.pend_reg != 0).then_some((run.pend_reg as u8, run.pend_val));
    let delay = op.flags & crate::cpu::block::op_flags::DELAY_SLOT != 0;
    if delay {
        cpu.pending_pc = (run.taken != 0).then_some(run.target);
    }
    let mut refetch = false;
    let mut ram_ok = run.ram_ok != 0;
    run.shadow_to(cpu);
    let stepped = cpu.batch_step(
        bus,
        op,
        pc,
        delay,
        addr,
        &mut run.issue,
        &mut refetch,
        run.limit,
        &mut ram_ok,
    );
    debug_assert!(
        stepped.is_some(),
        "a quiet access in compiled code never waits on a fetch"
    );
    run.ran += 1;
    // A store elsewhere leaves the block's words (and the word after it) as
    // they were in RAM: the decoded flags still answer the GTE hazard.
    let stored = matches!(op.class, crate::cpu::block::OpClass::Store);
    run.ram_ok =
        u32::from(ram_ok || (stored && run.ram_ok != 0 && !store_hits_block(run, addr, 4)));
    if device {
        // SAFETY: the batch state outlives the native run.
        let st = unsafe { &mut *run.batch };
        if !cpu.device_step_quiet(bus, addr, raised, st) {
            run.status = NATIVE_STOPPED;
            run.shadow_from(cpu);
            match cpu.pending_load.take() {
                Some((reg, value)) => {
                    run.pend_reg = u32::from(reg);
                    run.pend_val = value;
                }
                None => run.pend_reg = 0,
            }
            return NATIVE_STOP;
        }
        // What the access may have changed: the limits (just taken again),
        // the boundary's quiet clock, the interrupt lines, and RAM under
        // the block (a DMA).
        run.limit = st.limit;
        run.boundary_until = bus.boundary_quiet_until();
        run.slow_boundary |= u32::from(bus.irq().pending());
        run.ram_ok =
            u32::from(run.irq_enabled != 0 && cpu.block_ram_matches(bus, run.block as usize - 1));
    }
    run.shadow_from(cpu);
    match cpu.pending_load.take() {
        Some((reg, value)) => {
            run.pend_reg = u32::from(reg);
            run.pend_val = value;
        }
        None => run.pend_reg = 0,
    }
    if run.ram_ok == 0 && run.irq_enabled != 0 {
        NATIVE_STOP
    } else {
        NATIVE_CONTINUE
    }
}

/// Run op `index` of the block, an [`OpClass::Other`](crate::cpu::block::OpClass)
/// op or a GTE command (a trapping add, multiply/divide, HI/LO, MFC0 or a
/// GTE register move; in a delay slot too), as the batch does: stop before it
/// ([`NATIVE_STOP`], nothing changed) when the budget is spent or the clock
/// would reach the limit; otherwise the interpreter's batched step. When
/// the op trapped, sets the PC to the exception vector, `status` to
/// [`NATIVE_EXCEPTION`] and returns [`NATIVE_STOP`].
///
/// # Safety
/// As [`jit_batch_memory`].
#[doc(hidden)]
pub unsafe extern "C" fn jit_batch_other(run: *mut NativeRun, index: u32) -> u32 {
    // SAFETY: per the contract, the pointers are live and unaliased here.
    let run = unsafe { &mut *run };
    let (cpu, bus) = unsafe { (&mut *run.cpu, &mut *run.bus) };
    // The clock may move past the quiet span here, where the GPU's state
    // matters: apply the decay compiled code deferred first.
    bus.jit_settle_decay(std::mem::take(&mut run.decay));
    let op = unsafe { *run.ops.add(index as usize) };
    if run.ran >= run.budget_left || bus.cycles() + run.issue + 1 >= run.limit {
        return NATIVE_STOP;
    }
    let pc = run.vaddr.wrapping_add(4 * index);
    cpu.tick = run.tick0 + run.ran;
    cpu.pc = pc;
    cpu.pending_load = (run.pend_reg != 0).then_some((run.pend_reg as u8, run.pend_val));
    let delay = op.flags & crate::cpu::block::op_flags::DELAY_SLOT != 0;
    if delay {
        cpu.pending_pc = (run.taken != 0).then_some(run.target);
    }
    if op.word >> 21 == 0x10 << 5 | 4 {
        // MTC0: SR (the interrupt enables) may change.
        run.slow_boundary = 1;
    }
    let mut refetch = false;
    let mut ram_ok = run.ram_ok != 0;
    run.shadow_to(cpu);
    let stepped = cpu.batch_step(
        bus,
        op,
        pc,
        delay,
        0,
        &mut run.issue,
        &mut refetch,
        run.limit,
        &mut ram_ok,
    );
    debug_assert!(stepped.is_some(), "compiled code never waits on a fetch");
    run.ran += 1;
    run.shadow_from(cpu);
    match cpu.pending_load.take() {
        Some((reg, value)) => {
            run.pend_reg = u32::from(reg);
            run.pend_val = value;
        }
        None => run.pend_reg = 0,
    }
    if let Some(vector) = cpu.pending_exception_pc.take() {
        cpu.pc = vector;
        run.status = NATIVE_EXCEPTION;
        return NATIVE_STOP;
    }
    NATIVE_CONTINUE
}

/// The main-RAM load `word` (LB, LH, LW, LBU or LHU) at `addr`, which
/// compiled code has checked is main RAM and aligned, after its issue
/// cycle: what the batch's step does once the load is in hand, less the
/// load-delay bookkeeping the compiled code keeps itself. Settles the clock,
/// charges the RAM stalls, reads, and starts a load shadow when the load
/// came from cached code. Returns the value, sign- or zero-extended.
///
/// # Safety
/// As [`jit_batch_memory`].
#[doc(hidden)]
pub unsafe extern "C" fn jit_ram_load(run: *mut NativeRun, addr: u32, word: u32) -> u32 {
    // SAFETY: per the contract, the pointers are live and unaliased here.
    let run = unsafe { &mut *run };
    let (cpu, bus) = unsafe { (&mut *run.cpu, &mut *run.bus) };
    // The clock may move past the quiet span here, where the GPU's state
    // matters: apply the decay compiled code deferred first.
    bus.jit_settle_decay(std::mem::take(&mut run.decay));
    bus.advance_quiet(run.issue);
    run.issue = 0;
    let value = match word >> 26 {
        0x20 => bus.cpu_ram_load(addr, 1) as u8 as i8 as i32 as u32,
        0x24 => bus.cpu_ram_load(addr, 1),
        0x21 => bus.cpu_ram_load(addr, 2) as u16 as i16 as i32 as u32,
        0x25 => bus.cpu_ram_load(addr, 2),
        _ => bus.cpu_ram_load(addr, 4),
    };
    if bus.take_ram_load_from_cached_code() {
        run.shadow = 1;
        run.shadow_pos = 0;
        run.shadow_reg = (word >> 16) & 0x1F;
    }
    let _ = cpu;
    value
}

/// The main-RAM word store of `value` at `addr` (SW, checked by compiled
/// code as main RAM and aligned), after its issue cycle: settle the clock,
/// charge the store, write, and mark the block's RAM check stale.
///
/// # Safety
/// As [`jit_batch_memory`].
#[doc(hidden)]
pub unsafe extern "C" fn jit_ram_store32(run: *mut NativeRun, addr: u32, value: u32, word: u32) {
    // SAFETY: per the contract, the pointers are live and unaliased here.
    let run = unsafe { &mut *run };
    let (cpu, bus) = unsafe { (&mut *run.cpu, &mut *run.bus) };
    // The clock may move past the quiet span here, where the GPU's state
    // matters: apply the decay compiled code deferred first.
    bus.jit_settle_decay(std::mem::take(&mut run.decay));
    bus.advance_quiet(run.issue);
    run.issue = 0;
    bus.cpu_ram_store32(addr, value);
    if store_hits_block(run, addr, 4) {
        run.ram_ok = 0;
    }
    if bus.take_ram_load_from_cached_code() {
        run.shadow = 1;
        run.shadow_pos = 0;
        run.shadow_reg = (word >> 16) & 0x1F;
    }
    let _ = cpu;
}

/// The GTE interrupt hazard before op `index`, with interrupts enabled, as
/// the batch does it: when the op is a watched GTE command or the next
/// instruction (the branch target after a taken branch's delay slot) is a
/// GTE command, sample the interrupt line ([`NATIVE_STOP`], the step left
/// to the interpreter, unless that sample is quiet) and set the watch;
/// otherwise clear it.
///
/// # Safety
/// As [`jit_batch_memory`].
#[doc(hidden)]
pub unsafe extern "C" fn jit_gte_hazard(run: *mut NativeRun, index: u32) -> u32 {
    use crate::cpu::block::op_flags;
    // SAFETY: per the contract, the pointers are live and unaliased here.
    let run = unsafe { &mut *run };
    let (cpu, bus) = unsafe { (&mut *run.cpu, &mut *run.bus) };
    let op = unsafe { *run.ops.add(index as usize) };
    let pc = run.vaddr.wrapping_add(4 * index);
    let delay = op.flags & op_flags::DELAY_SLOT != 0;
    let next = if delay && run.taken != 0 {
        run.target
    } else {
        pc.wrapping_add(4)
    };
    let next_gte = if delay || run.ram_ok == 0 {
        bus.peek_is_gte_command(next)
    } else if op.flags & op_flags::LAST != 0 {
        run.after_is_gte != 0
    } else {
        op.flags & op_flags::NEXT_GTE != 0
    };
    let watched = cpu.gte_irq_watch == Some(pc);
    if watched || next_gte {
        if !cpu.batch_gte_sample(bus, &mut run.issue) {
            return NATIVE_STOP;
        }
        cpu.gte_irq_watch = next_gte.then_some(next);
    } else {
        cpu.gte_irq_watch = None;
    }
    run.watch = u32::from(cpu.gte_irq_watch.is_some());
    NATIVE_CONTINUE
}

/// Whether a store of up to `len` bytes at `addr` can change a RAM word of
/// the running block or the word after it (any mirror).
fn store_hits_block(run: &NativeRun, addr: u32, len: u32) -> bool {
    use psx_hw::memory;
    let size = memory::ram::SIZE as u32;
    let phys = memory::to_physical(addr);
    if phys >= memory::ram::MIRROR_END {
        return false;
    }
    let n = run.op_count;
    let start = memory::to_physical(run.vaddr) % size;
    let end = start + 4 * (n + 1);
    let lo = (phys % size) & !3;
    lo < end && lo + len > start
}
