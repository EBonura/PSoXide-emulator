// SPDX-License-Identifier: GPL-2.0-or-later
//! Test support for the HLE kernel: build a guest program that calls BIOS
//! functions in sequence, run it on a bus with the HLE BIOS enabled and read
//! back each call's result.

#![allow(missing_docs)]

use crate::hle_asm::*;
use crate::{Bus, Cpu};

/// Where the guest program is put.
pub const PROGRAM: u32 = 0x8001_0000;
/// Result `i` (the v0 of call `i`) is stored at `RESULTS + 4 * i`.
pub const RESULTS: u32 = 0x8002_0F00;

/// An argument standing for "the v0 of call `k`".
pub const fn res(k: u32) -> u32 {
    0xD000_0000 | k
}

/// One step of a guest program.
pub enum Step {
    /// `vector` (A0h, B0h or C0h) function `func` with four arguments.
    Call(i16, u32, [u32; 4]),
    /// A few instructions of the test's own.
    Code(fn(&mut Asm)),
}

/// A(func).
pub fn a(func: u32, args: [u32; 4]) -> Step {
    Step::Call(0xA0, func, args)
}

/// B(func).
pub fn b(func: u32, args: [u32; 4]) -> Step {
    Step::Call(0xB0, func, args)
}

/// The program: interrupts on (IEc, IM2), then the steps. Ends in a loop on
/// its last two words.
pub fn program(steps: &[Step]) -> Vec<u32> {
    let mut a = Asm::new(PROGRAM);
    a.li(T0, 0x0000_0401);
    a.mtc0(T0, 12);
    a.li(S0, RESULTS);
    a.li(S1, RESULTS);
    for step in steps {
        match step {
            Step::Code(code) => code(&mut a),
            Step::Call(vector, func, args) => {
                for (i, v) in args.iter().enumerate() {
                    if *v >> 24 == 0xD0 {
                        a.lw(A0 + i as u32, (4 * (*v & 0xFFFF)) as i16, S1);
                    } else {
                        a.li(A0 + i as u32, *v);
                    }
                }
                a.addiu(T2, ZERO, *vector);
                a.jalr(T2);
                a.addiu(T1, ZERO, *func as i16);
                a.sw(V0, 0, S0);
                a.addiu(S0, S0, 4);
            }
        }
    }
    a.label("end");
    a.b("end");
    a.nop();
    a.finish()
}

/// A bus with the HLE BIOS, a pad and a fresh card in slot 1 and none in
/// slot 2.
pub fn bus_with_card() -> Bus {
    let mut bus = Bus::new_without_bios();
    bus.enable_hle_bios();
    bus.attach_digital_pad_port1();
    bus.attach_memcard_port1(Vec::new());
    bus.detach_memcard_port2();
    bus
}

/// Run until the program reaches its final loop, then `extra` more VBlanks.
pub fn run(bus: &mut Bus, words: &[u32], extra: u64) -> Cpu {
    let mut cpu = Cpu::new();
    for (i, w) in words.iter().enumerate() {
        bus.write32(PROGRAM + 4 * i as u32, *w);
    }
    cpu.gprs_mut_for_test()[29] = 0x801F_FF00;
    cpu.set_pc_for_test(PROGRAM);
    let end = PROGRAM + 4 * (words.len() as u32 - 2);
    let mut target = None;
    for n in 0..1_000_000_000u32 {
        let vblanks = bus.irq().raise_counts()[0];
        if cpu.pc() == end && target.is_none() {
            target = Some(vblanks + extra);
        }
        if target.is_some_and(|t| vblanks >= t) {
            return cpu;
        }
        cpu.step(bus).unwrap();
        if n % 65_536 == 0 {
            bus.run_spu_to_current_cycle();
            let _ = bus.spu.drain_audio();
        }
    }
    panic!("program did not finish, pc={:#x}", cpu.pc());
}

/// The v0 of call `i`.
pub fn result(bus: &mut Bus, i: u32) -> u32 {
    bus.read32(RESULTS + 4 * i)
}

/// Write the zero-terminated string at `at`.
pub fn put_str(bus: &mut Bus, at: u32, s: &str) {
    for (i, byte) in s.bytes().chain([0]).enumerate() {
        bus.write8_safe(at + i as u32, byte);
    }
}

/// The bytes `at..at + len`.
pub fn get_bytes(bus: &Bus, at: u32, len: u32) -> Vec<u8> {
    (0..len)
        .map(|k| bus.try_read8(at + k).unwrap_or(0))
        .collect()
}
