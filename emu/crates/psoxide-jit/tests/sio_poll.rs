//! A batch may run past reads of the controller port's status register while
//! it polls, but must still stop on the instruction that completes a pad
//! poll (what `Cpu::run`'s `stop_after` looks at). Lockstep against plain
//! `Cpu::step` over a loop that runs pad transactions and polls the port.
#![cfg(target_arch = "aarch64")]

use psoxide_jit::testgen::{machine, state_bytes, CODE_BASE};

const fn r(funct: u32, rs: u32, rt: u32, rd: u32, sa: u32) -> u32 {
    rs << 21 | rt << 16 | rd << 11 | sa << 6 | funct
}
const fn i(op: u32, rs: u32, rt: u32, imm: u32) -> u32 {
    op << 26 | rs << 21 | rt << 16 | (imm & 0xffff)
}
const NOP: u32 = 0;

/// Sets the port up, then endlessly clocks the five bytes of a digital-pad
/// poll through it, polling the status register `extra_polls` times while
/// each byte is on the wire.
fn pad_program(extra_polls: u32) -> Vec<u32> {
    let mut c = vec![
        i(0x0f, 0, 16, 0x1f80),  // lui s0, 0x1f80
        i(0x0d, 16, 16, 0x1040), // ori s0, s0, 0x1040
        i(0x09, 0, 8, 0x000d),
        i(0x29, 16, 8, 0x8), // sh t0, 8(s0): MODE
        i(0x09, 0, 8, 0x0088),
        i(0x29, 16, 8, 0xe), // sh t0, 0xe(s0): BAUD
        i(0x09, 0, 8, 0x1003),
        i(0x29, 16, 8, 0xa), // sh t0, 0xa(s0): CTRL, select pad 1
    ];
    let outer = c.len();
    for byte in [0x01, 0x42, 0x00, 0x00, 0x00] {
        c.push(i(0x09, 0, 9, byte));
        c.push(i(0x28, 16, 9, 0)); // sb t1, 0(s0): DATA
                                   // Poll STAT while the byte is on the wire.
        c.push(i(0x09, 0, 12, extra_polls));
        let spin = c.len();
        c.push(i(0x25, 16, 10, 4)); // lhu t2, 4(s0): STAT
        c.push(i(0x09, 12, 12, 0xffff)); // addiu t4, t4, -1
        let back = spin as i32 - (c.len() as i32 + 1);
        c.push(i(0x05, 12, 0, back as u32)); // bne t4, r0, spin
        c.push(NOP);
        // Wait for the received byte, then take it.
        let wait = c.len();
        c.push(i(0x25, 16, 10, 4)); // lhu t2, 4(s0)
        c.push(NOP); // load delay
        c.push(i(0x0c, 10, 10, 2)); // andi t2, t2, 2
        let back = wait as i32 - (c.len() as i32 + 1);
        c.push(i(0x04, 10, 0, back as u32)); // beq t2, r0, wait
        c.push(NOP);
        c.push(i(0x24, 16, 11, 0)); // lbu t3, 0(s0): DATA
    }
    let back = outer as i32 - (c.len() as i32 + 1);
    c.push(i(0x04, 0, 0, back as u32)); // b outer
    c.push(NOP);
    c.push(r(0x08, 31, 0, 0, 0)); // unreachable guard
    c
}

fn stops_with_the_poll(extra_polls: u32, tier: bool) {
    let code = pad_program(extra_polls);
    let (mut cpu, mut bus) = machine(&code, &[NOP; 4]);
    let (mut rcpu, mut rbus) = machine(&code, &[NOP; 4]);
    bus.attach_digital_pad_port1();
    rbus.attach_digital_pad_port1();
    if tier {
        psoxide_jit::install_tier(&mut cpu).expect("executable memory");
    }
    assert_eq!(cpu.pc(), CODE_BASE);
    for poll in 0..12 {
        let before = bus.port1_completed_polls();
        let (ran, result) = cpu.run(&mut bus, 5_000_000, u64::MAX, |bus| {
            bus.port1_completed_polls() != before
        });
        result.expect("run");
        assert_ne!(
            bus.port1_completed_polls(),
            before,
            "poll {poll} never completed"
        );
        for _ in 0..ran {
            rcpu.step(&mut rbus).expect("step");
        }
        assert_eq!(rcpu.tick(), cpu.tick());
        assert_eq!(
            rbus.port1_completed_polls(),
            bus.port1_completed_polls(),
            "the reference reaches the poll at the same instruction"
        );
        // As anything that looks at the SPU does, catch both up: a batch
        // leaves the samples due after its last device access to the next
        // one.
        rbus.run_spu_to_current_cycle();
        bus.run_spu_to_current_cycle();
        assert_eq!(
            state_bytes(&rcpu, &rbus),
            state_bytes(&cpu, &bus),
            "poll {poll}, extra polls {extra_polls}, tier {tier}"
        );
    }
}

#[test]
fn run_stops_on_the_instruction_that_completes_a_pad_poll() {
    for extra_polls in [1, 2, 3, 17, 60] {
        stops_with_the_poll(extra_polls, false);
        stops_with_the_poll(extra_polls, true);
    }
}
