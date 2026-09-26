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
    /// A load shadow is active (in and out).
    pub shadow: u32,
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
}

/// Helper result: go on with the next op.
pub const NATIVE_CONTINUE: u32 = 0;
/// Helper result: stop; the op was not run (or was the last to run, see
/// the helper).
pub const NATIVE_STOP: u32 = 1;

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
    let hidden = cpu.load_shadow.is_some() && cpu.hides_in_load_shadow(word, bus);
    run.shadow = u32::from(cpu.load_shadow.is_some());
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
    let mut refetch = false;
    let mut ram_ok = run.ram_ok != 0;
    let stepped = cpu.batch_step(
        bus,
        op,
        pc,
        false,
        addr,
        &mut run.issue,
        &mut refetch,
        run.limit,
        &mut ram_ok,
    );
    debug_assert!(
        stepped == Some(None),
        "a quiet access in a compiled prefix steps plainly"
    );
    run.ran += 1;
    run.ram_ok = u32::from(ram_ok);
    run.shadow = u32::from(cpu.load_shadow.is_some());
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
