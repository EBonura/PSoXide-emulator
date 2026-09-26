//! Differential fuzzing: random programs run under the recompiler and the
//! interpreter must leave identical machines behind, block by block.

use psoxide_jit::testgen::{lockstep, program, Rng};

fn run(seed: u64, body: usize, gte_percent: u32, instructions: u64) {
    let mut rng = Rng::new(seed);
    let (code, handler) = program(&mut rng, body, gte_percent);
    let coverage = lockstep(&code, &handler, instructions, 2_000)
        .unwrap_or_else(|e| panic!("seed {seed} body {body} gte {gte_percent}%: {e}"));
    assert!(coverage.native > 0, "seed {seed}: nothing ran compiled");
}

#[test]
fn short_loops_without_gte() {
    for seed in 1..=24 {
        run(seed, 40, 0, 200_000);
    }
}

#[test]
fn short_loops_with_gte() {
    for seed in 100..=124 {
        run(seed, 60, 12, 200_000);
    }
}

#[test]
fn bodies_larger_than_the_icache() {
    for seed in 200..=205 {
        run(seed, 2_500, 5, 400_000);
    }
}
