//! With strict Expansion 2 stores on, a store beyond the 128-byte window is a
//! data bus error in every execution tier: plain `Cpu::step`, the decoded
//! block cache and compiled code reach the same CAUSE and EPC. With it off
//! the same program runs on, printing to the debug console port.
#![cfg(all(target_arch = "aarch64", feature = "strict-exp2"))]

use emulator_core::{Bus, Cpu};
use psoxide_jit::testgen::{machine, CODE_BASE};

const fn i(op: u32, rs: u32, rt: u32, imm: u32) -> u32 {
    op << 26 | rs << 21 | rt << 16 | (imm & 0xffff)
}

const SB: u32 = 0x28;
const SH: u32 = 0x29;
const SW: u32 = 0x2b;
/// `beq $zero, $zero, +1`: a taken branch whose delay slot is the store.
const BRANCH: u32 = 0x1000_0001;
const NOP: u32 = 0;

/// Handler at the exception vector: CAUSE into `$s0`, EPC into `$s1`, then a
/// self-loop.
const HANDLER: [u32; 4] = [0x4010_6800, 0x4011_7000, 0x0800_0022, NOP];

/// Stores `H` to the port at `0xBF80_2000 + offset` with `op`, in a loop
/// (so compiled code gets to run a hot block), in the delay slot of a taken
/// branch when `delay` is set.
fn program(op: u32, offset: u32, delay: bool) -> Vec<u32> {
    let mut code = vec![
        i(0x0f, 0, 8, 0xbf80), // lui   $t0, 0xbf80
        i(0x0d, 8, 8, offset), // ori   $t0, $t0, offset
        i(0x09, 0, 9, 0x48),   // addiu $t1, $zero, 'H'
        i(0x09, 0, 10, 0),     // addiu $t2, $zero, 0
    ];
    let loop_start = code.len() as u32;
    code.push(i(0x09, 10, 10, 1)); // addiu $t2, $t2, 1
    if delay {
        code.push(BRANCH);
    }
    code.push(i(op, 8, 9, 0)); // store $t1, 0($t0)
    let branch_index = code.len() as i32;
    let back = loop_start as i32 - (branch_index + 1);
    code.push(i(0x05, 10, 0, back as u32)); // bne $t2, $zero, loop
    code.push(NOP);
    code
}

fn run_to_the_end(code: &[u32], strict: bool, tier: bool) -> (Cpu, Bus) {
    let (mut cpu, mut bus) = machine(code, &HANDLER);
    bus.set_expansion2_strict(strict);
    let stats = tier.then(|| psoxide_jit::install_tier(&mut cpu).expect("tier available"));
    let (_, result) = cpu.run(&mut bus, 400, u64::MAX, |_| false);
    result.expect("run");
    if let Some(stats) = stats {
        assert!(stats.lock().unwrap().compiled > 0, "the loop ran compiled");
    }
    (cpu, bus)
}

fn step_to_the_end(code: &[u32], strict: bool) -> (Cpu, Bus) {
    let (mut cpu, mut bus) = machine(code, &HANDLER);
    bus.set_expansion2_strict(strict);
    while cpu.tick() < 400 {
        cpu.step(&mut bus).expect("step");
    }
    (cpu, bus)
}

#[test]
fn a_store_beyond_the_window_faults_in_every_tier() {
    for op in [SB, SH, SW] {
        for delay in [false, true] {
            let code = program(op, 0x2080, delay);
            let (reference, _) = step_to_the_end(&code, true);
            // CAUSE: ExcCode 7, BD when the store sat in a delay slot.
            let cause = reference.gprs()[16];
            assert_eq!(cause & 0x7fff_ffff, 7 << 2, "op {op:x} delay {delay}");
            assert_eq!(cause >> 31, delay as u32, "op {op:x} delay {delay}");
            // EPC is the store, or the branch whose delay slot it is: word 5.
            assert_eq!(
                reference.gprs()[17],
                CODE_BASE + 20,
                "op {op:x} delay {delay}"
            );
            for tier in [false, true] {
                let (cpu, _) = run_to_the_end(&code, true, tier);
                assert_eq!(
                    cpu.gprs(),
                    reference.gprs(),
                    "op {op:x} delay {delay} tier {tier}"
                );
                assert_eq!(
                    cpu.cop0(),
                    reference.cop0(),
                    "op {op:x} delay {delay} tier {tier}"
                );
                assert_eq!(
                    cpu.tick(),
                    reference.tick(),
                    "op {op:x} delay {delay} tier {tier}"
                );
            }
        }
    }
}

#[test]
fn a_store_inside_the_window_runs_on_in_every_tier() {
    for op in [SB, SH, SW] {
        for delay in [false, true] {
            let code = program(op, 0x2040, delay);
            let (reference, _) = step_to_the_end(&code, true);
            assert_eq!(
                reference.gprs()[16],
                0,
                "no exception: op {op:x} delay {delay}"
            );
            for tier in [false, true] {
                let (cpu, _) = run_to_the_end(&code, true, tier);
                assert_eq!(
                    cpu.gprs(),
                    reference.gprs(),
                    "op {op:x} delay {delay} tier {tier}"
                );
            }
        }
    }
}

#[test]
fn default_mode_prints_to_the_port_in_every_tier() {
    for op in [SB, SH, SW] {
        let code = program(op, 0x2080, false);
        let (reference, _) = step_to_the_end(&code, false);
        assert_eq!(reference.gprs()[16], 0, "op {op:x}");
        for tier in [false, true] {
            let (cpu, _) = run_to_the_end(&code, false, tier);
            assert_eq!(cpu.gprs(), reference.gprs(), "op {op:x} tier {tier}");
        }
    }
}
