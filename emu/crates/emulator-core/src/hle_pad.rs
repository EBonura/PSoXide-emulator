// SPDX-License-Identifier: GPL-2.0-or-later
//! Controller driver of the HLE kernel: InitPAD2/StartPAD2/StopPAD2,
//! PAD_init2/PAD_dr, and the VBlank handler (psx-spx's PadCardIrq) that
//! reads both controller ports and then lets the card driver run.
//!
//! Sources: psx-spx "BIOS Joypad Functions" (the API and buffer format),
//! "Controllers - Communication Sequence" (01h, 42h, then 00h/output bytes;
//! the ID's low nibble counts the halfwords that follow, 0 meaning 16),
//! "Controller and Memory Card Overview" (each byte but the last is
//! acknowledged on /ACK, IRQ7; the kernel waits about 100 cycles after a
//! byte before clearing the old IRQ7 and waiting for the next), "Serial
//! Interfaces" (SIO0 registers; 250 kHz with a reload of 88h) and the
//! "BIOS Patches" notes on pad error handling and output clipping.
//!
//! The reader drives the emulated SIO0 registers, so transfer and /ACK
//! timing are the port model's. It runs inside the exception handler as a
//! kernel trap that is retried while it waits, with its progress in kernel
//! RAM ([`kvar`]), so a save state taken mid-read resumes it.

use crate::hle_kernel::{peek32, poke32, stub_addr};
use crate::Bus;

/// Kernel variables of the pad driver.
pub mod kvar {
    /// InitPAD2 buffers for port 1 and 2 (2 words).
    pub const BUF: u32 = 0x0B80;
    /// InitPAD2 buffer sizes (2 words).
    pub const SIZE: u32 = 0x0B88;
    /// PAD_init2's button_dest (0 = PAD_dr is not called by the handler).
    pub const USER_BUTTONS: u32 = 0x0B90;
    /// Reader phase ([`super::phase`]).
    pub const PHASE: u32 = 0x0B94;
    /// Port being read (0 or 1).
    pub const PORT: u32 = 0x0B98;
    /// Byte index within the current port's transfer.
    pub const STEP: u32 = 0x0B9C;
    /// Bytes still to exchange after the current one.
    pub const LEFT: u32 = 0x0BA0;
    /// Cycle count the current wait ends at.
    pub const DEADLINE: u32 = 0x0BA4;
}

/// PAD_init2's own buffers (psx-spx: "hidden" in the kernel variables).
pub const INTERNAL_BUF1: u32 = 0x0BB0;
/// Second PAD_init2 buffer.
pub const INTERNAL_BUF2: u32 = 0x0BD4;
/// Bytes the handler stores per port (psx-spx: up to 22h).
pub const BUF_LEN: u32 = 0x22;

/// The PadCardIrq chain element (psx-spx "Priority Chains", priority 2).
pub const HI_PAD: u32 = 0x0000_3160;

/// Kernel-internal trap functions of the driver.
pub mod internal {
    /// First function of [`super::HI_PAD`]: is VBlank pending?
    pub const VERIFIER: u8 = 0x30;
    /// Second function: read the pads, then the card driver's VBlank step.
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
const IRQ_SIO0: u32 = 1 << 7;

const STAT_RX_READY: u16 = 1 << 1;

const CTRL_TXEN: u16 = 1 << 0;
const CTRL_SELECT: u16 = 1 << 1;
const CTRL_ACKNOWLEDGE: u16 = 1 << 4;
const CTRL_RESET: u16 = 1 << 6;
const CTRL_ACK_IRQ: u16 = 1 << 12;
const CTRL_PORT2: u16 = 1 << 13;

/// Reload 88h: about 250 kHz, the rate controllers and cards expect
/// (psx-spx "Serial Interfaces").
const BAUD: u16 = 0x88;
/// MUL1, 8-bit characters, no parity, clock high when idle.
const MODE: u16 = 0x000D;

/// Cycles one byte takes on the wire: eight bits of 88h cycles.
const BYTE_CYCLES: u32 = 8 * BAUD as u32;
/// Wait after selecting a port before the first byte. psx-spx gives no
/// figure; PSoXide's console sweep found official pads silent without a
/// setup delay, so the reader allows one byte time.
const SELECT_SETTLE: u32 = BYTE_CYCLES;
/// Wait after a byte before clearing the old IRQ7 (psx-spx: "100 cycles
/// or so").
const AFTER_BYTE: u32 = 100;
/// How long to wait for /ACK once the byte is out: two byte times.
const ACK_TIMEOUT: u32 = 2 * BYTE_CYCLES;
/// How long the other port stays selected after a failed transfer, while
/// the pad error handling is in place (psx-spx gives no figure).
const RESELECT_HOLD: u32 = 100;

/// Reader phases.
mod phase {
    /// Nothing in progress: start port [`super::kvar::PORT`].
    pub const IDLE: u32 = 0;
    /// Port selected; the first byte goes out when the settle time ends.
    pub const SETTLE: u32 = 1;
    /// A byte is out; the old IRQ7 is cleared when the short wait ends.
    pub const SENT: u32 = 2;
    /// Waiting for /ACK (IRQ7), or for the last byte to arrive.
    pub const WAIT: u32 = 3;
    /// After a failed transfer, the other port is selected for a moment.
    pub const RESELECT: u32 = 4;
}

fn wait_cycles(bus: &mut Bus, cycles: u32) {
    let end = (bus.cycles() as u32).wrapping_add(cycles);
    poke32(bus, kvar::DEADLINE, end);
}

fn waited(bus: &Bus) -> bool {
    (bus.cycles() as u32).wrapping_sub(peek32(bus, kvar::DEADLINE)) as i32 >= 0
}

/// Lay out the chain element; the driver starts stopped.
pub fn install(bus: &mut Bus) {
    poke32(bus, HI_PAD, 0);
    poke32(bus, HI_PAD + 4, stub_addr(3, internal::HANDLER));
    poke32(bus, HI_PAD + 8, stub_addr(3, internal::VERIFIER));
    poke32(bus, HI_PAD + 12, 0);
    for var in (kvar::BUF..=kvar::DEADLINE).step_by(4) {
        poke32(bus, var, 0);
    }
}

/// B(12h) InitPAD2(buf1, siz1, buf2, siz2): remember the buffers, zero
/// them for their sizes, clear SetPadOutput's buffers and set the pad
/// enable flag. The element is not enqueued yet.
pub fn init_pad(bus: &mut Bus, buf1: u32, siz1: u32, buf2: u32, siz2: u32) -> u32 {
    for (port, (buf, size)) in [(buf1, siz1), (buf2, siz2)].into_iter().enumerate() {
        poke32(bus, kvar::BUF + 4 * port as u32, buf);
        poke32(bus, kvar::SIZE + 4 * port as u32, size);
        for k in 0..size.min(0x100) {
            bus.write8_safe(buf.wrapping_add(k), 0);
        }
    }
    for i in 0..4 {
        poke32(bus, crate::hle_kernel::kvar::PAD_OUTPUT + 4 * i, 0);
    }
    poke32(bus, kvar::PHASE, phase::IDLE);
    poke32(bus, kvar::PORT, 0);
    poke32(bus, crate::hle_kernel::kvar::PAD_STARTED, 1);
    1
}

/// Reset SIO0 and set the controller rate and character format.
pub(crate) fn setup_sio0(bus: &mut Bus) {
    bus.write16(SIO_CTRL, CTRL_RESET);
    bus.write16(SIO_MODE, MODE);
    bus.write16(SIO_BAUD, BAUD);
    bus.write16(SIO_CTRL, 0);
}

/// Start the shared pad/card VBlank handler (StartPAD2 and StartCARD2):
/// enable the VBlank IRQ, let this handler rather than the root-counter
/// one acknowledge VBlank, and put [`HI_PAD`] at the head of priority
/// chain 2 (once).
pub fn enqueue_handler(bus: &mut Bus) {
    let mask = bus.read32(I_MASK);
    bus.write32(I_MASK, mask | IRQ_VBLANK);
    crate::hle_exceptions::change_clear_rcnt(bus, 3, 0);
    crate::hle_exceptions::deq_int(bus, 2, HI_PAD);
    crate::hle_exceptions::enq_int(bus, 2, HI_PAD);
}

/// B(13h) StartPAD2: set up SIO0, restart the reader and start the shared
/// handler. Returns 1.
pub fn start_pad(bus: &mut Bus) -> u32 {
    setup_sio0(bus);
    poke32(bus, kvar::PHASE, phase::IDLE);
    poke32(bus, kvar::PORT, 0);
    enqueue_handler(bus);
    1
}

/// B(14h) StopPAD2: dequeue the element (which stops the card driver's
/// VBlank step too) and hand VBlank back to the root-counter handler.
/// Returns 1.
pub fn stop_pad(bus: &mut Bus) -> u32 {
    crate::hle_exceptions::deq_int(bus, 2, HI_PAD);
    crate::hle_exceptions::change_clear_rcnt(bus, 3, 1);
    1
}

/// B(15h) PAD_init2(type, button_dest, unused, unused), psx-spx: refused
/// (0) unless type is 20000000h or 20000001h. Otherwise the kernel's own
/// buffers are filled with FFh, then InitPAD2 and StartPAD2 run on them,
/// button_dest is remembered for PAD_dr, the two unused arguments are
/// written back to the caller's stack at sp+8 and sp+0Ch, and it returns 2.
pub fn pad_init2(bus: &mut Bus, ty: u32, button_dest: u32, a2: u32, a3: u32, sp: u32) -> u32 {
    if ty != 0x2000_0000 && ty != 0x2000_0001 {
        return 0;
    }
    for buf in [INTERNAL_BUF1, INTERNAL_BUF2] {
        for k in 0..BUF_LEN {
            bus.write8_safe(buf + k, 0xFF);
        }
    }
    init_pad(bus, INTERNAL_BUF1, BUF_LEN, INTERNAL_BUF2, BUF_LEN);
    start_pad(bus);
    poke32(bus, kvar::USER_BUTTONS, button_dest);
    poke32(bus, sp.wrapping_add(8), a2);
    poke32(bus, sp.wrapping_add(12), a3);
    2
}

/// One pad's PAD_dr halfword, psx-spx: the two button bytes swapped (the
/// first in the upper half). A digital pad (41h) is passed through. A
/// neGcon-class device (23h) gets its unused bits set (07C7h) and its
/// analog I and II buttons, pressed past 10h, mapped onto the digital
/// bits they sit beside (bit 6 and bit 7 of the swapped halfword). Any
/// other device, or none, reads FFFFh.
fn pad_dr_half(bus: &Bus, buf: u32) -> u32 {
    let byte = |k: u32| u32::from(bus.try_read8(buf.wrapping_add(k)).unwrap_or(0xFF));
    if byte(0) != 0 {
        return 0xFFFF;
    }
    let buttons = (byte(2) << 8) | byte(3);
    match byte(1) {
        0x41 => buttons,
        0x23 => {
            let mut v = buttons | 0x07C7;
            if byte(5) > 0x10 {
                v &= !0x40;
            }
            if byte(6) > 0x10 {
                v &= !0x80;
            }
            v
        }
        _ => 0xFFFF,
    }
}

/// B(16h) PAD_dr: pad 1's halfword in the low half, pad 2's in the high
/// half, also stored at button_dest (psx-spx: even when that is 0).
pub fn pad_dr(bus: &mut Bus) -> u32 {
    let value = pad_dr_half(bus, INTERNAL_BUF1) | (pad_dr_half(bus, INTERNAL_BUF2) << 16);
    let dest = peek32(bus, kvar::USER_BUTTONS);
    poke32(bus, dest, value);
    value
}

/// First function of the element: nonzero while VBlank is pending and
/// enabled.
pub fn verifier(bus: &mut Bus) -> u32 {
    u32::from(bus.read32(I_STAT) & bus.read32(I_MASK) & IRQ_VBLANK != 0)
}

/// Second function: with the pad enable flag set, read both ports (`None`
/// while a transfer waits on the port), then PAD_dr for PAD_init2 users,
/// acknowledge VBlank when B(5Bh) auto-ack is on and no game cleared that
/// code ([`crate::hle_patch::pad_vblank_ack`]), and run the card driver's
/// VBlank step.
pub fn handler(bus: &mut Bus) -> Option<u32> {
    if peek32(bus, crate::hle_kernel::kvar::PAD_STARTED) != 0 {
        read_pads(bus)?;
        if peek32(bus, kvar::USER_BUTTONS) != 0 {
            pad_dr(bus);
        }
    }
    let auto_ack = peek32(bus, crate::hle_kernel::kvar::SIO0_AUTO_ACK) != 0;
    if auto_ack && crate::hle_patch::pad_vblank_ack(bus) {
        bus.write32(I_STAT, !IRQ_VBLANK);
    }
    if peek32(bus, crate::hle_card::kvar::STARTED) != 0 {
        crate::hle_card::vblank(bus);
    }
    Some(0)
}

/// Port `port`'s buffer, or 0 when InitPAD2 gave none.
fn buffer(bus: &Bus, port: u32) -> u32 {
    peek32(bus, kvar::BUF + 4 * port)
}

/// Byte sent at `step` of a port's transfer: 01h (address), 42h (read),
/// 00h (the multitap byte), then the output bytes. SetPadOutput's buffer
/// for the port supplies those from its first byte on; until a game
/// replaces the clipping code, each is sent as 0 or 1 (psx-spx
/// "patch_optional_pad_output").
fn tx_byte(bus: &Bus, port: u32, step: u32) -> u8 {
    match step {
        0 => 0x01,
        1 => 0x42,
        2 => 0x00,
        n => {
            let src = peek32(bus, crate::hle_kernel::kvar::PAD_OUTPUT + 8 * port);
            if src == 0 {
                return 0;
            }
            let byte = bus.try_read8(src.wrapping_add(n - 3)).unwrap_or(0);
            if crate::hle_patch::pad_output_clipped(bus) {
                u8::from(byte != 0)
            } else {
                byte
            }
        }
    }
}

fn port_ctrl(port: u32) -> u16 {
    if port == 0 {
        0
    } else {
        CTRL_PORT2
    }
}

/// Send byte `step` of the current port and start the short wait.
fn send(bus: &mut Bus, port: u32, step: u32) {
    bus.write8(SIO_DATA, tx_byte(bus, port, step));
    poke32(bus, kvar::STEP, step);
    wait_cycles(bus, AFTER_BYTE);
    poke32(bus, kvar::PHASE, phase::SENT);
}

/// The port is finished (or absent): deselect and go on to the next one.
/// Returns true when both ports are done.
fn next_port(bus: &mut Bus) -> bool {
    bus.write16(SIO_CTRL, 0);
    let port = peek32(bus, kvar::PORT) + 1;
    poke32(bus, kvar::PHASE, phase::IDLE);
    if port > 1 {
        poke32(bus, kvar::PORT, 0);
        return true;
    }
    poke32(bus, kvar::PORT, port);
    false
}

/// A transfer failed: status FFh. With the pad error handling still in
/// place (psx-spx "BIOS Patches"), the other port is then selected for a
/// moment before the reader moves on. Returns true when that wait started.
fn fail(bus: &mut Bus, port: u32) -> bool {
    let buf = buffer(bus, port);
    if buf != 0 {
        bus.write8_safe(buf, 0xFF);
    }
    if !crate::hle_patch::pad_error_reselect(bus) {
        return false;
    }
    bus.write16(SIO_CTRL, port_ctrl(1 - port) | CTRL_SELECT);
    wait_cycles(bus, RESELECT_HOLD);
    poke32(bus, kvar::PHASE, phase::RESELECT);
    true
}

/// Store byte `rx` received at `step`. False when the reply is wrong.
fn receive(bus: &mut Bus, port: u32, step: u32, rx: u8) -> bool {
    let buf = buffer(bus, port);
    match step {
        // Address byte: the reply is the idle line.
        0 => true,
        // ID: its low nibble counts the halfwords after the 5Ah byte.
        1 => {
            bus.write8_safe(buf + 1, rx);
            let halfwords = match u32::from(rx & 0x0F) {
                0 => 16,
                n => n,
            };
            poke32(bus, kvar::LEFT, 1 + 2 * halfwords);
            true
        }
        2 => rx == 0x5A,
        n => {
            let at = 2 + (n - 3);
            if at < BUF_LEN {
                bus.write8_safe(buf + at, rx);
            }
            true
        }
    }
}

/// Finish a failed port: either wait out the reselect, or move on.
/// Returns `Some(true)` when the read is complete.
fn after_failure(bus: &mut Bus, port: u32) -> Option<bool> {
    if fail(bus, port) {
        None
    } else {
        Some(next_port(bus))
    }
}

/// Move the read of both ports on as far as the port allows: `None` while
/// it waits, `Some(())` once both ports are done.
fn read_pads(bus: &mut Bus) -> Option<()> {
    loop {
        let port = peek32(bus, kvar::PORT);
        match peek32(bus, kvar::PHASE) {
            phase::IDLE => {
                if buffer(bus, port) == 0 {
                    if next_port(bus) {
                        return Some(());
                    }
                    continue;
                }
                bus.write16(SIO_CTRL, port_ctrl(port) | CTRL_SELECT);
                wait_cycles(bus, SELECT_SETTLE);
                poke32(bus, kvar::PHASE, phase::SETTLE);
            }
            phase::SETTLE => {
                if !waited(bus) {
                    return None;
                }
                let ctrl = port_ctrl(port) | CTRL_SELECT | CTRL_TXEN | CTRL_ACK_IRQ;
                bus.write16(SIO_CTRL, ctrl);
                poke32(bus, kvar::LEFT, 2);
                send(bus, port, 0);
            }
            phase::SENT => {
                if !waited(bus) {
                    return None;
                }
                let ctrl = bus.read16(SIO_CTRL);
                bus.write16(SIO_CTRL, ctrl | CTRL_ACKNOWLEDGE);
                bus.write32(I_STAT, !IRQ_SIO0);
                wait_cycles(bus, ACK_TIMEOUT);
                poke32(bus, kvar::PHASE, phase::WAIT);
            }
            phase::WAIT => {
                let last = peek32(bus, kvar::LEFT) == 0;
                let rx_ready = bus.read16(SIO_STAT) & STAT_RX_READY != 0;
                let acked = bus.read32(I_STAT) & IRQ_SIO0 != 0;
                if !(rx_ready && (acked || last)) {
                    if !waited(bus) {
                        return None;
                    }
                    match after_failure(bus, port) {
                        Some(true) => return Some(()),
                        _ => continue,
                    }
                }
                let rx = bus.read8(SIO_DATA);
                let step = peek32(bus, kvar::STEP);
                if !receive(bus, port, step, rx) {
                    match after_failure(bus, port) {
                        Some(true) => return Some(()),
                        _ => continue,
                    }
                }
                if peek32(bus, kvar::LEFT) == 0 {
                    bus.write8_safe(buffer(bus, port), 0x00);
                    if next_port(bus) {
                        return Some(());
                    }
                    continue;
                }
                let left = peek32(bus, kvar::LEFT);
                poke32(bus, kvar::LEFT, left - 1);
                send(bus, port, step + 1);
            }
            _ => {
                if !waited(bus) {
                    return None;
                }
                if next_port(bus) {
                    return Some(());
                }
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

    /// B-function calls with immediate arguments, then interrupts on and
    /// an endless loop.
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

    fn bus_with_pad(buttons: u16) -> Bus {
        let mut bus = Bus::new_without_bios();
        bus.enable_hle_bios();
        bus.attach_digital_pad_port1();
        bus.set_port1_buttons(ButtonState::from_bits(buttons));
        bus
    }

    /// Run `words` until `vblanks` more VBlank IRQs were raised.
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

    /// StartPAD2, StopPAD2, StartCARD2, StopCARD2 and PAD_init2 return
    /// with interrupts on. psx-spx does not say so; Nightmare Creatures
    /// calls StartPAD2 with interrupts off and then waits in a TestEvent
    /// loop for a card event that only an interrupt can deliver.
    #[test]
    fn pad_and_card_start_stop_leave_the_critical_section() {
        for (func, want) in [(0x13i16, 1), (0x14, 1), (0x4B, 1), (0x4C, 1), (0x15, 2)] {
            let mut bus = bus_with_pad(0);
            let mut a = Asm::new(PROGRAM);
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
            assert_eq!(cpu.cop0()[12] & 0x401, 0);
            for _ in 0..10_000 {
                if cpu.pc() == end {
                    break;
                }
                cpu.step(&mut bus).unwrap();
            }
            assert_eq!(cpu.pc(), end, "B({func:02X}h) returned");
            assert_eq!(cpu.gpr(2), want, "B({func:02X}h) return value");
            assert_eq!(cpu.cop0()[12] & 0x401, 0x401, "B({func:02X}h)");
        }
    }

    #[test]
    fn init_and_start_pad_read_both_ports_on_vblank() {
        let (buf1, buf2) = (0x8002_0000, 0x8002_0040);
        let mut bus = bus_with_pad(button::START | button::CROSS);
        for k in 0..0x22 {
            bus.write8_safe(buf1 + k, 0xAA);
            bus.write8_safe(buf2 + k, 0xAA);
        }
        let words = program(&[(0x12, [buf1, 0x22, buf2, 0x22]), (0x13, [0; 4])]);
        run_for_vblanks(&mut bus, &words, 3);
        // Status 00h, ID 41h, then the buttons active low (psx-spx:
        // Start is bit 3 of the first byte, Cross bit 6 of the second).
        assert_eq!(bytes(&bus, buf1, 4), vec![0x00, 0x41, 0xF7, 0xBF]);
        // No device on port 2: status FFh, the rest as InitPAD2 left it.
        assert_eq!(bytes(&bus, buf2, 4), vec![0xFF, 0x00, 0x00, 0x00]);
        assert_eq!(
            peek32(&bus, crate::hle_exceptions::kvar::RCNT_AUTOACK + 12),
            0,
            "VBlank is the pad handler's to acknowledge"
        );
        assert_eq!(bus.read32(I_MASK) & IRQ_VBLANK, IRQ_VBLANK);
    }

    #[test]
    fn stop_pad_leaves_the_buffers_alone() {
        let (buf1, buf2) = (0x8002_0000, 0x8002_0040);
        let mut bus = bus_with_pad(button::START);
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
        let mut bus = bus_with_pad(button::CROSS);
        bus.write32(dest, 0x1234_5678);
        // A type psx-spx does not list is refused before anything runs.
        let words = program(&[(0x15, [0x1000_0001, dest, 0, 0])]);
        run_for_vblanks(&mut bus, &words, 2);
        assert_eq!(bus.read32(dest), 0x1234_5678);

        let mut bus = bus_with_pad(button::CROSS);
        let words = program(&[(0x15, [0x2000_0001, dest, 0, 0])]);
        run_for_vblanks(&mut bus, &words, 3);
        // Pad 1: first button byte FFh in the upper half, second BFh
        // (Cross) in the lower; pad 2 absent: FFFFh.
        assert_eq!(bus.read32(dest), 0xFFFF_FFBF);
    }

    #[test]
    fn pad_init2_writes_its_unused_arguments_to_the_callers_stack() {
        let mut bus = Bus::new_without_bios();
        bus.enable_hle_bios();
        let sp = 0x801F_FE00;
        assert_eq!(pad_init2(&mut bus, 0x2000_0000, 0, 0x11, 0x22, sp), 2);
        assert_eq!((bus.read32(sp + 8), bus.read32(sp + 12)), (0x11, 0x22));
        assert_eq!(bus.read32(sp + 4), 0, "button_dest is not written back");
    }

    #[test]
    fn pad_dr_formats_negcon_and_unknown_devices() {
        let mut bus = Bus::new_without_bios();
        bus.enable_hle_bios();
        poke32(&mut bus, kvar::USER_BUTTONS, 0x8002_0000);
        // neGcon: buttons 12h 34h, steering 00h, analog I 20h, analog II 05h.
        for (k, b) in [0x00, 0x23, 0x12, 0x34, 0x00, 0x20, 0x05]
            .iter()
            .enumerate()
        {
            bus.write8_safe(INTERNAL_BUF1 + k as u32, *b);
        }
        // An analog pad in analog mode is not understood.
        for (k, b) in [0x00, 0x73, 0x12, 0x34].iter().enumerate() {
            bus.write8_safe(INTERNAL_BUF2 + k as u32, *b);
        }
        // 1234h | 07C7h = 17F7h; analog I past 10h clears bit 6.
        assert_eq!(pad_dr(&mut bus), 0xFFFF_17B7);
        assert_eq!(bus.read32(0x8002_0000), 0xFFFF_17B7);
    }

    #[test]
    fn pad_output_bytes_are_clipped_until_a_game_replaces_the_clipping() {
        let mut bus = Bus::new_without_bios();
        bus.enable_hle_bios();
        let out = 0x8002_0200;
        for (k, b) in [0x00, 0x40, 0xFF].iter().enumerate() {
            bus.write8_safe(out + k as u32, *b);
        }
        poke32(&mut bus, crate::hle_kernel::kvar::PAD_OUTPUT, out);
        assert_eq!(
            [
                tx_byte(&bus, 0, 0),
                tx_byte(&bus, 0, 1),
                tx_byte(&bus, 0, 2)
            ],
            [1, 0x42, 0]
        );
        assert_eq!([tx_byte(&bus, 0, 3), tx_byte(&bus, 0, 4)], [0x00, 0x01]);
        assert_eq!(tx_byte(&bus, 1, 3), 0, "no output buffer for pad 2");
        poke32(
            &mut bus,
            crate::hle_kernel::PAD_CARD_ENTRY + crate::hle_patch::site::PAD_OUTPUT_CLIP,
            0,
        );
        assert_eq!([tx_byte(&bus, 0, 4), tx_byte(&bus, 0, 5)], [0x40, 0xFF]);
    }
}
