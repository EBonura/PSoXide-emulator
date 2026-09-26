//! Narrow interface for the dynamic recompiler (`psoxide-jit`).
//!
//! The recompiler reproduces the interpreter exactly: same cycles, same
//! memory traffic, same state after every instruction. It does so by
//! running only register arithmetic and branch decisions natively and
//! handing everything with timing or bus semantics back to the functions
//! the interpreter itself uses. This module is where that hand-off lives:
//!
//! * guards that say when a block may run natively at all
//!   ([`Cpu::jit_can_enter`], [`Cpu::jit_code_word`]);
//! * [`Cpu::jit_account_run`], the per-instruction bookkeeping of a run of
//!   register-only instructions (issue cycle, load shadow, fetch flags),
//!   done ahead of the run when no scheduler event can fall inside it;
//! * [`Cpu::jit_step_plain`] and [`Cpu::jit_step`], which execute one
//!   instruction the way [`Cpu::step`] does, from state the compiled code
//!   hands over.
//!
//! Nothing here changes interpreter behaviour; it only exposes it.

use super::*;

/// Byte offset of the 32 general-purpose registers inside [`Cpu`]; compiled
/// code reads and writes guest registers there directly.
#[doc(hidden)]
pub fn gprs_offset() -> usize {
    core::mem::offset_of!(Cpu, gprs)
}

/// COP2 function instruction (a GTE command): the instructions the GTE
/// interrupt hazard watches for.
#[doc(hidden)]
#[inline]
pub fn is_gte_command(word: u32) -> bool {
    word & 0xFE00_0000 == 0x4A00_0000
}

/// Whether the HLE kernel intercepts execution at `vaddr` holding `word`:
/// its A0/B0/C0 dispatch vectors, the exception-return stub, and trap
/// words, all in the kernel's first 64 KiB. The interpreter's step hands
/// these to the HLE before fetching, so compiled code never includes them.
#[doc(hidden)]
#[inline]
pub fn is_hle_hook(vaddr: u32, word: u32) -> bool {
    let phys = memory::to_physical(vaddr);
    phys < 0x1_0000
        && (matches!(phys, 0xA0 | 0xB0 | 0xC0)
            || phys == memory::to_physical(crate::hle_bios::EXCEPTION_RETURN_STUB)
            || crate::hle_kernel::decode_trap(word).is_some())
}

/// Interpreter state that the recompiler's lockstep checker compares and
/// that has no public getter of its own.
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
    /// Whether compiled code may start at the current PC: not inside a
    /// branch delay, cache not isolated, I-cache enabled, and no profiler
    /// or limit oracle that needs the per-instruction hooks.
    #[doc(hidden)]
    #[inline]
    pub fn jit_can_enter(&self, bus: &Bus) -> bool {
        self.pending_pc.is_none()
            && !self.branch_delay_next
            && !self.cache_isolated()
            && self.cache_control & CACHE_CONTROL_IS1 != 0
            && !self.cpu_cycle_profile_enabled
            && !self.instruction_class_profile_enabled
            && !self.instruction_cache_event_profile_enabled
            && bus.jit_limits_idle()
    }

    /// The instruction at `vaddr` when compiled code may execute it: a
    /// cached segment of main RAM, an I-cache hit, and main RAM holding the
    /// same word (the GTE interrupt hazard peeks RAM, not the cache, so the
    /// two must agree). HLE hooks ([`is_hle_hook`]) are the caller's to
    /// exclude.
    #[doc(hidden)]
    #[inline]
    pub fn jit_code_word(&self, bus: &Bus, vaddr: u32) -> Option<u32> {
        if vaddr & 3 != 0 || vaddr >= 0xA000_0000 {
            return None;
        }
        let phys = memory::to_physical(vaddr);
        if phys >= memory::ram::MIRROR_END {
            return None;
        }
        let word = self.instruction_cache.hit(phys)?;
        (bus.peek_instruction(vaddr)? == word).then_some(word)
    }

    /// Changes so far to the I-cache's tags, valid bits or words. While it
    /// is unchanged, every word [`Cpu::jit_code_word`] found in the cache
    /// is still there. Restart the recompiler after replacing the CPU
    /// (loading a save state), which starts this count again.
    #[doc(hidden)]
    #[inline]
    pub fn jit_icache_epoch(&self) -> u64 {
        self.instruction_cache.epoch()
    }

    /// The I-cache word at `vaddr` if a fetch would hit, whatever main RAM
    /// holds; `None` outside the compilable range or on a miss.
    #[doc(hidden)]
    #[inline]
    pub fn jit_cached_word(&self, vaddr: u32) -> Option<u32> {
        if vaddr & 3 != 0 || vaddr >= 0xA000_0000 {
            return None;
        }
        let phys = memory::to_physical(vaddr);
        if phys >= memory::ram::MIRROR_END {
            return None;
        }
        self.instruction_cache.hit(phys)
    }

    /// Charge the front half of each instruction in `words`, a run of
    /// register-only instructions (ALU and branches, never the last of a
    /// block) that compiled code is about to execute: the SPU catch-up and
    /// interrupt-line sample, the cached fetch, the load shadow and the
    /// issue tick, exactly as [`Cpu::step`] would, in order.
    ///
    /// Register-only instructions neither read nor write anything these
    /// touch, so charging them ahead of executing the run is the same as
    /// interleaving. Returns `false`, changing nothing, when a scheduler
    /// event or SPU sample could fall due inside the run or the next fetch
    /// could wait on a streaming fill: the caller hands the run to the
    /// interpreter instead, so every event still lands on its instruction.
    #[doc(hidden)]
    pub fn jit_account_run(&mut self, bus: &mut Bus, words: &[u32]) -> bool {
        if !bus.jit_quiet_for(words.len() as u64) || !bus.jit_stream_settled() {
            return false;
        }
        bus.jit_settle_stream();
        // Each instruction's GTE hazard check consumes the watch. None of
        // these is a GTE command and none is followed by one inside the
        // block, so the check never samples.
        self.gte_irq_watch = None;
        // Inside a quiet window every bus call of the run is a plain counter
        // update, so when the clock is on its plain path the run's issue
        // cycles (one each, none for instructions hidden in a load shadow,
        // which is CPU-side logic) can be charged at once.
        if bus.jit_tick_is_plain() {
            let n = words.len() as u32;
            if bus.external_interrupt_pending() {
                self.irq_line_high_steps = self.irq_line_high_steps.saturating_add(n as u64);
                for _ in 1..n {
                    bus.external_interrupt_pending();
                }
            }
            bus.note_cached_fetch(true);
            // Without a shadow every instruction issues at BIAS; a run holds
            // no load, so none can start one.
            let issue = if self.load_shadow.is_none() {
                words.iter().map(|&instr| cycle_cost(instr)).sum()
            } else {
                let mut issue = 0;
                for &instr in words {
                    if !self.hides_in_load_shadow(instr, bus) {
                        issue += cycle_cost(instr);
                    }
                }
                issue
            };
            bus.jit_tick_quiet(issue);
            self.tick += n as u64;
            return true;
        }
        for &instr in words {
            if bus.external_interrupt_pending() {
                self.irq_line_high_steps = self.irq_line_high_steps.saturating_add(1);
            }
            bus.note_cached_fetch(true);
            bus.add_zero_cycles();
            let issue = if self.hides_in_load_shadow(instr, bus) {
                0
            } else {
                cycle_cost(instr)
            };
            bus.tick(issue);
            self.tick += 1;
        }
        true
    }

    /// Execute `instr` at `pc` as [`Cpu::step`] would, for an instruction in
    /// the middle of a compiled block that is not a branch, not a GTE
    /// command, not in a delay slot, and not followed by a GTE command. The
    /// word came from an I-cache hit that main RAM agrees with, and the
    /// block started with a settled fetch stream, so the fetch is a hit with
    /// no stall. `pending` is the load the previous instruction left in
    /// flight. Leaves the new in-flight load in [`Cpu::jit_take_pending_load`].
    #[doc(hidden)]
    pub fn jit_step_plain(
        &mut self,
        bus: &mut Bus,
        pc: u32,
        instr: u32,
        pending: Option<(u8, u32)>,
    ) -> Result<(), ExecutionError> {
        self.pc = pc;
        self.pending_load = pending;
        self.pending_pc = None;
        self.branch_delay_next = false;
        if bus.external_interrupt_pending() {
            self.irq_line_high_steps = self.irq_line_high_steps.saturating_add(1);
        }
        // GTE hazard: neither this word nor the next is a GTE command, so
        // the check only consumes the watch.
        self.gte_irq_watch = None;
        Self::block_fetch(bus, pc);
        self.execute_fetched(bus, pc, instr, false).map(|_| ())
    }

    /// Execute the instruction at `pc` through the interpreter's own step,
    /// after putting back the state compiled code kept elsewhere: the load
    /// in flight, and, when `delay` is set, that this instruction is a
    /// branch delay slot (`Some(target)` for a taken branch). Used for the
    /// last instruction of a block and for any instruction next to a GTE
    /// command.
    #[doc(hidden)]
    pub fn jit_step(
        &mut self,
        bus: &mut Bus,
        pc: u32,
        delay: Option<Option<u32>>,
        pending: Option<(u8, u32)>,
    ) -> Result<(), ExecutionError> {
        self.pc = pc;
        self.pending_load = pending;
        match delay {
            Some(target) => {
                self.pending_pc = target;
                self.branch_delay_next = true;
            }
            None => {
                self.pending_pc = None;
                self.branch_delay_next = false;
            }
        }
        self.execute_one_inner(bus).map(|_| ())
    }

    /// [`Cpu::jit_step`] for an instruction compiled code already fetched,
    /// an I-cache hit that is not an HLE hook ([`is_hle_hook`]): the front
    /// half of the interpreter's step (interrupt-line sample, GTE hazard,
    /// the hit fetch [`Cpu::block_fetch`]) and then its shared back half,
    /// [`Cpu::execute_fetched`].
    #[doc(hidden)]
    pub fn jit_step_cached(
        &mut self,
        bus: &mut Bus,
        pc: u32,
        instr: u32,
        delay: Option<Option<u32>>,
        pending: Option<(u8, u32)>,
    ) -> Result<(), ExecutionError> {
        self.pc = pc;
        self.pending_load = pending;
        match delay {
            Some(target) => {
                self.pending_pc = target;
                self.branch_delay_next = true;
            }
            None => {
                self.pending_pc = None;
                self.branch_delay_next = false;
            }
        }
        if bus.external_interrupt_pending() {
            self.irq_line_high_steps = self.irq_line_high_steps.saturating_add(1);
        }
        let gte_irq_taken_after = self.gte_irq_hazard(pc, bus);
        Self::block_fetch(bus, pc);
        self.execute_fetched(bus, pc, instr, gte_irq_taken_after)
            .map(|_| ())
    }

    /// Leave compiled code at an instruction boundary outside any delay
    /// slot: the next instruction is at `pc` and `pending` is in flight.
    #[doc(hidden)]
    #[inline]
    pub fn jit_exit(&mut self, pc: u32, pending: Option<(u8, u32)>) {
        self.pc = pc;
        self.pending_pc = None;
        self.branch_delay_next = false;
        self.pending_load = pending;
    }

    /// Take the load in flight, for compiled code to commit itself.
    #[doc(hidden)]
    #[inline]
    pub fn jit_take_pending_load(&mut self) -> Option<(u8, u32)> {
        self.pending_load.take()
    }

    /// State for the recompiler's lockstep checker.
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

    /// Cache-control register (BIU at `0xFFFE_0130`), for guards.
    #[doc(hidden)]
    #[inline]
    pub fn jit_cache_control(&self) -> u32 {
        self.cache_control
    }

    /// COP0 status register.
    #[doc(hidden)]
    #[inline]
    pub fn jit_status(&self) -> u32 {
        self.cop0[12]
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
/// issue cycle (through [`jit_shadow_issue`] while a load shadow is active),
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
    pub ops: *const super::block::DecodedOp,
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
    pub batch: *mut super::block::BatchState,
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

/// Bytes from a block's entry to its chaining entry: the frame setup that a
/// chained block (reached by a jump from another block's code) skips.
pub const NATIVE_CHAIN_OFFSET: usize = 32;

impl NativeRun {
    /// Fresh state for a native run inside the batch `batch`.
    pub(super) fn new(cpu: &mut Cpu, bus: &mut Bus, batch: &mut super::block::BatchState) -> Self {
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
        st: &mut super::block::BatchState,
    ) -> Option<usize> {
        use super::block::op_flags;
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
    let Some(index) = cpu.native_block_done(bus, run, st) else {
        run.status = NATIVE_BREAK;
        return 0;
    };
    let irq_enabled = cpu.irq_enabled();
    let ram_ok = irq_enabled && cpu.block_ram_matches(bus, index);
    match cpu.native_ready(bus, index, irq_enabled, ram_ok, st) {
        Some(entry) => {
            cpu.native_enter(run, index, irq_enabled, ram_ok, st);
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
}

/// Helper result: go on with the next op.
pub const NATIVE_CONTINUE: u32 = 0;
/// Helper result: stop; the op was not run (or was the last to run, see
/// the helper).
pub const NATIVE_STOP: u32 = 1;

impl NativeRun {
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

/// Issue cycles of the op `word` while a load shadow is active: the
/// interpreter's [`Cpu::hides_in_load_shadow`] step. Updates `shadow`.
///
/// # Safety
/// `run` must be the state of a running compiled prefix.
#[doc(hidden)]
pub unsafe extern "C" fn jit_shadow_issue(run: *mut NativeRun, word: u32) -> u32 {
    // SAFETY: per the contract, the pointers are live and unaliased here.
    let run = unsafe { &mut *run };
    let (cpu, bus) = unsafe { (&mut *run.cpu, &*run.bus) };
    run.shadow_to(cpu);
    let hidden = cpu.load_shadow.is_some() && cpu.hides_in_load_shadow(word, bus);
    run.shadow_from(cpu);
    if hidden {
        0
    } else {
        cycle_cost(word)
    }
}

/// Run op `index` of the block, a CPU load or store, as the batch does:
/// stop before it ([`NATIVE_STOP`], nothing changed) when the budget is
/// spent, the clock would reach the limit, or the access is not a quiet
/// one (main RAM or the scratchpad, aligned); otherwise the interpreter's
/// batched step. Returns [`NATIVE_STOP`] after running it too when it was a
/// store with interrupts enabled (the GTE hazard then needs RAM peeks the
/// compiled code does not do).
///
/// # Safety
/// `run` must be the state of a running compiled prefix and `index` one of
/// its ops.
#[doc(hidden)]
pub unsafe extern "C" fn jit_batch_memory(run: *mut NativeRun, index: u32) -> u32 {
    // SAFETY: per the contract, the pointers are live and unaliased here.
    let run = unsafe { &mut *run };
    let (cpu, bus) = unsafe { (&mut *run.cpu, &mut *run.bus) };
    let op = unsafe { *run.ops.add(index as usize) };
    if run.ran >= run.budget_left || bus.cycles() + run.issue + 1 >= run.limit {
        return NATIVE_STOP;
    }
    let addr = cpu.gpr(op.rs).wrapping_add((op.word as i16) as i32 as u32);
    if !super::block::quiet_access(op.word, addr) {
        return NATIVE_STOP;
    }
    let pc = run.vaddr.wrapping_add(4 * index);
    cpu.tick = run.tick0 + run.ran;
    cpu.pc = pc;
    cpu.pending_load = (run.pend_reg != 0).then_some((run.pend_reg as u8, run.pend_val));
    let delay = op.flags & super::block::op_flags::DELAY_SLOT != 0;
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
    let stored = matches!(op.class, super::block::OpClass::Store);
    run.ram_ok =
        u32::from(ram_ok || (stored && run.ram_ok != 0 && !store_hits_block(run, addr, 4)));
    run.shadow_from(cpu);
    match cpu.pending_load.take() {
        Some((reg, value)) => {
            run.pend_reg = u32::from(reg);
            run.pend_val = value;
        }
        None => run.pend_reg = 0,
    }
    if !ram_ok && run.irq_enabled != 0 {
        NATIVE_STOP
    } else {
        NATIVE_CONTINUE
    }
}

/// Run op `index` of the block, an [`OpClass::Other`](super::block::OpClass)
/// op outside a delay slot (a trapping add, multiply/divide, HI/LO, MFC0 or
/// a GTE register move), as the batch does: stop before it
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
    let op = unsafe { *run.ops.add(index as usize) };
    if run.ran >= run.budget_left || bus.cycles() + run.issue + 1 >= run.limit {
        return NATIVE_STOP;
    }
    let pc = run.vaddr.wrapping_add(4 * index);
    cpu.tick = run.tick0 + run.ran;
    cpu.pc = pc;
    cpu.pending_load = (run.pend_reg != 0).then_some((run.pend_reg as u8, run.pend_val));
    let mut refetch = false;
    let mut ram_ok = run.ram_ok != 0;
    run.shadow_to(cpu);
    let stepped = cpu.batch_step(
        bus,
        op,
        pc,
        false,
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

/// Whether main RAM holds a GTE command at `vaddr`: the GTE interrupt
/// hazard's look at the instruction after a delay slot.
///
/// # Safety
/// As [`jit_batch_memory`].
#[doc(hidden)]
pub unsafe extern "C" fn jit_peek_gte(run: *mut NativeRun, vaddr: u32) -> u32 {
    // SAFETY: per the contract, the bus pointer is live.
    let bus = unsafe { &*(*run).bus };
    u32::from(bus.peek_is_gte_command(vaddr))
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
    use super::block::op_flags;
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
