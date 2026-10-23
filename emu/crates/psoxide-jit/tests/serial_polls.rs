//! Status polls of the controller port must not end a native run early when
//! they change nothing a host's `stop_after` can see, and must when they do.
//! A program talks to the pad over SIO0 (the `0x42` poll protocol, status
//! register loops, receive-register reads); one machine runs it through the
//! native tier with a `stop_after` that fires on any change of the port's
//! state (the strictest a host could ask for),
//! the other steps one instruction at a time with the same predicate checked
//! after each. The stops must land on the same instruction and the machines
//! must stay equal.
#![cfg(target_arch = "aarch64")]

use psoxide_jit::install_tier;
use psoxide_jit::testgen::{machine, program, state_bytes, Rng};

const fn r(funct: u32, rs: u32, rt: u32, rd: u32, sa: u32) -> u32 {
    rs << 21 | rt << 16 | rd << 11 | sa << 6 | funct
}
const fn i(op: u32, rs: u32, rt: u32, imm: u32) -> u32 {
    op << 26 | rs << 21 | rt << 16 | (imm & 0xffff)
}
const NOP: u32 = 0;
const SIO: u32 = 16;

/// Send one byte and wait for the reply in the receive slot, polling the
/// status register with the given load (`lw`, `lhu` or `lbu`), then pop it.
fn exchange(code: &mut Vec<u32>, byte: u32, load: u32, spin_first: bool) {
    code.push(i(0x0d, 0, 8, byte)); // ori t0, zero, byte
    code.push(i(0x28, SIO, 8, 0)); // sb t0, 0(s0)
    if spin_first {
        // A few reads of the status register before the reply is there.
        for _ in 0..3 {
            code.push(i(load, SIO, 9, 4));
            code.push(NOP);
        }
    }
    let wait = code.len() as i32;
    code.push(i(load, SIO, 9, 4)); // status
    code.push(NOP);
    code.push(i(0x0c, 9, 9, 2)); // andi t1, t1, 2: receive slot not empty
    let back = wait - (code.len() as i32 + 1);
    code.push(i(0x04, 9, 0, back as u32 & 0xffff)); // beq t1, zero, wait
    code.push(NOP);
    code.push(i(0x24, SIO, 10, 0)); // lbu t2, 0(s0): pop the reply
    code.push(r(0x21, 10, 11, 11, 0)); // addu t3, t2, t3
}

fn pad_program(load: u32) -> Vec<u32> {
    let mut code = vec![
        i(0x0f, 0, SIO, 0x1f80),
        i(0x0d, SIO, SIO, 0x1040),
        i(0x0d, 0, 8, 0x000d),
        i(0x29, SIO, 8, 8), // MODE
        i(0x0d, 0, 8, 0x0088),
        i(0x29, SIO, 8, 0xe), // BAUD
        i(0x0d, 0, 8, 0x1003),
        i(0x29, SIO, 8, 0xa), // CTRL: transmit enabled, port 1 selected
    ];
    let top = code.len() as i32;
    for (k, byte) in [0x01, 0x42, 0x00, 0x00, 0x00].into_iter().enumerate() {
        exchange(&mut code, byte, load, k % 2 == 0);
    }
    // Idle a little with the registers read in other widths between polls.
    code.extend([
        i(0x25, SIO, 12, 4),
        i(0x23, SIO, 12, 8),
        i(0x25, SIO, 12, 0xa),
        i(0x23, SIO, 12, 0xe),
    ]);
    let back = top - (code.len() as i32 + 1);
    code.push(i(0x04, 0, 0, back as u32 & 0xffff)); // b top
    code.push(NOP);
    code
}

/// Everything of the controller port a host could look at: the completed
/// polls and the port's registers and deadlines.
fn port_signature(bus: &emulator_core::Bus) -> impl PartialEq + std::fmt::Debug {
    let s = bus.sio0();
    (
        bus.port1_completed_polls(),
        s.debug_stat(),
        s.debug_ctrl(),
        s.debug_pending_irq(),
        s.debug_irq_latched(),
        s.debug_transfer_busy(),
        s.debug_awaiting_ack(),
        s.debug_transfer_deadline(),
        s.debug_ack_deadline(),
        s.debug_ack_end_deadline(),
        s.debug_rx(),
        s.debug_queued_tx(),
    )
}

fn check(load: u32, seed: u64, instructions: u64) {
    let code = pad_program(load);
    let (_, handler) = program(&mut Rng::new(1), 1, 0);
    let (mut ci, mut bi) = machine(&code, &handler);
    let (mut cr, mut br) = machine(&code, &handler);
    install_tier(&mut cr).expect("executable memory");
    let mut rng = Rng::new(seed);
    let mut stops = 0u32;
    while cr.tick() < instructions {
        let before = port_signature(&br);
        let chunk = 1 + u64::from(rng.below(40_000));
        let (_, rr) = cr.run(&mut br, chunk, u64::MAX, |bus| {
            port_signature(bus) != before
        });
        rr.expect("run");
        // The reference: single steps, the predicate after each, up to the
        // same point (it must fire exactly there, or not at all if the run
        // simply used up its steps).
        let before_i = port_signature(&bi);
        let mut fired = false;
        while ci.tick() < cr.tick() {
            ci.step(&mut bi).expect("step");
            if port_signature(&bi) != before_i {
                fired = true;
                assert_eq!(
                    ci.tick(),
                    cr.tick(),
                    "load {load:#x} seed {seed}: the poll completed at tick {} but the run went on to {}",
                    ci.tick(),
                    cr.tick()
                );
                break;
            }
        }
        if port_signature(&br) != before {
            assert!(
                fired,
                "load {load:#x} seed {seed}: the run stopped on a poll the steps never saw"
            );
            stops += 1;
        }
        assert_eq!(ci.tick(), cr.tick());
        assert_eq!(
            bi.cycles(),
            br.cycles(),
            "load {load:#x} seed {seed} tick {}",
            cr.tick()
        );
        assert_eq!(ci.gprs(), cr.gprs());
        // As anything that looks at the SPU does, catch both up: a batch
        // leaves the samples due after its last device access to the next
        // one.
        bi.run_spu_to_current_cycle();
        br.run_spu_to_current_cycle();
        assert_eq!(
            state_bytes(&ci, &bi),
            state_bytes(&cr, &br),
            "load {load:#x} seed {seed} tick {}",
            cr.tick()
        );
    }
    assert!(
        stops > 3,
        "the program completed too few pad polls ({stops})"
    );
}

#[test]
fn status_polls_of_the_pad_port_stop_where_the_steps_stop() {
    for load in [0x23, 0x25, 0x24] {
        for seed in 1..=2 {
            check(load, seed, 60_000);
        }
    }
}
