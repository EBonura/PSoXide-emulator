// SPDX-License-Identifier: GPL-2.0-or-later
//! The memory card file device ("bu00:", "bu10:") of the HLE kernel:
//! open (existing or new files), read, write, close, firstfile/nextfile,
//! erase, rename and format, over the directory cache and sector commands
//! of [`crate::hle_card`].
//!
//! Behaviour follows psx-spx "BIOS Memory Card Functions", "BIOS File
//! Functions" and "Memory Card Data Format", with OpenBIOS `card/device.c`
//! and `card/backupunit.c` (pcsx-redux, MIT) as the specification for the
//! order of sector writes (inner directory frames before first frames),
//! the synchronous and asynchronous (open mode bit 15) paths, error codes
//! and events. Where OpenBIOS documents a retail bug that would hang or
//! corrupt data (the endless loop after a failed allocation, the broken
//! reallocation retry), the failure is reported instead.
//!
//! Driver functions run as kernel traps retried while a sector command is
//! on the wire, with their progress in kernel RAM ([`kvar`]).

use crate::hle_card::{self as card, dir_entry, BROKEN, BU_BUFFER};
use crate::hle_files::{fcb, read_cstr};
use crate::hle_kernel::{peek32, poke32};
use crate::Bus;

/// Kernel variables of the device (`0x3BA0..0x3BFF`).
pub mod kvar {
    /// A sector command of the current operation is on the wire.
    pub const IO_PENDING: u32 = 0x3BA0;
    /// Phase of the current synchronous operation (0 = not started).
    pub const PHASE: u32 = 0x3BA4;
    /// Loop index of the current operation.
    pub const INDEX: u32 = 0x3BA8;
    /// Sectors done / directory entry of the current operation.
    pub const AUX: u32 = 0x3BAC;
    /// Directory frames still to write: inner blocks (bit per entry).
    pub const MASK_INNER: u32 = 0x3BB0;
    /// Directory frames still to write: first blocks.
    pub const MASK_FIRST: u32 = 0x3BB4;
    /// Last error index of a sector command (1..4), for read/write.
    pub const OP_ERROR: u32 = 0x3BB8;
    /// C(1Ah)/C(1Dh) find mode: 0 = files, 1 = deleted files.
    pub const FIND_MODE: u32 = 0x3BBC;
    /// firstfile/nextfile: index of the last match.
    pub const FIND_INDEX: u32 = 0x3BC0;
    /// Asynchronous read/write per slot: next relative sector.
    pub const ASYNC_SECTOR: u32 = 0x3BC4;
    /// Asynchronous read/write per slot: sectors left.
    pub const ASYNC_COUNT: u32 = 0x3BCC;
    /// Asynchronous read/write per slot: buffer.
    pub const ASYNC_BUFFER: u32 = 0x3BD4;
    /// Asynchronous read/write per slot: FCB.
    pub const ASYNC_FCB: u32 = 0x3BDC;
    /// Sub-phase of the directory load inside an operation.
    pub const INIT_PHASE: u32 = 0x3BE4;
    /// Result of the operation's directory load (1 ok, 2 failed).
    pub const INIT_RESULT: u32 = 0x3BE8;
}

/// firstfile pattern, 20 characters and a terminator.
pub const PATTERN: u32 = 0x4160;

/// Backup-unit operations (continuing [`card::bu_op`]).
pub mod bu_op {
    /// Asynchronous read.
    pub const READ: u32 = 2;
    /// Asynchronous write.
    pub const WRITE: u32 = 3;
    /// Asynchronous write: final status probe.
    pub const WRITE_INFO: u32 = 7;
}

/// Kernel-internal trap functions of the device.
pub mod internal {
    /// open(fcb, name, mode).
    pub const OPEN: u8 = 0x21;
    /// read(fcb, dst, len).
    pub const READ: u8 = 0x22;
    /// write(fcb, src, len).
    pub const WRITE: u8 = 0x23;
    /// close(fcb).
    pub const CLOSE: u8 = 0x24;
    /// erase(fcb, name).
    pub const ERASE: u8 = 0x25;
    /// firstfile(fcb, name, direntry).
    pub const FIRSTFILE: u8 = 0x26;
    /// nextfile(fcb, direntry).
    pub const NEXTFILE: u8 = 0x27;
    /// format(fcb).
    pub const FORMAT: u8 = 0x2A;
    /// rename(fcb, old, fcb2, new).
    pub const RENAME: u8 = 0x2B;
    /// undelete(fcb, name).
    pub const UNDELETE: u8 = 0x2C;
}

/// File error numbers (psx-spx).
mod err {
    pub const NOENT: u32 = 0x02;
    pub const BUSY: u32 = 0x10;
    pub const EXIST: u32 = 0x11;
    pub const INVAL: u32 = 0x16;
    pub const NOSPC: u32 = 0x1C;
}

const MODE_CREATE: u32 = 0x200;
const MODE_ASYNC: u32 = 0x8000;

fn slot_of(bus: &Bus, f: u32) -> u32 {
    let d = peek32(bus, f + fcb::DEVICE_ID) as i32;
    let d = if d < 0 { d + 15 } else { d };
    ((d >> 4) & 1) as u32
}

fn device(bus: &Bus, f: u32) -> u32 {
    peek32(bus, f + fcb::DEVICE_ID)
}

fn var(base: u32, slot: u32) -> u32 {
    base + 4 * (slot & 1)
}

fn bu_busy(bus: &Bus, slot: u32) -> bool {
    peek32(bus, card::kvar::BU_OP + 4 * slot) != card::bu_op::NONE
}

fn set_error(bus: &mut Bus, f: u32, e: u32) {
    poke32(bus, f + fcb::ERROR, e);
}

fn buffer(slot: u32) -> u32 {
    BU_BUFFER + 0x80 * (slot & 1)
}

/// Operation finished: reset the per-call state and return `v`.
fn done(bus: &mut Bus, v: u32) -> Option<u32> {
    poke32(bus, kvar::PHASE, 0);
    poke32(bus, kvar::INDEX, 0);
    poke32(bus, kvar::AUX, 0);
    poke32(bus, kvar::IO_PENDING, 0);
    poke32(bus, kvar::INIT_PHASE, 0);
    Some(v)
}

/// One synchronous sector command: `Some(0)` on success, `Some(n)` for
/// error callback n (1..4), `None` while it runs. A command the driver
/// refuses (slot busy, bad sector) is error 1.
fn sector_io(bus: &mut Bus, dev: u32, sector: u32, buf: u32, write: bool) -> Option<u32> {
    if peek32(bus, kvar::IO_PENDING) == 0 {
        reset_status(bus);
        let queued = if write {
            card::card_write(bus, dev, sector, buf)
        } else {
            card::card_read(bus, dev, sector, buf)
        };
        if queued == 0 {
            return Some(1);
        }
        poke32(bus, kvar::IO_PENDING, 1);
        return None;
    }
    let status = wait_index(bus)?;
    poke32(bus, kvar::IO_PENDING, 0);
    Some(status)
}

/// `_card_info` as a synchronous step (after a write).
fn info_io(bus: &mut Bus, dev: u32) -> Option<u32> {
    if peek32(bus, kvar::IO_PENDING) == 0 {
        reset_status(bus);
        if card::card_info(bus, dev) == 0 {
            return Some(1);
        }
        poke32(bus, kvar::IO_PENDING, 1);
        return None;
    }
    let status = wait_index(bus)?;
    poke32(bus, kvar::IO_PENDING, 0);
    Some(status)
}

fn reset_status(bus: &mut Bus) {
    poke32(bus, card::kvar::SUCCESS, 0);
    for i in 0..4 {
        poke32(bus, card::kvar::ERRORS + 4 * i, 0);
    }
    for spec in [0x0004, 0x8000, 0x2000, 0x0100] {
        crate::hle_exceptions::undeliver_event(bus, card::EVENT_BU, spec);
    }
}

/// OpenBIOS mcWaitForStatusAndReturnIndex: 0 success, i+1 for error i.
fn wait_index(bus: &mut Bus) -> Option<u32> {
    if peek32(bus, card::kvar::SUCCESS) != 0 {
        reset_status(bus);
        return Some(0);
    }
    let failed = (0..4).find(|&i| peek32(bus, card::kvar::ERRORS + 4 * i) != 0)?;
    reset_status(bus);
    Some(failed + 1)
}

// ------------------------------------------------------ directory helpers

fn alloc_state(bus: &Bus, slot: u32, i: u32) -> u32 {
    peek32(bus, dir_entry(slot, i))
}

fn next_block(bus: &Bus, slot: u32, i: u32) -> u16 {
    (peek32(bus, dir_entry(slot, i) + 8) & 0xFFFF) as u16
}

fn set_next(bus: &mut Bus, slot: u32, i: u32, next: u16) {
    let e = dir_entry(slot, i) + 8;
    let w = peek32(bus, e);
    poke32(bus, e, (w & 0xFFFF_0000) | u32::from(next));
}

fn entry_name(bus: &Bus, slot: u32, i: u32) -> String {
    read_cstr(bus, dir_entry(slot, i) + 0x0A, 21)
}

fn set_name(bus: &mut Bus, slot: u32, i: u32, name: &str) {
    let e = dir_entry(slot, i) + 0x0A;
    for k in 0..21 {
        bus.write8_safe(e + k, 0);
    }
    for (k, b) in name.bytes().take(20).enumerate() {
        bus.write8_safe(e + k as u32, b);
    }
}

fn free_entry(bus: &mut Bus, slot: u32, i: u32) {
    poke32(bus, dir_entry(slot, i), 0xA0);
    poke32(bus, dir_entry(slot, i) + 4, 0);
    set_next(bus, slot, i, 0xFFFF);
}

/// OpenBIOS patternMatch: `?` matches any character; the pattern may be
/// longer than the name only by one `?` or its end.
fn pattern_match(name: &str, pattern: &[u8]) -> bool {
    let mut p = 0;
    for c in name.bytes() {
        let pc = pattern.get(p).copied().unwrap_or(0);
        if pc != b'?' && pc != c {
            return false;
        }
        p += 1;
    }
    let pc = pattern.get(p).copied().unwrap_or(0);
    pc == 0 || pc == b'?'
}

/// First entry from `start` that is a file's first block (or a deleted
/// one in find mode 1) with a name matching `pattern`.
fn find_file(bus: &Bus, slot: u32, start: u32, pattern: &[u8]) -> Option<u32> {
    let want = if peek32(bus, kvar::FIND_MODE) == 0 {
        0x51
    } else {
        0xA1
    };
    (start..15).find(|&i| {
        let name = entry_name(bus, slot, i);
        alloc_state(bus, slot, i) == want && !name.is_empty() && pattern_match(&name, pattern)
    })
}

fn cstr_bytes(bus: &Bus, addr: u32) -> Vec<u8> {
    read_cstr(bus, addr, 64).into_bytes()
}

/// Build directory frame `i` in the slot's buffer (cached 20h bytes, zero
/// fill, XOR checksum).
fn frame_from_entry(bus: &mut Bus, slot: u32, i: u32) -> u32 {
    let buf = buffer(slot);
    for k in 0..0x80 {
        let b = if k < card::DIR_ENTRY_SIZE {
            bus.try_read8(dir_entry(slot, i) + k).unwrap_or(0)
        } else {
            0
        };
        bus.write8_safe(buf + k, b);
    }
    checksum(bus, buf);
    buf
}

fn checksum(bus: &mut Bus, buf: u32) {
    let sum = (0..0x7F).fold(0u8, |acc, k| acc ^ bus.try_read8(buf + k).unwrap_or(0));
    bus.write8_safe(buf + 0x7F, sum);
}

/// Relative sector of a file (blocks chained through the directory) to
/// the absolute card sector; `None` past the chain.
fn absolute_sector(bus: &Bus, slot: u32, first: u32, sector: u32) -> Option<u32> {
    let mut block = first;
    let mut s = sector;
    while s > 0x3F {
        let next = next_block(bus, slot, block);
        if next == 0xFFFF || next >= 15 {
            return None;
        }
        block = u32::from(next);
        s -= 0x40;
    }
    Some(block * 0x40 + s + 0x40)
}

/// Sector after the broken-sector list's reallocation, if any.
fn reallocated(bus: &Bus, slot: u32, sector: u32) -> u32 {
    (0..20)
        .find(|&i| peek32(bus, BROKEN + 0x50 * slot + 4 * i) == sector)
        .map_or(sector, |i| i + 36)
}

// ------------------------------------------------------- directory load

/// OpenBIOS buDevInit, as a sub-step of an operation: read sector 0; a
/// card that reports "changed" gets its directory reloaded; the result is
/// whether the card is formatted. `None` while running.
fn dev_init(bus: &mut Bus, slot: u32, dev: u32) -> Option<bool> {
    match peek32(bus, kvar::INIT_RESULT) {
        1 => return Some(true),
        2 => return Some(false),
        _ => {}
    }
    let buf = buffer(slot);
    let result = loop {
        match peek32(bus, kvar::INIT_PHASE) {
            0 => {
                let status = sector_io(bus, dev, 0, buf, false)?;
                match status {
                    0 => {
                        let mc = bus.try_read8(buf) == Some(b'M')
                            && bus.try_read8(buf + 1) == Some(b'C');
                        break mc;
                    }
                    3 => poke32(bus, kvar::INIT_PHASE, 1),
                    _ => break false,
                }
            }
            // Card changed: reload this slot's directory (the _bu_init
            // sequence for one slot).
            _ => break reload_slot(bus, slot, dev)?,
        }
    };
    poke32(bus, kvar::INIT_RESULT, if result { 1 } else { 2 });
    Some(result)
}

/// Reload one slot's directory and broken-sector list: sector 0 with the
/// card-changed flag ignored, a write to 3Fh to clear that flag, frames
/// 1..15 and 16..35. Uses INIT_PHASE 1.. and INDEX.
fn reload_slot(bus: &mut Bus, slot: u32, dev: u32) -> Option<bool> {
    let buf = buffer(slot);
    loop {
        let phase = peek32(bus, kvar::INIT_PHASE);
        let index = peek32(bus, kvar::INDEX);
        match phase {
            1 => {
                if peek32(bus, kvar::IO_PENDING) == 0 {
                    card::new_card(bus);
                }
                if sector_io(bus, dev, 0, buf, false)? != 0
                    || bus.try_read8(buf) != Some(b'M')
                    || bus.try_read8(buf + 1) != Some(b'C')
                {
                    return Some(false);
                }
                poke32(bus, kvar::INIT_PHASE, 2);
            }
            2 => {
                if peek32(bus, kvar::IO_PENDING) == 0 {
                    card::new_card(bus);
                }
                sector_io(bus, dev, 0x3F, buf, true)?;
                poke32(bus, kvar::INDEX, 0);
                poke32(bus, kvar::INIT_PHASE, 3);
            }
            3 => {
                if sector_io(bus, dev, index + 1, buf, false)? != 0 {
                    return Some(false);
                }
                let e = dir_entry(slot, index);
                for k in 0..card::DIR_ENTRY_SIZE {
                    let b = bus.try_read8(buf + k).unwrap_or(0);
                    bus.write8_safe(e + k, b);
                }
                if index + 1 < 15 {
                    poke32(bus, kvar::INDEX, index + 1);
                } else {
                    poke32(bus, kvar::INDEX, 0);
                    poke32(bus, kvar::INIT_PHASE, 4);
                }
            }
            _ => {
                if sector_io(bus, dev, index + 16, buf, false)? != 0 {
                    return Some(false);
                }
                let word = peek32(bus, buf);
                poke32(bus, BROKEN + 0x50 * slot + 4 * index, word);
                if index + 1 < 20 {
                    poke32(bus, kvar::INDEX, index + 1);
                } else {
                    poke32(bus, kvar::INDEX, 0);
                    return Some(true);
                }
            }
        }
    }
}

/// Write the marked directory frames: inner blocks, a status probe, first
/// blocks, a status probe (OpenBIOS buWriteTOC). `Some(true)` on success.
fn write_toc(bus: &mut Bus, slot: u32, dev: u32) -> Option<bool> {
    loop {
        let inner = peek32(bus, kvar::MASK_INNER);
        let first = peek32(bus, kvar::MASK_FIRST);
        // Bit 31 of each mask: its trailing status probe is pending.
        let (mask_var, mask) = if inner != 0 {
            (kvar::MASK_INNER, inner)
        } else if first != 0 {
            (kvar::MASK_FIRST, first)
        } else {
            return Some(true);
        };
        let frames = mask & 0x7FFF;
        if frames != 0 {
            let i = frames.trailing_zeros();
            if peek32(bus, kvar::IO_PENDING) == 0 {
                frame_from_entry(bus, slot, i);
            }
            if sector_io(bus, dev, i + 1, buffer(slot), true)? != 0 {
                poke32(bus, kvar::MASK_INNER, 0);
                poke32(bus, kvar::MASK_FIRST, 0);
                return Some(false);
            }
            poke32(bus, mask_var, mask & !(1 << i));
            continue;
        }
        if info_io(bus, dev)? != 0 {
            poke32(bus, kvar::MASK_INNER, 0);
            poke32(bus, kvar::MASK_FIRST, 0);
            return Some(false);
        }
        poke32(bus, mask_var, 0);
    }
}

// ----------------------------------------------------------- the driver

/// open(fcb, name, mode): 0 = opened, 1 = failed (FCB error set).
pub fn open(bus: &mut Bus, f: u32, name: u32, mode: u32) -> Option<u32> {
    let slot = slot_of(bus, f);
    let dev = device(bus, f);
    if peek32(bus, kvar::PHASE) == 0 {
        set_error(bus, f, err::BUSY);
        if bu_busy(bus, slot) {
            return done(bus, 1);
        }
        reset_status(bus);
        poke32(bus, kvar::INIT_RESULT, 0);
        poke32(bus, kvar::PHASE, 1);
    }
    if peek32(bus, kvar::PHASE) == 1 {
        if mode & MODE_ASYNC == 0 && !dev_init(bus, slot, dev)? {
            return done(bus, 1);
        }
        poke32(bus, kvar::FIND_MODE, 0);
        let pattern = cstr_bytes(bus, name);
        let found = find_file(bus, slot, 0, &pattern);
        if mode & MODE_CREATE == 0 {
            let Some(index) = found else {
                set_error(bus, f, err::NOENT);
                return done(bus, 1);
            };
            return finish_open(bus, f, slot, index);
        }
        if found.is_some() {
            set_error(bus, f, err::EXIST);
            return done(bus, 1);
        }
        // A 0-block file still takes one block (with size 0).
        let blocks = (mode >> 16).max(1);
        let free: Vec<u32> = (0..15)
            .filter(|&i| alloc_state(bus, slot, i) & 0xF0 == 0xA0)
            .collect();
        if blocks as usize > free.len() {
            set_error(bus, f, err::NOSPC);
            return done(bus, 1);
        }
        let size = (mode >> 16) << 13;
        let chain = &free[..blocks as usize];
        let name = read_cstr(bus, name, 20);
        let (mut inner, mut first) = (0u32, 0u32);
        for (n, &i) in chain.iter().enumerate() {
            let e = dir_entry(slot, i);
            if n == 0 {
                poke32(bus, e, 0x51);
                poke32(bus, e + 4, size);
                set_name(bus, slot, i, &name);
                first |= 1 << i;
            } else {
                poke32(bus, e, if n + 1 == chain.len() { 0x53 } else { 0x52 });
                inner |= 1 << i;
            }
            let next = chain.get(n + 1).map_or(0xFFFF, |&j| j as u16);
            set_next(bus, slot, i, next);
        }
        poke32(bus, kvar::MASK_INNER, inner);
        poke32(bus, kvar::MASK_FIRST, first);
        poke32(bus, kvar::AUX, chain[0]);
        poke32(bus, kvar::PHASE, 2);
    }
    // Phase 2: write the new directory frames.
    let index = peek32(bus, kvar::AUX);
    if !write_toc(bus, slot, dev)? {
        // The retail kernel loops forever here (OpenBIOS notes); free the
        // blocks and report the failure instead.
        let mut i = index;
        for _ in 0..15 {
            let next = next_block(bus, slot, i);
            free_entry(bus, slot, i);
            if next == 0xFFFF || next >= 15 {
                break;
            }
            i = u32::from(next);
        }
        set_error(bus, f, err::BUSY);
        return done(bus, 1);
    }
    finish_open(bus, f, slot, index)
}

fn finish_open(bus: &mut Bus, f: u32, slot: u32, index: u32) -> Option<u32> {
    poke32(bus, f + fcb::LBA, index);
    poke32(bus, f + fcb::FPOS, 0);
    set_error(bus, f, 0);
    let size = peek32(bus, dir_entry(slot, index) + 4);
    poke32(bus, f + fcb::SIZE, size);
    done(bus, 0)
}

/// close(fcb): 0, or 1 while an asynchronous operation runs.
pub fn close(bus: &mut Bus, f: u32) -> u32 {
    let slot = slot_of(bus, f);
    if bu_busy(bus, slot) {
        return 1;
    }
    reset_status(bus);
    0
}

/// read/write(fcb, buf, len): bytes moved, or a negative error. With open
/// mode bit 15 the first sector command is only queued (returns 0) and the
/// rest continues on completion callbacks.
pub fn read_write(bus: &mut Bus, f: u32, buf: u32, len: u32, write: bool) -> Option<u32> {
    let slot = slot_of(bus, f);
    let dev = device(bus, f);
    let first_block = peek32(bus, f + fcb::LBA);
    if peek32(bus, kvar::PHASE) == 0 {
        if bu_busy(bus, slot) {
            return done(bus, u32::MAX);
        }
        reset_status(bus);
        let offset = peek32(bus, f + fcb::FPOS);
        if offset & 0x7F != 0 || offset >= peek32(bus, f + fcb::SIZE) {
            set_error(bus, f, err::INVAL);
            return done(bus, u32::MAX);
        }
        let count = ((len as i32) / 0x80).max(0) as u32;
        let start = offset >> 7;
        if peek32(bus, f + fcb::STATUS) & MODE_ASYNC != 0 {
            return Some(start_async(bus, f, slot, dev, start, count, buf, write));
        }
        poke32(bus, kvar::INDEX, 0);
        poke32(bus, kvar::AUX, count);
        poke32(bus, kvar::OP_ERROR, 0);
        poke32(bus, kvar::PHASE, 1);
    }
    let start = peek32(bus, f + fcb::FPOS) >> 7;
    let count = peek32(bus, kvar::AUX);
    loop {
        let i = peek32(bus, kvar::INDEX);
        if i >= count {
            break;
        }
        let Some(abs) = absolute_sector(bus, slot, first_block, start + i) else {
            break;
        };
        let sector = reallocated(bus, slot, abs);
        let status = sector_io(bus, dev, sector, buf + 0x80 * i, write)?;
        if status != 0 {
            poke32(bus, kvar::OP_ERROR, status);
            break;
        }
        // A write's last sector of each 40h-sector block is followed by a
        // status probe; approximated here by one probe after the last.
        poke32(bus, kvar::INDEX, i + 1);
    }
    if write && peek32(bus, kvar::INDEX) == count && count != 0 {
        let status = info_io(bus, dev)?;
        if status != 0 {
            poke32(bus, kvar::OP_ERROR, status);
            poke32(bus, kvar::INDEX, count - 1);
        }
    }
    let moved = peek32(bus, kvar::INDEX) * 0x80;
    let pos = peek32(bus, f + fcb::FPOS);
    poke32(bus, f + fcb::FPOS, pos + moved);
    set_error(bus, f, 0);
    poke32(bus, card::kvar::BU_OP + 4 * slot, card::bu_op::NONE);
    if moved != len {
        let e = peek32(bus, kvar::OP_ERROR);
        return done(bus, (e as i32).wrapping_neg() as u32);
    }
    done(bus, len)
}

#[allow(clippy::too_many_arguments)]
fn start_async(
    bus: &mut Bus,
    f: u32,
    slot: u32,
    dev: u32,
    start: u32,
    count: u32,
    buf: u32,
    write: bool,
) -> u32 {
    let op = if write { bu_op::WRITE } else { bu_op::READ };
    poke32(bus, card::kvar::BU_OP + 4 * slot, op);
    poke32(bus, var(kvar::ASYNC_SECTOR, slot), start);
    poke32(bus, var(kvar::ASYNC_COUNT, slot), count);
    poke32(bus, var(kvar::ASYNC_BUFFER, slot), buf);
    poke32(bus, var(kvar::ASYNC_FCB, slot), f);
    reset_status(bus);
    if count == 0 {
        set_error(bus, f, 0);
        poke32(bus, card::kvar::SUCCESS, 1);
        card::bu_finish(bus, slot, 0x0004);
        return 0;
    }
    let first_block = peek32(bus, f + fcb::LBA);
    let Some(abs) = absolute_sector(bus, slot, first_block, start) else {
        return u32::MAX;
    };
    let sector = reallocated(bus, slot, abs);
    set_error(bus, f, err::BUSY);
    let queued = if write {
        card::card_write(bus, dev, sector, buf)
    } else {
        card::card_read(bus, dev, sector, buf)
    };
    if queued == 0 {
        return u32::MAX;
    }
    set_error(bus, f, 0);
    0
}

/// Completion callback part for asynchronous reads and writes (OpenBIOS
/// buLowLevelOpCompleted cases 2, 3 and 7).
pub fn async_completed(bus: &mut Bus, slot: u32, device: u32, op: u32) {
    let f = peek32(bus, var(kvar::ASYNC_FCB, slot));
    let fd = peek32(bus, f + fcb::NUMBER);
    match op {
        bu_op::WRITE_INFO => {
            crate::hle_exceptions::queue_event(bus, fd, 0x0004);
            let pos = peek32(bus, f + fcb::FPOS);
            poke32(bus, f + fcb::FPOS, pos + 0x80);
            card::bu_finish(bus, slot, 0x0004);
        }
        bu_op::READ | bu_op::WRITE => {
            let left = peek32(bus, var(kvar::ASYNC_COUNT, slot)) - 1;
            poke32(bus, var(kvar::ASYNC_COUNT, slot), left);
            if left == 0 {
                if op == bu_op::READ {
                    card::bu_finish(bus, slot, 0x0004);
                    crate::hle_exceptions::queue_event(bus, fd, 0x0004);
                } else {
                    poke32(bus, card::kvar::SUCCESS, 0);
                    if card::card_info(bus, device) == 0 {
                        poke32(bus, card::kvar::ERRORS, 1);
                        card::bu_finish(bus, slot, 0x8000);
                        return;
                    }
                    poke32(bus, card::kvar::BU_OP + 4 * slot, bu_op::WRITE_INFO);
                }
                return;
            }
            let buf = peek32(bus, var(kvar::ASYNC_BUFFER, slot)) + 0x80;
            poke32(bus, var(kvar::ASYNC_BUFFER, slot), buf);
            let pos = peek32(bus, f + fcb::FPOS);
            poke32(bus, f + fcb::FPOS, pos + 0x80);
            let rel = peek32(bus, var(kvar::ASYNC_SECTOR, slot)) + 1;
            poke32(bus, var(kvar::ASYNC_SECTOR, slot), rel);
            let first_block = peek32(bus, f + fcb::LBA);
            let queued = absolute_sector(bus, slot, first_block, rel).is_some_and(|abs| {
                let sector = reallocated(bus, slot, abs);
                if op == bu_op::READ {
                    card::card_read(bus, device, sector, buf) != 0
                } else {
                    card::card_write(bus, device, sector, buf) != 0
                }
            });
            if !queued {
                poke32(bus, card::kvar::SUCCESS, 0);
                poke32(bus, card::kvar::ERRORS, 1);
                card::bu_finish(bus, slot, 0x8000);
            }
        }
        _ => {}
    }
}

/// firstfile(fcb, name, direntry): the first match, or 0.
pub fn firstfile(bus: &mut Bus, f: u32, name: u32, direntry: u32) -> Option<u32> {
    let slot = slot_of(bus, f);
    let dev = device(bus, f);
    if peek32(bus, kvar::PHASE) == 0 {
        set_error(bus, f, err::BUSY);
        if bu_busy(bus, slot) {
            return done(bus, 0);
        }
        reset_status(bus);
        poke32(bus, kvar::INIT_RESULT, 0);
        poke32(bus, kvar::PHASE, 1);
    }
    if !dev_init(bus, slot, dev)? {
        return done(bus, 0);
    }
    // Pattern: 19 "?" by default; the name up to a "*", which pads the
    // rest with "?".
    let given = cstr_bytes(bus, name);
    let mut pattern = vec![b'?'; 19];
    if !given.is_empty() {
        pattern.clear();
        let star = given.iter().position(|&c| c == b'*');
        pattern.extend_from_slice(&given[..star.unwrap_or(given.len())]);
        if star.is_some() {
            pattern.resize(20, b'?');
        }
    }
    pattern.truncate(20);
    for k in 0..21 {
        bus.write8_safe(PATTERN + k, pattern.get(k as usize).copied().unwrap_or(0));
    }
    poke32(bus, kvar::FIND_INDEX, u32::MAX);
    done(bus, 0)?;
    Some(nextfile(bus, f, direntry))
}

/// nextfile(fcb, direntry): the next match of the firstfile pattern, or 0.
/// The direntry gets name, attribute (allocation state & F0h), size and
/// first sector (psx-spx layout).
pub fn nextfile(bus: &mut Bus, f: u32, direntry: u32) -> u32 {
    let slot = slot_of(bus, f);
    if bu_busy(bus, slot) {
        set_error(bus, f, err::BUSY);
        return 0;
    }
    reset_status(bus);
    let pattern = cstr_bytes(bus, PATTERN);
    let start = peek32(bus, kvar::FIND_INDEX).wrapping_add(1);
    let Some(index) = find_file(bus, slot, start, &pattern) else {
        set_error(bus, f, err::NOENT);
        return 0;
    };
    poke32(bus, kvar::FIND_INDEX, index);
    let name = entry_name(bus, slot, index);
    for k in 0..0x14 {
        let b = name.as_bytes().get(k as usize).copied().unwrap_or(0);
        bus.write8_safe(direntry + k, b);
    }
    poke32(bus, direntry + 0x14, alloc_state(bus, slot, index) & 0xF0);
    poke32(
        bus,
        direntry + 0x18,
        peek32(bus, dir_entry(slot, index) + 4),
    );
    poke32(bus, direntry + 0x20, (index + 1) * 0x40);
    set_error(bus, f, 0);
    direntry
}

/// erase(fcb, name): 0 = deleted (blocks marked A1h/A2h/A3h), 1 = failed.
pub fn erase(bus: &mut Bus, f: u32, name: u32) -> Option<u32> {
    let slot = slot_of(bus, f);
    let dev = device(bus, f);
    if peek32(bus, kvar::PHASE) == 0 {
        set_error(bus, f, err::BUSY);
        if bu_busy(bus, slot) {
            return done(bus, 1);
        }
        reset_status(bus);
        poke32(bus, kvar::INIT_RESULT, 0);
        poke32(bus, kvar::PHASE, 1);
    }
    if peek32(bus, kvar::PHASE) == 1 {
        if !dev_init(bus, slot, dev)? {
            return done(bus, 1);
        }
        poke32(bus, kvar::FIND_MODE, 0);
        let pattern = cstr_bytes(bus, name);
        let Some(index) = find_file(bus, slot, 0, &pattern) else {
            set_error(bus, f, err::NOENT);
            return done(bus, 1);
        };
        let (mut inner, first) = (0u32, 1u32 << index);
        poke32(bus, dir_entry(slot, index), 0xA1);
        let size = peek32(bus, dir_entry(slot, index) + 4) as i32;
        let mut count = (size.max(0) >> 13) - 1;
        let mut i = index;
        while count > 0 {
            let next = next_block(bus, slot, i);
            if next == 0xFFFF || next >= 15 {
                break;
            }
            i = u32::from(next);
            match alloc_state(bus, slot, i) {
                0x52 => poke32(bus, dir_entry(slot, i), 0xA2),
                0x53 => poke32(bus, dir_entry(slot, i), 0xA3),
                _ => break,
            }
            inner |= 1 << i;
            count -= 1;
        }
        poke32(bus, kvar::MASK_INNER, inner);
        poke32(bus, kvar::MASK_FIRST, first);
        poke32(bus, kvar::AUX, inner | first);
        poke32(bus, kvar::PHASE, 2);
    }
    if !write_toc(bus, slot, dev)? {
        let touched = peek32(bus, kvar::AUX);
        for i in (0..15).filter(|i| touched & (1 << i) != 0) {
            free_entry(bus, slot, i);
        }
        set_error(bus, f, err::BUSY);
        return done(bus, 1);
    }
    set_error(bus, f, 0);
    done(bus, 0)
}

/// rename(fcb, old, fcb2, new): 0 = renamed, 1 = failed.
pub fn rename(bus: &mut Bus, f: u32, old: u32, new: u32) -> Option<u32> {
    let slot = slot_of(bus, f);
    let dev = device(bus, f);
    if peek32(bus, kvar::PHASE) == 0 {
        set_error(bus, f, err::BUSY);
        if bu_busy(bus, slot) {
            return done(bus, 1);
        }
        reset_status(bus);
        poke32(bus, kvar::INIT_RESULT, 0);
        poke32(bus, kvar::PHASE, 1);
    }
    if peek32(bus, kvar::PHASE) == 1 {
        if !dev_init(bus, slot, dev)? {
            return done(bus, 1);
        }
        poke32(bus, kvar::FIND_MODE, 0);
        if find_file(bus, slot, 0, &cstr_bytes(bus, new)).is_some() {
            set_error(bus, f, err::EXIST);
            return done(bus, 1);
        }
        let Some(index) = find_file(bus, slot, 0, &cstr_bytes(bus, old)) else {
            set_error(bus, f, err::NOENT);
            return done(bus, 1);
        };
        let name = read_cstr(bus, new, 20);
        set_name(bus, slot, index, &name);
        poke32(bus, kvar::MASK_INNER, 0);
        poke32(bus, kvar::MASK_FIRST, 1 << index);
        poke32(bus, kvar::AUX, index);
        poke32(bus, kvar::PHASE, 2);
    }
    if !write_toc(bus, slot, dev)? {
        let index = peek32(bus, kvar::AUX);
        let name = read_cstr(bus, old, 20);
        set_name(bus, slot, index, &name);
        set_error(bus, f, err::BUSY);
        return done(bus, 1);
    }
    set_error(bus, f, 0);
    done(bus, 0)
}

/// format(fcb): write "MC", 15 free directory frames and 20 empty
/// broken-sector frames. 0 = formatted, 1 = failed.
pub fn format(bus: &mut Bus, f: u32) -> Option<u32> {
    let slot = slot_of(bus, f);
    let dev = device(bus, f);
    let buf = buffer(slot);
    if peek32(bus, kvar::PHASE) == 0 {
        if bu_busy(bus, slot) {
            set_error(bus, f, err::BUSY);
            return done(bus, 1);
        }
        reset_status(bus);
        poke32(bus, kvar::INDEX, 0);
        poke32(bus, kvar::PHASE, 1);
    }
    loop {
        let index = peek32(bus, kvar::INDEX);
        if index >= 36 {
            break;
        }
        if peek32(bus, kvar::IO_PENDING) == 0 {
            for k in 0..0x80 {
                bus.write8_safe(buf + k, 0);
            }
            match index {
                0 => {
                    bus.write8_safe(buf, b'M');
                    bus.write8_safe(buf + 1, b'C');
                    card::new_card(bus);
                }
                1..=15 => {
                    free_entry(bus, slot, index - 1);
                    set_name(bus, slot, index - 1, "");
                    let e = dir_entry(slot, index - 1);
                    for k in 0..card::DIR_ENTRY_SIZE {
                        let b = bus.try_read8(e + k).unwrap_or(0);
                        bus.write8_safe(buf + k, b);
                    }
                }
                _ => {
                    poke32(bus, buf, u32::MAX);
                    poke32(bus, buf + 8, 0xFFFF);
                    poke32(bus, BROKEN + 0x50 * slot + 4 * (index - 16), u32::MAX);
                }
            }
            checksum(bus, buf);
        }
        // OpenBIOS ignores the status of each write.
        sector_io(bus, dev, index, buf, true)?;
        poke32(bus, kvar::INDEX, index + 1);
    }
    set_error(bus, f, 0);
    done(bus, 0)
}

/// undelete(fcb, name): not supported (the retail function exists; no
/// title in the compatibility list uses it). Fails with "busy".
pub fn undelete(bus: &mut Bus, f: u32) -> u32 {
    set_error(bus, f, err::BUSY);
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hle_asm::*;
    use crate::Cpu;

    const PROGRAM: u32 = 0x8001_0000;
    const RESULTS: u32 = 0x8002_0F00;
    const STRINGS: u32 = 0x8002_0E00;

    /// Argument placeholder: the result of call `i` is `RESULT | i`.
    const RESULT: u32 = 0xFFFF_FF00;

    fn program(calls: &[(i16, u32, [u32; 3])]) -> Vec<u32> {
        let mut a = Asm::new(PROGRAM);
        a.li(T0, 0x0000_0401);
        a.mtc0(T0, 12);
        a.li(S0, RESULTS);
        a.li(S1, RESULTS);
        for (vector, func, args) in calls {
            for (i, v) in args.iter().enumerate() {
                if v & 0xFFFF_FF00 == RESULT {
                    a.lw(A0 + i as u32, (4 * (v & 0xFF)) as i16, S1);
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

    fn run(bus: &mut Bus, words: &[u32]) {
        let mut cpu = Cpu::new();
        for (i, w) in words.iter().enumerate() {
            bus.write32(PROGRAM + 4 * i as u32, *w);
        }
        cpu.gprs_mut_for_test()[29] = 0x801F_FF00;
        cpu.set_pc_for_test(PROGRAM);
        let end = PROGRAM + 4 * (words.len() as u32 - 2);
        for _ in 0..600_000_000u32 {
            if cpu.pc() == end {
                return;
            }
            cpu.step(bus).unwrap();
        }
        panic!("program did not finish, pc={:#x}", cpu.pc());
    }

    fn string(bus: &mut Bus, index: u32, s: &str) -> u32 {
        let at = STRINGS + 0x20 * index;
        for (k, b) in s.bytes().chain([0]).enumerate() {
            bus.write8_safe(at + k as u32, b);
        }
        at
    }

    const A: i16 = 0xA0;
    const B: i16 = 0xB0;

    #[test]
    fn files_are_created_written_listed_read_and_erased_on_the_card() {
        let mut bus = Bus::new_without_bios();
        bus.enable_hle_bios();
        bus.attach_digital_pad_port1();
        bus.attach_memcard_port1(Vec::new());
        bus.detach_memcard_port2();
        let name = string(&mut bus, 0, "bu00:BASLUS-00000TEST");
        let pattern = string(&mut bus, 1, "bu00:BA*");
        let device = string(&mut bus, 2, "bu00:");
        let (src, dst, dirent) = (0x8003_0000, 0x8003_0100, 0x8003_0200);
        for k in 0..0x100 {
            bus.write8_safe(src + k, (k * 7 + 1) as u8);
        }
        let create = 0x0001_0000 | 0x200 | 0x3;
        let words = program(&[
            (B, 0x4A, [1, 0, 0]),                // 0 InitCARD2
            (B, 0x4B, [0, 0, 0]),                // 1 StartCARD2
            (A, 0x70, [0, 0, 0]),                // 2 _bu_init
            (B, 0x32, [name, create, 0]),        // 3 open (create, 1 block)
            (B, 0x35, [RESULT | 3, src, 0x100]), // 4 write 2 sectors
            (B, 0x36, [RESULT | 3, 0, 0]),       // 5 close
            (B, 0x32, [name, 1, 0]),             // 6 open existing
            (B, 0x34, [RESULT | 6, dst, 0x100]), // 7 read
            (B, 0x36, [RESULT | 6, 0, 0]),       // 8 close
            (B, 0x42, [pattern, dirent, 0]),     // 9 firstfile
            (B, 0x43, [dirent, 0, 0]),           // 10 nextfile: no more
            (B, 0x45, [name, 0, 0]),             // 11 erase
            (B, 0x32, [name, 1, 0]),             // 12 open: gone
            (B, 0x41, [device, 0, 0]),           // 13 format
        ]);
        run(&mut bus, &words);
        let r = |bus: &mut Bus, i: u32| bus.read32(RESULTS + 4 * i);
        assert_eq!(r(&mut bus, 3), 2, "fd 2 (0 and 1 are the TTY)");
        assert_eq!(r(&mut bus, 4), 0x100);
        assert_eq!(r(&mut bus, 5), 2);
        assert_eq!(r(&mut bus, 7), 0x100);
        let back: Vec<u8> = (0..0x100)
            .map(|k| bus.try_read8(dst + k).unwrap())
            .collect();
        let want: Vec<u8> = (0..0x100u32).map(|k| (k * 7 + 1) as u8).collect();
        assert_eq!(back, want);
        // firstfile found it: name, attribute 50h, size 2000h, block 1
        // (first sector 40h).
        assert_eq!(r(&mut bus, 9), dirent);
        assert_eq!(read_cstr(&bus, dirent, 20), "BASLUS-00000TEST");
        assert_eq!(bus.read32(dirent + 0x14), 0x50);
        assert_eq!(bus.read32(dirent + 0x18), 0x2000);
        assert_eq!(bus.read32(dirent + 0x20), 0x40);
        assert_eq!(r(&mut bus, 10), 0);
        assert_eq!(r(&mut bus, 11), 1, "erase ok");
        assert_eq!(r(&mut bus, 12), u32::MAX, "erased file does not open");
        assert_eq!(r(&mut bus, 13), 1, "format ok");

        // The card holds what the kernel wrote: after the format, a
        // standard empty directory.
        let card = bus.memcard_port1_snapshot().expect("card on port 1");
        assert_eq!(&card[..2], b"MC");
        assert_eq!(card[0x80], 0xA0);
        assert_eq!(card[0x80 + 0x7F], 0xA0, "free frame checksum");
        // The erased file's data sectors are left alone by format.
        assert_eq!(card[0x2000], 1);
    }
}
