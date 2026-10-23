//! Differential tests for code that misses the I-cache line after line.
//!
//! A function is called, then a second function 4 KiB away (the I-cache is
//! 4 KiB and direct-mapped) takes its lines out of the cache, and the loop
//! goes round again: the function runs through lines that are all missed, which
//! the batch loop takes as one run with a miss to make at the start of each
//! line. Run through `Cpu::run` (with and without the native tier) every
//! program must leave what `Cpu::step` leaves, chunk by chunk.
//!
//! The functions are made to put the run to the test: stores that patch words
//! of lines further on just before they are filled, branches and delay slots
//! across line ends, GTE commands next to the ends of lines, exceptions inside
//! a run, and the other fill modes (two-word fills, no streaming).

use psoxide_jit::testgen::{lockstep, CODE_BASE};

fn i(op: u32, rs: u32, rt: u32, imm: u32) -> u32 {
    op << 26 | rs << 21 | rt << 16 | (imm & 0xffff)
}

fn r(funct: u32, rs: u32, rt: u32, rd: u32, sa: u32) -> u32 {
    rs << 21 | rt << 16 | rd << 11 | sa << 6 | funct
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
const T3: u32 = 11;
const T4: u32 = 12;
const S0: u32 = 16;
const S1: u32 = 17;
const RA: u32 = 31;
/// Where the function under test is, and the function that takes its lines
/// out of the cache.
const BODY: u32 = 0x200;
const EVICTOR: u32 = BODY + 0x1000;
/// The RAM the loads and stores of a function use.
const DATA: u32 = 0x8010_0000;

/// What a function is made of, word by word.
#[derive(Clone, Copy, Debug)]
enum W {
    /// Register arithmetic.
    Alu(u32),
    /// A word load from the data area.
    Load(u32),
    /// A word store to the data area.
    Store(u32),
    /// A branch that is always taken over the `n` words after its delay slot.
    Taken(u32),
    /// A branch that is never taken over the `n` words after its delay slot.
    NotTaken(u32),
    /// A GTE command.
    Gte,
    /// A GTE register move.
    Mtc2(u32),
    /// SYSCALL (the handler steps over it).
    Syscall,
    /// BREAK (the handler steps over it).
    Break,
    /// The store that patches the function (see [`Spec::patch`]).
    Patch,
    /// A load from the word the function patches.
    ReadPatched,
}

/// One program.
#[derive(Clone, Debug)]
struct Spec {
    /// The function's words (it ends with `jr ra` and a NOP by itself).
    words: Vec<W>,
    /// The word the function is entered at.
    enter: usize,
    /// How many words long the function that evicts it is (from the same
    /// cache lines); fewer than the function's own leaves the others cached.
    evict: usize,
    /// For [`W::Patch`]: the word it patches, and the two instructions it
    /// patches in turn.
    patch: Option<(usize, [u32; 2])>,
    /// The cache-control register, if the program sets it.
    cache_control: Option<u32>,
    /// Enable the timer interrupt and the GTE (SR), as the fuzz programs do.
    interrupts: bool,
}

fn alu(n: u32) -> u32 {
    // addiu v0..a0, a different register and immediate for each.
    i(0x09, 2 + (n % 3), 2 + (n % 3), 1 + n)
}

fn build(spec: &Spec) -> Vec<u32> {
    let mut code = vec![NOP; (EVICTOR as usize / 4) + spec.words.len() + spec.evict + 16];
    // The prologue: the data base, the patch address and its values, the
    // cache-control register, SR.
    let mut pro = vec![
        i(0x0f, 0, 20, DATA >> 16), // lui s4, DATA
        i(0x0f, 0, T0, CODE_BASE >> 16),
    ];
    if let Some((target, values)) = spec.patch {
        pro.extend([
            i(0x0d, T0, T0, BODY + 4 * target as u32),
            i(0x0f, 0, T1, values[0] >> 16),
            i(0x0d, T1, T1, values[0]),
            i(0x0f, 0, T3, values[1] >> 16),
            i(0x0d, T3, T3, values[1]),
            // t4 = values[0] ^ values[1]: toggling t1 by it alternates them.
            r(0x26, T1, T3, T4, 0),
        ]);
    }
    if let Some(value) = spec.cache_control {
        pro.extend([
            i(0x0f, 0, S0, 0xfffe),
            i(0x0d, S0, S0, 0x0130),
            i(0x0f, 0, S1, value >> 16),
            i(0x0d, S1, S1, value),
            i(0x2b, S0, S1, 0), // sw s1, 0(s0)
        ]);
    }
    if spec.interrupts {
        // Timer 2 interrupts every few hundred cycles; I_MASK = VBlank |
        // Timer 2; SR = IEc | IM2 | CU2.
        pro.extend([
            i(0x0f, 0, 1, 0x1f80),
            i(0x09, 0, 5, 0x180),
            i(0x29, 1, 5, 0x1128), // sh target
            i(0x09, 0, 5, 0x0058),
            i(0x29, 1, 5, 0x1124), // sh mode
            i(0x09, 0, 5, 0x41),
            i(0x2b, 1, 5, 0x1074), // sw i_mask
            i(0x0f, 0, 5, 0x4000),
            i(0x0d, 5, 5, 0x0401),
            0x4085_6000, // mtc0 a1, sr
        ]);
    }
    let loop_at = 4 * pro.len() as u32;
    // The loop: run the function, evict it, toggle the patch.
    pro.extend([jal(BODY + 4 * spec.enter as u32), NOP, jal(EVICTOR), NOP]);
    if spec.patch.is_some() {
        pro.push(r(0x26, T1, T4, T1, 0)); // xor t1, t1, t4
    }
    pro.extend([j(loop_at), NOP]);
    assert!(
        4 * (pro.len() as u32) < BODY,
        "the prologue fits before the function"
    );
    code[..pro.len()].copy_from_slice(&pro);
    let mut put = |offset: u32, word: u32| code[(offset / 4) as usize] = word;
    // The function.
    let mut pos = BODY;
    for &word in &spec.words {
        match word {
            W::Alu(k) => put(pos, alu(k)),
            W::Load(k) => put(pos, i(0x23, 20, 2 + (k % 3), 4 * (k % 16))),
            W::Store(k) => put(pos, i(0x2b, 20, 2 + (k % 3), 4 * (k % 16))),
            W::Taken(skip) => put(pos, i(0x04, 0, 0, skip + 1)), // beq r0, r0, over
            W::NotTaken(skip) => put(pos, i(0x05, 0, 0, skip + 1)), // bne r0, r0, over
            W::Gte => put(pos, 0x4a18_0001),                     // RTPS
            W::Mtc2(k) => put(pos, 0x4880_0000 | (2 + (k % 3)) << 16 | (k % 8) << 11),
            W::Syscall => put(pos, 0x0000_000c),
            W::Break => put(pos, 0x0000_000d),
            W::Patch => put(pos, i(0x2b, T0, T1, 0)), // sw t1, 0(t0)
            W::ReadPatched => put(pos, i(0x23, T0, 6, 0)), // lw a2, 0(t0)
        }
        pos += 4;
    }
    // The return.
    put(pos, r(0x08, RA, 0, 0, 0));
    put(pos + 4, NOP);
    // The patched word starts as the second of its two values.
    if let Some((target, values)) = spec.patch {
        put(BODY + 4 * target as u32, values[1]);
    }
    // The function that takes the lines out of the cache.
    for n in 0..spec.evict {
        put(EVICTOR + 4 * n as u32, alu(n as u32 + 7));
    }
    put(EVICTOR + 4 * spec.evict as u32, r(0x08, RA, 0, 0, 0));
    put(EVICTOR + 4 * spec.evict as u32 + 4, NOP);
    code
}

fn run(spec: &Spec, seeds: std::ops::RangeInclusive<u64>) {
    let code = build(spec);
    // The handler: an interrupt returns, anything else steps over its
    // instruction (as the fuzz programs' does).
    let handler = vec![
        0x401a_6800, // mfc0 k0, cause
        NOP,
        i(0x0c, 26, 26, 0x7c),
        i(0x05, 26, 0, 7),
        NOP,
        i(0x0f, 0, 27, 0x1f80),
        i(0x2b, 27, 0, 0x1070),
        0x401a_7000, // mfc0 k0, epc
        NOP,
        r(0x08, 26, 0, 0, 0),
        0x4200_0010, // rfe
        0x401a_7000,
        NOP,
        i(0x09, 26, 26, 4),
        r(0x08, 26, 0, 0, 0),
        0x4200_0010,
    ];
    for tier in [false, true] {
        if tier && !cfg!(target_arch = "aarch64") {
            continue;
        }
        for seed in seeds.clone() {
            if let Err(e) = lockstep(&code, &handler, 60_000, 40, seed, tier) {
                panic!("{spec:?} seed {seed} tier {tier}: {e}");
            }
        }
    }
}

fn straight(n: usize) -> Vec<W> {
    (0..n as u32).map(W::Alu).collect()
}

/// An ALU word every `every` words, loads and stores between.
fn mixed(n: usize) -> Vec<W> {
    (0..n as u32)
        .map(|k| match k % 5 {
            0 | 1 => W::Alu(k),
            2 => W::Load(k),
            3 => W::Store(k),
            _ => W::Alu(k + 3),
        })
        .collect()
}

fn spec(words: Vec<W>) -> Spec {
    let evict = words.len();
    Spec {
        words,
        enter: 0,
        evict,
        patch: None,
        cache_control: None,
        interrupts: false,
    }
}

#[test]
fn straight_code_misses_line_after_line() {
    for n in [3, 4, 5, 9, 16, 17, 31, 40, 70] {
        run(&spec(straight(n)), 1..=2);
        run(&spec(mixed(n)), 1..=2);
    }
}

#[test]
fn a_run_starts_at_any_word_of_a_line() {
    for enter in 0..9 {
        let mut s = spec(mixed(30));
        s.enter = enter;
        run(&s, 1..=2);
    }
}

#[test]
fn a_line_that_stays_cached_cuts_the_run() {
    // The evicting function takes only some of the lines, so the function
    // meets cached lines between missed ones.
    for evict in [2, 4, 6, 8, 12, 13, 20, 27] {
        let mut s = spec(mixed(36));
        s.evict = evict;
        run(&s, 1..=2);
    }
}

/// The last missed line ends in a branch whose delay slot is the first word
/// of a line that stayed cached, or in the word before a GTE command that
/// starts one.
#[test]
fn the_end_of_a_run_next_to_a_cached_line() {
    for evict in [4, 8, 12] {
        for what in 0..4 {
            let mut words = mixed(24);
            match what {
                0 => {
                    words[evict - 1] = W::Taken(0);
                    words[evict] = W::Alu(80);
                }
                1 => {
                    words[evict - 1] = W::NotTaken(1);
                    words[evict] = W::Store(81);
                }
                2 => words[evict] = W::Gte,
                _ => {
                    words[evict - 2] = W::Mtc2(3);
                    words[evict - 1] = W::Alu(5);
                    words[evict] = W::Gte;
                }
            }
            let mut s = spec(words);
            s.evict = evict;
            s.interrupts = what >= 2;
            run(&s, 1..=2);
        }
    }
}

#[test]
fn branches_and_delay_slots_across_line_ends() {
    // A branch on each word of a line, taken and not, over 0..3 words, with
    // its delay slot on the next line for the last word.
    for at in 0..8 {
        for skip in 0..3 {
            let mut words = mixed(20);
            words[at] = W::Taken(skip);
            words[at + 1] = W::Alu(90);
            run(&spec(words.clone()), 1..=1);
            words[at] = W::NotTaken(skip);
            run(&spec(words), 1..=1);
        }
    }
}

#[test]
fn two_branches_in_one_run() {
    for at in 0..4 {
        let mut words = mixed(34);
        words[at] = W::NotTaken(1);
        words[at + 1] = W::Alu(91);
        words[at + 9] = W::Taken(0);
        words[at + 10] = W::Store(92);
        run(&spec(words), 1..=2);
    }
}

/// A store that patches a word of a line further on just before that line is
/// filled: the line must be filled with what the store wrote.
#[test]
fn a_store_patches_a_later_line_of_the_run() {
    let values = [i(0x09, 0, T3, 0x1111), i(0x09, 0, T3, 0x2222)];
    for patched in [5, 6, 7, 8, 9, 12, 13, 16, 17, 20] {
        for store in 0..patched.min(12) {
            let mut words = mixed(24);
            words[store] = W::Patch;
            // Read it back after the patch has been fetched.
            words[23] = W::ReadPatched;
            let mut s = spec(words);
            s.patch = Some((patched, values));
            run(&s, 1..=2);
        }
    }
}

/// The patching store is the last word of a line, or the word before the
/// first of the line patched.
#[test]
fn a_store_at_the_end_of_a_line_patches_the_next() {
    let values = [i(0x09, 0, T3, 0x1111), i(0x09, 0, T3, 0x2222)];
    for store in [3, 7, 11] {
        for patched in store + 1..store + 6 {
            let mut words = mixed(24);
            words[store] = W::Patch;
            let mut s = spec(words);
            s.patch = Some((patched, values));
            run(&s, 1..=3);
        }
    }
}

/// The patch makes the word a branch, and a store, and a GTE command.
#[test]
fn the_patched_word_changes_what_follows() {
    let kinds = [
        [i(0x09, 0, T3, 1), 0x4a18_0001],
        [i(0x09, 0, T3, 1), i(0x04, 0, 0, 2)],
        [i(0x2b, 20, T3, 8), i(0x23, 20, T3, 8)],
        [i(0x09, 0, T3, 1), 0x0000_000c],
    ];
    for values in kinds {
        for patched in [4, 5, 7, 8, 9] {
            let mut words = mixed(20);
            words[1] = W::Patch;
            let mut s = spec(words);
            s.patch = Some((patched, values));
            s.interrupts = true;
            run(&s, 1..=2);
        }
    }
}

#[test]
fn gte_commands_next_to_line_ends() {
    for at in 0..12 {
        let mut words = mixed(20);
        words[at] = W::Gte;
        let mut s = spec(words.clone());
        s.interrupts = true;
        run(&s, 1..=3);
        words[at] = W::Mtc2(at as u32);
        words[(at + 1) % 20] = W::Gte;
        let mut s = spec(words);
        s.interrupts = true;
        run(&s, 1..=2);
    }
}

#[test]
fn exceptions_inside_a_run() {
    for at in 0..14 {
        let mut words = mixed(20);
        words[at] = W::Syscall;
        run(&spec(words.clone()), 1..=2);
        words[at] = W::Break;
        let mut s = spec(words);
        s.interrupts = true;
        run(&s, 1..=2);
    }
}

#[test]
fn other_fill_modes() {
    // IBLKSZ 0 (two-word fills), 1 (four words), NOSTR.
    for control in [0x0001_e888, 0x0001_e988, 0x0003_e988, 0x0003_e888] {
        for n in [9, 17, 30] {
            let mut s = spec(mixed(n));
            s.cache_control = Some(control);
            run(&s, 1..=2);
            let mut s = spec(mixed(n));
            s.cache_control = Some(control);
            s.interrupts = true;
            s.enter = 2;
            run(&s, 1..=1);
        }
    }
}
