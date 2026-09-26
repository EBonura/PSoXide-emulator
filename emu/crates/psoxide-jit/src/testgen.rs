//! Random R3000A programs for differential testing of the recompiler
//! against the interpreter, and a lockstep runner for them.
//!
//! A program sets up registers, enables interrupts (VBlank and a fast
//! Timer 2) and runs a
//! random body in an endless loop: register arithmetic, loads and stores
//! (aligned and not) to RAM and the scratchpad, multiply/divide and HI/LO
//! moves, trapping adds, forward branches of every kind with random delay
//! slots, counted loops, calls to small leaf functions, and GTE transfers
//! and commands. An
//! exception handler at the general vector acknowledges interrupts and steps
//! over faulting instructions, so address errors, overflows and interrupts
//! all occur and are compared.

use emulator_core::{Bus, Cpu, EmulatorStateRef};

use crate::Jit;

/// Code load address.
pub const CODE_BASE: u32 = 0x8001_0000;
/// RAM data area the loads and stores use.
pub const DATA_BASE: u32 = 0x8010_0000;
const SCRATCH_BASE: u32 = 0x1F80_0000;

/// xorshift64*.
pub struct Rng(u64);

impl Rng {
    /// Seeded generator (seed 0 is remapped).
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    /// Next 64 random bits.
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in `0..n`.
    pub fn below(&mut self, n: u32) -> u32 {
        (self.next() % n as u64) as u32
    }
    /// True with probability `p` percent.
    pub fn chance(&mut self, p: u32) -> bool {
        self.below(100) < p
    }
}

fn r(funct: u32, rs: u32, rt: u32, rd: u32, sa: u32) -> u32 {
    rs << 21 | rt << 16 | rd << 11 | sa << 6 | funct
}
fn i(op: u32, rs: u32, rt: u32, imm: u32) -> u32 {
    op << 26 | rs << 21 | rt << 16 | (imm & 0xFFFF)
}
fn j(op: u32, target: u32) -> u32 {
    op << 26 | ((target >> 2) & 0x03FF_FFFF)
}
const NOP: u32 = 0;
const BASE_RAM: u32 = 20;
const BASE_SCRATCH: u32 = 21;
/// Holds 0x1F80_1000 (I/O registers); outside the random register pool.
const BASE_IO: u32 = 30;

/// Destination/source pool: r1..r19 plus r22..r25 (r20/r21 hold the data
/// bases, r26/r27 belong to the handler, r28..r31 are left alone).
fn reg(rng: &mut Rng) -> u32 {
    let n = rng.below(23);
    if n < 19 {
        n + 1
    } else {
        n + 3
    }
}
fn src(rng: &mut Rng) -> u32 {
    // Sources may also be r0 and the base registers.
    match rng.below(10) {
        0 => 0,
        1 => BASE_RAM,
        _ => reg(rng),
    }
}

fn alu(rng: &mut Rng) -> u32 {
    let (rd, rs, rt) = (reg(rng), src(rng), src(rng));
    let imm = rng.next() as u32 & 0xFFFF;
    match rng.below(20) {
        0 => r(0x21, rs, rt, rd, 0),
        1 => r(0x23, rs, rt, rd, 0),
        2 => r(0x24, rs, rt, rd, 0),
        3 => r(0x25, rs, rt, rd, 0),
        4 => r(0x26, rs, rt, rd, 0),
        5 => r(0x27, rs, rt, rd, 0),
        6 => r(0x2A, rs, rt, rd, 0),
        7 => r(0x2B, rs, rt, rd, 0),
        8 => r(0x00, 0, rt, rd, rng.below(32)),
        9 => r(0x02, 0, rt, rd, rng.below(32)),
        10 => r(0x03, 0, rt, rd, rng.below(32)),
        11 => r(0x04, rs, rt, rd, 0),
        12 => r(0x06, rs, rt, rd, 0),
        13 => r(0x07, rs, rt, rd, 0),
        14 => i(0x09, rs, rd, imm),
        15 => i(0x0A, rs, rd, imm),
        16 => i(0x0B, rs, rd, imm),
        17 => i(0x0C, rs, rd, imm),
        18 => i(0x0D, rs, rd, imm),
        _ => {
            if rng.chance(50) {
                i(0x0E, rs, rd, imm)
            } else {
                i(0x0F, 0, rd, imm)
            }
        }
    }
}

fn mem(rng: &mut Rng) -> u32 {
    if rng.chance(6) {
        // A device read: root counters and modes, I_STAT/I_MASK, GPUSTAT.
        const REGS: [u32; 9] = [
            0x100, 0x104, 0x110, 0x114, 0x120, 0x124, 0x070, 0x074, 0x814,
        ];
        let off = REGS[rng.below(9) as usize];
        let op = if rng.chance(50) { 0x23 } else { 0x25 }; // LW / LHU
        return i(op, BASE_IO, reg(rng), off);
    }
    let base = if rng.chance(75) {
        BASE_RAM
    } else {
        BASE_SCRATCH
    };
    let aligned = rng.chance(92);
    let mut off = rng.below(256);
    let op = [
        0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x28, 0x29, 0x2A, 0x2B, 0x2E,
    ][rng.below(12) as usize];
    if aligned {
        off &= match op {
            0x21 | 0x25 | 0x29 => !1,
            0x23 | 0x2B => !3,
            _ => !0,
        };
    }
    i(op, base, reg(rng), off)
}

fn muldiv(rng: &mut Rng) -> u32 {
    let (rd, rs, rt) = (reg(rng), src(rng), src(rng));
    match rng.below(8) {
        0 => r(0x18, rs, rt, 0, 0),
        1 => r(0x19, rs, rt, 0, 0),
        2 => r(0x1A, rs, rt, 0, 0),
        3 => r(0x1B, rs, rt, 0, 0),
        4 => r(0x10, 0, 0, rd, 0),
        5 => r(0x12, 0, 0, rd, 0),
        6 => r(0x11, rs, 0, 0, 0),
        _ => r(0x13, rs, 0, 0, 0),
    }
}

fn trapping(rng: &mut Rng) -> u32 {
    let (rd, rs, rt) = (reg(rng), src(rng), src(rng));
    match rng.below(3) {
        0 => r(0x20, rs, rt, rd, 0),
        1 => r(0x22, rs, rt, rd, 0),
        _ => i(0x08, rs, rd, rng.next() as u32),
    }
}

fn gte(rng: &mut Rng) -> u32 {
    const COMMANDS: [u32; 8] = [
        0x4A18_0001, // RTPS
        0x4A28_0030, // RTPT
        0x4B40_0006, // NCLIP
        0x4B58_002D, // AVSZ3
        0x4B68_002E, // AVSZ4
        0x4AA0_0428, // SQR
        0x4B70_000C, // OP
        0x4B90_003D, // GPF
    ];
    let rt = reg(rng);
    let rd = rng.below(32);
    match rng.below(6) {
        0 | 1 => COMMANDS[rng.below(8) as usize],
        2 => 0x4800_0000 | rt << 16 | rd << 11, // MFC2
        3 => 0x4880_0000 | rt << 16 | rd << 11, // MTC2
        4 => 0x4840_0000 | rt << 16 | rd << 11, // CFC2
        _ => 0x48C0_0000 | rt << 16 | (rd & 0x1F) << 11, // CTC2
    }
}

/// Any non-branch instruction, for bodies and delay slots.
fn plain(rng: &mut Rng, gte_percent: u32) -> u32 {
    let roll = rng.below(100);
    if roll < gte_percent {
        return gte(rng);
    }
    match rng.below(100) {
        0..=49 => alu(rng),
        50..=79 => mem(rng),
        80..=89 => muldiv(rng),
        90..=92 => trapping(rng),
        _ => NOP,
    }
}

/// Build a program; returns `(code words at CODE_BASE, handler words at
/// 0x8000_0080)`.
pub fn program(rng: &mut Rng, body_len: usize, gte_percent: u32) -> (Vec<u32>, Vec<u32>) {
    let mut code = Vec::new();
    // Timer 2 interrupts every few hundred cycles (reset at target, IRQ on
    // target, repeat); I_MASK = VBlank | Timer 2; SR = IEc | IM2 | CU2.
    let target = 0x100 + rng.below(0x700);
    code.extend([
        i(0x0F, 0, 1, 0x1F80), // lui r1, 0x1f80
        i(0x09, 0, 2, target), // addiu r2, r0, target
        i(0x29, 1, 2, 0x1128), // sh r2, 0x1128(r1)
        i(0x09, 0, 2, 0x0058), // addiu r2, r0, 0x58
        i(0x29, 1, 2, 0x1124), // sh r2, 0x1124(r1)
        i(0x09, 0, 2, 0x41),   // addiu r2, r0, 0x41
        i(0x2B, 1, 2, 0x1074), // sw r2, 0x1074(r1)
        i(0x0F, 0, 2, 0x4000), // lui r2, 0x4000
        i(0x0D, 2, 2, 0x0401), // ori r2, r2, 0x401
        0x4082_6000,           // mtc0 r2, sr
        i(0x0F, 0, BASE_RAM, DATA_BASE >> 16),
        i(0x0F, 0, BASE_SCRATCH, SCRATCH_BASE >> 16),
        i(0x0F, 0, BASE_IO, 0x1F80),
        i(0x0D, BASE_IO, BASE_IO, 0x1000),
    ]);
    for rd in (1..=19).chain(22..=25) {
        let v = rng.next() as u32;
        code.push(i(0x0F, 0, rd, v >> 16));
        code.push(i(0x0D, rd, rd, v));
    }
    // Leaf functions after the loop; calls patched below.
    let body_start = code.len();
    let mut calls = Vec::new();
    let mut n = 0;
    while n < body_len {
        let roll = rng.below(100);
        if roll < 12 {
            // Forward branch over 1..=4 instructions (plus its delay slot).
            let skip = 1 + rng.below(4);
            let (rs, rt) = (src(rng), src(rng));
            let word = match rng.below(8) {
                0 => i(0x04, rs, rt, skip),
                1 => i(0x05, rs, rt, skip),
                2 => i(0x06, rs, 0, skip),
                3 => i(0x07, rs, 0, skip),
                4 => i(0x01, rs, 0x00, skip),
                5 => i(0x01, rs, 0x01, skip),
                6 => i(0x01, rs, 0x10, skip),
                _ => i(0x01, rs, 0x11, skip),
            };
            code.push(word);
            code.push(plain(rng, gte_percent));
            for _ in 0..skip {
                code.push(plain(rng, gte_percent));
            }
            n += 2 + skip as usize;
        } else if roll < 15 {
            calls.push(code.len());
            code.push(NOP); // JAL patched below
            code.push(plain(rng, gte_percent));
            n += 2;
        } else if roll < 19 {
            // A short counted loop on r28 (outside the random pool): a block
            // whose branch goes back to its own start.
            let count = 1 + rng.below(20);
            let k = 1 + rng.below(5);
            code.push(i(0x0D, 0, 28, count)); // ori gp, r0, count
            for _ in 0..k {
                code.push(plain(rng, gte_percent));
            }
            code.push(i(0x09, 28, 28, 0xFFFF)); // addiu gp, gp, -1
            code.push(i(0x05, 28, 0, (-(k as i32 + 2)) as u32)); // bne gp, r0, loop
            code.push(plain(rng, gte_percent));
            n += k as usize + 4;
        } else {
            code.push(plain(rng, gte_percent));
            n += 1;
        }
    }
    let loop_target = CODE_BASE + 4 * body_start as u32;
    code.push(j(0x02, loop_target));
    code.push(plain(rng, gte_percent));
    // Leaf functions: a few instructions, then jr ra (sometimes jalr via a
    // register would clobber the pool, so keep jr).
    let mut leaves = Vec::new();
    for _ in 0..4 {
        leaves.push(CODE_BASE + 4 * code.len() as u32);
        for _ in 0..(1 + rng.below(6)) {
            code.push(plain(rng, gte_percent));
        }
        code.push(r(0x08, 31, 0, 0, 0)); // jr ra
        code.push(plain(rng, gte_percent));
    }
    for at in calls {
        let leaf = leaves[rng.below(leaves.len() as u32) as usize];
        code[at] = j(0x03, leaf);
    }
    // General exception vector: acknowledge interrupts and return to EPC;
    // step over anything else.
    let handler = vec![
        0x401A_6800, // mfc0 k0, cause
        NOP,
        i(0x0C, 26, 26, 0x7C), // andi k0, k0, 0x7c
        i(0x05, 26, 0, 7),     // bne k0, r0, exc
        NOP,
        i(0x0F, 0, 27, 0x1F80), // lui k1, 0x1f80
        i(0x2B, 27, 0, 0x1070), // sw r0, 0x1070(k1)
        0x401A_7000,            // mfc0 k0, epc
        NOP,
        r(0x08, 26, 0, 0, 0), // jr k0
        0x4200_0010,          // rfe
        // exc:
        0x401A_7000, // mfc0 k0, epc
        NOP,
        i(0x09, 26, 26, 4),   // addiu k0, k0, 4
        r(0x08, 26, 0, 0, 0), // jr k0
        0x4200_0010,          // rfe
    ];
    (code, handler)
}

/// A machine loaded with `code` and `handler`, about to run the program.
pub fn machine(code: &[u32], handler: &[u32]) -> (Cpu, Bus) {
    let mut bus = Bus::new_without_bios();
    let mut cpu = Cpu::new();
    let bytes: Vec<u8> = code.iter().flat_map(|w| w.to_le_bytes()).collect();
    bus.load_exe_payload(CODE_BASE, &bytes);
    let bytes: Vec<u8> = handler.iter().flat_map(|w| w.to_le_bytes()).collect();
    bus.load_exe_payload(0x8000_0080, &bytes);
    cpu.seed_from_exe(CODE_BASE, 0, None);
    (cpu, bus)
}

/// Serialized machine state.
pub fn state_bytes(cpu: &Cpu, bus: &Bus) -> Vec<u8> {
    postcard::to_allocvec(&EmulatorStateRef { cpu, bus }).expect("serialize state")
}

/// Run `instructions` instructions of the program under the recompiler
/// (block by block) and the interpreter (step by step) side by side,
/// comparing CPU state after every block and the whole machine every
/// `full_every` blocks and at the end. `Err` describes the first
/// difference; `Ok` carries what was exercised.
pub fn lockstep(
    code: &[u32],
    handler: &[u32],
    instructions: u64,
    full_every: u64,
) -> Result<Coverage, String> {
    let (mut ci, mut bi) = machine(code, handler);
    let (mut cj, mut bj) = machine(code, handler);
    let mut jit = Jit::new().ok_or("no executable memory")?;
    let mut blocks = 0u64;
    while cj.tick() < instructions {
        let from = (cj.pc(), cj.tick());
        let rj = jit.step(&mut cj, &mut bj).map_err(|e| e.to_string());
        let mut ri = Ok(());
        while ci.tick() < cj.tick() && ri.is_ok() {
            ri = ci.step(&mut bi).map_err(|e| e.to_string());
        }
        // An error retires nothing: the interpreter's next step must fail
        // the same way.
        if rj.is_err() && ri.is_ok() && ci.tick() == cj.tick() {
            ri = ci.step(&mut bi).map_err(|e| e.to_string());
        }
        if rj != ri {
            return Err(format!(
                "results differ from {from:x?}: jit {rj:?} interp {ri:?}"
            ));
        }
        blocks += 1;
        let same = ci.tick() == cj.tick()
            && ci.pc() == cj.pc()
            && ci.gprs() == cj.gprs()
            && ci.hi() == cj.hi()
            && ci.lo() == cj.lo()
            && ci.cop0() == cj.cop0()
            && bi.cycles() == bj.cycles()
            && ci.jit_debug_state() == cj.jit_debug_state();
        if !same {
            return Err(format!(
                "state differs after block from pc {:08x} tick {}:\n interp pc={:08x} cycles={} {:?}\n   jit  pc={:08x} cycles={} {:?}\n interp gprs={:08x?}\n   jit  gprs={:08x?}",
                from.0, from.1, ci.pc(), bi.cycles(), ci.jit_debug_state(),
                cj.pc(), bj.cycles(), cj.jit_debug_state(), ci.gprs(), cj.gprs()
            ));
        }
        if rj.is_err() {
            break;
        }
        if (full_every != 0 && blocks.is_multiple_of(full_every))
            && state_bytes(&ci, &bi) != state_bytes(&cj, &bj)
        {
            return Err(format!(
                "full state differs at tick {} (CPU state equal)",
                cj.tick()
            ));
        }
    }
    if state_bytes(&ci, &bi) != state_bytes(&cj, &bj) {
        return Err(format!("final full state differs at tick {}", cj.tick()));
    }
    let stats = jit.stats();
    let counts = cj.exception_counts();
    Ok(Coverage {
        instructions: cj.tick(),
        native: stats.native_instructions,
        interrupts: counts[0],
        address_errors: counts[4] + counts[5],
        overflows: counts[12],
    })
}

/// What one lockstep run exercised.
#[derive(Clone, Copy, Debug, Default)]
pub struct Coverage {
    /// Instructions retired.
    pub instructions: u64,
    /// Of which inside compiled blocks.
    pub native: u64,
    /// Interrupts taken.
    pub interrupts: u64,
    /// Load/store address errors.
    pub address_errors: u64,
    /// Arithmetic overflows.
    pub overflows: u64,
}

/// Instruction mix for [`synthetic`], in percent (the rest are NOPs).
#[derive(Clone, Copy, Debug)]
pub struct Mix {
    /// Register arithmetic.
    pub alu: u32,
    /// Aligned loads and stores to RAM and the scratchpad.
    pub mem: u32,
    /// Forward conditional branches (with an ALU delay slot).
    pub branch: u32,
    /// Multiply/divide and HI/LO moves.
    pub muldiv: u32,
}

/// A CPU-only loop for throughput measurements: no interrupts, no
/// exceptions (aligned accesses, no trapping adds), no GTE. The body of
/// `body_len` instructions repeats forever.
pub fn synthetic(rng: &mut Rng, body_len: usize, mix: Mix) -> (Vec<u32>, Vec<u32>) {
    let mut code = vec![
        i(0x0F, 0, BASE_RAM, DATA_BASE >> 16),
        i(0x0F, 0, BASE_SCRATCH, SCRATCH_BASE >> 16),
    ];
    for rd in (1..=19).chain(22..=25) {
        let v = rng.next() as u32;
        code.push(i(0x0F, 0, rd, v >> 16));
        code.push(i(0x0D, rd, rd, v));
    }
    let body_start = code.len();
    let mut n = 0;
    while n < body_len {
        let roll = rng.below(100);
        if roll < mix.alu {
            code.push(alu(rng));
            n += 1;
        } else if roll < mix.alu + mix.mem {
            let mut word = mem(rng);
            // Keep it aligned: word/half forms get their low bits cleared.
            let op = word >> 26;
            if matches!(op, 0x21 | 0x25 | 0x29) {
                word &= !1;
            } else if matches!(op, 0x23 | 0x2B) {
                word &= !3;
            }
            code.push(word);
            n += 1;
        } else if roll < mix.alu + mix.mem + mix.branch {
            let (rs, rt) = (src(rng), src(rng));
            let op = [0x04, 0x05, 0x06, 0x07][rng.below(4) as usize];
            code.push(i(op, rs, if op < 6 { rt } else { 0 }, 2));
            code.push(alu(rng));
            code.push(alu(rng));
            code.push(alu(rng));
            n += 4;
        } else if roll < mix.alu + mix.mem + mix.branch + mix.muldiv {
            code.push(muldiv(rng));
            n += 1;
        } else {
            code.push(NOP);
            n += 1;
        }
    }
    code.push(j(0x02, CODE_BASE + 4 * body_start as u32));
    code.push(NOP);
    (code, vec![NOP; 4])
}

/// Like [`lockstep`], but checks the interpreter's batched path: one machine
/// runs `Cpu::run` in chunks of random length (drawn from `seed`), the other
/// plain `Cpu::step`, compared after every chunk and in full every
/// `full_every` chunks and at the end.
pub fn lockstep_run(
    code: &[u32],
    handler: &[u32],
    instructions: u64,
    full_every: u64,
    seed: u64,
    tier: bool,
) -> Result<u64, String> {
    let (mut ci, mut bi) = machine(code, handler);
    let (mut cr, mut br) = machine(code, handler);
    if tier {
        crate::install_tier(&mut cr).ok_or("no executable memory")?;
    }
    let mut rng = Rng::new(seed ^ 0x5eed);
    let mut chunks = 0u64;
    while cr.tick() < instructions {
        let from = (cr.pc(), cr.tick());
        let span = match std::env::var("PSOXIDE_FUZZ_SPAN")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            Some(span) => span,
            None if rng.chance(20) => 5000,
            None => 64,
        };
        let chunk = 1 + u64::from(rng.below(span));
        let (_, rr) = cr.run(&mut br, chunk, u64::MAX, |_| false);
        let rr = rr.map_err(|e| e.to_string());
        let mut ri = Ok(());
        while ci.tick() < cr.tick() && ri.is_ok() {
            ri = ci.step(&mut bi).map_err(|e| e.to_string());
        }
        if rr.is_err() && ri.is_ok() && ci.tick() == cr.tick() {
            ri = ci.step(&mut bi).map_err(|e| e.to_string());
        }
        if rr != ri {
            return Err(format!(
                "results differ from {from:x?}: run {rr:?} step {ri:?}"
            ));
        }
        chunks += 1;
        let same = ci.tick() == cr.tick()
            && ci.pc() == cr.pc()
            && ci.gprs() == cr.gprs()
            && ci.hi() == cr.hi()
            && ci.lo() == cr.lo()
            && ci.cop0() == cr.cop0()
            && bi.cycles() == br.cycles()
            && ci.jit_debug_state() == cr.jit_debug_state();
        if !same {
            let words: Vec<String> = (0..12)
                .map(|k| {
                    format!(
                        "{:08x}",
                        code.get(((from.0.wrapping_sub(CODE_BASE)) / 4 + k) as usize)
                            .copied()
                            .unwrap_or(0)
                    )
                })
                .collect();
            return Err(format!(
                "state differs after chunk of {chunk} from pc {:08x} tick {} (code there: {words:?}):\n step pc={:08x} cycles={} {:?}\n  run pc={:08x} cycles={} {:?}\n step gprs={:08x?}\n  run gprs={:08x?}",
                from.0, from.1, ci.pc(), bi.cycles(), ci.jit_debug_state(),
                cr.pc(), br.cycles(), cr.jit_debug_state(), ci.gprs(), cr.gprs()
            ));
        }
        if rr.is_err() {
            break;
        }
        if full_every != 0
            && chunks.is_multiple_of(full_every)
            && state_bytes(&ci, &bi) != state_bytes(&cr, &br)
        {
            return Err(format!(
                "full state differs at tick {} (CPU state equal)",
                cr.tick()
            ));
        }
    }
    if state_bytes(&ci, &bi) != state_bytes(&cr, &br) {
        return Err(format!("final full state differs at tick {}", cr.tick()));
    }
    Ok(cr.tick())
}
