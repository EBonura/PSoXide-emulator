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
use emulator_core::cpu::jit_abi::{jit_batch_memory, jit_shadow_issue, native_run as off};

use crate::a64::{Asm, Cond, ZR};
use crate::codebuf::CodeBuffer;
use crate::decode::{alu_dest, classify, Class};
use crate::{emit_alu, X_GPRS};

// Registers held across the prefix (callee-saved).
const X_RUN: u8 = 19;
const X_CYCLES: u8 = 20;
const X_LIMIT: u8 = 21;
const X_ISSUE: u8 = 23;
const X_RAN: u8 = 24;
const X_BUDGET: u8 = 25;

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
}

/// The compiler the block cache calls for hot blocks.
pub struct TierCompiler {
    code: CodeBuffer,
    stats: std::sync::Arc<std::sync::Mutex<TierStats>>,
}

impl TierCompiler {
    /// A compiler with its own code buffer, and a handle to its counters.
    pub fn new() -> Option<(Self, std::sync::Arc<std::sync::Mutex<TierStats>>)> {
        let stats = std::sync::Arc::new(std::sync::Mutex::new(TierStats::default()));
        Some((
            Self {
                code: CodeBuffer::new(TIER_CODE_BYTES)?,
                stats: stats.clone(),
            },
            stats,
        ))
    }
}

/// How the prefix runs one op.
enum Step {
    Alu(crate::decode::Alu),
    Memory,
}

/// The ops of `block` the prefix covers.
fn prefix(block: &Block) -> Vec<Step> {
    let n = block.ops.len();
    let mut steps = Vec::new();
    for op in block.ops.iter().take(n.saturating_sub(1)) {
        if op.flags & (op_flags::DELAY_SLOT | op_flags::NEXT_GTE | op_flags::LAST) != 0
            || op.flags & op_flags::BATCH == 0
        {
            break;
        }
        match op.class {
            OpClass::Alu => match classify(op.word) {
                Class::Alu(alu) => steps.push(Step::Alu(alu)),
                _ => break,
            },
            OpClass::Load | OpClass::Store if op.word >> 26 <= 0x2E => steps.push(Step::Memory),
            _ => break,
        }
    }
    steps
}

impl BlockCompiler for TierCompiler {
    fn compile(&mut self, block: &Block) -> usize {
        let started = std::time::Instant::now();
        let steps = prefix(block);
        let mut stats = self.stats.lock().expect("tier stats");
        if steps.len() < MIN_PREFIX {
            stats.declined += 1;
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

fn emit(block: &Block, steps: &[Step]) -> Asm {
    let mut a = Asm::default();
    // Frame: x29/x30 and x19..x26.
    a.stp_x_pre(29, 30, -80);
    a.mov_fp_sp();
    a.stp_x(19, 20, 16);
    a.stp_x(21, 22, 32);
    a.stp_x(23, 24, 48);
    a.stp_x(25, 26, 64);
    a.mov_x(X_RUN, 0);
    a.ldr_x(X_CYCLES, X_RUN, off::CYCLES as u32);
    a.ldr_x(X_LIMIT, X_RUN, off::LIMIT as u32);
    a.ldr_x(X_GPRS, X_RUN, off::GPRS as u32);
    a.ldr_x(X_ISSUE, X_RUN, off::ISSUE as u32);
    a.ldr_x(X_RAN, X_RUN, off::RAN as u32);
    a.ldr_x(X_BUDGET, X_RUN, off::BUDGET_LEFT as u32);

    let mut exits = Vec::new();
    // The entry state may carry a load in flight.
    let mut maybe_pending = true;
    for (i, step) in steps.iter().enumerate() {
        let word = block.ops[i].word;
        match step {
            Step::Alu(alu) => {
                // Stop before the op: budget spent, or the clock would reach
                // the limit (cycles + issue + 1 >= limit).
                a.cmp_x(X_RAN, X_BUDGET);
                exits.push(a.b_cond(Cond::Hs));
                a.ldr_x(9, X_CYCLES, 0);
                a.add_x(9, 9, X_ISSUE);
                a.add_x_imm(9, 9, 1);
                a.cmp_x(9, X_LIMIT);
                exits.push(a.b_cond(Cond::Hs));
                // Issue: one cycle (BIAS), or the load-shadow rule.
                a.ldr_w(10, X_RUN, off::SHADOW as u32);
                let shadow = a.cbnz_w(10);
                a.add_x_imm(X_ISSUE, X_ISSUE, 1);
                let joined = a.b();
                a.bind(shadow);
                call(&mut a, jit_shadow_issue as *const () as usize as u64, word);
                a.mov_w(0, 0);
                a.add_x(X_ISSUE, X_ISSUE, 0);
                a.bind(joined);
                emit_alu(&mut a, *alu, word);
                if maybe_pending {
                    emit_commit(&mut a, alu_dest(Class::Alu(*alu), word));
                    maybe_pending = false;
                }
                a.add_x_imm(X_RAN, X_RAN, 1);
            }
            Step::Memory => {
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
                maybe_pending = true;
            }
        }
    }
    for fixup in exits {
        a.bind(fixup);
    }
    a.str_x(X_ISSUE, X_RUN, off::ISSUE as u32);
    a.str_x(X_RAN, X_RUN, off::RAN as u32);
    a.ldp_x(19, 20, 16);
    a.ldp_x(21, 22, 32);
    a.ldp_x(23, 24, 48);
    a.ldp_x(25, 26, 64);
    a.ldp_x_post(29, 30, 80);
    a.ret();
    a
}
