//! The native tier's batch loop: [`Cpu::run_fast`] with a
//! [`BlockCompiler`] installed runs here instead. It follows the same rules
//! as the plain batch loop in the parent module, and also enters compiled
//! code, steps across the GPU's quiet span one exact step at a time, runs
//! GTE commands and LWC2/SWC2 outside delay slots, and runs single
//! interpreter steps where they stay quiet. Kept apart so the plain loop is
//! untouched when no compiler is installed.

use super::*;

/// A second tier that turns hot blocks into native code (the recompiler,
/// `psoxide-jit`). Installed with [`Cpu::set_block_compiler`].
pub trait BlockCompiler: Send + Sync {
    /// Compile the run of ops of `block` the native tier handles, from its
    /// first op. Returns the entry address of a
    /// [`jit_abi::NativeFn`](crate::cpu::jit_abi::NativeFn), or 0 when nothing
    /// is worth compiling (the block then stays interpreted).
    fn compile(&mut self, block: &Block) -> usize;
}

/// How the native tier handed control back to the batch loop
/// ([`Cpu::run_native`]).
enum NativeExit {
    /// Carry on interpreting at the cursor.
    Resume,
    /// End the batch (an interrupt was taken, an exception raised, or no
    /// block could be entered).
    Break,
}

/// The batch loop's running state ([`Cpu::run_fast`]), shared with the
/// native tier's chaining helper.
#[doc(hidden)]
pub struct BatchState {
    /// Issue cycles not yet applied to the bus clock.
    pub issue: u64,
    /// Steps retired in this batch.
    pub done: u64,
    /// Steps the batch may retire.
    pub budget: u64,
    /// Steps since the interrupt line was last counted.
    pub uncounted: u64,
    /// Clock limit the batch must never reach.
    pub hard: u64,
    /// `hard`, or the end of the GPU's quiet span when that is earlier.
    pub limit: u64,
    /// The caller's cycle limit.
    pub until_cycle: u64,
    /// The fetch flags need setting for a plain cache hit.
    pub refetch: bool,
}

/// Entries after which a block is handed to the native tier.
const NATIVE_THRESHOLD: u32 = 16;
/// [`Block::native`] for a block the native tier declined.
pub const NATIVE_DECLINED: usize = 1;

/// Whether the tier's loop runs `op` itself: everything but terminators.
/// Beyond the plain loop's [`op_flags::BATCH`] ops that is GTE commands,
/// LWC2/SWC2, and the ops that can trap in a delay slot (the step enters
/// the exception as a delay slot's, and the batch ends there).
#[inline(always)]
pub fn tier_batch(op: &DecodedOp) -> bool {
    op.class != OpClass::Terminator
}

impl BlockCache {
    /// Where the link generation lives, for compiled code.
    pub(in crate::cpu) fn link_gen_ptr(&self) -> *const u64 {
        &self.link_gen
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
    pub(in crate::cpu) fn run_tiered(
        &mut self,
        bus: &mut Bus,
        budget: u64,
        until_cycle: u64,
    ) -> u64 {
        if self.cursor.block == 0 || self.cursor.pc != self.pc {
            self.cursor.block = 0;
            let entered = self.block_entry_ok() && self.enter_block(bus, self.pc);
            if !entered && !self.can_interpret_in_batch(bus) {
                return 0;
            }
        }
        // The clock limit: `hard` must never be reached inside the batch;
        // `limit` is also below the GPU's quiet span, past which each
        // step's issue cycle is applied at once (`eager`) so the GPU list
        // walk and FIFO advance exactly as under per-instruction ticks.
        let (hard, soft) = bus.quiet_limits();
        let hard = hard.min(until_cycle);
        let mut st = BatchState {
            issue: 0,
            done: 0,
            budget,
            uncounted: 0,
            hard,
            limit: hard.min(soft),
            until_cycle,
            refetch: true,
        };
        let mut eager = false;
        // Set when the native tier handed back control at the start of a
        // block, so it is not asked again there.
        let mut skip_native = false;
        'blocks: loop {
            if self.cursor.block == 0 || self.cursor.pc != self.pc {
                // Outside any block (an I-cache miss, a delay slot entered
                // from outside, a block end): run single steps through the
                // interpreter while that keeps the batch's guarantees.
                self.cursor.block = 0;
                if !(self.block_entry_ok() && self.enter_block(bus, self.pc)) {
                    if !self.batch_interpret_step(bus, &mut st) {
                        break 'blocks;
                    }
                    continue 'blocks;
                }
            }
            let irq_enabled = self.irq_enabled();
            let index = (self.cursor.block - 1) as usize;
            // The hazard reads the next word from the block while RAM
            // matches it; a store in the batch ends that for this block.
            let mut ram_ok = irq_enabled && self.block_ram_matches(bus, index);
            let after_is_gte = self.blocks.blocks[index].after_is_gte;
            if !skip_native && self.cursor.op == 0 && self.blocks.compiler.is_some() {
                match self.run_native(bus, index, irq_enabled, ram_ok, &mut st) {
                    Some(NativeExit::Resume) => {
                        skip_native = true;
                        continue 'blocks;
                    }
                    Some(NativeExit::Break) => break 'blocks,
                    None => {}
                }
            }
            skip_native = false;
            let ops = self.blocks.blocks[index].ops.as_ptr();
            loop {
                let pc = self.pc;
                // SAFETY: the cursor indexes an op of this block (it stops at
                // the op flagged LAST), and nothing below touches the block
                // cache until the loop leaves this block.
                let op = unsafe { *ops.add(self.cursor.op as usize) };
                let delay = op.flags & op_flags::DELAY_SLOT != 0;
                if st.done >= st.budget {
                    break 'blocks;
                }
                if !tier_batch(&op) {
                    // Not a batch op: a single interpreter step, when safe.
                    if !self.batch_interpret_step(bus, &mut st) {
                        break 'blocks;
                    }
                    continue 'blocks;
                }
                if bus.cycles() + st.issue + 1 >= st.limit {
                    if bus.cycles() + st.issue + 1 >= st.hard {
                        break 'blocks;
                    }
                    // Past the GPU's quiet span: settle the clock exactly
                    // and go on one step at a time.
                    bus.advance_exact(st.issue);
                    st.issue = 0;
                    self.batch_new_limits(bus, &mut st);
                    if bus.cycles() + 1 >= st.hard {
                        break 'blocks;
                    }
                    eager = bus.cycles() + 1 >= st.limit;
                }
                if irq_enabled {
                    // The GTE interrupt hazard (`Cpu::gte_irq_hazard`): around
                    // a GTE command the step samples the interrupt line.
                    let next = if delay {
                        self.pending_pc.unwrap_or(pc.wrapping_add(4))
                    } else {
                        pc.wrapping_add(4)
                    };
                    let next_gte = if delay {
                        bus.peek_is_gte_command(next)
                    } else if ram_ok {
                        if op.flags & op_flags::LAST != 0 {
                            after_is_gte
                        } else {
                            op.flags & op_flags::NEXT_GTE != 0
                        }
                    } else {
                        bus.peek_is_gte_command(next)
                    };
                    let watched = self.gte_irq_watch == Some(pc);
                    if watched || next_gte {
                        if !self.batch_gte_sample(bus, &mut st.issue) {
                            break 'blocks;
                        }
                        self.gte_irq_watch = next_gte.then_some(next);
                    } else {
                        self.gte_irq_watch = None;
                    }
                } else {
                    self.gte_irq_watch = None;
                }
                let memory = matches!(op.class, OpClass::Load | OpClass::Store);
                let mut addr = 0;
                // A device access (I/O, BIOS, expansion) may change anything
                // a caller looks at: run it, then end the batch, unless it
                // was one [`Cpu::device_step_quiet`] lets the batch go past.
                let mut device = false;
                let mut raised = 0;
                if memory {
                    addr = self.gpr(op.rs).wrapping_add((op.word as i16) as i32 as u32);
                    match tier_access_kind(op.word, addr) {
                        Access::Quiet => {}
                        Access::Device => {
                            device = true;
                            raised = irq_raise_total(bus);
                        }
                        Access::Unsafe => break 'blocks,
                    }
                }
                // -- the step, as `execute_one_inner` + `execute_fetched` --
                let Some(branch_after_this) = self.batch_step(
                    bus,
                    op,
                    pc,
                    delay,
                    addr,
                    &mut st.issue,
                    &mut st.refetch,
                    st.limit,
                    &mut ram_ok,
                ) else {
                    break 'blocks;
                };
                if eager {
                    // This step's issue cycle, applied as its tick would.
                    bus.advance_exact(st.issue);
                    st.issue = 0;
                    eager = false;
                }
                self.tick += 1;
                st.done += 1;
                st.uncounted += 1;
                if let Some(vector) = self.pending_exception_pc.take() {
                    // An `Other` op trapped (overflow, coprocessor
                    // unusable). Entering the exception left the block.
                    // In a taken branch's delay slot the step still does
                    // the branch-boundary work, at the vector.
                    self.pc = vector;
                    if branch_after_this.is_some() {
                        let _ = self.batch_taken_boundary(bus, vector, &mut st);
                    }
                    break 'blocks;
                }
                if device && self.device_step_quiet(bus, addr, raised, &mut st) {
                    device = false;
                    // A DMA it started may have written this block's words.
                    ram_ok = false;
                }
                self.branch_delay_next = op.class == OpClass::Branch;
                if let Some(target) = branch_after_this {
                    if !self.batch_taken_boundary(bus, target, &mut st) || device {
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
                    continue 'blocks;
                }
                self.cursor.op += 1;
                self.cursor.pc = self.pc;
                if device {
                    break 'blocks;
                }
            }
        }
        bus.advance_quiet(st.issue);
        if st.uncounted != 0 && bus.external_interrupt_pending_quiet(st.uncounted) {
            self.irq_line_high_steps = self.irq_line_high_steps.saturating_add(st.uncounted);
        }
        st.done
    }

    /// After a batched step that accessed a device at `addr`: whether the
    /// batch may go on. The caller's `stop_after` looks only at interrupt
    /// counts, telemetry and pad polls ([`Cpu::run`]), so the step must be
    /// to the I/O ports other than the serial ports (pads, memory cards)
    /// and must not have raised an interrupt (`raised` is the raise total
    /// before it). Anything else the access changed that the batch relies
    /// on is taken up again: the clock limits (it may have scheduled an
    /// event or ended the GPU's quiet span) are recomputed here, and the
    /// caller drops its RAM check. The branch-boundary work reads the rest
    /// afresh each time.
    #[inline(never)]
    pub(in crate::cpu) fn device_step_quiet(
        &mut self,
        bus: &Bus,
        addr: u32,
        raised: u64,
        st: &mut BatchState,
    ) -> bool {
        let phys = memory::to_physical(addr);
        if !(0x1F80_1000..0x1F80_2000).contains(&phys)
            || (0x1F80_1040..0x1F80_1060).contains(&phys)
            || irq_raise_total(bus) != raised
        {
            return false;
        }
        self.batch_new_limits(bus, st);
        true
    }

    /// SR IEc and IM2 are both set: the interrupt line can be taken.
    #[inline(always)]
    pub(in crate::cpu) fn irq_enabled(&self) -> bool {
        const IRQ_ENABLED: u32 = 0x401;
        self.cop0[12] & IRQ_ENABLED == IRQ_ENABLED
    }

    /// Recompute the batch's clock limits after the clock or the scheduler
    /// moved outside a quiet stretch.
    #[inline(always)]
    fn batch_new_limits(&self, bus: &Bus, st: &mut BatchState) {
        let (hard, soft) = bus.quiet_limits();
        st.hard = hard.min(st.until_cycle);
        st.limit = st.hard.min(soft);
    }

    /// Whether the interpreter's step at the PC changes nothing but the
    /// CPU, main RAM, the scratchpad and the clock (so a batch may run it
    /// and go on), given that it does not reach the batch's hard limit: a
    /// cached, executable, non-HLE PC outside a taken branch's delay slot
    /// (whose step drains the scheduler), not next to a GTE command with
    /// interrupts enabled (whose step samples the interrupt line), and an
    /// instruction that is register arithmetic, a branch, a trapping or
    /// multiply/divide or COP0/GTE register op, a GTE command without
    /// interrupts, or a load or store to main RAM or the scratchpad. What
    /// the fetch returns is the cache's word on a hit, RAM's on a miss.
    fn can_interpret_in_batch(&self, bus: &Bus) -> bool {
        let pc = self.pc;
        if !self.block_cache_enabled
            || self.cpu_cycle_profile_enabled
            || self.instruction_class_profile_enabled
            || self.instruction_cache_event_profile_enabled
            || self.pending_pc.is_some()
            || self.cop0[12] & (1 << 16) != 0
            || self.cache_control & CACHE_CONTROL_IS1 == 0
            || pc & 3 != 0
            || pc >= 0xA000_0000
        {
            return false;
        }
        let phys = memory::to_physical(pc);
        if phys >= memory::ram::MIRROR_END || (bus.hle_bios_enabled && phys < 0x1_0000) {
            return false;
        }
        let word = match self.instruction_cache.hit(phys) {
            Some(word) => word,
            None => bus.ram_word((phys % RAM_BYTES) as usize),
        };
        if self.irq_enabled()
            && (self.gte_irq_watch.is_some() || bus.peek_is_gte_command(pc.wrapping_add(4)))
        {
            return false;
        }
        match classify(word) {
            Some(OpClass::Alu | OpClass::Branch | OpClass::Other) => true,
            Some(OpClass::GteCommand) => !self.irq_enabled(),
            Some(OpClass::Load | OpClass::Store) => {
                let addr = self
                    .gpr(((word >> 21) & 0x1F) as u8)
                    .wrapping_add((word as i16) as i32 as u32);
                matches!(tier_access_kind(word, addr), Access::Quiet)
            }
            Some(OpClass::Terminator) | None => false,
        }
    }

    /// Inside a batch, run one instruction through the interpreter's step
    /// (see [`Cpu::can_interpret_in_batch`]) after settling the batch's
    /// clock and interrupt-line count. `false`, having changed nothing, when
    /// that is not safe or the budget or clock limit is reached; the batch
    /// then ends. The batch also ends after a step that reached the hard
    /// limit, so the caller sees what it dispatched.
    fn batch_interpret_step(&mut self, bus: &mut Bus, st: &mut BatchState) -> bool {
        if st.done >= st.budget
            || bus.cycles() + st.issue + 1 >= st.hard
            || !self.can_interpret_in_batch(bus)
        {
            return false;
        }
        bus.advance_quiet(st.issue);
        st.issue = 0;
        if st.uncounted != 0 && bus.external_interrupt_pending_quiet(st.uncounted) {
            self.irq_line_high_steps = self.irq_line_high_steps.saturating_add(st.uncounted);
        }
        st.uncounted = 0;
        let stepped = self.execute_one_cached(bus);
        debug_assert!(stepped.is_ok(), "a batch step runs a valid instruction");
        st.done += 1;
        // The step set the fetch flags its own way.
        st.refetch = true;
        if bus.cycles() >= st.hard {
            return false;
        }
        // A quiet step moves nothing the limits depend on but the clock;
        // past the GPU's quiet span its own clock advance ran the walk.
        if bus.cycles() + 1 >= st.limit {
            self.batch_new_limits(bus, st);
        }
        true
    }

    /// The GTE interrupt hazard's sample of the interrupt line at the start
    /// of a step (`Cpu::gte_irq_sample`), inside a batch: the clock is
    /// settled, then the sample's branch-boundary drain is done only when it
    /// has nothing to do ([`Bus::post_op_quiet`]) and no interrupt is
    /// pending, in which case the sample's only effect is the watch the
    /// caller sets. `false` (the step is left to the interpreter) otherwise:
    /// then an interrupt could be taken after a GTE command, which the batch
    /// does not model.
    #[inline(always)]
    pub(in crate::cpu) fn batch_gte_sample(&mut self, bus: &mut Bus, issue: &mut u64) -> bool {
        bus.advance_quiet(*issue);
        *issue = 0;
        !bus.irq().pending() && bus.post_op_quiet()
    }

    /// The branch-boundary work after a taken branch's delay slot, as the
    /// interpreter's step does it: settle the clock, count the interrupt
    /// line, the kernel-call intercept, the scheduler and CD drain (or its
    /// quiet shortcut, then new clock limits), and the interrupt check.
    /// Leaves the PC at `target` and the cursor outside any block. `false`
    /// when an interrupt was taken (the PC is then at its vector).
    #[inline(always)]
    pub(in crate::cpu) fn batch_taken_boundary(
        &mut self,
        bus: &mut Bus,
        target: u32,
        st: &mut BatchState,
    ) -> bool {
        // Taken branch: the branch-boundary work reads the clock and may
        // raise interrupts; settle everything first.
        self.pc = target;
        bus.advance_quiet(st.issue);
        st.issue = 0;
        if bus.external_interrupt_pending_quiet(st.uncounted) {
            self.irq_line_high_steps = self.irq_line_high_steps.saturating_add(st.uncounted);
        }
        st.uncounted = 0;
        self.cursor.block = 0;
        self.apply_redux_bios_kernel_call_intercept();
        if !bus.post_op_quiet() {
            bus.drain_scheduler_events_post_op();
            self.batch_new_limits(bus, st);
        }
        if self.should_take_interrupt(bus) {
            self.should_take_interrupt_steps = self.should_take_interrupt_steps.saturating_add(1);
            self.enter_exception(ExceptionCode::Interrupt, self.pc, false);
            self.pc = self
                .pending_exception_pc
                .take()
                .expect("enter_exception staged a vector");
            return false;
        }
        true
    }

    /// The native code for block `index`, entered at its first op, when
    /// the native tier may run it now: compiled (compiling it once it is
    /// hot), a plain cache-hit fetch at hand, the decoded words standing in
    /// for RAM when the GTE hazard needs them, no watch, budget left.
    #[inline(always)]
    pub(in crate::cpu) fn native_ready(
        &mut self,
        bus: &Bus,
        index: usize,
        irq_enabled: bool,
        ram_ok: bool,
        st: &BatchState,
    ) -> Option<usize> {
        let mut native = self.blocks.blocks[index].native;
        if native == 0 {
            if self.blocks.blocks[index].hits < NATIVE_THRESHOLD {
                return None;
            }
            let compiler = self.blocks.compiler.as_mut()?;
            native = match compiler.compile(&self.blocks.blocks[index]) {
                0 => NATIVE_DECLINED,
                entry => entry,
            };
            self.blocks.blocks[index].native = native;
        }
        if native == NATIVE_DECLINED
            || !bus.code_stream_idle()
            || !bus.jit_fetch_off_ram_bus()
            || (irq_enabled && !ram_ok)
            || self.pending_pc.is_some()
            || self.gte_irq_watch.is_some()
            || st.done >= st.budget
        {
            return None;
        }
        Some(native)
    }

    /// Point `run` at block `index`, about to run from its first op.
    #[inline(always)]
    pub(in crate::cpu) fn native_enter(
        &self,
        bus: &Bus,
        run: &mut crate::cpu::jit_abi::NativeRun,
        index: usize,
        irq_enabled: bool,
        ram_ok: bool,
        st: &BatchState,
    ) {
        let block = &self.blocks.blocks[index];
        run.ops = block.ops.as_ptr();
        run.vaddr = block.vaddr;
        run.op_count = block.ops.len() as u32;
        run.tick0 = self.tick;
        run.issue = st.issue;
        run.limit = st.limit;
        run.budget_left = st.budget - st.done;
        run.ran = 0;
        run.ram_ok = u32::from(ram_ok);
        run.irq_enabled = u32::from(irq_enabled);
        run.taken = 0;
        run.target = 0;
        run.after_is_gte = u32::from(block.after_is_gte);
        run.status = crate::cpu::jit_abi::NATIVE_IN_BLOCK;
        run.ran0 = 0;
        run.block = index as u32 + 1;
        run.watch = 0;
        run.link = std::ptr::null_mut();
        run.boundary_until = bus.boundary_quiet_until();
        run.slow_boundary = u32::from(bus.irq().pending());
        run.last_boundary = 0;
    }

    /// [`jit_chain`](crate::cpu::jit_abi::jit_chain)'s common case: compiled
    /// code ran every op of the current block, and both the branch-boundary
    /// work and the entry into the next block have nothing to do but move
    /// the cursor. That is: after a taken branch, the clock is below
    /// [`Bus::boundary_quiet_until`] (no scheduler, SPU, timer or CD-ROM
    /// work due), no interrupt is pending and SR has not been written (so
    /// no interrupt can be taken and the interrupt-line count adds
    /// nothing), and the target is not a kernel call vector; then the next
    /// block is decoded, unchanged since its I-cache lines were last
    /// checked, compiled, within the budget, with no GTE watch and (with
    /// interrupts enabled) RAM still holding its words. The clock is left
    /// unsettled (issue cycles keep adding up, as within a block) and the
    /// boundary's cycle is kept in `run` for the bus. Returns the next
    /// block's native entry with `run` and the cursor pointing at it;
    /// `None`, having changed nothing but `last_boundary`, otherwise.
    #[inline(always)]
    pub(in crate::cpu) fn chain_fast(
        &mut self,
        bus: &Bus,
        run: &mut crate::cpu::jit_abi::NativeRun,
    ) -> Option<usize> {
        let n = run.op_count as usize;
        // SAFETY: `ops` points at the current block's `op_count` ops.
        let last = unsafe { *run.ops.add(n - 1) };
        let pc = if last.flags & op_flags::DELAY_SLOT != 0 && run.taken != 0 {
            let now = bus.cycles() + run.issue;
            // A delay-slot access may have crossed the GPU quiet span and
            // completed DMA. The cached interrupt state then needs a fresh
            // boundary check even when the scheduler deadline is later.
            if now >= run.limit || now >= run.boundary_until || run.slow_boundary != 0 {
                return None;
            }
            // `apply_redux_bios_kernel_call_intercept`'s vectors.
            let base = (run.target >> 20) & 0x0FFC;
            if matches!(base, 0x000 | 0x800 | 0xA00) && run.target & (RAM_BYTES - 1) < 0x100 {
                return None;
            }
            run.last_boundary = now;
            run.target
        } else {
            run.vaddr.wrapping_add(4 * n as u32)
        };
        if run.ran >= run.budget_left
            || self.gte_irq_watch.is_some()
            || pc & 3 != 0
            || pc >= 0xA000_0000
        {
            return None;
        }
        let phys = memory::to_physical(pc);
        if phys >= memory::ram::MIRROR_END || (bus.hle_bios_enabled && phys < 0x1_0000) {
            return None;
        }
        let id = *self.blocks.slots.get(((phys % RAM_BYTES) >> 2) as usize)?;
        if id == 0 {
            return None;
        }
        let index = (id - 1) as usize;
        let block = &self.blocks.blocks[index];
        if block.vaddr != pc
            || block.checked_epoch != self.instruction_cache.epoch()
            || block.native <= NATIVE_DECLINED
        {
            return None;
        }
        let native = block.native;
        let ram_ok = run.irq_enabled != 0;
        if ram_ok && !self.block_ram_matches(bus, index) {
            return None;
        }
        let block = &self.blocks.blocks[index];
        self.cursor = Cursor {
            block: id,
            op: 0,
            pc,
            cache_control: self.cache_control,
        };
        self.pc = pc;
        run.ops = block.ops.as_ptr();
        run.vaddr = pc;
        run.op_count = block.ops.len() as u32;
        run.block = id;
        run.ran0 = run.ran;
        run.ram_ok = u32::from(ram_ok);
        run.taken = 0;
        run.target = 0;
        run.after_is_gte = u32::from(block.after_is_gte);
        run.status = crate::cpu::jit_abi::NATIVE_IN_BLOCK;
        Some(native)
    }

    /// Link exit `link` (when not null) to the block `run` now points at,
    /// whose compiled code is at `entry`: see [`LinkCell`]. Left unlinked
    /// when main RAM does not hold the block's words.
    ///
    /// [`LinkCell`]: crate::cpu::jit_abi::LinkCell
    pub(in crate::cpu) fn fill_link(
        &mut self,
        bus: &Bus,
        run: &crate::cpu::jit_abi::NativeRun,
        link: *mut crate::cpu::jit_abi::LinkCell,
        entry: usize,
    ) {
        if link.is_null() {
            return;
        }
        let index = run.block as usize - 1;
        if !self.block_ram_matches(bus, index) {
            return;
        }
        let block = &self.blocks.blocks[index];
        let offset = memory::to_physical(block.vaddr) % RAM_BYTES;
        let last = (offset + 4 * block.ops.len() as u32) % RAM_BYTES;
        let (pages, _) = bus.jit_ram_pages();
        let page1 = (offset >> 12) & (RAM_BYTES / 4096 - 1);
        let page2 = (last >> 12) & (RAM_BYTES / 4096 - 1);
        // SAFETY: `link` is a cell of the installed compiler's code, and the
        // page indices are below the page count.
        unsafe {
            *link = crate::cpu::jit_abi::LinkCell {
                entry: entry + crate::cpu::jit_abi::NATIVE_BODY_OFFSET,
                gen: self
                    .instruction_cache
                    .epoch()
                    .wrapping_add(self.blocks.link_gen),
                ops: block.ops.as_ptr() as usize,
                vaddr: block.vaddr,
                block: run.block,
                op_count: block.ops.len() as u32,
                after_is_gte: u32::from(block.after_is_gte),
                page1,
                count1: *pages.add(page1 as usize),
                page2,
                count2: *pages.add(page2 as usize),
            };
        }
    }

    /// Account the ops compiled code retired in the current block.
    #[inline(always)]
    fn native_retired(&mut self, run: &crate::cpu::jit_abi::NativeRun, st: &mut BatchState) {
        self.tick = run.tick0 + run.ran;
        st.done += run.ran;
        st.uncounted += run.ran;
        st.issue = run.issue;
    }

    /// The native tier from a block entered at its first op: run compiled
    /// code, chained from block to block, until it hands back. `None` when
    /// the block cannot run natively now (the batch loop runs it itself).
    /// On return the CPU, the cursor and the batch state are where the
    /// interpreted loop picks up, as if it had run the same ops itself.
    fn run_native(
        &mut self,
        bus: &mut Bus,
        index: usize,
        irq_enabled: bool,
        ram_ok: bool,
        st: &mut BatchState,
    ) -> Option<NativeExit> {
        let native = self.native_ready(bus, index, irq_enabled, ram_ok, st)?;
        if st.refetch {
            bus.note_cached_fetch(true);
            st.refetch = false;
        }
        let mut run = crate::cpu::jit_abi::NativeRun::new(self, bus, st);
        self.native_enter(bus, &mut run, index, irq_enabled, ram_ok, st);
        if let Some((reg, value)) = self.pending_load.take() {
            run.pend_reg = u32::from(reg);
            run.pend_val = value;
        }
        run.shadow_from(self);
        // SAFETY: `native` came from the installed compiler for this block's
        // current ops, and the code only touches the state `run` points at.
        unsafe {
            let entry: crate::cpu::jit_abi::NativeFn = std::mem::transmute(native);
            entry(&mut run);
        }
        run.flush_boundary(bus);
        run.shadow_to(self);
        self.pending_load = (run.pend_reg != 0).then_some((run.pend_reg as u8, run.pend_val));
        match run.status {
            crate::cpu::jit_abi::NATIVE_BREAK => return Some(NativeExit::Break),
            crate::cpu::jit_abi::NATIVE_NEXT_BLOCK => return Some(NativeExit::Resume),
            _ => {}
        }
        // After a device access the batch may not go past, it ends once
        // the op's step is finished (the branch-boundary work too, when it
        // was a delay slot).
        let stopped = run.status == crate::cpu::jit_abi::NATIVE_STOPPED;
        if run.status == crate::cpu::jit_abi::NATIVE_EXCEPTION {
            // The helper left the PC at the vector (and the cursor cleared).
            self.native_retired(&run, st);
            self.branch_delay_next = false;
            self.executing_in_branch_delay = false;
            // The op that trapped; in a taken branch's delay slot its step
            // still does the branch-boundary work, at the vector.
            // SAFETY: `ops` points at the block's ops, and the op ran.
            let op = unsafe { *run.ops.add((run.ran.wrapping_sub(run.ran0) - 1) as usize) };
            if op.flags & op_flags::DELAY_SLOT != 0 && run.taken != 0 {
                let _ = self.batch_taken_boundary(bus, self.pc, st);
            }
            return Some(NativeExit::Break);
        }
        // Stopped in the block it was running (reached by linked code,
        // which leaves the cursor to be set here).
        self.cursor = Cursor {
            block: run.block,
            op: 0,
            pc: run.vaddr,
            cache_control: self.cache_control,
        };
        let index = (self.cursor.block - 1) as usize;
        // Ops of the current block that retired.
        let ran = run.ran - run.ran0;
        if ran as usize == self.blocks.blocks[index].ops.len() {
            // It ran the whole block (stopping after its last op): finish it
            // as the batch loop would.
            return Some(match self.native_block_done(bus, &run, st) {
                Some(_) if !stopped => NativeExit::Resume,
                _ => NativeExit::Break,
            });
        }
        self.native_retired(&run, st);
        self.branch_delay_next = false;
        self.executing_in_branch_delay = false;
        let block = &self.blocks.blocks[index];
        debug_assert!((ran as usize) < block.ops.len(), "a finished block chains");
        self.pc = run.vaddr.wrapping_add(4 * ran as u32);
        self.cursor.op = ran as u32;
        self.cursor.pc = self.pc;
        if block.ops[ran as usize].flags & op_flags::DELAY_SLOT != 0 {
            // Stopped in front of the delay slot: the branch has run.
            self.pending_pc = (run.taken != 0).then_some(run.target);
            self.branch_delay_next = true;
        }
        Some(if stopped {
            NativeExit::Break
        } else {
            NativeExit::Resume
        })
    }

    /// One batched step of `op` at `pc` after its checks (budget, clock
    /// limit, GTE hazard, access kind): the fetch, the issue cycle, the
    /// delay-slot and load-delay bookkeeping, the operation and the load
    /// shadow. Returns the taken branch's target when `op` is a delay slot,
    /// and `None` (having changed nothing but the clock) when a streaming
    /// fetch would leave the quiet span. The caller counts the step.
    /// Shared by [`Cpu::run_fast`] and the recompiler's memory helper.
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    pub(in crate::cpu) fn batch_step(
        &mut self,
        bus: &mut Bus,
        op: DecodedOp,
        pc: u32,
        delay: bool,
        addr: u32,
        issue: &mut u64,
        refetch: &mut bool,
        limit: u64,
        ram_ok: &mut bool,
    ) -> Option<Option<u32>> {
        if !bus.code_stream_idle() {
            if !Self::batch_fetch_streaming(bus, pc, issue, limit) {
                return None;
            }
            *refetch = true;
        } else if *refetch {
            bus.note_cached_fetch(true);
            *refetch = false;
        }
        *issue += if self.load_shadow.is_some() && self.hides_in_load_shadow(op.word, bus) {
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
        let memory = matches!(op.class, OpClass::Load | OpClass::Store);
        match op.class {
            OpClass::Alu | OpClass::Branch => self.execute_register_op(op, pc),
            OpClass::Load | OpClass::Store if op.word >> 26 >= 0x30 => {
                // LWC2/SWC2: through `execute`, which checks the GTE is
                // enabled.
                bus.advance_quiet(*issue);
                *issue = 0;
                self.pc = pc;
                let _ = self.execute(op.word, pc, delay, bus);
                if op.class == OpClass::Store {
                    *ram_ok = false;
                }
            }
            OpClass::Load | OpClass::Store => {
                bus.advance_quiet(*issue);
                *issue = 0;
                self.execute_memory_op(op, addr, bus);
                if op.class == OpClass::Store {
                    *ram_ok = false;
                }
            }
            _ => {
                // Multiply/divide, HI/LO and GTE moves read the clock.
                bus.advance_quiet(*issue);
                *issue = 0;
                self.pc = pc;
                let _ = self.execute(op.word, pc, delay, bus);
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
        Some(branch_after_this)
    }
}

/// Interrupts raised so far, all sources.
#[inline(always)]
pub(in crate::cpu) fn irq_raise_total(bus: &Bus) -> u64 {
    bus.irq().raise_counts().iter().sum()
}

/// Whether a batched load or store of `word` to `addr` is [`Access::Quiet`]
/// (LWC2/SWC2 too: word aligned).
#[inline(always)]
pub(in crate::cpu) fn quiet_access(word: u32, addr: u32) -> bool {
    matches!(tier_access_kind(word, addr), Access::Quiet)
}

/// Whether a batched load or store of `word` to `addr` goes to a device
/// (I/O, BIOS, expansion): not quiet, but one the batch may run.
#[inline(always)]
pub(in crate::cpu) fn device_access(word: u32, addr: u32) -> bool {
    matches!(tier_access_kind(word, addr), Access::Device)
}

#[inline(always)]
fn tier_access_kind(word: u32, addr: u32) -> Access {
    if matches!(word >> 26, 0x32 | 0x3A) && addr & 3 != 0 {
        return Access::Unsafe;
    }
    access_kind(word, addr)
}
