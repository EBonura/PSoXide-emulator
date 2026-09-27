//! Differential fuzzing: random programs run through `Cpu::run` (with and
//! without the native tier) must leave the same machine behind as plain
//! `Cpu::step`, chunk by chunk.

use psoxide_jit::testgen::{lockstep, program, Rng};

fn run(seed: u64, body: usize, gte_percent: u32, instructions: u64) {
    for tier in [false, true] {
        if tier && !cfg!(target_arch = "aarch64") {
            continue;
        }
        let mut rng = Rng::new(seed);
        let (code, handler) = program(&mut rng, body, gte_percent);
        if let Err(e) = lockstep(&code, &handler, instructions, 500, seed, tier) {
            panic!("seed {seed} body {body} gte {gte_percent}% tier {tier}: {e}");
        }
    }
}

#[test]
fn short_loops_without_gte() {
    for seed in 1..=16 {
        run(seed, 40, 0, 150_000);
    }
}

#[test]
fn short_loops_with_gte() {
    for seed in 100..=116 {
        run(seed, 60, 12, 150_000);
    }
}

#[test]
fn bodies_larger_than_the_icache() {
    for seed in 200..=203 {
        run(seed, 2_500, 5, 300_000);
    }
}
