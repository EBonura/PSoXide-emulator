//! The native tier over the interpreter's decoded-block cache.
//!
//! The cached interpreter (`Cpu::run`, `cpu/block.rs`) hands a block to
//! [`TierCompiler`] once it is hot, and the compiler emits AArch64 for as
//! many of its ops, from the first, as the batch may run. Per op the code
//! does what the batch loop does: stop before the op when the batch budget
//! is spent or the clock would reach the quiet limit; the GTE interrupt
//! hazard (inline, or through `jit_gte_hazard` around GTE commands); the
//! issue cycle with the load-shadow rule inline; the operation; the commit
//! of the load in flight unless the op wrote the same register. Register
//! arithmetic and branch decisions run inline, main-RAM loads and word
//! stores through lean helpers, everything else through the batch's own
//! step (`jit_batch_memory`, `jit_batch_other`). At the end of a block
//! `jit_chain` does the batch loop's block-to-block work and returns the
//! next block's code, which is entered by a jump.

use emulator_core::cpu::block::{op_flags, Block, BlockCompiler, OpClass};
use emulator_core::cpu::jit_abi::{
    jit_batch_memory, jit_batch_other, jit_chain, jit_gte_hazard, jit_ram_load, jit_ram_store32,
    link_cell as cell, native_run as off, LinkCell, NATIVE_BODY_OFFSET, NATIVE_CHAIN_OFFSET,
};

use crate::a64::{Asm, Cond, ZR};
use crate::codebuf::CodeBuffer;
use crate::decode::{alu_dest, classify, Class};
use crate::ops::{emit_alu, emit_branch_decision, load_reg, X_GPRS};

// Registers held across the prefix (callee-saved).
const X_RUN: u8 = 19;
const X_CYCLES: u8 = 20;
const X_LIMIT: u8 = 21;
const X_ISSUE: u8 = 23;
const X_RAN: u8 = 24;
const X_BUDGET: u8 = 25;
const X_IRQ: u8 = 26;
// The load in flight while a helper runs (callee-saved).
const W_OLD_REG: u8 = 27;
const W_OLD_VAL: u8 = 28;

/// Shortest prefix worth compiling.
const MIN_PREFIX: usize = 2;
/// Code buffer for the tier.
const TIER_CODE_BYTES: usize = 32 * 1024 * 1024;

/// Counters for reports.
#[derive(Clone, Copy, Debug, Default)]
pub struct TierStats {
    /// Blocks compiled.
    pub compiled: u64,
    /// Blocks declined (prefix too short, buffer full).
    pub declined: u64,
    /// Ops compiled.
    pub ops: u64,
    /// Code bytes emitted.
    pub code_bytes: u64,
    /// Host nanoseconds spent compiling.
    pub compile_ns: u64,
    /// Blocks decoded again with the same words, given their earlier code.
    pub reused: u64,
}

/// The compiler the block cache calls for hot blocks.
pub struct TierCompiler {
    code: CodeBuffer,
    /// Code already emitted, by block address and decoded ops: the block
    /// cache decodes a block again whenever its I-cache lines are refilled
    /// with other code and back, and the same ops compile the same.
    emitted: std::collections::HashMap<(u32, Vec<u64>), usize>,
    /// Exit cells of the emitted code (taken, not taken), at addresses the
    /// code holds: boxed so they stay put as the list grows.
    #[allow(clippy::vec_box)]
    cells: Vec<Box<[LinkCell; 2]>>,
    stats: std::sync::Arc<std::sync::Mutex<TierStats>>,
}

impl TierCompiler {
    /// A compiler with its own code buffer, and a handle to its counters.
    pub fn new() -> Option<(Self, std::sync::Arc<std::sync::Mutex<TierStats>>)> {
        let stats = std::sync::Arc::new(std::sync::Mutex::new(TierStats::default()));
        Some((
            Self {
                code: CodeBuffer::new(TIER_CODE_BYTES)?,
                emitted: std::collections::HashMap::new(),
                cells: Vec::new(),
                stats: stats.clone(),
            },
            stats,
        ))
    }
}

/// How the compiled code runs one op.
enum Step {
    Alu(crate::decode::Alu),
    Branch(crate::decode::Branch),
    /// LB/LH/LW/LBU/LHU: inline when the address is plain main RAM.
    Load,
    /// SW: inline when the address is plain main RAM.
    StoreWord,
    Memory,
    /// Trapping adds, multiply/divide, HI/LO and COP0/GTE register moves,
    /// and GTE commands.
    Other,
}

/// The ops of `block` the compiled code covers: from the first, every op
/// the batch may run, up to the first it may not.
fn plan(block: &Block) -> Vec<Step> {
    let mut steps = Vec::new();
    for op in &block.ops {
        if !emulator_core::cpu::block::tier_batch(op) {
            break;
        }
        steps.push(match (op.class, classify(op.word)) {
            (OpClass::Alu, Class::Alu(alu)) => Step::Alu(alu),
            (OpClass::Branch, Class::Branch(branch)) => Step::Branch(branch),
            (OpClass::Load, _) if matches!(op.word >> 26, 0x20 | 0x21 | 0x23 | 0x24 | 0x25) => {
                Step::Load
            }
            (OpClass::Store, _) if op.word >> 26 == 0x2B => Step::StoreWord,
            (OpClass::Load | OpClass::Store, _) => Step::Memory,
            (OpClass::Other | OpClass::GteCommand, _) => Step::Other,
            _ => break,
        });
    }
    steps
}

impl BlockCompiler for TierCompiler {
    fn compile(&mut self, block: &Block) -> usize {
        let started = std::time::Instant::now();
        let key = (
            block.vaddr,
            block
                .ops
                .iter()
                .map(|op| u64::from(op.word) | u64::from(op.flags) << 32)
                .collect::<Vec<_>>(),
        );
        let mut stats = self.stats.lock().expect("tier stats");
        if let Some(&entry) = self.emitted.get(&key) {
            stats.reused += 1;
            return entry;
        }
        let steps = plan(block);
        if steps.len() < MIN_PREFIX {
            stats.declined += 1;
            self.emitted.insert(key, 0);
            return 0;
        }
        let cells = Box::new([LinkCell::default(), LinkCell::default()]);
        let cell_addrs = [
            &cells[0] as *const LinkCell as u64,
            &cells[1] as *const LinkCell as u64,
        ];
        self.cells.push(cells);
        let asm = emit(block, &steps, cell_addrs);
        let Some(entry) = self.code.install(&asm.code) else {
            stats.declined += 1;
            return 0;
        };
        stats.compiled += 1;
        stats.ops += steps.len() as u64;
        stats.code_bytes += asm.code.len() as u64 * 4;
        stats.compile_ns += started.elapsed().as_nanos() as u64;
        self.emitted.insert(key, entry as usize);
        entry as usize
    }
}

fn call(a: &mut Asm, helper: u64, arg1: u32) {
    a.mov_x(0, X_RUN);
    a.mov32(1, arg1);
    a.mov64(16, helper);
    a.blr(16);
}

/// Commit the load in flight after an op that wrote `dest` through the
/// squashing path (`None`: wrote nothing that way).
fn emit_commit(a: &mut Asm, dest: Option<u8>) {
    a.ldr_w(14, X_RUN, off::PEND_REG as u32);
    let none = a.cbz_w(14);
    let mut squash = None;
    if let Some(d) = dest.filter(|&d| d != 0) {
        a.cmp_w_imm(14, d as u32);
        squash = Some(a.b_cond(Cond::Eq));
    }
    a.ldr_w(15, X_RUN, off::PEND_VAL as u32);
    a.str_w_idx4(15, X_GPRS, 14);
    if let Some(fixup) = squash {
        a.bind(fixup);
    }
    a.str_w(ZR, X_RUN, off::PEND_REG as u32);
    a.bind(none);
}

/// Stop before the op when the budget is spent or the clock would reach
/// the limit (cycles + issue + 1 >= limit).
fn emit_budget_checks(a: &mut Asm, exits: &mut Vec<crate::a64::Fixup>) {
    a.cmp_x(X_RAN, X_BUDGET);
    exits.push(a.b_cond(Cond::Hs));
    a.ldr_x(9, X_CYCLES, 0);
    a.add_x(9, 9, X_ISSUE);
    a.add_x_imm(9, 9, 1);
    a.cmp_x(9, X_LIMIT);
    exits.push(a.b_cond(Cond::Hs));
}

/// The GTE interrupt hazard's check before an op, as the batch does it:
/// with interrupts enabled, ops next to a GTE command (and delay slots, and
/// GTE commands, which may be watched) go through [`jit_gte_hazard`]. The
/// other ops only clear a watch that cannot be set: compiled code runs with
/// RAM holding the block's words, so the decoded flags say where GTE
/// commands are.
///
/// Inside compiled code the watch is only ever set for the op right after
/// the one that set it, which is then a GTE command. So for any other op
/// the helper has work only when the next instruction is a GTE command:
/// known from the block for ops inside it, `after_is_gte` for the last op,
/// and a look at RAM for a delay slot (the branch target when taken).
fn emit_gte_check(
    a: &mut Asm,
    flags: u8,
    gte_command: bool,
    index: usize,
    pc: u32,
    exits: &mut Vec<crate::a64::Fixup>,
) {
    if flags & (op_flags::DELAY_SLOT | op_flags::NEXT_GTE | op_flags::LAST) == 0 && !gte_command {
        return;
    }
    let off_irq = a.cbz_w(X_IRQ);
    let mut quiet = Vec::new();
    if !gte_command && flags & op_flags::NEXT_GTE == 0 {
        if flags & op_flags::DELAY_SLOT != 0 {
            // w12 = the next instruction's address.
            a.ldr_w(10, X_RUN, off::TAKEN as u32);
            a.ldr_w(12, X_RUN, off::TARGET as u32);
            let taken = a.cbnz_w(10);
            a.mov32(12, pc.wrapping_add(4));
            a.bind(taken);
            // Outside main RAM: the helper looks.
            a.mov32(10, 0x1FFF_FFFF);
            a.and_w(13, 12, 10);
            a.lsr_w_imm(10, 13, 23);
            let outside = a.cbnz_w(10);
            a.mov32(10, 0x1F_FFFC);
            a.and_w(13, 13, 10);
            a.ldr_x(11, X_RUN, off::RAM as u32);
            a.add_x(11, 11, 13);
            a.ldr_w(13, 11, 0);
            a.lsr_w_imm(13, 13, 25);
            a.cmp_w_imm(13, 0x4A00_0000 >> 25);
            quiet.push(a.b_cond(Cond::Ne));
            a.bind(outside);
        } else {
            // The last op: the word after the block.
            a.ldr_w(10, X_RUN, off::AFTER_IS_GTE as u32);
            quiet.push(a.cbz_w(10));
        }
    }
    a.str_x(X_ISSUE, X_RUN, off::ISSUE as u32);
    call(a, jit_gte_hazard as *const () as usize as u64, index as u32);
    a.ldr_x(X_ISSUE, X_RUN, off::ISSUE as u32);
    exits.push(a.cbnz_w(0));
    a.bind(off_irq);
    for fixup in quiet {
        a.bind(fixup);
    }
}

/// The issue cycle: one (BIAS), or none for an op hidden in a load
/// shadow. The interpreter's rule (`Cpu::hides_in_load_shadow`) inline:
/// each op moves the shadow one position on; an op that uses the bus,
/// reads the loaded register (as rs or rt), or comes more than six
/// positions after the load ends it and issues normally; positions three to
/// six are hidden. Every fetch here is a cache hit.
fn emit_issue(a: &mut Asm, word: u32) {
    let rs = (word >> 21) & 0x1F;
    let rt = (word >> 16) & 0x1F;
    let touches_bus = word >> 26 >= 0x20;
    a.ldr_w(10, X_RUN, off::SHADOW as u32);
    let no_shadow = a.cbz_w(10);
    let mut ends = Vec::new();
    let hidden;
    if touches_bus {
        ends.push(a.b());
    } else {
        a.ldr_w(11, X_RUN, off::SHADOW_POS as u32);
        a.add_w_imm(11, 11, 1);
        a.str_w(11, X_RUN, off::SHADOW_POS as u32);
        a.ldr_w(12, X_RUN, off::SHADOW_REG as u32);
        let reg_zero = a.cbz_w(12);
        a.cmp_w_imm(12, rs);
        ends.push(a.b_cond(Cond::Eq));
        a.cmp_w_imm(12, rt);
        ends.push(a.b_cond(Cond::Eq));
        a.bind(reg_zero);
        a.cmp_w_imm(11, 6);
        ends.push(a.b_cond(Cond::Hi));
        a.cmp_w_imm(11, 2);
        hidden = Some(a.b_cond(Cond::Hi));
        // Positions one and two: still shadowed, issued normally.
        let plain = a.b();
        a.bind(no_shadow);
        a.bind(plain);
        a.add_x_imm(X_ISSUE, X_ISSUE, 1);
        let joined = a.b();
        for fixup in ends {
            a.bind(fixup);
        }
        a.str_w(ZR, X_RUN, off::SHADOW as u32);
        a.add_x_imm(X_ISSUE, X_ISSUE, 1);
        a.bind(joined);
        if let Some(fixup) = hidden {
            a.bind(fixup);
        }
        return;
    }
    // A bus op ends any shadow and issues normally.
    for fixup in ends {
        a.bind(fixup);
    }
    a.str_w(ZR, X_RUN, off::SHADOW as u32);
    a.bind(no_shadow);
    a.add_x_imm(X_ISSUE, X_ISSUE, 1);
}

/// `cells`: the addresses of the block's exit cells (taken, not taken).
fn emit(block: &Block, steps: &[Step], cells: [u64; 2]) -> Asm {
    let mut a = Asm::default();
    // Frame: x29/x30 and x19..x28.
    a.stp_x_pre(29, 30, -96);
    a.mov_fp_sp();
    a.stp_x(19, 20, 16);
    a.stp_x(21, 22, 32);
    a.stp_x(23, 24, 48);
    a.stp_x(25, 26, 64);
    a.stp_x(27, 28, 80);
    a.mov_x(X_RUN, 0);
    // Chained entry: another block's code jumps here with the frame set up.
    assert_eq!(a.pos() * 4, NATIVE_CHAIN_OFFSET);
    a.ldr_x(X_CYCLES, X_RUN, off::CYCLES as u32);
    a.ldr_x(X_LIMIT, X_RUN, off::LIMIT as u32);
    a.ldr_x(X_GPRS, X_RUN, off::GPRS as u32);
    a.ldr_x(X_ISSUE, X_RUN, off::ISSUE as u32);
    a.ldr_x(X_RAN, X_RUN, off::RAN as u32);
    a.ldr_x(X_BUDGET, X_RUN, off::BUDGET_LEFT as u32);
    a.ldr_w(X_IRQ, X_RUN, off::IRQ_ENABLED as u32);
    // Linked entry: another block's code jumps here with everything set up.
    assert_eq!(a.pos() * 4, NATIVE_BODY_OFFSET);

    let mut exits = Vec::new();
    // The entry state may carry a load in flight.
    let mut maybe_pending = true;
    for (i, step) in steps.iter().enumerate() {
        let op = block.ops[i];
        let word = op.word;
        let pc = block.vaddr.wrapping_add(4 * i as u32);
        match step {
            Step::Alu(_) | Step::Branch(_) => {
                emit_budget_checks(&mut a, &mut exits);
                emit_gte_check(
                    &mut a,
                    op.flags,
                    op.class == OpClass::GteCommand,
                    i,
                    pc,
                    &mut exits,
                );
                emit_issue(&mut a, word);
                let dest = match step {
                    Step::Alu(alu) => {
                        emit_alu(&mut a, *alu, word);
                        alu_dest(Class::Alu(*alu), word)
                    }
                    Step::Branch(branch) => {
                        emit_branch_decision(&mut a, *branch, word, pc);
                        a.str_w(11, X_RUN, off::TAKEN as u32);
                        a.str_w(12, X_RUN, off::TARGET as u32);
                        alu_dest(Class::Branch(*branch), word)
                    }
                    _ => unreachable!(),
                };
                if maybe_pending {
                    emit_commit(&mut a, dest);
                    maybe_pending = false;
                }
                a.add_x_imm(X_RAN, X_RAN, 1);
            }
            Step::Load | Step::StoreWord => {
                let store = matches!(step, Step::StoreWord);
                emit_budget_checks(&mut a, &mut exits);
                emit_gte_check(
                    &mut a,
                    op.flags,
                    op.class == OpClass::GteCommand,
                    i,
                    pc,
                    &mut exits,
                );
                let slow = emit_ram_address(&mut a, word);
                emit_issue(&mut a, word);
                if maybe_pending {
                    emit_take_pending(&mut a);
                }
                let fast = (!store).then(|| {
                    let helper = emit_fast_load(&mut a, word);
                    let done = a.b();
                    for fixup in helper {
                        a.bind(fixup);
                    }
                    done
                });
                a.str_x(X_ISSUE, X_RUN, off::ISSUE as u32);
                // Form the address again for the helper (register values
                // have not changed since).
                load_reg(&mut a, 9, (word >> 21) & 0x1F);
                a.mov32(10, (word as i16) as i32 as u32);
                a.add_w(1, 9, 10);
                a.mov_x(0, X_RUN);
                if store {
                    load_reg(&mut a, 2, (word >> 16) & 0x1F);
                    a.mov32(3, word);
                    a.mov64(16, jit_ram_store32 as *const () as usize as u64);
                } else {
                    a.mov32(2, word);
                    a.mov64(16, jit_ram_load as *const () as usize as u64);
                }
                a.blr(16);
                if let Some(done) = fast {
                    a.bind(done);
                }
                a.mov_x(X_ISSUE, ZR);
                let rt = (word >> 16) & 0x1F;
                if !store && rt != 0 {
                    a.str_w(0, X_RUN, off::PEND_VAL as u32);
                    a.mov32(13, rt);
                    a.str_w(13, X_RUN, off::PEND_REG as u32);
                }
                if maybe_pending {
                    emit_commit_taken(&mut a);
                }
                a.add_x_imm(X_RAN, X_RAN, 1);
                if store {
                    // A store into the block's own words: RAM no longer
                    // matches it for the GTE hazard, which compiled code
                    // needs while interrupts are enabled.
                    let off_irq = a.cbz_w(X_IRQ);
                    a.ldr_w(10, X_RUN, off::RAM_OK as u32);
                    exits.push(a.cbz_w(10));
                    a.bind(off_irq);
                }
                let joined = a.b();
                // The general path: scratchpad, devices, misaligned, KSEG2.
                for fixup in slow {
                    a.bind(fixup);
                }
                a.str_x(X_ISSUE, X_RUN, off::ISSUE as u32);
                a.str_x(X_RAN, X_RUN, off::RAN as u32);
                call(
                    &mut a,
                    jit_batch_memory as *const () as usize as u64,
                    i as u32,
                );
                a.ldr_x(X_ISSUE, X_RUN, off::ISSUE as u32);
                a.ldr_x(X_RAN, X_RUN, off::RAN as u32);
                // A device access may have moved the clock limit.
                a.ldr_x(X_LIMIT, X_RUN, off::LIMIT as u32);
                exits.push(a.cbnz_w(0));
                a.bind(joined);
                maybe_pending = !store;
            }
            Step::Memory | Step::Other => {
                // The budget and clock checks come before the hazard's
                // sample, as in the batch: a sample that consumed a GTE
                // command's watch must not be followed by a stop.
                emit_budget_checks(&mut a, &mut exits);
                emit_gte_check(
                    &mut a,
                    op.flags,
                    op.class == OpClass::GteCommand,
                    i,
                    pc,
                    &mut exits,
                );
                a.str_x(X_ISSUE, X_RUN, off::ISSUE as u32);
                a.str_x(X_RAN, X_RUN, off::RAN as u32);
                let helper = if matches!(step, Step::Memory) {
                    jit_batch_memory as *const () as usize as u64
                } else {
                    jit_batch_other as *const () as usize as u64
                };
                call(&mut a, helper, i as u32);
                a.ldr_x(X_ISSUE, X_RUN, off::ISSUE as u32);
                a.ldr_x(X_RAN, X_RUN, off::RAN as u32);
                a.ldr_x(X_LIMIT, X_RUN, off::LIMIT as u32);
                exits.push(a.cbnz_w(0));
                maybe_pending = true;
            }
        }
    }
    if steps.len() == block.ops.len() {
        // Every op ran: on to the next block, straight through a linked
        // exit when its checks hold, else by `jit_chain` (which links it).
        emit_link_exit(&mut a, block, cells);
        a.str_x(X_ISSUE, X_RUN, off::ISSUE as u32);
        a.str_x(X_RAN, X_RUN, off::RAN as u32);
        a.mov_x(0, X_RUN);
        a.mov64(16, jit_chain as *const () as usize as u64);
        a.blr(16);
        let back = a.cbz_x(0);
        a.br(0);
        a.bind(back);
        let done = a.b();
        for fixup in exits {
            a.bind(fixup);
        }
        a.str_x(X_ISSUE, X_RUN, off::ISSUE as u32);
        a.str_x(X_RAN, X_RUN, off::RAN as u32);
        a.bind(done);
        emit_epilogue(&mut a);
        return a;
    }
    for fixup in exits {
        a.bind(fixup);
    }
    a.str_x(X_ISSUE, X_RUN, off::ISSUE as u32);
    a.str_x(X_RAN, X_RUN, off::RAN as u32);
    emit_epilogue(&mut a);
    a
}

/// The linked exit at the end of a block: [`Cpu::chain_fast`]'s checks
/// and work inline, with the next block taken from the exit's
/// [`LinkCell`]. Falls through, with `run.link` pointing at the cell, to
/// the `jit_chain` call when a check fails.
///
/// [`Cpu::chain_fast`]: emulator_core::Cpu
fn emit_link_exit(a: &mut Asm, block: &Block, cells: [u64; 2]) {
    let n = block.ops.len();
    let mut slow = Vec::new();
    let branch = if block.ops[n - 1].flags & op_flags::DELAY_SLOT != 0 {
        match classify(block.ops[n - 2].word) {
            Class::Branch(branch) => Some(branch),
            _ => None,
        }
    } else {
        None
    };
    match branch {
        None => a.mov64(11, cells[1]),
        Some(branch) => {
            use crate::decode::Branch;
            let pc = block.vaddr.wrapping_add(4 * (n as u32 - 2));
            let word = block.ops[n - 2].word;
            let unconditional =
                matches!(branch, Branch::J | Branch::Jal | Branch::Jr | Branch::Jalr);
            let not_taken = if unconditional {
                None
            } else {
                a.ldr_w(10, X_RUN, off::TAKEN as u32);
                Some(a.cbz_w(10))
            };
            a.mov64(11, cells[0]);
            // The branch-boundary work has nothing to do below the quiet
            // clock and with no interrupt pending or SR written.
            a.ldr_x(9, X_CYCLES, 0);
            a.add_x(9, 9, X_ISSUE);
            a.ldr_x(10, X_RUN, off::BOUNDARY_UNTIL as u32);
            a.cmp_x(9, 10);
            slow.push(a.b_cond(Cond::Hs));
            a.ldr_w(10, X_RUN, off::SLOW_BOUNDARY as u32);
            slow.push(a.cbnz_w(10));
            match branch {
                Branch::Jr | Branch::Jalr => {
                    // Kernel call vectors (low addresses) the slow way; the
                    // cell holds the target it was linked for.
                    a.ldr_w(12, X_RUN, off::TARGET as u32);
                    a.mov32(13, 0x1F_FFFF);
                    a.and_w(13, 12, 13);
                    a.cmp_w_imm(13, 0x100);
                    slow.push(a.b_cond(Cond::Lo));
                    a.ldr_w(13, 11, cell::VADDR as u32);
                    a.cmp_w(13, 12);
                    slow.push(a.b_cond(Cond::Ne));
                }
                _ => {
                    let target = if matches!(branch, Branch::J | Branch::Jal) {
                        (pc.wrapping_add(4) & 0xF000_0000) | ((word & 0x03FF_FFFF) << 2)
                    } else {
                        pc.wrapping_add(4)
                            .wrapping_add((((word as i16) as i32) << 2) as u32)
                    };
                    let base = (target >> 20) & 0x0FFC;
                    if matches!(base, 0x000 | 0x800 | 0xA00) && target & 0x1F_FFFF < 0x100 {
                        slow.push(a.b());
                    }
                }
            }
            a.str_x(9, X_RUN, off::LAST_BOUNDARY as u32);
            if let Some(fixup) = not_taken {
                let joined = a.b();
                a.bind(fixup);
                a.mov64(11, cells[1]);
                a.bind(joined);
            }
        }
    }
    // x11 = the cell. Budget, watch, linked, still current.
    a.cmp_x(X_RAN, X_BUDGET);
    slow.push(a.b_cond(Cond::Hs));
    a.ldr_w(10, X_RUN, off::WATCH as u32);
    slow.push(a.cbnz_w(10));
    a.ldr_x(12, 11, cell::ENTRY as u32);
    slow.push(a.cbz_x(12));
    a.ldr_x(13, X_RUN, off::EPOCH_PTR as u32);
    a.ldr_x(13, 13, 0);
    a.ldr_x(14, X_RUN, off::LINK_GEN_PTR as u32);
    a.ldr_x(14, 14, 0);
    a.add_x(13, 13, 14);
    a.ldr_x(14, 11, cell::GEN as u32);
    a.cmp_x(13, 14);
    slow.push(a.b_cond(Cond::Ne));
    // With interrupts enabled, RAM must still hold the target's words.
    let no_irq = a.cbz_w(X_IRQ);
    a.ldr_x(13, X_RUN, off::PAGES as u32);
    for (page, count) in [(cell::PAGE1, cell::COUNT1), (cell::PAGE2, cell::COUNT2)] {
        a.ldr_w(14, 11, page as u32);
        a.ldr_w_idx4(14, 13, 14);
        a.ldr_w(15, 11, count as u32);
        a.cmp_w(14, 15);
        slow.push(a.b_cond(Cond::Ne));
    }
    a.bind(no_irq);
    // Point `run` at the target and jump.
    a.ldr_x(13, 11, cell::OPS as u32);
    a.str_x(13, X_RUN, off::OPS as u32);
    for (from, to) in [
        (cell::VADDR, off::VADDR),
        (cell::OP_COUNT, off::OP_COUNT),
        (cell::BLOCK, off::BLOCK),
        (cell::AFTER_IS_GTE, off::AFTER_IS_GTE),
    ] {
        a.ldr_w(13, 11, from as u32);
        a.str_w(13, X_RUN, to as u32);
    }
    a.str_x(X_RAN, X_RUN, off::RAN0 as u32);
    a.str_w(X_IRQ, X_RUN, off::RAM_OK as u32);
    a.str_w(ZR, X_RUN, off::TAKEN as u32);
    a.br(12);
    for fixup in slow {
        a.bind(fixup);
    }
    a.str_x(11, X_RUN, off::LINK as u32);
}

fn emit_epilogue(a: &mut Asm) {
    a.ldp_x(19, 20, 16);
    a.ldp_x(21, 22, 32);
    a.ldp_x(23, 24, 48);
    a.ldp_x(25, 26, 64);
    a.ldp_x(27, 28, 80);
    a.ldp_x_post(29, 30, 96);
    a.ret();
}

/// w9 = the effective address of the memory op `word`; branches to the
/// returned fixup unless it is aligned, below KSEG2 and in main RAM.
fn emit_ram_address(a: &mut Asm, word: u32) -> Vec<crate::a64::Fixup> {
    let rs = (word >> 21) & 0x1F;
    let imm = (word as i16) as i32 as u32;
    let mut slow = Vec::new();
    load_reg(a, 9, rs);
    a.mov32(10, imm);
    a.add_w(9, 9, 10);
    let align = match word >> 26 {
        0x21 | 0x25 => 1,
        0x23 | 0x2B => 3,
        _ => 0,
    };
    if align != 0 {
        a.mov32(10, align);
        a.and_w(10, 9, 10);
        slow.push(a.cbnz_w(10));
    }
    // KSEG2 (cache control and above) goes the slow way.
    a.lsr_w_imm(10, 9, 30);
    a.cmp_w_imm(10, 3);
    slow.push(a.b_cond(Cond::Eq));
    // Physical address below the end of the RAM mirrors.
    a.mov32(10, 0x1FFF_FFFF);
    a.and_w(10, 9, 10);
    a.lsr_w_imm(10, 10, 23);
    slow.push(a.cbnz_w(10));
    slow
}

/// A main-RAM load at w9 (checked aligned and in RAM) in the common case,
/// leaving the value in w0; what `jit_ram_load` does, inline: no I-cache
/// fill still using the bus, no DRAM refresh due and the load's end below
/// the quiet limit, so the load costs its six wait clocks, and it comes from
/// cached code (the fetch was a hit), so
/// a load shadow starts. The clock moves here; its GPU decay is left in
/// `run.decay`. Returns the jumps to the helper for the other cases.
fn emit_fast_load(a: &mut Asm, word: u32) -> Vec<crate::a64::Fixup> {
    const LOAD_WAIT: u32 = 6;
    let state = off::LOAD_STATE as u32;
    let mut helper = Vec::new();
    // x10 = now, the clock with this op's issue applied.
    a.ldr_x(10, X_CYCLES, 0);
    a.add_x(10, 10, X_ISSUE);
    a.ldr_x(13, X_RUN, state);
    a.ldr_x(13, 13, 0);
    a.cmp_x(10, 13);
    helper.push(a.b_cond(Cond::Lo));
    a.ldr_x(13, X_RUN, state + 8);
    a.ldr_x(13, 13, 0);
    a.cmp_x(10, 13);
    helper.push(a.b_cond(Cond::Hs));
    // The wait must stay below the quiet limit too: past it, moving the
    // clock can run a GPU list walk, which the helper's clock does.
    a.add_x_imm(14, 10, LOAD_WAIT);
    a.cmp_x(14, X_LIMIT);
    helper.push(a.b_cond(Cond::Hs));
    a.ldr_x(13, X_RUN, state + 16);
    a.str_x(10, 13, 0);
    a.str_x(14, X_CYCLES, 0);
    a.ldr_x(13, X_RUN, off::DECAY as u32);
    a.add_x(13, 13, X_ISSUE);
    a.add_x_imm(13, 13, LOAD_WAIT);
    a.str_x(13, X_RUN, off::DECAY as u32);
    a.mov_x(X_ISSUE, ZR);
    // x14 = the host address.
    a.mov32(13, 0x1F_FFFF);
    a.and_w(13, 9, 13);
    a.ldr_x(14, X_RUN, off::RAM as u32);
    a.add_x(14, 14, 13);
    a.ldr_x(15, X_RUN, state + 24);
    let op = word >> 26;
    if op == 0x23 {
        a.ldr_w(0, 14, 0);
        a.str_w(0, 15, 0);
    } else {
        // Byte or halfword: merge it into the latch at its lane.
        let (lane_mask, width_mask) = if matches!(op, 0x21 | 0x25) {
            a.ldrh_w(0, 14);
            (2, 0xFFFF)
        } else {
            a.ldrb_w(0, 14);
            (3, 0xFF)
        };
        a.mov32(12, lane_mask);
        a.and_w(12, 9, 12);
        a.lsl_w_imm(12, 12, 3);
        a.mov32(13, width_mask);
        a.lslv_w(13, 13, 12);
        a.orn_w(11, ZR, 13);
        a.ldr_w(13, 15, 0);
        a.and_w(13, 13, 11);
        a.lslv_w(11, 0, 12);
        a.orr_w(13, 13, 11);
        a.str_w(13, 15, 0);
        match op {
            0x20 => {
                a.lsl_w_imm(0, 0, 24);
                a.asr_w_imm(0, 0, 24);
            }
            0x21 => {
                a.lsl_w_imm(0, 0, 16);
                a.asr_w_imm(0, 0, 16);
            }
            _ => {}
        }
    }
    // The load shadow starts.
    a.movz_w(13, 1, 0);
    a.str_w(13, X_RUN, off::SHADOW as u32);
    a.str_w(ZR, X_RUN, off::SHADOW_POS as u32);
    a.mov32(13, (word >> 16) & 0x1F);
    a.str_w(13, X_RUN, off::SHADOW_REG as u32);
    helper
}

/// Take the load in flight into w27/w28 and clear it (the op about to run
/// commits it afterwards, as the step does).
fn emit_take_pending(a: &mut Asm) {
    a.ldr_w(W_OLD_REG, X_RUN, off::PEND_REG as u32);
    a.ldr_w(W_OLD_VAL, X_RUN, off::PEND_VAL as u32);
    a.str_w(ZR, X_RUN, off::PEND_REG as u32);
}

/// Commit the load taken by [`emit_take_pending`] (memory ops never write a
/// register through the squashing path).
fn emit_commit_taken(a: &mut Asm) {
    let none = a.cbz_w(W_OLD_REG);
    a.str_w_idx4(W_OLD_VAL, X_GPRS, W_OLD_REG);
    a.bind(none);
}
