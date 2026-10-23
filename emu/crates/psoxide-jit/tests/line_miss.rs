//! Differential tests for code that runs into I-cache lines that are not
//! cached: the batch loop reads a missed line's words from RAM before the
//! fill, so a store that lands on them just before the fill must show.
//!
//! The program calls a function whose last word on its line is a store that
//! patches a word of the next line, after the cache lost that line to code
//! 4 KiB away, and enters the function at that store, so the store opens a
//! block of its own with the missed line behind it. Run through `Cpu::run`
//! (with the native tier) it must leave what `Cpu::step` leaves.

use psoxide_jit::testgen::{lockstep, CODE_BASE};

fn i(op: u32, rs: u32, rt: u32, imm: u32) -> u32 {
    op << 26 | rs << 21 | rt << 16 | (imm & 0xffff)
}

fn r(funct: u32, rs: u32, rt: u32, rd: u32) -> u32 {
    rs << 21 | rt << 16 | rd << 11 | funct
}

fn jal(offset: u32) -> u32 {
    3 << 26 | (((CODE_BASE + offset) >> 2) & 0x03ff_ffff)
}

fn j(offset: u32) -> u32 {
    2 << 26 | (((CODE_BASE + offset) >> 2) & 0x03ff_ffff)
}

const NOP: u32 = 0;
const T0: u32 = 8;
const T1: u32 = 9;
const T2: u32 = 10;
const T5: u32 = 13;
const RA: u32 = 31;

/// A program that patches word `patched` of the line at offset 0x50 from the
/// store on the last word of the line before it, and runs both before and
/// after the cache loses the patched line. `store_word` is the word of its
/// own line the store sits on.
fn program(patched: u32, store_word: u32) -> Vec<u32> {
    let mut code = vec![NOP; 0x1060 / 4];
    let mut put = |offset: u32, word: u32| code[(offset / 4) as usize] = word;
    // t0 = the patched word's address, t1 the word to patch in (toggled
    // between two additions), t2 the toggle.
    put(0x00, i(0x0f, 0, T0, CODE_BASE >> 16));
    put(0x04, i(0x0d, T0, T0, 0x50 + 4 * patched));
    put(0x08, i(0x0f, 0, T1, 0x25ad));
    put(0x0c, i(0x0d, T1, T1, 0x0001));
    put(0x10, i(0x0d, 0, T2, 0x0101));
    // The loop: run the function, make the cache lose the line the store
    // patches (the function at 0x1050 maps to the same cache line), toggle
    // the patch, enter the function at the store.
    put(0x18, jal(0x40));
    put(0x20, jal(0x1050));
    put(0x28, r(0x26, T1, T2, T1));
    put(0x2c, jal(0x40 + 4 * store_word));
    put(0x34, j(0x18));
    // The function: words before the store, the store on the line's last
    // word if `store_word` is 3, the patched line of additions, a return.
    for word in 0..store_word {
        put(0x40 + 4 * word, NOP);
    }
    put(0x40 + 4 * store_word, i(0x2b, T0, T1, 0));
    for word in store_word + 1..4 {
        put(0x40 + 4 * word, NOP);
    }
    for word in 0..4 {
        put(0x50 + 4 * word, i(0x09, T5, T5, 1));
    }
    put(0x60, r(0x08, RA, 0, 0));
    // The function that takes the patched line's place in the cache.
    put(0x1054, r(0x08, RA, 0, 0));
    code
}

fn run(patched: u32, store_word: u32, seed: u64) {
    let code = program(patched, store_word);
    for tier in [false, true] {
        if tier && !cfg!(target_arch = "aarch64") {
            continue;
        }
        if let Err(e) = lockstep(&code, &[], 150_000, 50, seed, tier) {
            panic!(
                "patched word {patched} store on word {store_word} seed {seed} tier {tier}: {e}"
            );
        }
    }
}

#[test]
fn a_store_on_the_last_word_of_a_line_patches_the_next_line() {
    for patched in 0..4 {
        for seed in 1..=4 {
            run(patched, 3, seed);
        }
    }
}

#[test]
fn a_store_inside_a_line_patches_the_next_line() {
    for store_word in 0..3 {
        for seed in 1..=3 {
            run(0, store_word, seed);
        }
    }
}
