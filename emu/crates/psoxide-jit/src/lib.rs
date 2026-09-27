//! Native tier for the PSoXide R3000A.
//!
//! The interpreter's batched path (`Cpu::run`, over the decoded-block cache
//! in `emulator-core`'s `cpu/block.rs`) is the specification; this crate
//! only specialises it. [`install_tier`] gives `Cpu::run` a
//! [`BlockCompiler`](emulator_core::cpu::block::BlockCompiler) that turns
//! hot blocks into AArch64 code doing exactly what the batch does for them:
//! register arithmetic and branch decisions natively, everything with
//! timing or bus semantics through the batch's own code
//! (`emulator_core::cpu::jit_abi`). The design and its measurements are in
//! `designs/psoxide-jit.md` of the perf workspace.

pub mod a64;
mod codebuf;
pub mod decode;
mod ops;
#[doc(hidden)]
pub mod testgen;
pub mod tier;

use emulator_core::Cpu;

/// Install the native tier over the interpreter's block cache: `Cpu::run`
/// then compiles hot blocks and runs them natively, with results identical
/// to the interpreter's. Returns a handle to its counters, or `None` when
/// the tier is not available here (not an AArch64 host, or executable
/// memory cannot be mapped) or `PSOXIDE_JIT=0` turns it off; the
/// interpreter then runs alone.
pub fn install_tier(cpu: &mut Cpu) -> Option<std::sync::Arc<std::sync::Mutex<tier::TierStats>>> {
    if !cfg!(target_arch = "aarch64") || std::env::var("PSOXIDE_JIT").is_ok_and(|v| v == "0") {
        return None;
    }
    let (compiler, stats) = tier::TierCompiler::new()?;
    cpu.set_block_compiler(Some(Box::new(compiler)));
    Some(stats)
}
