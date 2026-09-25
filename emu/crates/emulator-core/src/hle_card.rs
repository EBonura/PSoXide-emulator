// SPDX-License-Identifier: GPL-2.0-or-later
//! Memory card driver of the HLE kernel: the low-level sector functions
//! (InitCARD2, StartCARD2, _card_info, _card_read, _card_write, _new_card,
//! _card_status, _card_wait), the IRQ7 byte engine that runs them, the
//! "bufs_cb" completion callbacks, and the backup-unit directory cache
//! (_bu_init, _card_load).
//!
//! Behaviour follows psx-spx "BIOS Memory Card Functions", "Memory Card -
//! Higher/Lower Level Events" and "Memory Card Read/Write Commands", with
//! OpenBIOS `sio0/card.c`, `sio0/driver.c` and `card/backupunit.c`
//! (pcsx-redux, MIT) as the specification for the command bytes, the
//! per-VBlank scheduling (one sector per frame, the two slots on alternate
//! frames) and the order of flags, callbacks and events.
//!
//! Every byte goes through the emulated SIO0 registers and advances on the
//! card's own ACK interrupt, so a sector takes as long as it does on a
//! console. All state lives in kernel RAM ([`kvar`]).

use crate::hle_exceptions as ex;
use crate::hle_kernel::{peek32, poke32, stub_addr};
use crate::Bus;

/// Kernel variables of the card driver (`0x3B00..0x3BFF`).
pub mod kvar {
    /// Per-slot status (2 words): 01h ready, 02h read, 04h write, 08h
    /// info, 11h timeout, 21h error (psx-spx `_card_status`).
    pub const FLAGS: u32 = 0x3B00;
    /// Per-slot device id of the current operation (00h, 10h, ...).
    pub const DEVICE: u32 = 0x3B08;
    /// Per-slot sector.
    pub const SECTOR: u32 = 0x3B10;
    /// Per-slot user buffer.
    pub const BUFFER: u32 = 0x3B18;
    /// Per-slot running checksum.
    pub const CHECKSUM: u32 = 0x3B20;
    /// Per-slot FLAG byte of the last write.
    pub const FLAG_BYTE: u32 = 0x3B28;
    /// Per-slot operation ([`super::op`]).
    pub const OP: u32 = 0x3B30;
    /// StartCARD2 has run (the VBlank handler schedules operations).
    pub const STARTED: u32 = 0x3B38;
    /// A command is on the wire.
    pub const ACTIVE: u32 = 0x3B3C;
    /// Slot the VBlank handler looked at last (flips every VBlank).
    pub const PORT: u32 = 0x3B40;
    /// Slot of the last completed or failed operation.
    pub const LAST_PORT: u32 = 0x3B44;
    /// The current operation already reported its error.
    pub const GOT_ERROR: u32 = 0x3B48;
    /// Byte step of the current command.
    pub const STEP: u32 = 0x3B4C;
    /// `_new_card`: ignore the card-changed flag on the next command.
    pub const SKIP_NEW_CARD: u32 = 0x3B50;
    /// InitCARD2 ran before (its return value).
    pub const INITIALISED: u32 = 0x3B54;
    /// Data bytes moved in the current sector's data phase.
    pub const DATA_COUNT: u32 = 0x3B58;
    /// Nonzero while the data phase runs (retail: the "fast track").
    pub const DATA_PHASE: u32 = 0x3B5C;
    /// Per-slot backup-unit operation ([`super::bu_op`]).
    pub const BU_OP: u32 = 0x3B60;
    /// Per-slot `_card_load` state.
    pub const BU_STATE: u32 = 0x3B68;
    /// Per-slot `_card_load` sector index.
    pub const BU_INDEX: u32 = 0x3B70;
    /// `_card_auto` flag.
    pub const AUTO_FORMAT: u32 = 0x3B78;
    /// Set by the completion callback; cleared by the synchronous waits.
    pub const SUCCESS: u32 = 0x3B7C;
    /// Error callbacks 1..3 (and a general error), 4 words.
    pub const ERRORS: u32 = 0x3B80;
    /// `_bu_init` progress: phase, slot, index.
    pub const INIT_PHASE: u32 = 0x3B90;
    /// `_bu_init` slot.
    pub const INIT_PORT: u32 = 0x3B94;
    /// `_bu_init` sector index.
    pub const INIT_INDEX: u32 = 0x3B98;
}

/// Per-slot 80h-byte sector buffers of the backup-unit layer.
pub const BU_BUFFER: u32 = 0x3C00;
/// Per-slot directory cache: 15 entries of [`DIR_ENTRY_SIZE`] bytes (the
/// first 20h bytes of each directory frame).
pub const DIRECTORY: u32 = 0x3D00;
/// Bytes cached per directory entry.
pub const DIR_ENTRY_SIZE: u32 = 0x20;
/// Per-slot broken-sector list: 20 words (-1 = unused).
pub const BROKEN: u32 = 0x40C0;

/// Chain element of the card IRQ handler (priority 1).
pub const HI_CARD: u32 = 0x0000_3170;

/// Kernel-internal trap functions of the card driver.
pub mod internal {
    /// IRQ7 verifier of the card handler.
    pub const VERIFIER: u8 = 0x32;
    /// Card handler: one byte of the current command per IRQ7.
    pub const HANDLER: u8 = 0x33;
    /// Data byte from the early card IRQ routine, then return from the
    /// exception.
    pub const FAST: u8 = 0x34;
}

/// Low-level hardware event class (psx-spx "HwCARD").
pub const EVENT_CARD: u32 = 0xF000_0011;
/// Backup-unit event class (psx-spx "SwCARD").
pub const EVENT_BU: u32 = 0xF400_0001;

/// Operations of the byte engine.
pub mod op {
    /// Read a sector.
    pub const READ: u32 = 1;
    /// Write a sector.
    pub const WRITE: u32 = 2;
    /// Status probe (`_card_info`).
    pub const INFO: u32 = 3;
}

/// Backup-unit operations waiting on the byte engine.
pub mod bu_op {
    /// None.
    pub const NONE: u32 = 0;
    /// `_card_info`.
    pub const INFO: u32 = 1;
    /// `_card_load` (directory and broken-sector list).
    pub const LOAD: u32 = 4;
}

const I_STAT: u32 = 0x1F80_1070;
const I_MASK: u32 = 0x1F80_1074;
const SIO_DATA: u32 = 0x1F80_1040;
const SIO_MODE: u32 = 0x1F80_1048;
const SIO_CTRL: u32 = 0x1F80_104A;
const SIO_BAUD: u32 = 0x1F80_104E;
const IRQ_CONTROLLER: u32 = 1 << 7;
const CTRL_TXEN: u16 = 1 << 0;
const CTRL_DTR: u16 = 1 << 1;
const CTRL_ACK: u16 = 1 << 4;
const CTRL_RESET: u16 = 1 << 6;
const CTRL_ACK_IRQ: u16 = 1 << 12;
const CTRL_PORT2: u16 = 1 << 13;

/// `_patch_card_info` bit in the kernel patch flags.
const PATCH_CARD_INFO: u32 = 1 << 0;

fn slot_of(device: u32) -> u32 {
    // Device ids 00h..0Fh are slot 1, 10h..1Fh slot 2 (retail rounds
    // negative ids toward zero first).
    let d = device as i32;
    let d = if d < 0 { d + 15 } else { d };
    ((d >> 4) & 1) as u32
}

fn var(base: u32, slot: u32) -> u32 {
    base + 4 * (slot & 1)
}

// ------------------------------------------------------------------ setup

/// Chain element and initial slot state (both slots ready).
pub fn install(bus: &mut Bus) {
    poke32(bus, HI_CARD, 0);
    poke32(bus, HI_CARD + 4, stub_addr(3, internal::HANDLER));
    poke32(bus, HI_CARD + 8, stub_addr(3, internal::VERIFIER));
    poke32(bus, HI_CARD + 12, 0);
    for slot in 0..2 {
        poke32(bus, var(kvar::FLAGS, slot), 1);
        for i in 0..20 {
            poke32(bus, BROKEN + 0x50 * slot + 4 * i, u32::MAX);
        }
        clear_directory(bus, slot);
    }
}

/// B(4Ah) InitCARD2(pad_enable): both slots ready, no command active; the
/// pad driver runs alongside only when `pad_enable` is set. Returns
/// whether InitCARD2 ran before.
pub fn init_card(bus: &mut Bus, pad_enable: u32) -> u32 {
    install_early_handler(bus);
    poke32(bus, kvar::ACTIVE, 0);
    poke32(bus, kvar::PORT, 0);
    poke32(bus, var(kvar::FLAGS, 0), 1);
    poke32(bus, var(kvar::FLAGS, 1), 1);
    let before = peek32(bus, kvar::INITIALISED);
    poke32(bus, kvar::INITIALISED, 1);
    poke32(bus, crate::hle_kernel::kvar::PAD_STARTED, pad_enable);
    before
}

/// Exception handler slot 1 (C(06h)+70h): `lui v0; ori v0; jalr v0; nop`
/// calling the early card IRQ routine, as InitCARD2 installs it. Games
/// read the two immediates to find that routine and patch it (psx-spx
/// "early_card_irq_patch"), or overwrite the slot to uninstall it.
fn install_early_handler(bus: &mut Bus) {
    let early = ex::code().card_early;
    let slot = crate::hle_kernel::EXCEPTION_HANDLER + 0x70;
    poke32(bus, slot, 0x3C02_0000 | (early >> 16));
    poke32(bus, slot + 4, 0x3442_0000 | (early & 0xFFFF));
    poke32(bus, slot + 8, 0x0040_F809);
    poke32(bus, slot + 12, 0);
}

/// Early-routine data byte: move it and return the address of the
/// exception return.
pub fn fast(bus: &mut Bus) -> u32 {
    let slot = peek32(bus, kvar::PORT) & 1;
    data_byte(bus, slot);
    ex::code().card_fast_rfe
}

/// B(4Bh) StartCARD2: enqueue the pad/card VBlank handler and let it
/// schedule card commands. Returns 1.
pub fn start_card(bus: &mut Bus) -> u32 {
    crate::hle_pad::enqueue_handler(bus);
    let mask = bus.read32(I_MASK);
    bus.write32(I_MASK, mask | 1);
    poke32(bus, kvar::STARTED, 1);
    1
}

/// B(4Ch) StopCARD2. Returns 1.
pub fn stop_card(bus: &mut Bus) -> u32 {
    ex::change_clear_rcnt(bus, 3, 1);
    ex::deq_int(bus, 2, crate::hle_pad::HI_PAD);
    poke32(bus, kvar::STARTED, 0);
    1
}

/// Queue a command for `device`'s slot. Fails (0) when the slot is busy,
/// or for a sector outside 0..=400h (psx-spx: 400h is accepted).
fn request(bus: &mut Bus, device: u32, sector: u32, buffer: u32, operation: u32) -> u32 {
    let slot = slot_of(device);
    if peek32(bus, var(kvar::FLAGS, slot)) & 1 == 0 {
        return 0;
    }
    if operation != op::INFO && sector > 0x400 {
        return 0;
    }
    poke32(bus, kvar::STEP, 0);
    poke32(bus, var(kvar::DEVICE, slot), device);
    poke32(bus, var(kvar::BUFFER, slot), buffer);
    poke32(bus, var(kvar::SECTOR, slot), sector);
    poke32(bus, var(kvar::OP, slot), operation);
    let busy = match operation {
        op::READ => 2,
        op::WRITE => 4,
        _ => 8,
    };
    poke32(bus, var(kvar::FLAGS, slot), busy);
    1
}

/// B(4Fh) _card_read(device, sector, dst).
pub fn card_read(bus: &mut Bus, device: u32, sector: u32, dst: u32) -> u32 {
    request(bus, device, sector, dst, op::READ)
}

/// B(4Eh) _card_write(device, sector, src).
pub fn card_write(bus: &mut Bus, device: u32, sector: u32, src: u32) -> u32 {
    request(bus, device, sector, src, op::WRITE)
}

/// B(4Dh) _card_info_subfunc(device).
pub fn card_info_internal(bus: &mut Bus, device: u32) -> u32 {
    request(bus, device, 0, 0, op::INFO)
}

/// A(ABh) _card_info(device): a status probe whose result arrives as a
/// SwCARD event. Returns 1 when queued.
pub fn card_info(bus: &mut Bus, device: u32) -> u32 {
    let slot = slot_of(device);
    poke32(bus, var(kvar::BU_OP, slot), bu_op::INFO);
    let ok = card_info_internal(bus, device);
    if ok == 0 {
        poke32(bus, var(kvar::BU_OP, slot), bu_op::NONE);
    }
    u32::from(ok != 0)
}

/// B(50h) _new_card.
pub fn new_card(bus: &mut Bus) {
    poke32(bus, kvar::SKIP_NEW_CARD, 1);
}

/// B(58h) _card_chan: device id of the last finished operation.
pub fn card_chan(bus: &Bus) -> u32 {
    let last = peek32(bus, kvar::LAST_PORT);
    peek32(bus, var(kvar::DEVICE, last))
}

/// B(5Ch) _card_status(slot).
pub fn card_status(bus: &Bus, slot: u32) -> u32 {
    peek32(bus, var(kvar::FLAGS, slot)) & 0xFF
}

/// B(5Dh) _card_wait(slot): `None` while the slot is busy.
pub fn card_wait(bus: &Bus, slot: u32) -> Option<u32> {
    let flags = card_status(bus, slot);
    (flags & 1 != 0).then_some(flags)
}

/// A(ADh) _card_auto(flag): returns the previous setting.
pub fn set_auto_format(bus: &mut Bus, flag: u32) -> u32 {
    let old = peek32(bus, kvar::AUTO_FORMAT);
    poke32(bus, kvar::AUTO_FORMAT, flag);
    old
}

// ------------------------------------------------------------ byte engine

fn slot_ctrl(slot: u32) -> u16 {
    if slot == 0 {
        0
    } else {
        CTRL_PORT2
    }
}

/// Read the byte that arrived for the previous transfer, send `byte`, and
/// acknowledge the port and IRQ7.
fn exchange(bus: &mut Bus, byte: u8) -> u8 {
    let rx = bus.read8(SIO_DATA);
    bus.write8(SIO_DATA, byte);
    let c = bus.read16(SIO_CTRL);
    bus.write16(SIO_CTRL, c | CTRL_ACK);
    bus.write32(I_STAT, !IRQ_CONTROLLER);
    rx
}

/// First step of every command: select the slot and send 81h (plus the
/// multitap sub-address from the device id).
fn select(bus: &mut Bus, slot: u32) {
    bus.write16(
        SIO_CTRL,
        slot_ctrl(slot) | CTRL_TXEN | CTRL_ACK_IRQ | CTRL_DTR,
    );
    let device = peek32(bus, var(kvar::DEVICE, slot));
    exchange(bus, 0x81 + (device & 0x0F) as u8);
    poke32(bus, kvar::ACTIVE, 1);
}

/// Card-changed flag set: stop with a HwCARD 2000h error unless `_new_card`
/// was called.
fn new_card_error(bus: &mut Bus, slot: u32, spec: u32) -> i32 {
    poke32(bus, var(kvar::FLAGS, slot), 1);
    poke32(bus, kvar::LAST_PORT, slot);
    if spec == 0x8000 {
        low_level_error(bus, 0);
    } else {
        low_level_error(bus, 2);
    }
    ex::queue_event(bus, EVENT_CARD, spec);
    poke32(bus, kvar::GOT_ERROR, 1);
    -1
}

/// Step result for a response byte that must be `want`.
fn expect(b: u8, want: u8) -> i32 {
    if b == want {
        0
    } else {
        -1
    }
}

/// One byte of a read command: 81h 52h 00h 00h MSB LSB 00h 00h 00h 00h,
/// then 128 data bytes, checksum and end status 47h (psx-spx "Memory Card
/// Read/Write Commands").
fn read_step(bus: &mut Bus, slot: u32, step: u32) -> i32 {
    let sector = peek32(bus, var(kvar::SECTOR, slot));
    let buffer = peek32(bus, var(kvar::BUFFER, slot));
    match step {
        1 => {
            select(bus, slot);
            0
        }
        2 => {
            exchange(bus, b'R');
            0
        }
        3 => {
            let flag = exchange(bus, 0);
            if peek32(bus, kvar::SKIP_NEW_CARD) != 0 || flag & 0x08 == 0 {
                return 0;
            }
            poke32(bus, kvar::SKIP_NEW_CARD, 0);
            new_card_error(bus, slot, 0x2000)
        }
        4 => expect(exchange(bus, 0), 0x5A),
        5 => expect(exchange(bus, (sector >> 8) as u8), 0x5D),
        6 => {
            exchange(bus, sector as u8);
            0
        }
        7 => {
            exchange(bus, 0);
            0
        }
        8 => expect(exchange(bus, 0), 0x5C),
        9 => expect(exchange(bus, 0), 0x5D),
        10 => {
            if exchange(bus, 0) != (sector >> 8) as u8 {
                return -1;
            }
            poke32(
                bus,
                var(kvar::CHECKSUM, slot),
                (sector ^ (sector >> 8)) & 0xFF,
            );
            0
        }
        11 => {
            if exchange(bus, 0) != sector as u8 {
                return -1;
            }
            poke32(bus, kvar::DATA_COUNT, 0);
            poke32(bus, kvar::DATA_PHASE, 1);
            0
        }
        12 => {
            let b = exchange(bus, 0);
            bus.write8_safe(buffer.wrapping_add(0x7F), b);
            let sum = peek32(bus, var(kvar::CHECKSUM, slot)) ^ u32::from(b);
            poke32(bus, var(kvar::CHECKSUM, slot), sum);
            0
        }
        13 => {
            let b = exchange(bus, 0);
            if u32::from(b) != peek32(bus, var(kvar::CHECKSUM, slot)) {
                return -1;
            }
            let end = bus.read8(SIO_DATA);
            if end == 0x47 {
                1
            } else {
                -1
            }
        }
        _ => -1,
    }
}

/// One byte of a write command: 81h 57h 00h 00h MSB LSB, 128 data bytes,
/// checksum, 00h 00h 00h (responses 5Ch 5Dh and end status 47h).
fn write_step(bus: &mut Bus, slot: u32, step: u32) -> i32 {
    let sector = peek32(bus, var(kvar::SECTOR, slot));
    match step {
        1 => {
            select(bus, slot);
            0
        }
        2 => {
            exchange(bus, b'W');
            0
        }
        3 => {
            let flag = exchange(bus, 0);
            poke32(bus, var(kvar::FLAG_BYTE, slot), u32::from(flag));
            if peek32(bus, kvar::SKIP_NEW_CARD) != 0 || flag & 0x08 == 0 {
                return 0;
            }
            poke32(bus, kvar::SKIP_NEW_CARD, 0);
            new_card_error(bus, slot, 0x2000)
        }
        4 => expect(exchange(bus, 0), 0x5A),
        5 => {
            if exchange(bus, (sector >> 8) as u8) != 0x5D {
                return -1;
            }
            poke32(bus, var(kvar::CHECKSUM, slot), (sector >> 8) & 0xFF);
            0
        }
        6 => {
            exchange(bus, sector as u8);
            let sum = peek32(bus, var(kvar::CHECKSUM, slot)) ^ (sector & 0xFF);
            poke32(bus, var(kvar::CHECKSUM, slot), sum);
            poke32(bus, kvar::DATA_COUNT, 0);
            poke32(bus, kvar::DATA_PHASE, 1);
            0
        }
        7 => {
            let sum = peek32(bus, var(kvar::CHECKSUM, slot)) as u8;
            exchange(bus, sum);
            0
        }
        8 => {
            exchange(bus, 0);
            0
        }
        9 => expect(exchange(bus, 0), 0x5C),
        10 => {
            if exchange(bus, 0) != 0x5D {
                return -1;
            }
            let flag = peek32(bus, var(kvar::FLAG_BYTE, slot));
            if peek32(bus, kvar::SKIP_NEW_CARD) == 0 && flag & 0x04 != 0 {
                poke32(bus, kvar::LAST_PORT, slot);
                poke32(bus, var(kvar::FLAGS, slot), 1);
                low_level_error(bus, 2);
                ex::queue_event(bus, EVENT_CARD, 0x8001);
                poke32(bus, kvar::GOT_ERROR, 1);
            }
            let end = bus.read8(SIO_DATA);
            if end == 0x47 {
                1
            } else {
                -1
            }
        }
        _ => -1,
    }
}

/// `_card_info`: 81h 52h 00h, then the FLAG and first ID byte only.
fn info_step(bus: &mut Bus, slot: u32, step: u32) -> i32 {
    match step {
        1 => {
            select(bus, slot);
            0
        }
        2 => {
            exchange(bus, b'R');
            0
        }
        3 => {
            let flag = exchange(bus, 0);
            if peek32(bus, kvar::SKIP_NEW_CARD) != 0 || flag & 0x0C == 0 {
                return 0;
            }
            poke32(bus, kvar::SKIP_NEW_CARD, 0);
            new_card_error(bus, slot, if flag & 0x04 != 0 { 0x8000 } else { 0x2000 })
        }
        4 => {
            let b = bus.read8(SIO_DATA);
            // The retail function sends one byte too many here (psx-spx
            // "patch_card_info_step4"); `_patch_card_info` removes it.
            let patches = peek32(bus, crate::hle_kernel::kvar::PATCH_FLAGS);
            if patches & PATCH_CARD_INFO == 0 {
                bus.write8(SIO_DATA, 0);
            }
            let c = bus.read16(SIO_CTRL);
            bus.write16(SIO_CTRL, c | CTRL_ACK);
            bus.write32(I_STAT, !IRQ_CONTROLLER);
            if b == 0x5A {
                1
            } else {
                -1
            }
        }
        _ => -1,
    }
}

/// Data phase: one of the 128 data bytes per IRQ7 (the retail kernel
/// moves these in a short path at the top of its exception handler).
fn data_byte(bus: &mut Bus, slot: u32) {
    let buffer = peek32(bus, var(kvar::BUFFER, slot));
    let count = peek32(bus, kvar::DATA_COUNT);
    let (b, last) = if peek32(bus, var(kvar::OP, slot)) == op::WRITE {
        let _ = bus.read8(SIO_DATA);
        let b = bus.try_read8(buffer.wrapping_add(count)).unwrap_or(0);
        bus.write8(SIO_DATA, b);
        (b, count + 1 > 0x7F)
    } else {
        let b = bus.read8(SIO_DATA);
        bus.write8_safe(buffer.wrapping_add(count), b);
        bus.write8(SIO_DATA, 0);
        (b, count + 1 > 0x7E)
    };
    let c = bus.read16(SIO_CTRL);
    bus.write16(SIO_CTRL, c | CTRL_ACK);
    bus.write32(I_STAT, !IRQ_CONTROLLER);
    let sum = peek32(bus, var(kvar::CHECKSUM, slot)) ^ u32::from(b);
    poke32(bus, var(kvar::CHECKSUM, slot), sum);
    poke32(bus, kvar::DATA_COUNT, count + 1);
    if last {
        poke32(bus, kvar::DATA_PHASE, 0);
    }
}

/// Card IRQ verifier: IRQ7 enabled and pending.
pub fn verifier(bus: &mut Bus) -> u32 {
    let pending = bus.read32(I_STAT) & bus.read32(I_MASK) & IRQ_CONTROLLER;
    u32::from(pending != 0)
}

/// Card IRQ handler: advance the current command by one byte.
pub fn handler(bus: &mut Bus) {
    let slot = peek32(bus, kvar::PORT) & 1;
    if peek32(bus, kvar::DATA_PHASE) != 0 {
        data_byte(bus, slot);
        return;
    }
    let c = bus.read16(SIO_CTRL);
    bus.write16(SIO_CTRL, c | slot_ctrl(slot) | CTRL_ACK | CTRL_DTR);
    run_step(bus, slot);
}

fn run_step(bus: &mut Bus, slot: u32) {
    let step = peek32(bus, kvar::STEP) + 1;
    poke32(bus, kvar::STEP, step);
    let result = match peek32(bus, var(kvar::OP, slot)) {
        op::READ => read_step(bus, slot, step),
        op::WRITE => write_step(bus, slot, step),
        _ => info_step(bus, slot, step),
    };
    match result {
        0 => {
            bus.write32(I_STAT, !IRQ_CONTROLLER);
            let mask = bus.read32(I_MASK);
            bus.write32(I_MASK, mask | IRQ_CONTROLLER);
        }
        1 => {
            finish(bus);
            if peek32(bus, kvar::GOT_ERROR) != 0 {
                return;
            }
            poke32(bus, kvar::SKIP_NEW_CARD, 0);
            poke32(bus, var(kvar::FLAGS, slot), 1);
            poke32(bus, kvar::LAST_PORT, slot);
            low_level_completed(bus);
            ex::queue_event(bus, EVENT_CARD, 0x0004);
        }
        _ => {
            poke32(bus, kvar::ACTIVE, 0);
            bus.write16(SIO_CTRL, 0);
            if peek32(bus, kvar::GOT_ERROR) == 0 {
                poke32(bus, kvar::SKIP_NEW_CARD, 0);
                poke32(bus, var(kvar::FLAGS, slot), 0x21);
                poke32(bus, kvar::LAST_PORT, slot);
                low_level_error(bus, 0);
                ex::queue_event(bus, EVENT_CARD, 0x8000);
            }
            finish(bus);
            poke32(bus, kvar::GOT_ERROR, 0);
        }
    }
}

/// Command over: idle the port and take the handler off IRQ7.
fn finish(bus: &mut Bus) {
    poke32(bus, kvar::ACTIVE, 0);
    poke32(bus, kvar::DATA_PHASE, 0);
    bus.write16(SIO_CTRL, 0);
    poke32(bus, kvar::STEP, 0);
    ex::deq_int(bus, 1, HI_CARD);
    bus.write32(I_STAT, !IRQ_CONTROLLER);
    let mask = bus.read32(I_MASK);
    bus.write32(I_MASK, mask & !IRQ_CONTROLLER);
}

/// VBlank part of the pad/card handler: a command still on the wire from
/// the last frame timed out (status 11h, HwCARD 100h); otherwise flip to
/// the other slot and start its queued command, if any.
pub fn vblank(bus: &mut Bus) {
    for spec in [0x0004, 0x8000, 0x0100, 0x0200, 0x2000] {
        ex::undeliver_event(bus, EVENT_CARD, spec);
    }
    if peek32(bus, kvar::ACTIVE) != 0 {
        let slot = peek32(bus, kvar::PORT) & 1;
        poke32(bus, kvar::ACTIVE, 0);
        poke32(bus, kvar::STEP, 0);
        poke32(bus, kvar::DATA_PHASE, 0);
        bus.write32(I_STAT, !IRQ_CONTROLLER);
        let mask = bus.read32(I_MASK);
        bus.write32(I_MASK, mask & !IRQ_CONTROLLER);
        bus.write16(SIO_CTRL, 0);
        poke32(bus, kvar::SKIP_NEW_CARD, 0);
        poke32(bus, var(kvar::FLAGS, slot), 0x11);
        poke32(bus, kvar::LAST_PORT, slot);
        low_level_error(bus, 1);
        ex::queue_event(bus, EVENT_CARD, 0x0100);
        ex::deq_int(bus, 1, HI_CARD);
        bus.write16(SIO_CTRL, CTRL_RESET);
        bus.write16(SIO_BAUD, 0x88);
        bus.write16(SIO_MODE, 0x0D);
        bus.write16(SIO_CTRL, 0);
        return;
    }
    let slot = 1 - (peek32(bus, kvar::PORT) & 1);
    poke32(bus, kvar::PORT, slot);
    if peek32(bus, var(kvar::FLAGS, slot)) & 1 != 0 {
        return;
    }
    poke32(bus, kvar::DATA_PHASE, 0);
    poke32(bus, kvar::DATA_COUNT, 0);
    ex::deq_int(bus, 1, HI_CARD);
    ex::enq_int(bus, 1, HI_CARD);
    poke32(bus, kvar::STEP, 0);
    poke32(bus, kvar::GOT_ERROR, 0);
    let c = bus.read16(SIO_CTRL);
    bus.write16(SIO_CTRL, c | slot_ctrl(slot) | CTRL_ACK | CTRL_DTR);
    run_step(bus, slot);
}

// ------------------------------------------------ backup-unit callbacks

fn clear_directory(bus: &mut Bus, slot: u32) {
    for i in 0..15 {
        let e = dir_entry(slot, i);
        for k in 0..DIR_ENTRY_SIZE {
            bus.write8_safe(e + k, 0);
        }
        poke32(bus, e, 0xA0);
        poke32(bus, e + 8, 0xFFFF);
    }
}

/// Directory cache entry `index` (0..15) of `slot`.
pub fn dir_entry(slot: u32, index: u32) -> u32 {
    DIRECTORY + (slot & 1) * 15 * DIR_ENTRY_SIZE + index * DIR_ENTRY_SIZE
}

fn bu_buffer(slot: u32) -> u32 {
    BU_BUFFER + 0x80 * (slot & 1)
}

fn sector_checksum_ok(bus: &Bus, buf: u32) -> bool {
    let sum = (0..0x7F).fold(0u8, |acc, k| acc ^ bus.try_read8(buf + k).unwrap_or(0));
    bus.try_read8(buf + 0x7F).unwrap_or(0) == sum
}

/// End the slot's backup-unit operation with SwCARD `spec`.
pub(crate) fn bu_finish(bus: &mut Bus, slot: u32, spec: u32) {
    poke32(bus, var(kvar::BU_OP, slot), bu_op::NONE);
    poke32(bus, var(kvar::BU_STATE, slot), 0);
    poke32(bus, var(kvar::BU_INDEX, slot), 0);
    ex::queue_event(bus, EVENT_BU, spec);
}

/// A(A7h) bufs_cb_0: a low-level command finished; advance the
/// backup-unit operation waiting on it.
pub fn low_level_completed(bus: &mut Bus) {
    poke32(bus, kvar::SUCCESS, 1);
    let device = card_chan(bus);
    let slot = slot_of(device);
    let buf = bu_buffer(slot);
    match peek32(bus, var(kvar::BU_OP, slot)) {
        bu_op::INFO => bu_finish(bus, slot, 0x0004),
        bu_op::LOAD => load_step(bus, slot, device, buf),
        op @ (crate::hle_bu::bu_op::READ
        | crate::hle_bu::bu_op::WRITE
        | crate::hle_bu::bu_op::WRITE_INFO) => {
            crate::hle_bu::async_completed(bus, slot, device, op)
        }
        _ => {}
    }
}

fn load_fail(bus: &mut Bus, slot: u32, error: u32, spec: u32) {
    poke32(bus, kvar::SUCCESS, 0);
    poke32(bus, kvar::ERRORS + 4 * error, 1);
    bu_finish(bus, slot, spec);
}

/// `_card_load` progress after each sector: sector 0 must start with
/// "MC", then directory frames 1..15 and broken-sector frames 16..35.
fn load_step(bus: &mut Bus, slot: u32, device: u32, buf: u32) {
    let index = peek32(bus, var(kvar::BU_INDEX, slot));
    match peek32(bus, var(kvar::BU_STATE, slot)) {
        1 => {
            if bus.try_read8(buf) != Some(b'M') || bus.try_read8(buf + 1) != Some(b'C') {
                load_fail(bus, slot, 2, 0x2000);
                return;
            }
            clear_directory(bus, slot);
            if card_read(bus, device, 1, buf) == 0 {
                load_fail(bus, slot, 0, 0x8000);
                return;
            }
            poke32(bus, var(kvar::BU_INDEX, slot), 0);
            poke32(bus, var(kvar::BU_STATE, slot), 2);
        }
        2 => {
            if sector_checksum_ok(bus, buf) {
                copy_dir_frame(bus, slot, index, buf);
            }
            if index + 1 < 15 {
                poke32(bus, var(kvar::BU_INDEX, slot), index + 1);
                if card_read(bus, device, index + 2, buf) == 0 {
                    load_fail(bus, slot, 0, 0x8000);
                }
                return;
            }
            validate_directory(bus, slot);
            for i in 0..20 {
                poke32(bus, BROKEN + 0x50 * slot + 4 * i, u32::MAX);
            }
            poke32(bus, var(kvar::BU_INDEX, slot), 0);
            poke32(bus, var(kvar::BU_STATE, slot), 3);
            if card_read(bus, device, 16, buf) == 0 {
                bu_finish(bus, slot, 0x8000);
            }
        }
        3 => {
            if sector_checksum_ok(bus, buf) {
                let word = peek32(bus, buf);
                poke32(bus, BROKEN + 0x50 * slot + 4 * index, word);
            }
            if index + 1 < 20 {
                poke32(bus, var(kvar::BU_INDEX, slot), index + 1);
                if card_read(bus, device, 17 + index, buf) == 0 {
                    load_fail(bus, slot, 0, 0x8000);
                }
                return;
            }
            bu_finish(bus, slot, 0x0004);
        }
        _ => load_fail(bus, slot, 0, 0x8000),
    }
}

fn copy_dir_frame(bus: &mut Bus, slot: u32, index: u32, buf: u32) {
    let e = dir_entry(slot, index);
    for k in 0..DIR_ENTRY_SIZE {
        let b = bus.try_read8(buf + k).unwrap_or(0);
        bus.write8_safe(e + k, b);
    }
}

/// Free every block that is neither a file's first block (51h) nor
/// reached by a chain from one (psx-spx "Memory Card Data Format").
fn validate_directory(bus: &mut Bus, slot: u32) {
    let mut used = [0u32; 15];
    for i in 0..15 {
        let e = dir_entry(slot, i);
        if peek32(bus, e) != 0x51 {
            continue;
        }
        used[i as usize] = 1;
        let size = peek32(bus, e + 4) as i32;
        let mut blocks = size.max(0) >> 13;
        let mut next = (peek32(bus, e + 8) & 0xFFFF) as u16;
        loop {
            blocks -= 1;
            if blocks <= 0 || next == 0xFFFF || next >= 15 {
                break;
            }
            used[usize::from(next)] += 1;
            next = (peek32(bus, dir_entry(slot, u32::from(next)) + 8) & 0xFFFF) as u16;
        }
    }
    for i in 0..15 {
        if used[i as usize] == 0 {
            let e = dir_entry(slot, i);
            poke32(bus, e, 0xA0);
            poke32(bus, e + 4, 0);
            let w = peek32(bus, e + 8);
            poke32(bus, e + 8, (w & 0xFFFF_0000) | 0xFFFF);
        }
    }
}

/// A(A8h) bufs_cb_1 / A(A9h) bufs_cb_2 / A(AAh) bufs_cb_3: a low-level
/// command failed (general error, timeout, card changed or unformatted).
/// Ends the waiting backup-unit operation with SwCARD 8000h, 100h or
/// 2000h.
pub fn low_level_error(bus: &mut Bus, which: u32) {
    poke32(bus, kvar::ERRORS + 4 * (which & 3), 1);
    let slot = slot_of(card_chan(bus));
    if peek32(bus, var(kvar::BU_OP, slot)) != bu_op::NONE {
        let spec = [0x8000, 0x0100, 0x2000, 0x8000][(which & 3) as usize];
        bu_finish(bus, slot, spec);
    }
}

/// A(ACh) _card_load(device): read the directory and broken-sector list
/// into the kernel's cache in the background; SwCARD 4 when done.
pub fn card_load(bus: &mut Bus, device: u32) -> u32 {
    let slot = slot_of(device);
    poke32(bus, var(kvar::BU_OP, slot), bu_op::LOAD);
    if card_read(bus, device, 0, bu_buffer(slot)) != 0 {
        poke32(bus, var(kvar::BU_STATE, slot), 1);
        1
    } else {
        poke32(bus, var(kvar::BU_OP, slot), bu_op::NONE);
        0
    }
}

/// Clear the synchronous-wait status (OpenBIOS mcResetStatus).
fn reset_status(bus: &mut Bus) {
    poke32(bus, kvar::SUCCESS, 0);
    for i in 0..4 {
        poke32(bus, kvar::ERRORS + 4 * i, 0);
    }
    for spec in [0x0004, 0x8000, 0x2000, 0x0100] {
        ex::undeliver_event(bus, EVENT_BU, spec);
    }
}

/// Synchronous wait: `Some(true)` on success, `Some(false)` on an error,
/// `None` while the command runs.
fn wait_status(bus: &mut Bus) -> Option<bool> {
    if peek32(bus, kvar::SUCCESS) != 0 {
        reset_status(bus);
        return Some(true);
    }
    if (0..4).any(|i| peek32(bus, kvar::ERRORS + 4 * i) != 0) {
        reset_status(bus);
        return Some(false);
    }
    None
}

/// A(55h)/A(70h) _bu_init: reset the backup-unit state and load both
/// slots' directory synchronously (sector 0 "MC" check, a write to sector
/// 3Fh to clear the card-changed flag, directory and broken-sector
/// frames). `None` while waiting; the call is retried.
pub fn bu_init(bus: &mut Bus) -> Option<u32> {
    use init_phase::*;
    for _ in 0..8 {
        let phase = peek32(bus, kvar::INIT_PHASE);
        let slot = peek32(bus, kvar::INIT_PORT);
        let index = peek32(bus, kvar::INIT_INDEX);
        let device = slot << 4;
        let buf = bu_buffer(slot);
        let fail = |bus: &mut Bus| {
            for i in 0..15 {
                let e = dir_entry(slot, i);
                for k in 0..DIR_ENTRY_SIZE {
                    bus.write8_safe(e + k, 0);
                }
            }
            for i in 0..20 {
                poke32(bus, BROKEN + 0x50 * slot + 4 * i, u32::MAX);
            }
            next_slot(bus);
        };
        match phase {
            START => {
                if slot == 0 {
                    for s in 0..2 {
                        poke32(bus, var(kvar::BU_OP, s), bu_op::NONE);
                        clear_directory(bus, s);
                    }
                    poke32(bus, kvar::AUTO_FORMAT, 0);
                    reset_status(bus);
                }
                new_card(bus);
                if card_read(bus, device, 0, buf) == 0 {
                    fail(bus);
                    continue;
                }
                poke32(bus, kvar::INIT_PHASE, READ0);
                return None;
            }
            READ0 => {
                let ok = wait_status(bus)?;
                let mc = bus.try_read8(buf) == Some(b'M') && bus.try_read8(buf + 1) == Some(b'C');
                if !ok || !mc {
                    // Auto-format is off after _bu_init resets it.
                    fail(bus);
                    continue;
                }
                new_card(bus);
                card_write(bus, device, 0x3F, buf);
                poke32(bus, kvar::INIT_PHASE, WRITE3F);
                return None;
            }
            WRITE3F => {
                wait_status(bus)?;
                clear_directory(bus, slot);
                if card_read(bus, device, 1, buf) == 0 {
                    fail(bus);
                    continue;
                }
                poke32(bus, kvar::INIT_INDEX, 0);
                poke32(bus, kvar::INIT_PHASE, DIR);
                return None;
            }
            DIR => {
                let ok = wait_status(bus)?;
                if !ok || !sector_checksum_ok(bus, buf) {
                    fail(bus);
                    continue;
                }
                copy_dir_frame(bus, slot, index, buf);
                let (sector, phase) = if index + 1 < 15 {
                    (index + 2, DIR)
                } else {
                    validate_directory(bus, slot);
                    (16, BROKEN_LIST)
                };
                let next = if phase == DIR { index + 1 } else { 0 };
                if card_read(bus, device, sector, buf) == 0 {
                    fail(bus);
                    continue;
                }
                poke32(bus, kvar::INIT_INDEX, next);
                poke32(bus, kvar::INIT_PHASE, phase);
                return None;
            }
            BROKEN_LIST => {
                let ok = wait_status(bus)?;
                if !ok || !sector_checksum_ok(bus, buf) {
                    fail(bus);
                    continue;
                }
                let word = peek32(bus, buf);
                poke32(bus, BROKEN + 0x50 * slot + 4 * index, word);
                if index + 1 < 20 {
                    if card_read(bus, device, 17 + index, buf) == 0 {
                        fail(bus);
                        continue;
                    }
                    poke32(bus, kvar::INIT_INDEX, index + 1);
                    return None;
                }
                next_slot(bus);
            }
            _ => {
                poke32(bus, kvar::INIT_PHASE, START);
                poke32(bus, kvar::INIT_PORT, 0);
                poke32(bus, kvar::INIT_INDEX, 0);
                return Some(0);
            }
        }
    }
    None
}

/// `_bu_init` phases.
mod init_phase {
    pub const START: u32 = 0;
    pub const READ0: u32 = 1;
    pub const WRITE3F: u32 = 2;
    pub const DIR: u32 = 3;
    pub const BROKEN_LIST: u32 = 4;
    pub const DONE: u32 = 5;
}

fn next_slot(bus: &mut Bus) {
    let slot = peek32(bus, kvar::INIT_PORT);
    poke32(bus, kvar::INIT_INDEX, 0);
    if slot == 0 {
        poke32(bus, kvar::INIT_PORT, 1);
        poke32(bus, kvar::INIT_PHASE, init_phase::START);
    } else {
        poke32(bus, kvar::INIT_PHASE, init_phase::DONE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hle_asm::*;
    use crate::Cpu;

    const PROGRAM: u32 = 0x8001_0000;
    const RESULTS: u32 = 0x8002_0F00;

    /// Guest program: interrupts on (IEc, IM2), then table calls with
    /// immediate arguments; call `i`'s v0 is stored at RESULTS + 4i. Ends
    /// in a loop.
    fn program(calls: &[(i16, u32, [u32; 4])]) -> Vec<u32> {
        let mut a = Asm::new(PROGRAM);
        a.li(T0, 0x0000_0401);
        a.mtc0(T0, 12);
        a.li(S0, RESULTS);
        a.li(S1, RESULTS);
        for (vector, func, args) in calls {
            for (i, v) in args.iter().enumerate() {
                if *v == FIRST_RESULT {
                    a.lw(A0 + i as u32, 0, S1);
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
        a.label("end");
        a.b("end");
        a.nop();
        a.finish()
    }

    fn hle_bus_with_card() -> Bus {
        let mut bus = Bus::new_without_bios();
        bus.enable_hle_bios();
        bus.attach_digital_pad_port1();
        bus.attach_memcard_port1(Vec::new());
        bus
    }

    /// Run until the program reaches its final loop, then `extra` more
    /// VBlanks.
    fn run(bus: &mut Bus, words: &[u32], extra: u64) {
        let mut cpu = Cpu::new();
        for (i, w) in words.iter().enumerate() {
            bus.write32(PROGRAM + 4 * i as u32, *w);
        }
        cpu.gprs_mut_for_test()[29] = 0x801F_FF00;
        cpu.set_pc_for_test(PROGRAM);
        let end = PROGRAM + 4 * (words.len() as u32 - 2);
        let mut target = None;
        for _ in 0..400_000_000u32 {
            let vblanks = bus.irq().raise_counts()[0];
            if cpu.pc() == end && target.is_none() {
                target = Some(vblanks + extra);
            }
            if target.is_some_and(|t| vblanks >= t) {
                return;
            }
            cpu.step(bus).unwrap();
        }
        panic!("program did not finish, pc={:#x}", cpu.pc());
    }

    fn result(bus: &mut Bus, i: u32) -> u32 {
        bus.read32(RESULTS + 4 * i)
    }

    /// Argument placeholder: the first call's result.
    const FIRST_RESULT: u32 = 0xFFFF_FFF0;
    const B: i16 = 0xB0;
    const A: i16 = 0xA0;

    #[test]
    fn card_read_and_write_go_through_the_card_with_hwcard_events() {
        let mut bus = hle_bus_with_card();
        let (src, dst) = (0x8003_0000, 0x8003_0100);
        for k in 0..0x80 {
            bus.write8_safe(src + k, (k * 3) as u8);
        }
        let words = program(&[
            (B, 0x08, [EVENT_CARD, 4, 0x2000, 0]),
            (B, 0x4A, [1, 0, 0, 0]),
            (B, 0x4B, [0; 4]),
            (B, 0x50, [0; 4]),
            (B, 0x4E, [0x00, 0x40, src, 0]),
            (B, 0x5D, [0, 0, 0, 0]),
            (B, 0x4F, [0x00, 0x40, dst, 0]),
            (B, 0x5D, [0, 0, 0, 0]),
            (B, 0x4F, [0x00, 0x00, dst + 0x80, 0]),
            (B, 0x5D, [0, 0, 0, 0]),
        ]);
        run(&mut bus, &words, 1);
        let event = result(&mut bus, 0);
        assert_eq!(event >> 24, 0xF1);
        // InitCARD2 had not run before; the writes and reads were queued
        // and each _card_wait saw "ready".
        assert_eq!(result(&mut bus, 1), 0);
        assert_eq!([result(&mut bus, 4), result(&mut bus, 6)], [1, 1]);
        assert_eq!([result(&mut bus, 5), result(&mut bus, 7)], [1, 1]);
        let back: Vec<u8> = (0..0x80).map(|k| bus.try_read8(dst + k).unwrap()).collect();
        let want: Vec<u8> = (0..0x80u32).map(|k| (k * 3) as u8).collect();
        assert_eq!(back, want);
        // Sector 0 of a formatted card starts with "MC".
        assert_eq!(bus.try_read8(dst + 0x80), Some(b'M'));
        assert_eq!(bus.try_read8(dst + 0x81), Some(b'C'));
        // HwCARD 4 was delivered to the mark-ready event.
        let e = peek32(
            &bus,
            peek32(&bus, crate::hle_kernel::TOT + 0x20) + 0x1C * (event & 0xFFFF) + 4,
        );
        assert_eq!(
            e,
            crate::hle_exceptions::EV_DISABLED,
            "not enabled, so never ready"
        );
    }

    #[test]
    fn a_new_card_fails_reads_until_new_card_is_called() {
        let mut bus = hle_bus_with_card();
        let dst = 0x8003_0000;
        let words = program(&[
            (B, 0x4A, [1, 0, 0, 0]),
            (B, 0x4B, [0; 4]),
            (B, 0x4F, [0x00, 0x00, dst, 0]),
            (B, 0x5D, [0, 0, 0, 0]),
        ]);
        run(&mut bus, &words, 1);
        // The card-changed error leaves the slot ready (status 01h), with
        // the data untouched.
        assert_eq!(result(&mut bus, 3), 1);
        assert_eq!(bus.try_read8(dst), Some(0));
        assert_eq!(peek32(&bus, kvar::ERRORS + 8), 1, "bufs_cb_3 ran");
    }

    #[test]
    fn an_empty_slot_times_out_with_status_11h() {
        let mut bus = hle_bus_with_card();
        bus.detach_memcard_port2();
        let words = program(&[
            (B, 0x4A, [1, 0, 0, 0]),
            (B, 0x4B, [0; 4]),
            (B, 0x50, [0; 4]),
            (B, 0x4F, [0x10, 0x00, 0x8003_0000, 0]),
            (B, 0x5D, [1, 0, 0, 0]),
        ]);
        run(&mut bus, &words, 1);
        assert_eq!(result(&mut bus, 4), 0x11);
    }

    #[test]
    fn bu_init_loads_the_directory_and_card_info_reports_swcard() {
        let mut bus = hle_bus_with_card();
        bus.detach_memcard_port2();
        let words = program(&[
            (B, 0x08, [EVENT_BU, 4, 0x2000, 0]),
            (B, 0x0C, [FIRST_RESULT, 0, 0, 0]),
            (B, 0x4A, [1, 0, 0, 0]),
            (B, 0x4B, [0; 4]),
            (A, 0x70, [0; 4]),
            (A, 0xAB, [0x00, 0, 0, 0]),
            (B, 0x5D, [0, 0, 0, 0]),
            (B, 0x0B, [FIRST_RESULT, 0, 0, 0]),
        ]);
        run(&mut bus, &words, 1);
        assert_eq!(result(&mut bus, 0) >> 24, 0xF1);
        assert_eq!(result(&mut bus, 5), 1, "_card_info queued");
        assert_eq!(result(&mut bus, 7), 1, "SwCARD 4 delivered");
        // Slot 1: formatted card, every block free (A0h) with no chain.
        for i in 0..15 {
            assert_eq!(peek32(&bus, dir_entry(0, i)), 0xA0);
            assert_eq!(peek32(&bus, dir_entry(0, i) + 8) & 0xFFFF, 0xFFFF);
        }
        // Slot 2 is empty: its cache is cleared.
        assert_eq!(peek32(&bus, dir_entry(1, 0)), 0);
        assert_eq!(peek32(&bus, BROKEN + 0x50), u32::MAX);
    }
}
