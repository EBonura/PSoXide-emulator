// SPDX-License-Identifier: GPL-2.0-or-later
//! Memory card driver of the HLE kernel (low level): InitCARD2, StartCARD2,
//! StopCARD2, `_card_read`, `_card_write`, `_card_info_subfunc`,
//! `_new_card`, `_card_status`, `_card_wait`, and the interrupt-driven
//! command engine behind them. The backup unit (directory cache, `bu`
//! file device) sits on top in [`crate::hle_bu`].
//!
//! Sources: psx-spx "Memory Card Read/Write Commands" (the byte sequences,
//! the FLAG byte, the end bytes), "BIOS Memory Card Functions" (the
//! functions, the status values 01h/02h/04h/08h/11h/21h, one sector per
//! frame with the two slots taking alternate frames, sector 400h accepted,
//! `_new_card`), "BIOS Event Summary" (the HwCARD events of class
//! F0000011h and the SwCARD events of class F4000001h), "BIOS Patches"
//! (`early_card_irq_patch`, `patch_uninstall_early_card_irq_handler`,
//! `patch_card_info_step4`), "BIOS Interrupt/Exception Handling" (the
//! CardSpecificIrq element of priority chain 1) and "Controller and Memory
//! Card Overview" (IRQ7 after each byte but the last).
//!
//! How it runs. A request (`_card_read` and friends) only records the
//! command and marks the slot busy. On each VBlank, after the pad reader,
//! the driver ends a command that is still running (timeout: nothing
//! answered in a whole frame), then starts the command of the slot whose
//! turn it is. Slots alternate, so a lone slot gets a command every other
//! frame. A started command sends its first byte; every following byte goes
//! out from the card IRQ (IRQ7, one per byte) with the answer to the byte
//! before it checked on the way. The 128 data bytes of a sector are
//! exchanged by the early routine in handler slot 1, which games may
//! replace or remove; the ordinary handler then takes those IRQs too. The
//! last byte of read and write draws no IRQ, so the handler waits for it
//! (a retried trap, like the pad reader). All state is in kernel RAM
//! ([`kvar`]).

use crate::hle_exceptions as ex;
use crate::hle_kernel::{peek32, poke32, stub_addr};
use crate::Bus;

/// Kernel variables of the card driver, each per slot ones as two words.
pub mod kvar {
    /// StartCARD2 is in effect: the VBlank step runs.
    pub const STARTED: u32 = 0x3B00;
    /// InitCARD2 has run.
    pub const INITIALISED: u32 = 0x3B04;
    /// Slot whose turn the next VBlank step is.
    pub const TURN: u32 = 0x3B08;
    /// Auto format flag (A(ADh)).
    pub const AUTO_FORMAT: u32 = 0x3B0C;
    /// Status per slot (psx-spx values).
    pub const STATUS: u32 = 0x3B10;
    /// Requested operation per slot ([`super::op`]), 0 when idle.
    pub const OP: u32 = 0x3B18;
    /// Sector per slot.
    pub const SECTOR: u32 = 0x3B20;
    /// Buffer address per slot.
    pub const BUFFER: u32 = 0x3B28;
    /// Device (port) number per slot as the caller gave it.
    pub const DEVICE: u32 = 0x3B30;
    /// Outcome of the last finished command per slot ([`super::outcome`]).
    pub const RESULT: u32 = 0x3B38;
    /// Slot + 1 of the command whose bytes are being exchanged, else 0.
    pub const ACTIVE: u32 = 0x3B40;
    /// Index of the byte last sent in that command.
    pub const STEP: u32 = 0x3B44;
    /// Running XOR over the sector address and data.
    pub const CHECKSUM: u32 = 0x3B48;
    /// Nonzero while the early routine may take the data bytes.
    pub const DATA_PHASE: u32 = 0x3B4C;
    /// Data bytes exchanged so far in this sector.
    pub const DATA_COUNT: u32 = 0x3B50;
    /// FLAG byte the card answered.
    pub const FLAG_BYTE: u32 = 0x3B54;
    /// `_new_card` was called: the next command ignores the card changed
    /// flag.
    pub const NEW_CARD: u32 = 0x3B58;
    /// Device of the last finished command.
    pub const LAST_DEVICE: u32 = 0x3B5C;
}

/// Sector buffers of the backup unit, 80h bytes per slot.
pub const BU_BUFFER: u32 = 0x3C00;
/// Directory cache of the backup unit: 15 entries of [`DIR_ENTRY_SIZE`]
/// bytes per slot.
pub const DIRECTORY: u32 = 0x3D00;
/// Bytes of a cached directory entry (the first 20h bytes of its frame).
pub const DIR_ENTRY_SIZE: u32 = 0x20;
/// Broken sector lists: 20 words per slot.
pub const BROKEN: u32 = 0x40C0;

/// The CardSpecificIrq chain element (priority chain 1).
pub const HI_CARD: u32 = 0x0000_3170;

/// Kernel-internal trap functions of the card driver.
pub mod internal {
    /// First function of [`super::HI_CARD`]: is this IRQ7 for the card?
    pub const VERIFIER: u8 = 0x32;
    /// Second function: take the answer to the last byte, send the next.
    pub const HANDLER: u8 = 0x33;
    /// What the early routine calls with one data byte pending.
    pub const FAST: u8 = 0x34;
}

/// HwCARD, the low-level card events (class F0000011h).
pub const EVENT_CARD: u32 = 0xF000_0011;
/// SwCARD, the backup unit's events (class F4000001h).
pub const EVENT_BU: u32 = 0xF400_0001;

/// Operations a slot can be asked for.
pub mod op {
    /// `_card_read`.
    pub const READ: u32 = 1;
    /// `_card_write`.
    pub const WRITE: u32 = 2;
    /// `_card_info_subfunc`.
    pub const INFO: u32 = 3;
}

/// How a command ended.
pub mod outcome {
    /// Done, the card answered as it should.
    pub const OK: u32 = 1;
    /// Nothing answered in time.
    pub const TIMEOUT: u32 = 2;
    /// The card's FLAG byte says it was changed (and `_new_card` was not
    /// called).
    pub const CHANGED: u32 = 3;
    /// The card's FLAG byte says the last write failed.
    pub const WRITE_ERROR: u32 = 4;
    /// Anything else that went wrong.
    pub const ERROR: u32 = 5;
}

/// psx-spx `_card_status` values.
mod status {
    pub const READY: u32 = 0x01;
    pub const BUSY_READ: u32 = 0x02;
    pub const BUSY_WRITE: u32 = 0x04;
    pub const BUSY_INFO: u32 = 0x08;
    pub const TIMEOUT: u32 = 0x11;
    pub const ERROR: u32 = 0x21;
}

/// HwCARD event specs (psx-spx "BIOS Event Summary").
mod spec {
    pub const DONE: u32 = 0x4;
    pub const BUSY: u32 = 0x100;
    pub const ERR_CHANGED: u32 = 0x2000;
    pub const ERR: u32 = 0x8000;
    pub const ERR_WRITE: u32 = 0x8001;
}

const I_STAT: u32 = 0x1F80_1070;
const I_MASK: u32 = 0x1F80_1074;
const SIO_DATA: u32 = 0x1F80_1040;
const SIO_STAT: u32 = 0x1F80_1044;
const SIO_MODE: u32 = 0x1F80_1048;
const SIO_CTRL: u32 = 0x1F80_104A;
const SIO_BAUD: u32 = 0x1F80_104E;
const IRQ_SIO0: u32 = 1 << 7;
const STAT_RX_READY: u16 = 1 << 1;
const CTRL_TXEN: u16 = 1 << 0;
const CTRL_SELECT: u16 = 1 << 1;
const CTRL_ACKNOWLEDGE: u16 = 1 << 4;
const CTRL_RESET: u16 = 1 << 6;
const CTRL_ACK_IRQ: u16 = 1 << 12;
const CTRL_PORT2: u16 = 1 << 13;
/// Reload 88h and the 8-bit, no parity, MUL1 format the cards expect
/// (psx-spx "Serial Interfaces").
const BAUD: u16 = 0x88;
const MODE: u16 = 0x000D;

/// Cycles between selecting a card and the first byte.
const SELECT_SETTLE: u32 = 2 * BAUD as u32;

/// Bytes in a sector (psx-spx "Memory Card Data Format": a frame).
const SECTOR_BYTES: u32 = 0x80;
/// Highest sector number accepted. psx-spx: the valid range is 0..3FFh and
/// the function also accepts 400h.
const MAX_SECTOR: u32 = 0x400;

/// Slot (0 or 1) of a device number: psx-spx gives the port as 00h for
/// slot 1 and 10h for slot 2, the low nibble selecting a multitap card.
pub fn slot_of(device: u32) -> u32 {
    (device >> 4) & 1
}

/// Slot of the argument of `_card_status` and `_card_wait`, which psx-spx
/// gives as the port divided by 10h; a port number is accepted as well.
fn slot_arg(arg: u32) -> u32 {
    if arg <= 1 {
        arg
    } else {
        slot_of(arg)
    }
}

/// Address of the per-slot word of `base`.
fn var(base: u32, slot: u32) -> u32 {
    base + 4 * slot
}

fn get(bus: &Bus, base: u32, slot: u32) -> u32 {
    peek32(bus, var(base, slot))
}

fn set(bus: &mut Bus, base: u32, slot: u32, value: u32) {
    poke32(bus, var(base, slot), value);
}

/// Boot: nothing started, both slots ready, and the CardSpecificIrq element
/// laid out (it is enqueued by StartCARD2).
pub fn install(bus: &mut Bus) {
    for addr in (kvar::STARTED..=kvar::LAST_DEVICE).step_by(4) {
        poke32(bus, addr, 0);
    }
    for slot in 0..2 {
        set(bus, kvar::STATUS, slot, status::READY);
    }
    poke32(bus, HI_CARD, 0);
    poke32(bus, HI_CARD + 4, stub_addr(3, internal::HANDLER));
    poke32(bus, HI_CARD + 8, stub_addr(3, internal::VERIFIER));
    poke32(bus, HI_CARD + 12, 0);
}

/// Put the call to the early card routine in handler slot 1 (psx-spx
/// "early_card_irq_patch": `lui`, `ori`, `jalr`, `nop`; games read the two
/// immediates to find the routine).
fn install_early_handler(bus: &mut Bus) {
    use crate::hle_asm::{Asm, V0};
    let at = crate::hle_kernel::EXCEPTION_HANDLER + ex::HANDLER_SLOTS;
    let routine = ex::code().card_early;
    let mut a = Asm::new(at);
    a.lui(V0, (routine >> 16) as u16);
    a.ori(V0, V0, routine as u16);
    a.jalr(V0);
    a.nop();
    for (i, word) in a.finish().iter().enumerate() {
        poke32(bus, at + 4 * i as u32, *word);
    }
}

/// B(4Ah) InitCARD2(pad_enable): reset the driver, put the early routine in
/// place and set the pad enable flag from the argument (psx-spx: it selects
/// whether the pads are read together with the cards). Returns 0.
pub fn init_card(bus: &mut Bus, pad_enable: u32) -> u32 {
    install_early_handler(bus);
    for slot in 0..2 {
        set(bus, kvar::OP, slot, 0);
        set(bus, kvar::STATUS, slot, status::READY);
    }
    for addr in [kvar::ACTIVE, kvar::STEP, kvar::DATA_PHASE, kvar::NEW_CARD] {
        poke32(bus, addr, 0);
    }
    poke32(bus, kvar::INITIALISED, 1);
    poke32(
        bus,
        crate::hle_kernel::kvar::PAD_STARTED,
        u32::from(pad_enable != 0),
    );
    0
}

/// B(4Bh) StartCARD2: SIO0 for the cards, the shared pad handler and the
/// card's own IRQ element in their chains (priority 2 and 1). Returns 1.
pub fn start_card(bus: &mut Bus) -> u32 {
    crate::hle_pad::setup_sio0(bus);
    // The bytes of a command arrive as IRQ7, so it has to be enabled.
    let mask = bus.read32(I_MASK);
    bus.write32(I_MASK, mask | IRQ_SIO0);
    ex::deq_int(bus, 1, HI_CARD);
    ex::enq_int(bus, 1, HI_CARD);
    crate::hle_pad::enqueue_handler(bus);
    // The first VBlank step after StartCARD2 is slot 2's turn, so a lone
    // slot 1 gets its first command at the second step. psx-spx gives no
    // figure; this is what the previous kernel's card timing came to.
    poke32(bus, kvar::TURN, 1);
    poke32(bus, kvar::STARTED, 1);
    1
}

/// B(4Ch) StopCARD2: the card's IRQ element leaves its chain and the VBlank
/// step stops, ending a command in flight. Requests stay queued. Returns 1.
pub fn stop_card(bus: &mut Bus) -> u32 {
    ex::deq_int(bus, 1, HI_CARD);
    poke32(bus, kvar::STARTED, 0);
    if peek32(bus, kvar::ACTIVE) != 0 {
        finish(bus, outcome::TIMEOUT);
    }
    1
}

/// Record a request for the slot of `device`. Returns 1, or 0 when the
/// slot is still busy or the sector is out of range.
fn request(bus: &mut Bus, device: u32, sector: u32, buffer: u32, operation: u32) -> u32 {
    let slot = slot_of(device);
    if (operation != op::INFO && sector > MAX_SECTOR) || get(bus, kvar::OP, slot) != 0 {
        return 0;
    }
    set(bus, kvar::OP, slot, operation);
    set(bus, kvar::SECTOR, slot, sector);
    set(bus, kvar::BUFFER, slot, buffer);
    set(bus, kvar::DEVICE, slot, device);
    let busy = match operation {
        op::READ => status::BUSY_READ,
        op::WRITE => status::BUSY_WRITE,
        _ => status::BUSY_INFO,
    };
    set(bus, kvar::STATUS, slot, busy);
    1
}

/// B(4Fh) `_card_read(port, sector, dst)`: one sector, asynchronously.
pub fn card_read(bus: &mut Bus, device: u32, sector: u32, dst: u32) -> u32 {
    request(bus, device, sector, dst, op::READ)
}

/// B(4Eh) `_card_write(port, sector, src)`.
pub fn card_write(bus: &mut Bus, device: u32, sector: u32, src: u32) -> u32 {
    request(bus, device, sector, src, op::WRITE)
}

/// B(4Dh) `_card_info_subfunc(port)`: the incomplete read command.
pub fn card_info_internal(bus: &mut Bus, device: u32) -> u32 {
    request(bus, device, 0, 0, op::INFO)
}

/// B(50h) `_new_card`: the next command ignores the card changed flag.
pub fn new_card(bus: &mut Bus) {
    poke32(bus, kvar::NEW_CARD, 1);
}

/// B(58h) `_card_chan`: the port of the command that last finished.
pub fn card_chan(bus: &Bus) -> u32 {
    peek32(bus, kvar::LAST_DEVICE)
}

/// B(5Ch) `_card_status(slot)`.
pub fn card_status(bus: &Bus, slot: u32) -> u32 {
    get(bus, kvar::STATUS, slot_arg(slot))
}

/// B(5Dh) `_card_wait(slot)`: the status, or `None` while the command is
/// busy.
pub fn card_wait(bus: &Bus, slot: u32) -> Option<u32> {
    match card_status(bus, slot) {
        status::BUSY_READ | status::BUSY_WRITE | status::BUSY_INFO => None,
        done => Some(done),
    }
}

/// A(ADh) `_card_auto(flag)`: format a card that has no "MC" id on its own.
/// Returns the previous setting.
pub fn set_auto_format(bus: &mut Bus, flag: u32) -> u32 {
    let old = peek32(bus, kvar::AUTO_FORMAT);
    poke32(bus, kvar::AUTO_FORMAT, flag);
    old
}

/// Outcome of the last command of `slot` ([`outcome`]).
pub fn last_outcome(bus: &Bus, slot: u32) -> u32 {
    get(bus, kvar::RESULT, slot)
}

/// Whether `slot` has no request waiting or running.
pub fn idle(bus: &Bus, slot: u32) -> bool {
    get(bus, kvar::OP, slot) == 0
}

// ------------------------------------------------------------ the engine

/// SIO0 control value that selects `slot`'s port and enables the IRQ7 the
/// card's /ACK raises.
fn select_ctrl(slot: u32) -> u16 {
    let port = if slot == 0 { 0 } else { CTRL_PORT2 };
    port | CTRL_SELECT | CTRL_TXEN | CTRL_ACK_IRQ
}

/// Reset SIO0 and set it up for the cards again.
fn reset_sio0(bus: &mut Bus) {
    bus.write16(SIO_CTRL, CTRL_RESET);
    bus.write16(SIO_MODE, MODE);
    bus.write16(SIO_BAUD, BAUD);
    bus.write16(SIO_CTRL, 0);
}

/// The slot of the command in flight, if there is one.
fn active_slot(bus: &Bus) -> Option<u32> {
    peek32(bus, kvar::ACTIVE).checked_sub(1)
}

/// Byte `index` of the command in flight, sent to the card.
fn send(bus: &mut Bus, index: u32, byte: u8) {
    poke32(bus, kvar::STEP, index);
    bus.write8(SIO_DATA, byte);
}

/// The command byte of an operation (psx-spx: "R", "W"; the info command
/// is a read that is dropped after its third byte).
fn command_byte(operation: u32) -> u8 {
    if operation == op::WRITE {
        b'W'
    } else {
        b'R'
    }
}

/// Start the request of `slot`: forget the events of the last command (the
/// kernel un-delivers all but 8001h), select the card and send the address
/// byte, 81h plus the port's multitap nibble.
fn start_command(bus: &mut Bus, slot: u32) {
    for done in [spec::DONE, spec::BUSY, spec::ERR_CHANGED, spec::ERR] {
        ex::undeliver_event(bus, EVENT_CARD, done);
    }
    poke32(bus, kvar::ACTIVE, slot + 1);
    poke32(bus, kvar::DATA_PHASE, 0);
    poke32(bus, kvar::DATA_COUNT, 0);
    poke32(bus, kvar::FLAG_BYTE, 0);
    while bus.read16(SIO_STAT) & STAT_RX_READY != 0 {
        bus.read8(SIO_DATA);
    }
    bus.write32(I_STAT, !IRQ_SIO0);
    bus.write16(SIO_CTRL, select_ctrl(slot));
    // Give the card a moment to see /CS before the first byte: two bit
    // times. psx-spx gives no figure.
    bus.tick(SELECT_SETTLE);
    let device = get(bus, kvar::DEVICE, slot);
    send(bus, 0, 0x81u8.wrapping_add((device & 0x0F) as u8));
}

/// The VBlank step, run by the pad handler after the pads were read: end a
/// command that did not finish within a frame, then start the request of
/// the slot whose turn it is.
pub fn vblank(bus: &mut Bus) {
    if peek32(bus, kvar::ACTIVE) != 0 {
        finish(bus, outcome::TIMEOUT);
    }
    let slot = peek32(bus, kvar::TURN) & 1;
    poke32(bus, kvar::TURN, slot ^ 1);
    if get(bus, kvar::OP, slot) != 0 {
        start_command(bus, slot);
    }
}

/// End the command in flight: deselect the card (after a timeout, reset
/// SIO0 as well), set the slot's status, record the outcome, queue the
/// HwCARD event and tell the backup unit.
fn finish(bus: &mut Bus, result: u32) {
    let Some(slot) = active_slot(bus) else {
        return;
    };
    if result == outcome::TIMEOUT {
        reset_sio0(bus);
    } else {
        bus.write16(SIO_CTRL, 0);
    }
    bus.write32(I_STAT, !IRQ_SIO0);
    poke32(bus, kvar::ACTIVE, 0);
    poke32(bus, kvar::DATA_PHASE, 0);
    poke32(bus, kvar::NEW_CARD, 0);
    let (new_status, event) = match result {
        outcome::OK => (status::READY, spec::DONE),
        outcome::TIMEOUT => (status::TIMEOUT, spec::BUSY),
        outcome::CHANGED => (status::READY, spec::ERR_CHANGED),
        outcome::WRITE_ERROR => (status::READY, spec::ERR_WRITE),
        _ => (status::ERROR, spec::ERR),
    };
    set(bus, kvar::STATUS, slot, new_status);
    set(bus, kvar::OP, slot, 0);
    set(bus, kvar::RESULT, slot, result);
    poke32(bus, kvar::LAST_DEVICE, get(bus, kvar::DEVICE, slot));
    // The backup unit hears of it first: its own event (if the command was
    // part of a high-level operation) goes out before HwCARD's.
    crate::hle_bu::low_level_done(bus, slot);
    ex::queue_event(bus, EVENT_CARD, event);
}

/// First function of the CardSpecificIrq element: a command is in flight
/// and IRQ7 is pending and enabled.
pub fn verifier(bus: &mut Bus) -> u32 {
    let pending = bus.read32(I_STAT) & bus.read32(I_MASK) & IRQ_SIO0 != 0;
    u32::from(pending && peek32(bus, kvar::ACTIVE) != 0)
}

/// Second function: the answer to byte [`kvar::STEP`] has arrived. `None`
/// while the answer to the last byte of a command, which draws no IRQ, is
/// still on its way.
pub fn handler(bus: &mut Bus) -> Option<()> {
    let Some(slot) = active_slot(bus) else {
        return Some(());
    };
    let step = peek32(bus, kvar::STEP);
    let operation = get(bus, kvar::OP, slot);
    if step == last_byte(operation) {
        if bus.read16(SIO_STAT) & STAT_RX_READY == 0 {
            return None;
        }
        let end = bus.read8(SIO_DATA);
        finish(
            bus,
            if end == 0x47 {
                outcome::OK
            } else {
                outcome::ERROR
            },
        );
        return Some(());
    }
    let ctrl = bus.read16(SIO_CTRL);
    bus.write16(SIO_CTRL, ctrl | CTRL_ACKNOWLEDGE);
    bus.write32(I_STAT, !IRQ_SIO0);
    let rx = bus.read8(SIO_DATA);
    advance(bus, slot, operation, step, rx);
    if active_slot(bus).is_some() && peek32(bus, kvar::STEP) == last_byte(operation) {
        return None;
    }
    Some(())
}

/// Index of the byte of a command that draws no IRQ (psx-spx: the last
/// byte). The info command has none; it ends on an answer.
fn last_byte(operation: u32) -> u32 {
    match operation {
        op::READ => 139,
        op::WRITE => 137,
        _ => u32::MAX,
    }
}

/// Called by the early card routine (through its kernel function): one
/// data byte has arrived. Returns the address the exception continues at.
pub fn fast(bus: &mut Bus) -> u32 {
    if let Some(slot) = active_slot(bus) {
        let step = peek32(bus, kvar::STEP);
        let operation = get(bus, kvar::OP, slot);
        let ctrl = bus.read16(SIO_CTRL);
        bus.write16(SIO_CTRL, ctrl | CTRL_ACKNOWLEDGE);
        bus.write32(I_STAT, !IRQ_SIO0);
        let rx = bus.read8(SIO_DATA);
        advance(bus, slot, operation, step, rx);
    }
    ex::code().card_fast_rfe
}

/// What the FLAG byte says (psx-spx "FLAG Byte"): bit 3 means the card was
/// changed since its directory was read, which makes a command fail unless
/// `_new_card` was called; bit 2 means the last write failed.
fn flag_error(bus: &Bus, flag: u8) -> Option<u32> {
    if flag & 0x08 != 0 && peek32(bus, kvar::NEW_CARD) == 0 {
        Some(outcome::CHANGED)
    } else if flag & 0x04 != 0 {
        Some(outcome::WRITE_ERROR)
    } else {
        None
    }
}

/// Send the next data byte of a write.
fn send_data(bus: &mut Bus, slot: u32, index: u32) {
    let count = peek32(bus, kvar::DATA_COUNT);
    let src = get(bus, kvar::BUFFER, slot);
    let byte = bus.try_read8(src.wrapping_add(count)).unwrap_or(0);
    poke32(
        bus,
        kvar::CHECKSUM,
        peek32(bus, kvar::CHECKSUM) ^ u32::from(byte),
    );
    poke32(bus, kvar::DATA_COUNT, count + 1);
    poke32(bus, kvar::DATA_PHASE, u32::from(count + 1 < SECTOR_BYTES));
    send(bus, index, byte);
}

/// Take `rx`, the answer to byte `step` of the command, check it and send
/// the next byte, or end the command. The sequences are psx-spx's
/// "Reading Data from Memory Card" and "Writing Data to Memory Card"; the
/// info command is a read that is dropped once the ID byte is in.
fn advance(bus: &mut Bus, slot: u32, operation: u32, step: u32, rx: u8) {
    let sector = get(bus, kvar::SECTOR, slot);
    let (msb, lsb) = ((sector >> 8) as u8, sector as u8);
    let buffer = get(bus, kvar::BUFFER, slot);
    let check = |bus: &mut Bus, ok: bool, next: u32, byte: u8| {
        if ok {
            send(bus, next, byte);
        } else {
            finish(bus, outcome::ERROR);
        }
    };
    match (operation, step) {
        (_, 0) => send(bus, 1, command_byte(operation)),
        (_, 1) => {
            poke32(bus, kvar::FLAG_BYTE, u32::from(rx));
            match flag_error(bus, rx) {
                Some(bad) => finish(bus, bad),
                None => send(bus, 2, 0),
            }
        }
        (op::INFO, 2) => {
            // psx-spx: the info function sends one byte too many after the
            // last one, and does not look at the answer.
            if crate::hle_patch::card_info_extra_byte(bus) {
                send(bus, 3, 0);
            }
            finish(bus, outcome::OK);
        }
        (_, 2) => check(bus, rx == 0x5A, 3, 0),
        (_, 3) => check(bus, rx == 0x5D, 4, msb),
        (_, 4) => send(bus, 5, lsb),
        (op::READ, 5) => send(bus, 6, 0),
        (op::READ, 6) => check(bus, rx == 0x5C, 7, 0),
        (op::READ, 7) => check(bus, rx == 0x5D, 8, 0),
        // The card repeats the address; a sector it does not have comes
        // back as FFFFh.
        (op::READ, 8) => check(bus, rx == msb, 9, 0),
        (op::READ, 9) => {
            if rx != lsb {
                return finish(bus, outcome::ERROR);
            }
            poke32(bus, kvar::CHECKSUM, u32::from(msb ^ lsb));
            poke32(bus, kvar::DATA_COUNT, 0);
            poke32(bus, kvar::DATA_PHASE, 1);
            send(bus, 10, 0);
        }
        (op::READ, 10..=137) => {
            let count = step - 10;
            bus.write8_safe(buffer.wrapping_add(count), rx);
            poke32(
                bus,
                kvar::CHECKSUM,
                peek32(bus, kvar::CHECKSUM) ^ u32::from(rx),
            );
            poke32(bus, kvar::DATA_COUNT, count + 1);
            poke32(bus, kvar::DATA_PHASE, u32::from(count + 1 < SECTOR_BYTES));
            send(bus, step + 1, 0);
        }
        (op::READ, 138) => {
            let ok = u32::from(rx) == peek32(bus, kvar::CHECKSUM);
            check(bus, ok, 139, 0)
        }
        (op::WRITE, 5) => {
            poke32(bus, kvar::CHECKSUM, u32::from(msb ^ lsb));
            poke32(bus, kvar::DATA_COUNT, 0);
            send_data(bus, slot, 6);
        }
        (op::WRITE, 6..=132) => send_data(bus, slot, step + 1),
        (op::WRITE, 133) => {
            let sum = peek32(bus, kvar::CHECKSUM) as u8;
            send(bus, 134, sum);
        }
        (op::WRITE, 134) => send(bus, 135, 0),
        (op::WRITE, 135) => check(bus, rx == 0x5C, 136, 0),
        (op::WRITE, 136) => check(bus, rx == 0x5D, 137, 0),
        _ => finish(bus, outcome::ERROR),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hle_asm::*;
    use crate::hle_testkit::*;

    /// Pseudo call: clear the call to the early card routine in handler
    /// slot 1 (psx-spx `patch_uninstall_early_card_irq_handler`).
    fn nop_early() -> Step {
        Step::Code(|a| {
            a.li(T3, crate::hle_kernel::EXCEPTION_HANDLER + ex::HANDLER_SLOTS);
            for k in 0..4 {
                a.sw(ZERO, 4 * k, T3);
            }
        })
    }

    /// InitCARD2(1), StartCARD2, _new_card: calls 0..2.
    fn init() -> Vec<Step> {
        vec![b(0x4A, [1, 0, 0, 0]), b(0x4B, [0; 4]), b(0x50, [0; 4])]
    }

    /// OpenEvent(HwCARD, spec, 0x2000, 0) and EnableEvent: calls 0 and 1.
    fn watch(spec: u32) -> Vec<Step> {
        vec![
            b(0x08, [EVENT_CARD, spec, 0x2000, 0]),
            b(0x0C, [res(0), 0, 0, 0]),
        ]
    }

    #[test]
    fn a_sector_written_to_the_card_reads_back_and_the_events_arrive() {
        let mut bus = bus_with_card();
        let (src, dst) = (0x8003_0000, 0x8003_0100);
        for k in 0..0x80 {
            bus.write8_safe(src + k, (k * 3 + 1) as u8);
        }
        let mut steps = watch(4);
        steps.extend(init());
        steps.extend([
            b(0x4E, [0x00, 0x40, src, 0]),        // 5 write sector 40h
            b(0x5D, [0; 4]),                      // 6 wait
            b(0x0B, [res(0), 0, 0, 0]),           // 7 TestEvent: HwCARD 4 arrived
            b(0x4F, [0x00, 0x40, dst, 0]),        // 8 read it back
            b(0x5D, [0; 4]),                      // 9
            b(0x4F, [0x00, 0x00, dst + 0x80, 0]), // 10 sector 0
            b(0x5D, [0; 4]),                      // 11
        ]);
        run(&mut bus, &program(&steps), 1);
        assert_eq!(result(&mut bus, 0) >> 24, 0xF1, "an event handle");
        assert_eq!([result(&mut bus, 5), result(&mut bus, 8)], [1, 1], "queued");
        assert_eq!([result(&mut bus, 6), result(&mut bus, 9)], [1, 1], "ready");
        assert_eq!(result(&mut bus, 7), 1, "HwCARD 4 delivered");
        assert_eq!(
            get_bytes(&bus, dst, 0x80),
            (0..0x80u32).map(|k| (k * 3 + 1) as u8).collect::<Vec<_>>()
        );
        // Sector 0 of a formatted card starts with "MC".
        assert_eq!(get_bytes(&bus, dst + 0x80, 2), b"MC");
        assert_eq!(bus.read32(DIRECTORY), 0, "no directory is cached");
    }

    #[test]
    fn a_changed_card_fails_until_new_card_is_called() {
        let mut bus = bus_with_card();
        let dst = 0x8003_0000;
        let mut steps = watch(0x2000);
        steps.extend([
            b(0x4A, [1, 0, 0, 0]),
            b(0x4B, [0; 4]),
            b(0x4F, [0x00, 0x00, dst, 0]),
            b(0x5D, [0; 4]),
            b(0x0B, [res(0), 0, 0, 0]),
        ]);
        run(&mut bus, &program(&steps), 1);
        // The fresh card says it was changed: the read fails with HwCARD
        // 2000h, leaves the slot ready and does not touch the buffer.
        assert_eq!(result(&mut bus, 5), 1);
        assert_eq!(result(&mut bus, 6), 1, "event delivered");
        assert_eq!(bus.try_read8(dst), Some(0));
        assert_eq!(peek32(&bus, var(kvar::RESULT, 0)), outcome::CHANGED);
    }

    #[test]
    fn an_empty_slot_times_out_with_status_11h_and_hwcard_100h() {
        let mut bus = bus_with_card();
        let mut steps = watch(0x100);
        steps.extend(init());
        steps.extend([
            b(0x4F, [0x10, 0x00, 0x8003_0000, 0]),
            b(0x5D, [1, 0, 0, 0]),
            b(0x0B, [res(0), 0, 0, 0]),
        ]);
        run(&mut bus, &program(&steps), 1);
        assert_eq!(result(&mut bus, 6), 0x11);
        assert_eq!(result(&mut bus, 7), 1, "HwCARD 100h delivered");
    }

    #[test]
    fn requests_check_the_sector_and_a_busy_slot() {
        let mut bus = bus_with_card();
        let dst = 0x8003_0000;
        let mut steps = init();
        steps.extend([
            b(0x4F, [0, 0x401, dst, 0]), // 3 out of range
            b(0x4F, [0, 0x400, dst, 0]), // 4 psx-spx: 400h is accepted
            b(0x4F, [0, 0x01, dst, 0]),  // 5 slot busy
            b(0x5C, [0; 4]),             // 6 busy/read
            b(0x5D, [0; 4]),             // 7
            b(0x4F, [0, 0x01, dst, 0]),  // 8 free again
        ]);
        run(&mut bus, &program(&steps), 1);
        assert_eq!(result(&mut bus, 3), 0);
        assert_eq!(result(&mut bus, 4), 1);
        assert_eq!(result(&mut bus, 5), 0);
        assert_eq!(result(&mut bus, 6), 2);
        assert_eq!(result(&mut bus, 8), 1);
    }

    #[test]
    fn requests_wait_while_the_driver_is_stopped() {
        let mut bus = bus_with_card();
        let steps = [
            b(0x4A, [1, 0, 0, 0]),
            b(0x4B, [0; 4]),
            b(0x4C, [0; 4]),
            b(0x50, [0; 4]),
            b(0x4F, [0, 0x01, 0x8003_0000, 0]),
            b(0x5C, [0; 4]),
        ];
        run(&mut bus, &program(&steps), 6);
        assert_eq!(result(&mut bus, 4), 1);
        assert_eq!(result(&mut bus, 5), 2, "still busy: nothing services it");
    }

    #[test]
    fn the_ordinary_handler_takes_the_data_bytes_when_the_early_routine_is_gone() {
        let mut bus = bus_with_card();
        let (src, dst) = (0x8003_0000, 0x8003_0100);
        for k in 0..0x80 {
            bus.write8_safe(src + k, (k ^ 0x5A) as u8);
        }
        let mut steps = init();
        // The write goes through the early routine. A game then removes the
        // call from handler slot 1 and the read has to be served by the
        // ordinary handler, one IRQ7 per byte. Through the kernel's
        // exception path that does not fit in a frame, so the read ends in
        // the VBlank timeout, but the bytes that did arrive are in the
        // buffer.
        steps.extend([b(0x4E, [0, 0x41, src, 0]), b(0x5D, [0; 4]), nop_early()]);
        steps.extend([b(0x4F, [0, 0x41, dst, 0]), b(0x5D, [0; 4])]);
        run(&mut bus, &program(&steps), 1);
        assert_eq!(result(&mut bus, 4), 1, "the write finished");
        assert_eq!(result(&mut bus, 6), 0x11, "the read ran out of its frame");
        let want: Vec<u8> = (0..16u32).map(|k| (k ^ 0x5A) as u8).collect();
        assert_eq!(
            get_bytes(&bus, dst, 16),
            want,
            "data bytes came through the ordinary handler"
        );
        assert_eq!(
            peek32(
                &bus,
                crate::hle_kernel::EXCEPTION_HANDLER + ex::HANDLER_SLOTS
            ),
            0
        );
    }
}
