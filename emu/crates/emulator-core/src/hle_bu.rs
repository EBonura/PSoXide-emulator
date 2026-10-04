// SPDX-License-Identifier: GPL-2.0-or-later
//! Backup unit of the HLE kernel: the directory cache with its loading
//! (`_bu_init`, `_card_load`, `_card_info`), and the "bu" file device that
//! opens, reads, writes, lists, erases, renames, undeletes and formats
//! files on the cards. The sector transport is [`crate::hle_card`].
//!
//! Sources: psx-spx "Memory Card Data Format" (the header frame "MC", the
//! 15 directory frames with their allocation states 51h/52h/53h/A0h..A3h,
//! the size, next-block and name fields and the XOR checksum, the broken
//! sector list in frames 16..35 with replacements 20 frames later, the
//! write test frame 3Fh), "BIOS Memory Card Functions" (the functions, one
//! sector per access, 80h-byte alignment, files in whole 2000h-byte blocks,
//! synchronous and asynchronous access, the extra sector 0 read when a
//! file is opened, auto format, `_card_info` after a write), "BIOS File
//! Functions" (the open mode bits, error numbers, the direntry layout, the
//! wildcards and their bug, find mode, the two-character events) and "BIOS
//! Event Summary" (the SwCARD events of class F4000001h). psx-spx leaves a
//! few things open; each is decided where it comes up.
//!
//! All work is driven from guest calls that are retried while a sector is
//! on its way (a call returns `None` until it is done) or, for the
//! asynchronous calls, from the card driver's completion hook
//! ([`low_level_done`]). Progress lives in kernel RAM ([`kvar`]), so a save
//! state taken in the middle resumes.

use crate::hle_card::{self as card, outcome, BROKEN, BU_BUFFER, DIRECTORY, DIR_ENTRY_SIZE, EVENT_BU};
use crate::hle_exceptions as ex;
use crate::hle_files::fcb;
use crate::hle_kernel::{peek32, poke32};
use crate::Bus;

/// Kernel variables of the backup unit. Per-slot ones are two words.
pub mod kvar {
    /// The slot's directory cache is valid (2 words).
    pub const LOADED: u32 = 0x3B80;
    /// A sector request of this module is with the card driver, per slot.
    pub const IO_PENDING: u32 = 0x3B88;
    /// Phase of the directory load, per slot.
    pub const LOAD_PHASE: u32 = 0x3B90;
    /// Counter of the directory load, per slot.
    pub const LOAD_INDEX: u32 = 0x3B98;
    /// Counter of a format, per slot.
    pub const TOC_INDEX: u32 = 0x3BA0;
    /// Phase of the file function in progress, 0 when none.
    pub const PHASE: u32 = 0x3BA8;
    /// Counter within that phase.
    pub const INDEX: u32 = 0x3BAC;
    /// Two values the function in progress keeps (sector count, first
    /// block).
    pub const AUX: u32 = 0x3BB0;
    /// Directory frames still to write: bit `i` is entry `i`.
    pub const DIRTY: u32 = 0x3BB8;
    /// Entry whose frame is written after the others, or FFh.
    pub const LAST_ENTRY: u32 = 0x3BBC;
    /// Slot `_bu_init` is loading.
    pub const INIT_SLOT: u32 = 0x3BC0;
    /// C(1Ah) find mode: 0 lists files, 1 deleted files.
    pub const FIND_MODE: u32 = 0x3BC4;
    /// Entry `nextfile` continues from.
    pub const FIND_INDEX: u32 = 0x3BC8;
    /// High-level operation under way per slot ([`super::bu_op`]), 2 words.
    pub const BU_OP: u32 = 0x3BCC;
    /// Asynchronous transfer: FCB, sectors done, sectors in all, buffer,
    /// and whether a sector is out.
    pub const ASYNC: u32 = 0x3BD4;
}

/// Name pattern of the search `firstfile` started: 15h bytes.
pub const PATTERN: u32 = 0x4160;

/// Operations the backup unit runs on its own after a call returned.
pub mod bu_op {
    /// Nothing.
    pub const NONE: u32 = 0;
    /// `_card_info`.
    pub const INFO: u32 = 1;
    /// `_card_load`.
    pub const LOAD: u32 = 2;
    /// Asynchronous file read.
    pub const READ: u32 = 3;
    /// Asynchronous file write.
    pub const WRITE: u32 = 4;
}

/// Kernel-internal trap functions of the "bu" device.
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
    /// rename(fcb1, name1, fcb2, name2).
    pub const RENAME: u8 = 0x2B;
    /// undelete(fcb, name).
    pub const UNDELETE: u8 = 0x2C;
}

/// File error numbers (psx-spx "BIOS File Functions").
mod err {
    pub const NOENT: u32 = 0x02;
    pub const IO: u32 = 0x10;
    pub const EXIST: u32 = 0x11;
    pub const INVAL: u32 = 0x16;
    pub const NOSPC: u32 = 0x1C;
}

/// Open mode bit: create a new file, its block count in bits 16..31.
const MODE_CREATE: u32 = 0x200;
/// Open mode bit: reads and writes return at once and finish by event.
const MODE_ASYNC: u32 = 0x8000;

/// Directory entries, and the blocks they stand for (1..15).
const ENTRIES: u32 = 15;
/// Broken sector list entries per card.
const BROKEN_ENTRIES: u32 = 20;
/// Bytes of a sector, of a block.
const SECTOR: u32 = 0x80;
const BLOCK: u32 = 0x2000;
/// Allocation states (psx-spx "Directory Frames").
const FIRST: u32 = 0x51;
const MIDDLE: u32 = 0x52;
const LAST: u32 = 0x53;
const FREE: u32 = 0xA0;
const DELETED_FIRST: u32 = 0xA1;
/// "No next block".
const NO_NEXT: u32 = 0xFFFF;
/// Phase constants shared by the file functions.
const IDLE: u32 = 0;
/// First sector of the write test frame.
const WRITE_TEST_FRAME: u32 = 0x3F;

// ------------------------------------------------------- directory cache

fn var(base: u32, slot: u32) -> u32 {
    base + 4 * slot
}

fn loaded(bus: &Bus, slot: u32) -> bool {
    peek32(bus, var(kvar::LOADED, slot)) != 0
}

/// Address of the cached directory entry `i` (0..14, block `i + 1`) of
/// `slot`.
pub fn dir_entry(slot: u32, i: u32) -> u32 {
    DIRECTORY + (slot * ENTRIES + i) * DIR_ENTRY_SIZE
}

/// Sector buffer of `slot`.
fn buffer(slot: u32) -> u32 {
    BU_BUFFER + slot * SECTOR
}

fn entry_state(bus: &Bus, slot: u32, i: u32) -> u32 {
    peek32(bus, dir_entry(slot, i))
}

fn entry_size(bus: &Bus, slot: u32, i: u32) -> u32 {
    peek32(bus, dir_entry(slot, i) + 4)
}

/// Next block of the file after entry `i`, as an entry index, or
/// [`NO_NEXT`].
fn entry_next(bus: &Bus, slot: u32, i: u32) -> u32 {
    let at = dir_entry(slot, i) + 8;
    u32::from(bus.try_read8(at).unwrap_or(0xFF)) | u32::from(bus.try_read8(at + 1).unwrap_or(0xFF)) << 8
}

fn set_next(bus: &mut Bus, slot: u32, i: u32, next: u32) {
    let at = dir_entry(slot, i) + 8;
    bus.write8_safe(at, next as u8);
    bus.write8_safe(at + 1, (next >> 8) as u8);
}

/// The 15h name bytes of entry `i`: up to 20 characters and a zero.
fn entry_name(bus: &Bus, slot: u32, i: u32) -> [u8; 21] {
    let at = dir_entry(slot, i) + 0x0A;
    std::array::from_fn(|k| bus.try_read8(at + k as u32).unwrap_or(0))
}

fn set_name(bus: &mut Bus, slot: u32, i: u32, name: &[u8]) {
    let at = dir_entry(slot, i) + 0x0A;
    for k in 0..21 {
        let byte = if k < 20 { name.get(k).copied().unwrap_or(0) } else { 0 };
        bus.write8_safe(at + k as u32, byte);
    }
}

/// Leave entry `i` free, the way a freshly formatted card has it.
fn clear_entry(bus: &mut Bus, slot: u32, i: u32) {
    for off in (0..DIR_ENTRY_SIZE).step_by(4) {
        poke32(bus, dir_entry(slot, i) + off, 0);
    }
    poke32(bus, dir_entry(slot, i), FREE);
    set_next(bus, slot, i, NO_NEXT);
}

fn broken_entry(bus: &Bus, slot: u32, j: u32) -> u32 {
    peek32(bus, BROKEN + 4 * (slot * BROKEN_ENTRIES + j))
}

/// Forget the cache of `slot`: no entries and no broken sectors.
fn clear_cache(bus: &mut Bus, slot: u32) {
    for i in 0..ENTRIES {
        for off in (0..DIR_ENTRY_SIZE).step_by(4) {
            poke32(bus, dir_entry(slot, i) + off, 0);
        }
    }
    for j in 0..BROKEN_ENTRIES {
        poke32(bus, BROKEN + 4 * (slot * BROKEN_ENTRIES + j), u32::MAX);
    }
    poke32(bus, var(kvar::LOADED, slot), 0);
}

/// Boot: both caches empty, nothing in progress.
pub fn install(bus: &mut Bus) {
    for addr in (kvar::LOADED..kvar::ASYNC + 20).step_by(4) {
        poke32(bus, addr, 0);
    }
    for slot in 0..2 {
        clear_cache(bus, slot);
    }
}

/// XOR of the first 7Fh bytes of a frame in the buffer.
fn frame_checksum(bus: &Bus, buf: u32) -> u8 {
    (0..0x7F).fold(0, |sum, k| sum ^ bus.try_read8(buf + k).unwrap_or(0))
}

/// Whether the frame in the buffer carries its own checksum.
fn frame_ok(bus: &Bus, buf: u32) -> bool {
    bus.try_read8(buf + 0x7F) == Some(frame_checksum(bus, buf))
}

/// Set the checksum byte of the frame in the buffer.
fn seal_frame(bus: &mut Bus, buf: u32) {
    let sum = frame_checksum(bus, buf);
    bus.write8_safe(buf + 0x7F, sum);
}

fn clear_buffer(bus: &mut Bus, buf: u32, fill: u8) {
    for k in 0..SECTOR {
        bus.write8_safe(buf + k, fill);
    }
}

/// Build the frame of directory entry `i` in the buffer: the cached 20h
/// bytes, zeros after them (psx-spx: garbage, usually zero) and the
/// checksum.
fn frame_from_entry(bus: &mut Bus, slot: u32, i: u32) {
    let buf = buffer(slot);
    clear_buffer(bus, buf, 0);
    for off in 0..DIR_ENTRY_SIZE {
        let byte = bus.try_read8(dir_entry(slot, i) + off).unwrap_or(0);
        bus.write8_safe(buf + off, byte);
    }
    seal_frame(bus, buf);
}

/// Where the data of `sector` really is: a sector on the card's broken
/// list lives in the replacement frame 20 frames after its list entry
/// (psx-spx "Broken Sector List").
fn replacement(bus: &Bus, slot: u32, sector: u32) -> u32 {
    (0..BROKEN_ENTRIES)
        .find(|&j| broken_entry(bus, slot, j) == sector)
        .map_or(sector, |j| 36 + j)
}

/// The sector of byte `offset` of the file whose first block is directory
/// entry `first`, following the chain of next-block pointers. `None` for a
/// chain that ends or leaves the directory early.
fn file_sector(bus: &Bus, slot: u32, first: u32, offset: u32) -> Option<u32> {
    let mut entry = first;
    for _ in 0..offset / BLOCK {
        entry = entry_next(bus, slot, entry);
        if entry >= ENTRIES {
            return None;
        }
    }
    let sector = (entry + 1) * 64 + offset % BLOCK / SECTOR;
    Some(replacement(bus, slot, sector))
}

// ------------------------------------------------------- sector transport

/// What a request to the card driver is for.
#[derive(Copy, Clone)]
enum Io {
    Read,
    Write,
    Info,
}

/// One request through the card driver on `device`'s slot: it is made on
/// the first call, then each call returns `None` until the driver has
/// finished, and then its outcome ([`outcome`]). Returns the outcome
/// [`outcome::ERROR`] when the driver refuses the request.
fn transfer(bus: &mut Bus, device: u32, sector: u32, buf: u32, io: Io) -> Option<u32> {
    let slot = card::slot_of(device);
    if peek32(bus, var(kvar::IO_PENDING, slot)) == 0 {
        let queued = match io {
            Io::Read => card::card_read(bus, device, sector, buf),
            Io::Write => card::card_write(bus, device, sector, buf),
            Io::Info => card::card_info_internal(bus, device),
        };
        if queued == 0 {
            return Some(outcome::ERROR);
        }
        poke32(bus, var(kvar::IO_PENDING, slot), 1);
        return None;
    }
    if !card::idle(bus, slot) {
        return None;
    }
    poke32(bus, var(kvar::IO_PENDING, slot), 0);
    Some(card::last_outcome(bus, slot))
}

/// The SwCARD event of a finished high-level operation (psx-spx "BIOS
/// Event Summary": 4 done, 100h busy, 2000h eject or unformatted, 8000h
/// error).
fn notify(bus: &mut Bus, result: u32) {
    let spec = match result {
        outcome::OK => 0x4,
        outcome::TIMEOUT => 0x100,
        outcome::CHANGED => 0x2000,
        _ => 0x8000,
    };
    ex::queue_event(bus, EVENT_BU, spec);
}

/// Forget the SwCARD events of the previous operation before a new one.
fn undeliver_events(bus: &mut Bus) {
    for spec in [0x4, 0x100, 0x2000, 0x8000] {
        ex::undeliver_event(bus, EVENT_BU, spec);
    }
}

// ----------------------------------------------------- the directory load

mod load {
    /// Read sector 0 and check for "MC".
    pub const HEADER: u32 = 0;
    /// Write the header back to the write test frame, which clears the
    /// card's changed flag.
    pub const WRITE_TEST: u32 = 1;
    /// Read the directory frames.
    pub const DIRECTORY: u32 = 2;
    /// Read the broken sector list.
    pub const BROKEN: u32 = 3;
    /// Format the card (auto format), then start over.
    pub const FORMAT: u32 = 4;
}

/// Load the directory cache of `slot`: sector 0 must say "MC" (psx-spx;
/// with auto format on, a card without it is formatted first), a write to
/// the write test frame clears the card's changed flag, then the 15
/// directory frames and the 20 broken list frames are read and kept. The
/// card is told `_new_card` before its sector 0 read and that write, so its
/// changed flag does not fail them. `None` while a sector is on its way,
/// then the outcome ([`outcome::OK`] when the cache is valid; a card
/// without "MC" counts as [`outcome::CHANGED`], which psx-spx sends as
/// the 2000h "eject or unformatted" event). A failure leaves the cache
/// empty.
fn load_directory(bus: &mut Bus, slot: u32) -> Option<u32> {
    let device = slot << 4;
    let buf = buffer(slot);
    loop {
        let index = peek32(bus, var(kvar::LOAD_INDEX, slot));
        let step = match peek32(bus, var(kvar::LOAD_PHASE, slot)) {
            load::HEADER => {
                card::new_card(bus);
                match transfer(bus, device, 0, buf, Io::Read)? {
                    outcome::OK => {
                        let mc = bus.try_read8(buf) == Some(b'M') && bus.try_read8(buf + 1) == Some(b'C');
                        if mc {
                            Ok(load::WRITE_TEST)
                        } else if peek32(bus, card::kvar::AUTO_FORMAT) != 0 && index == 0 {
                            poke32(bus, var(kvar::LOAD_INDEX, slot), 1);
                            Ok(load::FORMAT)
                        } else {
                            Err(outcome::CHANGED)
                        }
                    }
                    other => Err(other),
                }
            }
            load::WRITE_TEST => {
                card::new_card(bus);
                match transfer(bus, device, WRITE_TEST_FRAME, buf, Io::Write)? {
                    outcome::OK => {
                        poke32(bus, var(kvar::LOAD_INDEX, slot), 1);
                        Ok(load::DIRECTORY)
                    }
                    other => Err(other),
                }
            }
            load::DIRECTORY => match transfer(bus, device, index, buf, Io::Read)? {
                outcome::OK if directory_frame_ok(bus, slot, index) => {
                    poke32(bus, var(kvar::LOAD_INDEX, slot), index + 1);
                    Ok(if index + 1 > ENTRIES {
                        poke32(bus, var(kvar::LOAD_INDEX, slot), ENTRIES + 1);
                        load::BROKEN
                    } else {
                        load::DIRECTORY
                    })
                }
                outcome::OK => Err(outcome::ERROR),
                other => Err(other),
            },
            load::BROKEN => match transfer(bus, device, index, buf, Io::Read)? {
                outcome::OK if frame_ok(bus, buf) => {
                    let word = peek32(bus, buf);
                    poke32(bus, BROKEN + 4 * (slot * BROKEN_ENTRIES + index - (ENTRIES + 1)), word);
                    poke32(bus, var(kvar::LOAD_INDEX, slot), index + 1);
                    if index + 1 > ENTRIES + BROKEN_ENTRIES {
                        poke32(bus, var(kvar::LOADED, slot), 1);
                        poke32(bus, var(kvar::LOAD_PHASE, slot), 0);
                        poke32(bus, var(kvar::LOAD_INDEX, slot), 0);
                        return Some(outcome::OK);
                    }
                    Ok(load::BROKEN)
                }
                outcome::OK => Err(outcome::ERROR),
                other => Err(other),
            },
            _ => match write_format(bus, slot)? {
                outcome::OK => Ok(load::HEADER),
                other => Err(other),
            },
        };
        match step {
            Ok(next) => poke32(bus, var(kvar::LOAD_PHASE, slot), next),
            Err(result) => {
                clear_cache(bus, slot);
                poke32(bus, var(kvar::LOAD_PHASE, slot), 0);
                poke32(bus, var(kvar::LOAD_INDEX, slot), 0);
                return Some(result);
            }
        }
    }
}

/// Take directory frame `index` (1..15) from the buffer into the cache if
/// it is sound: its checksum holds and its allocation state is one of the
/// seven psx-spx lists.
fn directory_frame_ok(bus: &mut Bus, slot: u32, index: u32) -> bool {
    let buf = buffer(slot);
    let state = peek32(bus, buf);
    let valid = matches!(state, FIRST | MIDDLE | LAST | 0xA0..=0xA3);
    if !valid || !frame_ok(bus, buf) {
        return false;
    }
    for off in (0..DIR_ENTRY_SIZE).step_by(4) {
        let word = peek32(bus, buf + off);
        poke32(bus, dir_entry(slot, index - 1) + off, word);
    }
    true
}

/// Directory frames written after changing the cache, then a probe: the
/// frames of `DIRTY` in ascending order with `LAST_ENTRY` after the rest
/// (the first block of a file goes last, so a write that stops half way
/// leaves blocks nobody points at, not a file that points at garbage), and
/// an info command, because psx-spx says a write error only shows on the
/// next command. `None` while a sector is on its way, then the outcome.
fn write_toc(bus: &mut Bus, slot: u32) -> Option<u32> {
    let device = slot << 4;
    loop {
        let dirty = peek32(bus, kvar::DIRTY);
        let last = peek32(bus, kvar::LAST_ENTRY);
        let pending = |i: u32| dirty & (1 << i) != 0;
        let next = (0..ENTRIES)
            .find(|&i| pending(i) && i != last)
            .or((last < ENTRIES && pending(last)).then_some(last));
        let Some(entry) = next else {
            let result = transfer(bus, device, 0, 0, Io::Info)?;
            poke32(bus, kvar::LAST_ENTRY, 0xFF);
            return Some(result);
        };
        frame_from_entry(bus, slot, entry);
        match transfer(bus, device, entry + 1, buffer(slot), Io::Write)? {
            outcome::OK => poke32(bus, kvar::DIRTY, dirty & !(1 << entry)),
            other => {
                poke32(bus, kvar::DIRTY, 0);
                return Some(other);
            }
        }
    }
}

/// The frame a format writes to `frame`, built in the sector buffer
/// (psx-spx "Memory Card Data Format"): the header and its copy in the
/// write test frame, directory frames that are free and unchained, broken
/// list frames with no broken sector.
fn format_frame(bus: &mut Bus, slot: u32, frame: u32) {
    let buf = buffer(slot);
    clear_buffer(bus, buf, 0);
    match frame {
        0 | WRITE_TEST_FRAME => {
            bus.write8_safe(buf, b'M');
            bus.write8_safe(buf + 1, b'C');
        }
        1..=15 => {
            bus.write8_safe(buf, FREE as u8);
            bus.write8_safe(buf + 8, 0xFF);
            bus.write8_safe(buf + 9, 0xFF);
        }
        _ => poke32(bus, buf, u32::MAX),
    }
    seal_frame(bus, buf);
}

/// Format the card of `slot`: the broken list, the directory, the write
/// test frame and last the header, then an info probe. File data is left
/// where it is. The cache becomes an empty directory. `None` while a sector
/// is on its way, then the outcome.
fn write_format(bus: &mut Bus, slot: u32) -> Option<u32> {
    const PROBE: u32 = 37;
    let device = slot << 4;
    loop {
        let i = peek32(bus, var(kvar::TOC_INDEX, slot));
        if i == PROBE {
            let result = transfer(bus, device, 0, 0, Io::Info)?;
            poke32(bus, var(kvar::TOC_INDEX, slot), 0);
            if result == outcome::OK {
                clear_cache(bus, slot);
                for entry in 0..ENTRIES {
                    clear_entry(bus, slot, entry);
                }
                poke32(bus, var(kvar::LOADED, slot), 1);
            }
            return Some(result);
        }
        let frame = match i {
            0..=19 => 16 + i,
            20..=34 => i - 19,
            35 => WRITE_TEST_FRAME,
            _ => 0,
        };
        format_frame(bus, slot, frame);
        match transfer(bus, device, frame, buffer(slot), Io::Write)? {
            outcome::OK => poke32(bus, var(kvar::TOC_INDEX, slot), i + 1),
            other => {
                poke32(bus, var(kvar::TOC_INDEX, slot), 0);
                clear_cache(bus, slot);
                return Some(other);
            }
        }
    }
}

// -------------------------------------------- _bu_init, _card_load, _card_info

/// A(55h)/A(70h) `_bu_init`: load the directory of both slots, one after the
/// other, waiting for each sector. A slot without a card or without a
/// usable one is left with an empty cache and does not stop the call.
/// Returns 0. `None` while it waits.
pub fn bu_init(bus: &mut Bus) -> Option<u32> {
    loop {
        let slot = peek32(bus, kvar::INIT_SLOT);
        if slot >= 2 {
            poke32(bus, kvar::INIT_SLOT, 0);
            return Some(0);
        }
        load_directory(bus, slot)?;
        poke32(bus, kvar::INIT_SLOT, slot + 1);
    }
}

/// Whether [`bu_init`] would just keep waiting, without changing anything:
/// a sector it asked for is still with the card driver.
pub(crate) fn bu_init_waiting(bus: &Bus) -> bool {
    {
        let slot = peek32(bus, kvar::INIT_SLOT).min(1);
        peek32(bus, var(kvar::IO_PENDING, slot)) != 0 && !card::idle(bus, slot)
    }
}

/// Whether the slot has a high-level operation running or a request with
/// the card driver.
fn busy(bus: &Bus, slot: u32) -> bool {
    peek32(bus, var(kvar::BU_OP, slot)) != bu_op::NONE || !card::idle(bus, slot)
}

/// A(ACh) `_card_load(port)`: read the directory in the background. The
/// SwCARD event says how it went. Returns 1, or 0 when the slot is busy.
pub fn card_load(bus: &mut Bus, device: u32) -> u32 {
    let slot = card::slot_of(device);
    if busy(bus, slot) || peek32(bus, var(kvar::IO_PENDING, slot)) != 0 {
        return 0;
    }
    undeliver_events(bus);
    poke32(bus, var(kvar::BU_OP, slot), bu_op::LOAD);
    // Start the first sector now; the completion hook carries on.
    background_step(bus, slot);
    1
}

/// A(ABh) `_card_info(port)`: the info command, then the SwCARD event.
/// Returns 1, or 0 when the slot is busy.
pub fn card_info(bus: &mut Bus, device: u32) -> u32 {
    let slot = card::slot_of(device);
    if busy(bus, slot) {
        return 0;
    }
    undeliver_events(bus);
    poke32(bus, var(kvar::BU_OP, slot), bu_op::INFO);
    card::card_info_internal(bus, device)
}

/// The card driver finished a command on `slot`: carry on whatever the
/// backup unit started in the background there.
pub fn low_level_done(bus: &mut Bus, slot: u32) {
    match peek32(bus, var(kvar::BU_OP, slot)) {
        bu_op::INFO => {
            poke32(bus, var(kvar::BU_OP, slot), bu_op::NONE);
            let result = card::last_outcome(bus, slot);
            notify(bus, result);
        }
        bu_op::LOAD => background_step(bus, slot),
        bu_op::READ | bu_op::WRITE => file_step(bus, slot),
        _ => {}
    }
}

/// One step of a background directory load.
fn background_step(bus: &mut Bus, slot: u32) {
    if let Some(result) = load_directory(bus, slot) {
        poke32(bus, var(kvar::BU_OP, slot), bu_op::NONE);
        notify(bus, result);
    }
}

/// A(A7h) `bufs_cb_0`: the card operation went well.
pub fn low_level_completed(bus: &mut Bus) {
    notify(bus, outcome::OK);
}

/// A(A8h) `bufs_cb_1` (`which` 0): general error; A(A9h) `bufs_cb_2` (1):
/// busy; A(AAh) `bufs_cb_3` (2): card changed or unformatted; A(AEh)
/// `bufs_cb_4` (3): write error.
pub fn low_level_error(bus: &mut Bus, which: u32) {
    notify(
        bus,
        match which {
            1 => outcome::TIMEOUT,
            2 => outcome::CHANGED,
            _ => outcome::ERROR,
        },
    );
}

// ------------------------------------------------------- the "bu" device

/// Slot of the file's device (its FCB holds the port number).
fn slot_of(bus: &Bus, f: u32) -> u32 {
    card::slot_of(device(bus, f))
}

fn device(bus: &Bus, f: u32) -> u32 {
    peek32(bus, f + fcb::DEVICE_ID)
}

/// Leave the file function: nothing in progress any more.
fn reset(bus: &mut Bus) {
    for addr in [kvar::PHASE, kvar::INDEX, kvar::AUX, kvar::AUX + 4, kvar::DIRTY] {
        poke32(bus, addr, 0);
    }
    poke32(bus, kvar::LAST_ENTRY, 0xFF);
}

/// Fail with `e` in the FCB: -1.
fn fail(bus: &mut Bus, f: u32, e: u32) -> Option<u32> {
    reset(bus);
    poke32(bus, f + fcb::ERROR, e);
    Some(u32::MAX)
}

/// Succeed with `v`.
fn done(bus: &mut Bus, v: u32) -> Option<u32> {
    reset(bus);
    Some(v)
}

/// A write to the card went wrong: the cache cannot be trusted, drop it so
/// the next call reloads the directory.
fn fail_io(bus: &mut Bus, f: u32, slot: u32) -> Option<u32> {
    poke32(bus, var(kvar::LOADED, slot), 0);
    fail(bus, f, err::IO)
}

/// The directory of `slot` is cached: loaded now if it was not. `None`
/// while loading; `Some(false)` when the card could not be read.
fn ensure_directory(bus: &mut Bus, slot: u32) -> Option<bool> {
    if loaded(bus, slot) {
        return Some(true);
    }
    Some(load_directory(bus, slot)? == outcome::OK)
}

/// The guest string at `addr` as bytes (at most 20 and a zero).
fn name_bytes(bus: &Bus, addr: u32) -> Vec<u8> {
    (0..21)
        .map(|i| bus.try_read8(addr.wrapping_add(i)).unwrap_or(0))
        .take_while(|&b| b != 0)
        .collect()
}

/// Whether `name` (a directory name, 20 characters at most) matches
/// `pattern` the way psx-spx describes: `?` stands for any one character,
/// `*` (only when `star` is set, firstfile and nextfile) for everything
/// that follows, and psx-spx's bug is kept: a `?` that lands on the name's
/// ending zero ends the comparison with a match.
fn pattern_match(name: &[u8], pattern: &[u8], star: bool) -> bool {
    for (i, &p) in pattern.iter().enumerate() {
        if star && p == b'*' {
            return true;
        }
        let n = name.get(i).copied().unwrap_or(0);
        if p == b'?' {
            if n == 0 {
                return true;
            }
        } else if p != n {
            return false;
        }
    }
    name.len() <= pattern.len()
}

/// First entry from `start` on that is in `state` and whose name matches.
fn find_entry(bus: &Bus, slot: u32, start: u32, state: u32, pattern: &[u8], star: bool) -> Option<u32> {
    (start..ENTRIES).find(|&i| {
        entry_state(bus, slot, i) == state && {
            let name = entry_name(bus, slot, i);
            let len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
            pattern_match(&name[..len], pattern, star)
        }
    })
}

/// Open mode and the FCB's file fields for the file whose first block is
/// directory entry `first`.
fn attach_file(bus: &mut Bus, f: u32, slot: u32, first: u32) {
    poke32(bus, f + fcb::SIZE, entry_size(bus, slot, first));
    poke32(bus, f + fcb::LBA, first);
    poke32(bus, f + fcb::FPOS, 0);
}

/// Phases of [`open`].
mod open_phase {
    /// Sector 0 is read, which tells whether the card was changed.
    pub const CHECK: u32 = 1;
    /// The directory is cached.
    pub const DIRECTORY: u32 = 2;
    /// The new file's directory frames are written.
    pub const CREATE: u32 = 3;
}

/// open(fcb, name, mode) of the "bu" device. A synchronous open reads sector
/// 0 first (psx-spx lists it as a pointless extra); a card that says it was
/// changed has its directory loaded again. An existing file's size comes
/// from its directory entry. With mode bit 9 a new file of `mode >> 16`
/// blocks is created in the first free blocks (states A0h..A3h count as
/// free): error 11h if the name is taken, 1Ch if the blocks do not exist.
/// Returns 0, or -1 with the error in the FCB; `None` while waiting.
pub fn open(bus: &mut Bus, f: u32, name: u32, mode: u32) -> Option<u32> {
    let slot = slot_of(bus, f);
    let pattern = name_bytes(bus, name);
    if peek32(bus, kvar::PHASE) == IDLE {
        if busy(bus, slot) {
            return fail(bus, f, err::IO);
        }
        let first = if mode & MODE_ASYNC == 0 { open_phase::CHECK } else { open_phase::DIRECTORY };
        poke32(bus, kvar::PHASE, first);
    }
    if peek32(bus, kvar::PHASE) == open_phase::CHECK {
        match transfer(bus, device(bus, f), 0, buffer(slot), Io::Read)? {
            outcome::OK => {}
            outcome::CHANGED => poke32(bus, var(kvar::LOADED, slot), 0),
            _ => return fail(bus, f, err::IO),
        }
        poke32(bus, kvar::PHASE, open_phase::DIRECTORY);
    }
    if peek32(bus, kvar::PHASE) == open_phase::DIRECTORY {
        if !ensure_directory(bus, slot)? {
            return fail(bus, f, err::IO);
        }
        if mode & MODE_CREATE == 0 {
            return match find_entry(bus, slot, 0, FIRST, &pattern, false) {
                Some(first) => {
                    attach_file(bus, f, slot, first);
                    done(bus, 0)
                }
                None => fail(bus, f, err::NOENT),
            };
        }
        match create_file(bus, slot, &pattern, mode >> 16) {
            Ok(first) => {
                poke32(bus, kvar::AUX, first);
                poke32(bus, kvar::PHASE, open_phase::CREATE);
            }
            Err(e) => return fail(bus, f, e),
        }
    }
    // Create: the directory frames go to the card.
    match write_toc(bus, slot)? {
        outcome::OK => {
            let first = peek32(bus, kvar::AUX);
            attach_file(bus, f, slot, first);
            done(bus, 0)
        }
        _ => fail_io(bus, f, slot),
    }
}

/// Allocate `blocks` blocks for a file called `name` in the cache and mark
/// their frames to be written; returns the first entry. The entries chain
/// in ascending order, their states being 51h for the first block, 53h for
/// the last and 52h between (psx-spx); only the first carries the size and
/// the name.
fn create_file(bus: &mut Bus, slot: u32, name: &[u8], blocks: u32) -> Result<u32, u32> {
    if blocks == 0 || blocks > ENTRIES {
        return Err(err::INVAL);
    }
    if find_entry(bus, slot, 0, FIRST, name, false).is_some() {
        return Err(err::EXIST);
    }
    let free: Vec<u32> = (0..ENTRIES)
        .filter(|&i| matches!(entry_state(bus, slot, i), 0xA0..=0xA3))
        .take(blocks as usize)
        .collect();
    if (free.len() as u32) < blocks {
        return Err(err::NOSPC);
    }
    for (k, &entry) in free.iter().enumerate() {
        clear_entry(bus, slot, entry);
        let last = k + 1 == free.len();
        let state = match (k, last) {
            (0, _) => FIRST,
            (_, true) => LAST,
            _ => MIDDLE,
        };
        poke32(bus, dir_entry(slot, entry), state);
        set_next(bus, slot, entry, free.get(k + 1).copied().unwrap_or(NO_NEXT));
    }
    poke32(bus, dir_entry(slot, free[0]) + 4, blocks * BLOCK);
    set_name(bus, slot, free[0], name);
    poke32(bus, kvar::DIRTY, free.iter().fold(0, |mask, &i| mask | 1 << i));
    poke32(bus, kvar::LAST_ENTRY, free[0]);
    Ok(free[0])
}

/// close(fcb): nothing to do; the data is on the card already.
pub fn close(_bus: &mut Bus, _f: u32) -> u32 {
    0
}

/// read(fcb, dst, len) and write(fcb, src, len) of the "bu" device: whole
/// sectors from the file position, which must be sector aligned and inside
/// the file (error 16h), `len` rounded down to sectors and cut at the end of
/// the file. A file opened with mode bit 15 returns `len` at once and the
/// transfer finishes in the background (see [`file_step`]); otherwise the
/// call waits for every sector. Returns the byte count, or -1 with the
/// error in the FCB; `None` while waiting.
pub fn read_write(bus: &mut Bus, f: u32, buf: u32, len: u32, write: bool) -> Option<u32> {
    let slot = slot_of(bus, f);
    let device = device(bus, f);
    let pos = peek32(bus, f + fcb::FPOS);
    if peek32(bus, kvar::PHASE) == IDLE {
        let size = peek32(bus, f + fcb::SIZE);
        if busy(bus, slot) {
            return fail(bus, f, err::IO);
        }
        if pos % SECTOR != 0 || pos >= size || !loaded(bus, slot) {
            return fail(bus, f, if loaded(bus, slot) { err::INVAL } else { err::IO });
        }
        let sectors = len.min(size - pos) / SECTOR;
        if sectors == 0 {
            return fail(bus, f, err::INVAL);
        }
        poke32(bus, kvar::PHASE, 1);
        poke32(bus, kvar::AUX, sectors);
        poke32(bus, kvar::INDEX, 0);
        if peek32(bus, f + fcb::STATUS) & MODE_ASYNC != 0 {
            let op = if write { bu_op::WRITE } else { bu_op::READ };
            for (k, value) in [f, 0, sectors, buf].into_iter().enumerate() {
                poke32(bus, kvar::ASYNC + 4 * k as u32, value);
            }
            poke32(bus, kvar::ASYNC + 16, 0);
            poke32(bus, var(kvar::BU_OP, slot), op);
            undeliver_events(bus);
            reset(bus);
            file_step(bus, slot);
            return Some(sectors * SECTOR);
        }
    }
    let first = peek32(bus, f + fcb::LBA);
    loop {
        let done_sectors = peek32(bus, kvar::INDEX);
        let total = peek32(bus, kvar::AUX);
        if done_sectors == total {
            poke32(bus, f + fcb::FPOS, pos + total * SECTOR);
            return done(bus, total * SECTOR);
        }
        let Some(sector) = file_sector(bus, slot, first, pos + done_sectors * SECTOR) else {
            return fail(bus, f, err::INVAL);
        };
        let at = buf.wrapping_add(done_sectors * SECTOR);
        let io = if write { Io::Write } else { Io::Read };
        match transfer(bus, device, sector, at, io)? {
            outcome::OK => poke32(bus, kvar::INDEX, done_sectors + 1),
            _ => return fail(bus, f, err::IO),
        }
    }
}

/// One step of an asynchronous file transfer, run by the card driver's
/// completion hook (and once to start it). The sector that just finished
/// is counted; the next one is asked for; after the last, the file position
/// moves and the event goes out: class = the file handle, spec 4, when it
/// went well (psx-spx "BIOS Event Summary": "card file handle done okay"),
/// else the SwCARD error event.
fn file_step(bus: &mut Bus, slot: u32) {
    let f = peek32(bus, kvar::ASYNC);
    let (done_sectors, total, buf) = (
        peek32(bus, kvar::ASYNC + 4),
        peek32(bus, kvar::ASYNC + 8),
        peek32(bus, kvar::ASYNC + 12),
    );
    let write = peek32(bus, var(kvar::BU_OP, slot)) == bu_op::WRITE;
    let pos = peek32(bus, f + fcb::FPOS);
    // The first call has nothing finished yet; later ones follow a sector.
    let started = peek32(bus, kvar::ASYNC + 16) != 0;
    let mut finished = done_sectors;
    if started {
        let result = card::last_outcome(bus, slot);
        if result != outcome::OK {
            poke32(bus, var(kvar::BU_OP, slot), bu_op::NONE);
            poke32(bus, kvar::ASYNC + 16, 0);
            poke32(bus, var(kvar::LOADED, slot), 0);
            return notify(bus, result);
        }
        finished += 1;
        poke32(bus, kvar::ASYNC + 4, finished);
    }
    if finished == total {
        poke32(bus, f + fcb::FPOS, pos + total * SECTOR);
        poke32(bus, var(kvar::BU_OP, slot), bu_op::NONE);
        poke32(bus, kvar::ASYNC + 16, 0);
        let handle = peek32(bus, f + fcb::NUMBER);
        return ex::queue_event(bus, handle, 0x4);
    }
    let first = peek32(bus, f + fcb::LBA);
    let Some(sector) = file_sector(bus, slot, first, pos + finished * SECTOR) else {
        poke32(bus, var(kvar::BU_OP, slot), bu_op::NONE);
        poke32(bus, kvar::ASYNC + 16, 0);
        return notify(bus, outcome::ERROR);
    };
    let at = buf.wrapping_add(finished * SECTOR);
    let device = device(bus, f);
    let queued = if write {
        card::card_write(bus, device, sector, at)
    } else {
        card::card_read(bus, device, sector, at)
    };
    poke32(bus, kvar::ASYNC + 16, 1);
    if queued == 0 {
        poke32(bus, var(kvar::BU_OP, slot), bu_op::NONE);
        poke32(bus, kvar::ASYNC + 16, 0);
        notify(bus, outcome::ERROR);
    }
}

/// Phases shared by erase, undelete and rename.
mod edit_phase {
    /// The directory is cached.
    pub const DIRECTORY: u32 = 1;
    /// The changed frames are written.
    pub const WRITE: u32 = 2;
}

/// Run a directory edit: make sure the directory is cached, let `edit`
/// change the cache and mark the frames to write (it returns the error to
/// fail with, if any), then write them. Returns 0, or -1 with the error;
/// `None` while waiting.
fn edit_directory(
    bus: &mut Bus,
    f: u32,
    edit: impl FnOnce(&mut Bus, u32) -> Result<(), u32>,
) -> Option<u32> {
    let slot = slot_of(bus, f);
    if peek32(bus, kvar::PHASE) == IDLE {
        if busy(bus, slot) {
            return fail(bus, f, err::IO);
        }
        poke32(bus, kvar::PHASE, edit_phase::DIRECTORY);
    }
    if peek32(bus, kvar::PHASE) == edit_phase::DIRECTORY {
        if !ensure_directory(bus, slot)? {
            return fail(bus, f, err::IO);
        }
        if let Err(e) = edit(bus, slot) {
            return fail(bus, f, e);
        }
        poke32(bus, kvar::PHASE, edit_phase::WRITE);
    }
    match write_toc(bus, slot)? {
        outcome::OK => done(bus, 0),
        _ => fail_io(bus, f, slot),
    }
}

/// The entries of the file whose first block is `first`, in chain order.
fn chain(bus: &Bus, slot: u32, first: u32) -> Vec<u32> {
    let mut entries = vec![first];
    while let next @ 0..=14 = entry_next(bus, slot, *entries.last().unwrap_or(&first)) {
        if entries.contains(&next) {
            break;
        }
        entries.push(next);
    }
    entries
}

/// erase(fcb, name): the file's blocks become deleted blocks (51h to A1h,
/// 52h to A2h, 53h to A3h), keeping their chain so the file can be brought
/// back; the data stays where it is. Error 2 if there is no such file.
pub fn erase(bus: &mut Bus, f: u32, name: u32) -> Option<u32> {
    let pattern = name_bytes(bus, name);
    edit_directory(bus, f, |bus, slot| {
        let first = find_entry(bus, slot, 0, FIRST, &pattern, false).ok_or(err::NOENT)?;
        let blocks = chain(bus, slot, first);
        for &entry in &blocks {
            let state = entry_state(bus, slot, entry);
            poke32(bus, dir_entry(slot, entry), state + 0x50);
        }
        poke32(bus, kvar::DIRTY, blocks.iter().fold(0, |mask, &i| mask | 1 << i));
        poke32(bus, kvar::LAST_ENTRY, first);
        Ok(())
    })
}

/// undelete(fcb, name): the reverse of erase, for a deleted file whose
/// blocks are all still deleted (A1h..A3h). Error 2 if there is no such
/// deleted file, 11h if one of its blocks has been taken since.
pub fn undelete(bus: &mut Bus, f: u32, name: u32) -> Option<u32> {
    let pattern = name_bytes(bus, name);
    edit_directory(bus, f, |bus, slot| {
        let first = find_entry(bus, slot, 0, DELETED_FIRST, &pattern, false).ok_or(err::NOENT)?;
        let blocks = chain(bus, slot, first);
        if blocks.iter().any(|&entry| !matches!(entry_state(bus, slot, entry), 0xA1..=0xA3)) {
            return Err(err::EXIST);
        }
        for &entry in &blocks {
            let state = entry_state(bus, slot, entry);
            poke32(bus, dir_entry(slot, entry), state - 0x50);
        }
        poke32(bus, kvar::DIRTY, blocks.iter().fold(0, |mask, &i| mask | 1 << i));
        poke32(bus, kvar::LAST_ENTRY, first);
        Ok(())
    })
}

/// rename(fcb1, old, new): the first block's name changes; error 2 if there
/// is no such file, 11h if the new name is taken.
pub fn rename(bus: &mut Bus, f: u32, old: u32, new: u32) -> Option<u32> {
    let (old, new) = (name_bytes(bus, old), name_bytes(bus, new));
    edit_directory(bus, f, |bus, slot| {
        let first = find_entry(bus, slot, 0, FIRST, &old, false).ok_or(err::NOENT)?;
        if find_entry(bus, slot, 0, FIRST, &new, false).is_some() {
            return Err(err::EXIST);
        }
        set_name(bus, slot, first, &new);
        poke32(bus, kvar::DIRTY, 1 << first);
        Ok(())
    })
}

/// format(fcb): [`write_format`] on the file's card. Returns 0, or -1.
pub fn format(bus: &mut Bus, f: u32) -> Option<u32> {
    let slot = slot_of(bus, f);
    if peek32(bus, kvar::PHASE) == IDLE {
        if busy(bus, slot) {
            return fail(bus, f, err::IO);
        }
        poke32(bus, kvar::PHASE, 1);
    }
    match write_format(bus, slot)? {
        outcome::OK => done(bus, 0),
        _ => fail(bus, f, err::IO),
    }
}

/// Fill the direntry at `dst` for directory entry `i` (psx-spx "BIOS File
/// Functions": name, attribute 50h for a file or A0h for a deleted one,
/// size, an unused next pointer, first sector number, a reserved word).
fn fill_direntry(bus: &mut Bus, slot: u32, i: u32, dst: u32) {
    let name = entry_name(bus, slot, i);
    for k in 0..0x14 {
        bus.write8_safe(dst + k, if k < 20 { name[k as usize] } else { 0 });
    }
    let attribute = if entry_state(bus, slot, i) == FIRST { 0x50 } else { 0xA0 };
    poke32(bus, dst + 0x14, attribute);
    poke32(bus, dst + 0x18, entry_size(bus, slot, i));
    poke32(bus, dst + 0x1C, 0);
    poke32(bus, dst + 0x20, (i + 1) * 64);
    poke32(bus, dst + 0x24, 0);
}

/// The next entry of the search from `start`: files, or deleted files when
/// the find mode (C(1Ah)) says so. Fills the direntry and remembers where
/// to go on; returns the direntry, or 0.
fn next_match(bus: &mut Bus, slot: u32, start: u32, direntry: u32) -> u32 {
    let mut pattern = Vec::new();
    for k in 0..21 {
        match bus.try_read8(PATTERN + k).unwrap_or(0) {
            0 => break,
            b => pattern.push(b),
        }
    }
    let state = if peek32(bus, kvar::FIND_MODE) == 0 { FIRST } else { DELETED_FIRST };
    match find_entry(bus, slot, start, state, &pattern, true) {
        Some(i) => {
            fill_direntry(bus, slot, i, direntry);
            poke32(bus, kvar::FIND_INDEX, i + 1);
            direntry
        }
        None => {
            poke32(bus, kvar::FIND_INDEX, ENTRIES);
            0
        }
    }
}

/// firstfile(fcb, name, direntry): remember the name, make sure the
/// directory is cached (loading it takes card time; nothing else touches
/// the card) and answer with the first match. Returns the direntry, or 0.
pub fn firstfile(bus: &mut Bus, f: u32, name: u32, direntry: u32) -> Option<u32> {
    let slot = slot_of(bus, f);
    if peek32(bus, kvar::PHASE) == IDLE {
        if busy(bus, slot) {
            return Some(0);
        }
        poke32(bus, kvar::PHASE, 1);
        for k in 0..21 {
            let byte = if k < 20 { bus.try_read8(name.wrapping_add(k)).unwrap_or(0) } else { 0 };
            bus.write8_safe(PATTERN + k, byte);
        }
    }
    if !ensure_directory(bus, slot)? {
        return done(bus, 0);
    }
    let found = next_match(bus, slot, 0, direntry);
    done(bus, found)
}

/// nextfile(fcb, direntry): the next match of the search firstfile began,
/// or 0.
pub fn nextfile(bus: &mut Bus, f: u32, direntry: u32) -> u32 {
    let slot = slot_of(bus, f);
    if !loaded(bus, slot) {
        return 0;
    }
    let start = peek32(bus, kvar::FIND_INDEX);
    next_match(bus, slot, start, direntry)
}

