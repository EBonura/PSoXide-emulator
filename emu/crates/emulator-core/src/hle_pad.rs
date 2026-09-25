// SPDX-License-Identifier: GPL-2.0-or-later
//! Controller driver of the HLE kernel: InitPAD/StartPAD/StopPAD, the
//! PAD_init2/PAD_dr pair, and the VBlank pad/card handler that reads both
//! controller ports through SIO0.
//!
//! Behaviour follows psx-spx "BIOS Joypad Functions", "Priority Chains"
//! and the kernel-patch notes, with the OpenBIOS `sio0/driver.c` and
//! `sio0/pad.c` (pcsx-redux, MIT) as the specification for the byte
//! sequence, delays and timeouts of the retail reader.
//!
//! The reader drives the emulated SIO0 registers and polls `I_STAT` bit 7
//! for each controller ACK, so transfer and ACK timing are the port
//! model's own. It runs inside the exception handler as a kernel trap that
//! is retried while it waits (interrupts are off there, as on hardware),
//! with its progress in kernel RAM ([`kvar`]), so a save state taken
//! mid-read resumes it.

use crate::hle_kernel::{peek32, poke32, stub_addr};
use crate::Bus;

/// Kernel variables of the pad driver (`0x0B80..0x0BAF`).
pub mod kvar {
    /// Pad 1 / pad 2 receive buffers (InitPAD buf1, buf2).
    pub const BUF: u32 = 0x0B80;
    /// Pad 1 / pad 2 buffer sizes (only used for the initial zero fill).
    pub const SIZE: u32 = 0x0B88;
    /// PAD_init2 `button_dest`: PAD_dr runs after each read when nonzero.
    pub const USER_BUTTONS: u32 = 0x0B90;
    /// Reader phase (`phase::*`).
    pub const PHASE: u32 = 0x0B94;
    /// Port being read (0 or 1).
    pub const PORT: u32 = 0x0B98;
    /// Byte of the transfer: 0 = 01h, 1 = 42h, 2 = idlo, 3.. = data.
    pub const STEP: u32 = 0x0B9C;
    /// Data bytes left.
    pub const LEFT: u32 = 0x0BA0;
    /// Low 32 bits of the bus cycle that ends the current delay/timeout.
    pub const DEADLINE: u32 = 0x0BA4;
}

/// PAD_init2's internal buffers (psx-spx: "hidden within the BIOS
/// variables region"), 22h bytes each.
pub const INTERNAL_BUF1: u32 = 0x0BB0;
/// Second internal buffer.
pub const INTERNAL_BUF2: u32 = 0x0BD4;
/// Bytes per pad buffer the reader may fill (a multitap uses all 22h).
pub const BUF_LEN: u32 = 0x22;

/// Chain element of the pad/card VBlank handler (priority 2).
pub const HI_PAD: u32 = 0x0000_3160;

/// Kernel-internal trap functions of the pad driver.
pub mod internal {
    /// VBlank verifier of the pad/card handler.
    pub const VERIFIER: u8 = 0x30;
    /// Pad/card handler: reads both pads, acknowledges VBlank.
    pub const HANDLER: u8 = 0x31;
}

const I_STAT: u32 = 0x1F80_1070;
const I_MASK: u32 = 0x1F80_1074;
const SIO_DATA: u32 = 0x1F80_1040;
const SIO_STAT: u32 = 0x1F80_1044;
const SIO_MODE: u32 = 0x1F80_1048;
const SIO_CTRL: u32 = 0x1F80_104A;
const SIO_BAUD: u32 = 0x1F80_104E;

const IRQ_VBLANK: u32 = 1 << 0;
const IRQ_CONTROLLER: u32 = 1 << 7;

const STAT_TX_READY: u32 = 1 << 0;
const STAT_RX_READY: u32 = 1 << 1;

const CTRL_TXEN: u16 = 1 << 0;
const CTRL_DTR: u16 = 1 << 1;
const CTRL_ACK: u16 = 1 << 4;
const CTRL_RESET: u16 = 1 << 6;
const CTRL_ACK_IRQ: u16 = 1 << 12;
const CTRL_PORT2: u16 = 1 << 13;

/// Retail SIO0 setup: 2073600 / 15200 = 88h (1088 cycles per byte), mode
/// 0Dh (MUL1, 8 bits, no parity).
const BAUD: u16 = 0x88;
const MODE: u16 = 0x0D;

/// Cycles per `busyloop` count and the ACK poll timeout. The retail
/// reader waits with a counted delay loop and gives up on an ACK after 51h
/// polls of `I_STAT`; these are estimates of those loops' length on the
/// R3000A (about 10 cycles per delay count, 16 per poll), to be replaced
/// by the measured costs of plan phase P7. The poll timeout has to outlast
/// the 1088-cycle transfer, or no controller would ever answer.
const DELAY_UNIT: u32 = 10;
const ACK_TIMEOUT: u32 = 0x51 * 16;

/// Kernel patch bits (see [`crate::hle_kernel::PATCHES`]).
const PATCH_PAD_ANY: u32 = (1 << 3) | (1 << 4) | (1 << 5);
const PATCH_REMOVE_CHGCLRPAD: u32 = (1 << 6) | (1 << 7);
const PATCH_SEND_PAD: u32 = (1 << 8) | (1 << 9);

/// Reader phases.
mod phase {
    /// Start the port in [`super::kvar::PORT`].
    pub const START: u32 = 0;
    /// Port selected; wait, then enable transmit and send 01h.
    pub const SELECTED: u32 = 1;
    /// Waiting for TX ready to send the byte for STEP.
    pub const TX: u32 = 2;
    /// Byte sent; delay before acknowledging.
    pub const TX_DELAY: u32 = 3;
    /// Waiting for the received byte.
    pub const RX: u32 = 4;
    /// Delay after the first received byte.
    pub const POST_DELAY: u32 = 5;
    /// Waiting for the controller's ACK (IRQ7) before the next byte.
    pub const WAIT_ACK: u32 = 6;
    /// Abort: other slot selected briefly (unless patched out).
    pub const ABORT_DELAY: u32 = 7;
}

fn now(bus: &Bus) -> u32 {
    bus.cycles() as u32
}

fn set_deadline(bus: &mut Bus, cycles: u32) {
    let t = now(bus).wrapping_add(cycles);
    poke32(bus, kvar::DEADLINE, t);
}

fn deadline_passed(bus: &Bus) -> bool {
    now(bus).wrapping_sub(peek32(bus, kvar::DEADLINE)) as i32 >= 0
}

fn patches(bus: &Bus) -> u32 {
    peek32(bus, crate::hle_kernel::kvar::PATCH_FLAGS)
}

fn ctrl(bus: &mut Bus) -> u16 {
    bus.read16(SIO_CTRL)
}

// ------------------------------------------------------------------ setup

/// Write the handler's chain element (not enqueued).
pub fn install(bus: &mut Bus) {
    poke32(bus, HI_PAD, 0);
    poke32(bus, HI_PAD + 4, stub_addr(3, internal::HANDLER));
    poke32(bus, HI_PAD + 8, stub_addr(3, internal::VERIFIER));
    poke32(bus, HI_PAD + 12, 0);
}

/// B(12h) InitPAD2(buf1, siz1, buf2, siz2): remember and zero-fill the
/// buffers, clear PAD_init2's destination and the output buffers, and set
/// the pad enable flag. Returns 1.
pub fn init_pad(bus: &mut Bus, buf1: u32, siz1: u32, buf2: u32, siz2: u32) -> u32 {
    poke32(bus, kvar::USER_BUTTONS, 0);
    for i in 0..4 {
        poke32(bus, crate::hle_kernel::kvar::PAD_OUTPUT + 4 * i, 0);
    }
    for (i, (buf, size)) in [(buf1, siz1), (buf2, siz2)].into_iter().enumerate() {
        poke32(bus, kvar::BUF + 4 * i as u32, buf);
        poke32(bus, kvar::SIZE + 4 * i as u32, size);
        for k in 0..size.min(0x1_0000) {
            bus.write8_safe(buf.wrapping_add(k), 0);
        }
    }
    poke32(bus, crate::hle_kernel::kvar::PAD_STARTED, 1);
    1
}

/// Reset and configure SIO0 as the retail kernel does before enqueueing
/// the handler (reset, baud, mode, a select pulse on each port).
fn setup_sio0(bus: &mut Bus) {
    bus.write16(SIO_CTRL, CTRL_RESET);
    bus.write16(SIO_BAUD, BAUD);
    bus.write16(SIO_MODE, MODE);
    bus.write16(SIO_CTRL, 0);
    bus.write16(SIO_CTRL, CTRL_DTR);
    bus.write16(SIO_CTRL, CTRL_PORT2 | CTRL_DTR);
    bus.write16(SIO_CTRL, 0);
    poke32(bus, kvar::PHASE, phase::START);
    poke32(bus, kvar::PORT, 0);
}

/// Enqueue the pad/card handler at priority 2 with VBlank unmasked, pad
/// auto-ack on and the VBlank root-counter auto-ack off (so the priority-1
/// VBlank handler leaves the IRQ for this one). Shared by StartPAD2 and
/// StartCARD2.
pub fn enqueue_handler(bus: &mut Bus) {
    setup_sio0(bus);
    crate::hle_exceptions::deq_int(bus, 2, HI_PAD);
    crate::hle_exceptions::enq_int(bus, 2, HI_PAD);
    poke32(bus, crate::hle_kernel::kvar::SIO0_AUTO_ACK, 1);
    crate::hle_exceptions::change_clear_rcnt(bus, 3, 0);
}

/// B(13h) StartPAD2.
pub fn start_pad(bus: &mut Bus) -> u32 {
    enqueue_handler(bus);
    bus.write32(I_STAT, !IRQ_VBLANK);
    let mask = bus.read32(I_MASK);
    bus.write32(I_MASK, mask | IRQ_VBLANK);
    1
}

/// B(14h) StopPAD2: VBlank root-counter auto-ack back on, handler
/// dequeued (which stops memory cards too).
pub fn stop_pad(bus: &mut Bus) -> u32 {
    crate::hle_exceptions::change_clear_rcnt(bus, 3, 1);
    crate::hle_exceptions::deq_int(bus, 2, HI_PAD);
    1
}

/// B(15h) PAD_init2(type, button_dest, a2, a3): only types 20000000h and
/// 20000001h are accepted (returns 0 otherwise). FF-fills the internal
/// buffers, runs InitPAD2 on them (which zero-fills them again) and
/// StartPAD2, remembers `button_dest`, and writes the parameters back to
/// the caller's argument area. Returns 2.
pub fn pad_init2(bus: &mut Bus, ty: u32, button_dest: u32, a2: u32, a3: u32, sp: u32) -> u32 {
    poke32(bus, sp.wrapping_add(4), button_dest);
    poke32(bus, sp.wrapping_add(8), a2);
    poke32(bus, sp.wrapping_add(12), a3);
    if ty != 0x2000_0000 && ty != 0x2000_0001 {
        return 0;
    }
    for k in 0..BUF_LEN {
        bus.write8_safe(INTERNAL_BUF1 + k, 0xFF);
        bus.write8_safe(INTERNAL_BUF2 + k, 0xFF);
    }
    init_pad(bus, INTERNAL_BUF1, BUF_LEN, INTERNAL_BUF2, BUF_LEN);
    poke32(bus, kvar::USER_BUTTONS, button_dest);
    start_pad(bus);
    2
}

fn buttons_for(bus: &Bus, buf: u32) -> u16 {
    let b = |k: u32| bus.try_read8(buf.wrapping_add(k)).unwrap_or(0);
    if b(0) != 0 {
        return 0xFFFF;
    }
    let mut value = u16::from(b(2)) << 8 | u16::from(b(3));
    match b(1) {
        0x41 => value,
        0x23 => {
            value |= 0x07C7;
            if b(5) > 0x10 {
                value &= !0x40;
            }
            if b(6) > 0x10 {
                value &= !0x80;
            }
            value
        }
        _ => 0xFFFF,
    }
}

/// B(16h) PAD_dr: pad 1 buttons in the low halfword, pad 2 in the high
/// one, each with its first button byte in the upper 8 bits; FFFFh for
/// any device other than ID 41h/23h. The value is also stored at
/// PAD_init2's `button_dest` (address 0 when that was 0, as psx-spx
/// documents).
pub fn pad_dr(bus: &mut Bus) -> u32 {
    let value = u32::from(buttons_for(bus, INTERNAL_BUF1))
        | u32::from(buttons_for(bus, INTERNAL_BUF2)) << 16;
    let dest = peek32(bus, kvar::USER_BUTTONS);
    poke32(bus, dest, value);
    value
}

// ---------------------------------------------------------------- handler

/// Verifier: VBlank enabled and pending.
pub fn verifier(bus: &mut Bus) -> u32 {
    let pending = bus.read32(I_STAT) & bus.read32(I_MASK) & IRQ_VBLANK;
    u32::from(pending != 0)
}

/// Handler: read both pads when the pad driver is enabled, run PAD_dr for
/// PAD_init2 users, acknowledge VBlank when pad auto-ack is on (and
/// `_remove_ChgclrPAD` has not removed that), then let the card driver
/// schedule its next command. `None` while a transfer is waiting on the
/// port.
pub fn handler(bus: &mut Bus) -> Option<u32> {
    if peek32(bus, crate::hle_kernel::kvar::PAD_STARTED) != 0 {
        read_pads(bus)?;
        if peek32(bus, kvar::USER_BUTTONS) != 0 {
            pad_dr(bus);
        }
    }
    let auto_ack = peek32(bus, crate::hle_kernel::kvar::SIO0_AUTO_ACK) != 0;
    if auto_ack && patches(bus) & PATCH_REMOVE_CHGCLRPAD == 0 {
        bus.write32(I_STAT, !IRQ_VBLANK);
    }
    if peek32(bus, crate::hle_card::kvar::STARTED) != 0 {
        crate::hle_card::vblank(bus);
    }
    Some(0)
}

/// Advance the two-port read. `None` while waiting; `Some(())` once both
/// ports are done (the phase is reset for the next VBlank).
fn read_pads(bus: &mut Bus) -> Option<()> {
    // Bounded: every arm either waits (returns) or moves forward.
    for _ in 0..0x100 {
        let port = peek32(bus, kvar::PORT);
        if port >= 2 {
            poke32(bus, kvar::PORT, 0);
            poke32(bus, kvar::PHASE, phase::START);
            return Some(());
        }
        let buf = peek32(bus, kvar::BUF + 4 * port);
        let slot = if port == 0 { 0 } else { CTRL_PORT2 };
        match peek32(bus, kvar::PHASE) {
            phase::START => {
                if buf == 0 {
                    next_port(bus);
                    continue;
                }
                bus.write8_safe(buf, 0xFF);
                bus.write16(SIO_CTRL, slot | CTRL_DTR);
                let _ = bus.read8(SIO_DATA);
                set_deadline(bus, 40 * DELAY_UNIT);
                poke32(bus, kvar::PHASE, phase::SELECTED);
            }
            phase::SELECTED => {
                if !deadline_passed(bus) {
                    return None;
                }
                bus.write16(SIO_CTRL, slot | CTRL_TXEN | CTRL_DTR | CTRL_ACK_IRQ);
                poke32(bus, kvar::STEP, 0);
                poke32(bus, kvar::PHASE, phase::TX);
            }
            phase::TX => {
                if bus.read32(SIO_STAT) & STAT_TX_READY == 0 {
                    return None;
                }
                let step = peek32(bus, kvar::STEP);
                bus.write8(SIO_DATA, tx_byte(bus, port, step));
                let delay = match step {
                    0 | 2 => 20,
                    1 => 25,
                    _ => 10,
                };
                set_deadline(bus, delay * DELAY_UNIT);
                poke32(bus, kvar::PHASE, phase::TX_DELAY);
            }
            phase::TX_DELAY => {
                if !deadline_passed(bus) {
                    return None;
                }
                let c = ctrl(bus);
                bus.write16(SIO_CTRL, c | CTRL_ACK);
                bus.write32(I_STAT, !IRQ_CONTROLLER);
                poke32(bus, kvar::PHASE, phase::RX);
            }
            phase::RX => {
                if bus.read32(SIO_STAT) & STAT_RX_READY == 0 {
                    // A data byte whose ACK arrives before the byte
                    // itself is a transmission error.
                    if peek32(bus, kvar::STEP) >= 3 && bus.read32(I_STAT) & IRQ_CONTROLLER != 0 {
                        abort(bus, port, buf);
                        continue;
                    }
                    return None;
                }
                let byte = bus.read8(SIO_DATA);
                let step = peek32(bus, kvar::STEP);
                match step {
                    0 => {
                        set_deadline(bus, 40 * DELAY_UNIT);
                        poke32(bus, kvar::PHASE, phase::POST_DELAY);
                        continue;
                    }
                    1 => {
                        bus.write8_safe(buf.wrapping_add(1), byte);
                        let halfwords = match byte & 0x0F {
                            0 => 0x10,
                            n => u32::from(n),
                        };
                        poke32(bus, kvar::LEFT, 2 * halfwords);
                    }
                    2 => {
                        if byte != 0x5A {
                            abort(bus, port, buf);
                            continue;
                        }
                    }
                    _ => {
                        bus.write8_safe(buf.wrapping_add(2 + (step - 3)), byte);
                        let left = peek32(bus, kvar::LEFT) - 1;
                        poke32(bus, kvar::LEFT, left);
                        if left == 0 {
                            bus.write8_safe(buf, 0);
                            bus.write16(SIO_CTRL, 0);
                            next_port(bus);
                            continue;
                        }
                    }
                }
                wait_ack(bus, step + 1);
            }
            phase::POST_DELAY => {
                if !deadline_passed(bus) {
                    return None;
                }
                wait_ack(bus, 1);
            }
            phase::WAIT_ACK => {
                if bus.read32(I_STAT) & IRQ_CONTROLLER != 0 {
                    poke32(bus, kvar::PHASE, phase::TX);
                } else if deadline_passed(bus) {
                    abort(bus, port, buf);
                } else {
                    return None;
                }
            }
            phase::ABORT_DELAY => {
                if !deadline_passed(bus) {
                    return None;
                }
                bus.write16(SIO_CTRL, 0);
                next_port(bus);
            }
            _ => {
                poke32(bus, kvar::PHASE, phase::START);
                poke32(bus, kvar::PORT, 0);
                return Some(());
            }
        }
    }
    None
}

fn wait_ack(bus: &mut Bus, next_step: u32) {
    poke32(bus, kvar::STEP, next_step);
    set_deadline(bus, ACK_TIMEOUT);
    poke32(bus, kvar::PHASE, phase::WAIT_ACK);
}

fn next_port(bus: &mut Bus) {
    let port = peek32(bus, kvar::PORT);
    poke32(bus, kvar::PORT, port + 1);
    poke32(bus, kvar::PHASE, phase::START);
}

/// No ACK, or a bad reply: status FFh. The retail reader then selects the
/// other slot for a moment; `_patch_pad` removes that (psx-spx
/// "patch_pad_error_handling").
fn abort(bus: &mut Bus, port: u32, buf: u32) {
    bus.write8_safe(buf, 0xFF);
    if patches(bus) & PATCH_PAD_ANY == 0 {
        let other = if port == 0 { CTRL_PORT2 } else { 0 };
        bus.write16(SIO_CTRL, other | CTRL_DTR);
        set_deadline(bus, 10 * DELAY_UNIT);
        poke32(bus, kvar::PHASE, phase::ABORT_DELAY);
    } else {
        bus.write16(SIO_CTRL, 0);
        next_port(bus);
    }
}

/// Byte sent for `step`: 01h, 42h, then 00h, then the controller output
/// data. Output comes from setPadOutputData's buffer (its first byte
/// enables it; data byte n is buffer[1 + n]); without `_send_pad` each
/// byte is clipped to 0/1, as the retail kernel does (psx-spx
/// "patch_optional_pad_output").
fn tx_byte(bus: &Bus, port: u32, step: u32) -> u8 {
    match step {
        0 => 0x01,
        1 => 0x42,
        2 => 0x00,
        n => {
            let out = peek32(bus, crate::hle_kernel::kvar::PAD_OUTPUT + 8 * port);
            if out == 0 || bus.try_read8(out).unwrap_or(0) == 0 {
                return 0;
            }
            let byte = bus.try_read8(out.wrapping_add(1 + (n - 3))).unwrap_or(0);
            if patches(bus) & PATCH_SEND_PAD != 0 {
                byte
            } else {
                u8::from(byte != 0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hle_asm::*;
    use crate::pad::{button, ButtonState};
    use crate::Cpu;

    const PROGRAM: u32 = 0x8001_0000;

    /// Guest program: B-table calls with immediate arguments, then
    /// interrupts on (IEc and IM2) and an endless loop.
    fn program(calls: &[(u32, [u32; 4])]) -> Vec<u32> {
        let mut a = Asm::new(PROGRAM);
        for (func, args) in calls {
            for (i, v) in args.iter().enumerate() {
                a.li(A0 + i as u32, *v);
            }
            a.addiu(T2, ZERO, 0xB0);
            a.jalr(T2);
            a.addiu(T1, ZERO, *func as i16);
        }
        a.li(T0, 0x0000_0401);
        a.mtc0(T0, 12);
        a.label("end");
        a.b("end");
        a.nop();
        a.finish()
    }

    fn hle_bus_with_pad(buttons: u16) -> Bus {
        let mut bus = Bus::new_without_bios();
        bus.enable_hle_bios();
        bus.attach_digital_pad_port1();
        bus.set_port1_buttons(ButtonState::from_bits(buttons));
        bus
    }

    /// Run `words` until `vblanks` VBlank IRQs have been raised.
    fn run_for_vblanks(bus: &mut Bus, words: &[u32], vblanks: u64) -> Cpu {
        let mut cpu = Cpu::new();
        for (i, w) in words.iter().enumerate() {
            bus.write32(PROGRAM + 4 * i as u32, *w);
        }
        cpu.gprs_mut_for_test()[29] = 0x801F_FF00;
        cpu.set_pc_for_test(PROGRAM);
        let target = bus.irq().raise_counts()[0] + vblanks;
        for _ in 0..200_000_000u32 {
            if bus.irq().raise_counts()[0] >= target {
                return cpu;
            }
            cpu.step(bus).unwrap();
        }
        panic!("VBlanks did not arrive, pc={:#x}", cpu.pc());
    }

    fn bytes(bus: &Bus, addr: u32, n: u32) -> Vec<u8> {
        (0..n).map(|k| bus.try_read8(addr + k).unwrap()).collect()
    }

    /// StartPAD2, StopPAD2, StartCARD2, StopCARD2 and PAD_init2 end by
    /// leaving the critical section (OpenBIOS `sio0/driver.c`, `pad.c`). Nightmare Creatures
    /// calls StartPAD2 with interrupts off and then waits in a TestEvent
    /// loop for a memory card event, which only interrupts can deliver.
    #[test]
    fn pad_and_card_start_stop_leave_the_critical_section() {
        for (func, want) in [(0x13i16, 1), (0x14, 1), (0x4B, 1), (0x4C, 1), (0x15, 2)] {
            let mut bus = hle_bus_with_pad(0);
            let mut a = Asm::new(PROGRAM);
            // PAD_init2(20000000h, 0, 0, 0); the others ignore arguments.
            a.li(A0, 0x2000_0000);
            a.li(A1, 0);
            a.addiu(T2, ZERO, 0xB0);
            a.jalr(T2);
            a.addiu(T1, ZERO, func);
            a.label("end");
            a.b("end");
            a.nop();
            let words = a.finish();
            for (i, w) in words.iter().enumerate() {
                bus.write32(PROGRAM + 4 * i as u32, *w);
            }
            let end = PROGRAM + 4 * (words.len() as u32 - 2);
            let mut cpu = Cpu::new();
            cpu.gprs_mut_for_test()[29] = 0x801F_FF00;
            cpu.set_pc_for_test(PROGRAM);
            assert_eq!(cpu.cop0()[12] & 0x401, 0, "starts with interrupts off");
            for _ in 0..10_000 {
                if cpu.pc() == end {
                    break;
                }
                cpu.step(&mut bus).unwrap();
            }
            assert_eq!(cpu.pc(), end, "B({func:02X}h) returned");
            assert_eq!(cpu.gpr(2), want, "B({func:02X}h) return value");
            assert_eq!(
                cpu.cop0()[12] & 0x401,
                0x401,
                "B({func:02X}h) left the critical section"
            );
        }
    }

    #[test]
    fn init_and_start_pad_read_both_ports_on_vblank() {
        let (buf1, buf2) = (0x8002_0000, 0x8002_0040);
        let mut bus = hle_bus_with_pad(button::START | button::CROSS);
        for k in 0..0x22 {
            bus.write8_safe(buf1 + k, 0xAA);
            bus.write8_safe(buf2 + k, 0xAA);
        }
        let words = program(&[(0x12, [buf1, 0x22, buf2, 0x22]), (0x13, [0; 4])]);
        run_for_vblanks(&mut bus, &words, 3);
        // Digital pad: status 0, ID 41h, buttons active low (START is
        // bit 3 of the first byte, CROSS bit 6 of the second).
        assert_eq!(bytes(&bus, buf1, 4), vec![0x00, 0x41, 0xF7, 0xBF]);
        // Nothing on port 2: status FFh, rest as InitPAD left it.
        assert_eq!(bytes(&bus, buf2, 4), vec![0xFF, 0x00, 0x00, 0x00]);
        assert_eq!(bytes(&bus, buf1 + 4, 2), vec![0x00, 0x00]);
        // The handler acknowledged VBlank, and the VBlank root counter no
        // longer does.
        assert_eq!(
            peek32(&bus, crate::hle_exceptions::kvar::RCNT_AUTOACK + 12),
            0
        );
        assert_eq!(bus.read32(I_MASK) & IRQ_VBLANK, IRQ_VBLANK);
    }

    #[test]
    fn stop_pad_leaves_the_buffers_alone() {
        let (buf1, buf2) = (0x8002_0000, 0x8002_0040);
        let mut bus = hle_bus_with_pad(button::START);
        let words = program(&[
            (0x12, [buf1, 0x22, buf2, 0x22]),
            (0x13, [0; 4]),
            (0x14, [0; 4]),
        ]);
        run_for_vblanks(&mut bus, &words, 3);
        assert_eq!(bytes(&bus, buf1, 4), vec![0; 4]);
        assert_eq!(
            peek32(&bus, crate::hle_exceptions::kvar::RCNT_AUTOACK + 12),
            1
        );
    }

    #[test]
    fn pad_init2_stores_pad_dr_at_button_dest() {
        let dest = 0x8002_0100;
        let mut bus = hle_bus_with_pad(button::CROSS);
        bus.write32(dest, 0x1234_5678);
        // A bad type is refused before anything is set up.
        let words = program(&[(0x15, [0x1000_0001, dest, 0, 0])]);
        run_for_vblanks(&mut bus, &words, 2);
        assert_eq!(bus.read32(dest), 0x1234_5678);

        let mut bus = hle_bus_with_pad(button::CROSS);
        let words = program(&[(0x15, [0x2000_0001, dest, 0, 0])]);
        run_for_vblanks(&mut bus, &words, 3);
        // Pad 1: first button byte FFh in the upper half, second BFh
        // (CROSS) in the lower; pad 2 absent: FFFFh.
        assert_eq!(bus.read32(dest), 0xFFFF_FFBF);
    }

    #[test]
    fn pad_dr_formats_flying_v_and_unknown_devices() {
        let mut bus = Bus::new_without_bios();
        bus.enable_hle_bios();
        poke32(&mut bus, kvar::USER_BUTTONS, 0x8002_0000);
        for (k, b) in [0x00, 0x23, 0x12, 0x34, 0x00, 0x20, 0x05]
            .iter()
            .enumerate()
        {
            bus.write8_safe(INTERNAL_BUF1 + k as u32, *b);
        }
        for (k, b) in [0x00, 0x73, 0x12, 0x34].iter().enumerate() {
            bus.write8_safe(INTERNAL_BUF2 + k as u32, *b);
        }
        // 1234h | 7C7h = 17F7h, bit 6 cleared (analogue 20h > 10h).
        assert_eq!(pad_dr(&mut bus), 0xFFFF_17B7);
        assert_eq!(bus.read32(0x8002_0000), 0xFFFF_17B7);
    }

    #[test]
    fn pad_output_bytes_are_clipped_unless_send_pad_patched() {
        let mut bus = Bus::new_without_bios();
        bus.enable_hle_bios();
        let out = 0x8002_0200;
        for (k, b) in [0x01, 0x00, 0x40, 0xFF].iter().enumerate() {
            bus.write8_safe(out + k as u32, *b);
        }
        poke32(&mut bus, crate::hle_kernel::kvar::PAD_OUTPUT, out);
        assert_eq!([tx_byte(&bus, 0, 3), tx_byte(&bus, 0, 4)], [0x00, 0x01]);
        assert_eq!(tx_byte(&bus, 1, 3), 0, "no output buffer for pad 2");
        poke32(&mut bus, crate::hle_kernel::kvar::PATCH_FLAGS, 1 << 8);
        assert_eq!([tx_byte(&bus, 0, 4), tx_byte(&bus, 0, 5)], [0x40, 0xFF]);
        // A zero first byte disables output altogether.
        bus.write8_safe(out, 0);
        assert_eq!(tx_byte(&bus, 0, 4), 0);
    }
}
