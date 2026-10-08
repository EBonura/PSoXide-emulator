//! Per-instruction cycle-cost model.
//!
//! ## Provenance
//!
//! The R3000A issues one instruction per cycle when nothing stalls it
//! (PSX-SPX "CPU Specifications"); the stalls are charged separately by the
//! CPU and bus models from this project's console measurements. See
//! `LICENSE` and `docs/PROVENANCE.md`.

/// One issue cycle applied to each instruction retirement.
///
/// The R3000A issues ordinary cached instructions at one cycle each. Memory
/// and execution-unit stalls are charged separately by the CPU/bus model.
const BIAS: u32 = 1;

pub(super) fn cycle_cost(_instr: u32) -> u32 {
    BIAS
}
