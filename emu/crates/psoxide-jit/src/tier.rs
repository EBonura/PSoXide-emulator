//! The native tier over the interpreter's decoded-block cache.
//!
//! The cached interpreter (`Cpu::run`, `cpu/block.rs`) hands a block to
//! [`TierCompiler`] once it is hot. The compiler emits AArch64 for the
//! block's prefix, the ops from its first up to the first one that is not
//! register arithmetic or a CPU load or store, the block's last op, or an op
//! followed by a GTE command (see `jit_abi::NativeRun`). The interpreter
//! runs the compiled prefix and carries on with the rest of the block
//! itself, so the branch, its delay slot, the branch-boundary work and
//! everything the prefix does not cover stay the interpreter's.
//!
//! Per op the compiled code does what the batch loop does: stop before the
//! op when the batch budget is spent or the clock would reach the quiet
//! limit; add the issue cycle (through `jit_shadow_issue` while a load
//! shadow is active); run the operation; commit the load in flight unless
//! the op wrote the same register. Loads and stores call
//! `jit_batch_memory`, the interpreter's own batched step.

use emulator_core::cpu::block::{op_flags, Block, BlockCompiler, OpClass};
use emulator_core::cpu::jit_abi::{
    jit_batch_memory, jit_batch_other, jit_chain, jit_gte_hazard, jit_ram_load, jit_ram_store32,
    native_run as off, NATIVE_CHAIN_OFFSET,
};

use crate::a64::{Asm, Cond, ZR};
use crate::codebuf::CodeBuffer;
use crate::decode::{alu_dest, classify, Class};
use crate::{emit_alu, emit_branch_decision, X_GPRS};

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
        if op.flags & op_flags::BATCH == 0 {
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
        let asm = emit(block, &steps);
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
fn emit_gte_check(
    a: &mut Asm,
    flags: u8,
    gte_command: bool,
    index: usize,
    exits: &mut Vec<crate::a64::Fixup>,
) {
    if flags & (op_flags::DELAY_SLOT | op_flags::NEXT_GTE | op_flags::LAST) == 0 && !gte_command {
        return;
    }
    let off_irq = a.cbz_w(X_IRQ);
    a.str_x(X_ISSUE, X_RUN, off::ISSUE as u32);
    call(a, jit_gte_hazard as *const () as usize as u64, index as u32);
    a.ldr_x(X_ISSUE, X_RUN, off::ISSUE as u32);
    exits.push(a.cbnz_w(0));
    a.bind(off_irq);
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

fn emit(block: &Block, steps: &[Step]) -> Asm {
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
                    &mut exits,
                );
                let slow = emit_ram_address(&mut a, word);
                emit_issue(&mut a, word);
                if maybe_pending {
                    emit_take_pending(&mut a);
                }
                a.str_x(X_ISSUE, X_RUN, off::ISSUE as u32);
                // The shadow helper may have clobbered w9: form the address
                // again (register values have not changed since).
                crate::load_reg(&mut a, 9, (word >> 21) & 0x1F);
                a.mov32(10, (word as i16) as i32 as u32);
                a.add_w(1, 9, 10);
                a.mov_x(0, X_RUN);
                if store {
                    crate::load_reg(&mut a, 2, (word >> 16) & 0x1F);
                    a.mov32(3, word);
                    a.mov64(16, jit_ram_store32 as *const () as usize as u64);
                } else {
                    a.mov32(2, word);
                    a.mov64(16, jit_ram_load as *const () as usize as u64);
                }
                a.blr(16);
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
                exits.push(a.cbnz_w(0));
                maybe_pending = true;
            }
        }
    }
    if steps.len() == block.ops.len() {
        // Every op ran: on to the next block, by a jump when it runs
        // natively too.
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
    crate::load_reg(a, 9, rs);
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
