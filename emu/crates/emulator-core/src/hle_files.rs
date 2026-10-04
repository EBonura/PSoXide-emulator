// SPDX-License-Identifier: GPL-2.0-or-later
//! File and device layer of the HLE kernel, the TTY and CD-ROM devices, and
//! the kernel's CD-ROM driver.
//!
//! Sources: psx-spx "BIOS File Functions" (the file functions, the error
//! numbers, the firstfile/nextfile search FCB and the handles the
//! maintenance calls take), "BIOS Control Blocks" (FCB 2Ch bytes at 8648h,
//! DCB 50h bytes at 6EE0h and their fields), "BIOS CDROM Functions",
//! "BIOS Event Summary" and "BIOS Interrupt/Exception Handling" (the CD-ROM
//! events and the priority 0 chain). psx-spx does not say what a device
//! function answers; the convention here is that open, close and the
//! maintenance functions (format, erase, rename, undelete) answer 0 for
//! success and anything else for failure, that read, write and in_out
//! answer the byte count or -1, that a failing driver leaves the reason in
//! the FCB's error field, and that firstfile and nextfile answer the
//! direntry or 0.
//!
//! Device functions are called through the DCB in RAM, because games add
//! their own devices (WipEout's "sio:") and replace driver entry points
//! (the census saw writes to the memory card DCB). A file function that
//! calls a driver pushes a small frame on the guest stack and returns into
//! a continuation trap, so no state lives on the host.
//!
//! The CD-ROM driver talks to the emulated drive through its registers and
//! DMA channel 3 (Setloc, ReadN, one DMA per sector, Pause), so seek and
//! read timing are the drive's own. While it runs it masks the drive's
//! interrupt output and polls the flags for its blocking operations; a
//! waiting call is retried with interrupts serviced.

use crate::hle_kernel::{peek32, poke32, stub_addr};
use crate::Bus;

/// File control blocks.
pub const FCB_BASE: u32 = crate::hle_kernel::FCB_BASE;
/// Bytes per FCB.
pub const FCB_SIZE: u32 = 0x2C;
/// FCB count.
pub const FCB_COUNT: u32 = 16;
/// Device control blocks.
pub const DCB_BASE: u32 = crate::hle_kernel::DCB_BASE;
/// Bytes per DCB.
pub const DCB_SIZE: u32 = 0x50;
/// DCB count.
pub const DCB_COUNT: u32 = 10;

/// FCB fields.
pub mod fcb {
    /// Access mode, 0 = free.
    pub const STATUS: u32 = 0x00;
    /// Device number (cdrom: disk id).
    pub const DEVICE_ID: u32 = 0x04;
    /// Transfer address for `in_out`.
    pub const TADDR: u32 = 0x08;
    /// Transfer length for `in_out`.
    pub const TLEN: u32 = 0x0C;
    /// File position.
    pub const FPOS: u32 = 0x10;
    /// Copy of the DCB flags.
    pub const DEVICE_FLAGS: u32 = 0x14;
    /// Error code.
    pub const ERROR: u32 = 0x18;
    /// DCB of the file.
    pub const DCB: u32 = 0x1C;
    /// File size.
    pub const SIZE: u32 = 0x20;
    /// First sector.
    pub const LBA: u32 = 0x24;
    /// FCB number.
    pub const NUMBER: u32 = 0x28;
}

/// DCB fields (function pointers).
pub mod dcb {
    /// Name pointer, 0 = free.
    pub const NAME: u32 = 0x00;
    /// Device flags.
    pub const FLAGS: u32 = 0x04;
    /// Sector size.
    pub const BLOCK: u32 = 0x08;
    /// Long name pointer.
    pub const DESC: u32 = 0x0C;
    /// init().
    pub const INIT: u32 = 0x10;
    /// open(fcb, name, mode).
    pub const OPEN: u32 = 0x14;
    /// in_out(fcb, cmd).
    pub const INOUT: u32 = 0x18;
    /// close(fcb).
    pub const CLOSE: u32 = 0x1C;
    /// ioctl(fcb, cmd, arg).
    pub const IOCTL: u32 = 0x20;
    /// read(fcb, dst, len).
    pub const READ: u32 = 0x24;
    /// write(fcb, src, len).
    pub const WRITE: u32 = 0x28;
    /// erase(fcb, name).
    pub const ERASE: u32 = 0x2C;
    /// undelete(fcb, name).
    pub const UNDELETE: u32 = 0x30;
    /// firstfile(fcb, name, direntry).
    pub const FIRSTFILE: u32 = 0x34;
    /// nextfile(fcb, direntry).
    pub const NEXTFILE: u32 = 0x38;
    /// format(fcb).
    pub const FORMAT: u32 = 0x3C;
    /// cd(fcb, path).
    pub const CHDIR: u32 = 0x40;
    /// rename(fcb1, name1, fcb2, name2).
    pub const RENAME: u32 = 0x44;
    /// remove().
    pub const DEINIT: u32 = 0x48;
    /// testdevice(fcb, name).
    pub const CHECK: u32 = 0x4C;
}

/// Device flag: filesystem device (psx-spx: cdrom and bu are 14h).
pub const DEV_FS: u32 = 0x10;
/// Device flag: block device, read and write by sector. Devices without
/// it (the TTY) move data through in_out.
pub const DEV_BLOCK: u32 = 0x04;

/// File error numbers (psx-spx).
pub mod errno {
    /// File not found.
    pub const NOENT: u32 = 0x02;
    /// Invalid or unused file handle.
    pub const BADF: u32 = 0x09;
    /// General error.
    pub const IO: u32 = 0x10;
    /// Unknown device name.
    pub const NODEV: u32 = 0x13;
    /// Sector alignment, fpos past the end, bad seek type.
    pub const INVAL: u32 = 0x16;
    /// A rename between two devices.
    pub const XDEV: u32 = 0x12;
    /// No free file handle.
    pub const MFILE: u32 = 0x18;
}

/// Kernel variables of the file layer and CD driver.
pub mod kvar {
    /// Last file error (B(54h)).
    pub const ERRNO: u32 = 0x0A90;
    /// CD events opened at boot (5 handles: 10h, 20h, 40h, 80h, 8000h).
    pub const CD_EVENTS: u32 = 0x0AA0;
    /// CD driver phase.
    pub const CD_PHASE: u32 = 0x0B00;
    /// Next LBA to read.
    pub const CD_LBA: u32 = 0x0B04;
    /// Sectors left.
    pub const CD_COUNT: u32 = 0x0B08;
    /// Destination of the next sector.
    pub const CD_DST: u32 = 0x0B0C;
    /// Drive interrupt enable to restore afterwards.
    pub const CD_SAVED_MASK: u32 = 0x0B10;
    /// Last drive interrupt type seen by the kernel IRQ handler.
    pub const CD_LAST_INT: u32 = 0x0B14;
    /// Response bytes captured by the kernel IRQ handler (16 bytes).
    pub const CD_RESPONSE: u32 = 0x0B18;
    /// Filesystem lookup phase.
    pub const FS_PHASE: u32 = 0x0B40;
    /// Path component being searched.
    pub const FS_COMP: u32 = 0x0B44;
    /// Directory sector being searched.
    pub const FS_DIR_LBA: u32 = 0x0B48;
    /// Directory bytes left from that sector on.
    pub const FS_DIR_LEFT: u32 = 0x0B4C;
    /// Root directory extent (0 = not read yet).
    pub const FS_ROOT_LBA: u32 = 0x0B50;
    /// Root directory size.
    pub const FS_ROOT_SIZE: u32 = 0x0B54;
    /// Lookup result: extent and size.
    pub const FS_FOUND_LBA: u32 = 0x0B58;
    /// Lookup result size.
    pub const FS_FOUND_SIZE: u32 = 0x0B5C;
    /// read() stage.
    pub const RD_STAGE: u32 = 0x0B60;
    /// read() byte count.
    pub const RD_LEN: u32 = 0x0B64;
    /// Executable loader stage.
    pub const LD_STAGE: u32 = 0x0B70;
    /// Executable extent.
    pub const LD_LBA: u32 = 0x0B74;
    /// Executable file size.
    pub const LD_SIZE: u32 = 0x0B78;
    /// firstfile/nextfile search FCB (0 = none yet).
    pub const FIND_FCB: u32 = 0x0B7C;
}

/// Sector buffer for directory reads and partial sectors (2 KiB).
pub const SECTOR_BUF: u32 = 0x0000_3200;
/// Device name strings.
const STRINGS: u32 = 0x0000_3180;
/// Header buffer for A(42h)/A(51h) into Exec.
pub const EXEC_HEADER: u32 = 0x0000_3A00;

/// Kernel-internal trap functions of the file layer and devices.
pub mod internal {
    /// Continuation of a file function: a device function it called has
    /// returned ([`super::continue_call`]).
    pub const CONTINUE: u8 = 0x10;
    /// Driver entry that does nothing and returns 0.
    pub const NOP: u8 = 0x1F;
    /// TTY in_out(fcb, cmd).
    pub const TTY_INOUT: u8 = 0x20;
    /// CD-ROM open(fcb, name, mode).
    pub const CD_OPEN: u8 = 0x28;
    /// CD-ROM read(fcb, dst, len).
    pub const CD_READ: u8 = 0x29;
    /// Kernel CD-ROM I/O IRQ verifier (priority 0).
    pub const CD_IO_IRQ: u8 = 0x38;
    /// Kernel CD-ROM DMA IRQ verifier (priority 0).
    pub const CD_DMA_IRQ: u8 = 0x39;
}

/// Chain elements of the kernel CD-ROM IRQ handlers (16 bytes each).
const HI_CD_IO: u32 = 0x0000_3140;
const HI_CD_DMA: u32 = 0x0000_3150;

fn fcb_addr(fd: u32) -> u32 {
    FCB_BASE + FCB_SIZE * fd
}

fn dcb_addr(i: u32) -> u32 {
    DCB_BASE + DCB_SIZE * i
}

/// Guest string at `addr` (at most `max` bytes).
pub fn read_cstr(bus: &Bus, addr: u32, max: u32) -> String {
    let mut s = String::new();
    for i in 0..max {
        let b = bus.try_read8(addr.wrapping_add(i)).unwrap_or(0);
        if b == 0 {
            break;
        }
        s.push(b as char);
    }
    s
}

fn write_cstr(bus: &mut Bus, addr: u32, s: &str) -> u32 {
    for (i, b) in s.bytes().chain([0]).enumerate() {
        bus.write8_safe(addr + i as u32, b);
    }
    addr + s.len() as u32 + 1
}

// ------------------------------------------------------------- boot state

struct KernelDevice {
    name: &'static str,
    desc: &'static str,
    flags: u32,
    block: u32,
    /// (DCB offset, internal function) pairs; everything else is NOP.
    entries: &'static [(u32, u8)],
}

const TTY: KernelDevice = KernelDevice {
    name: "tty",
    desc: "CONSOLE",
    flags: 1,
    block: 1,
    entries: &[(dcb::INOUT, internal::TTY_INOUT)],
};

const BU: KernelDevice = KernelDevice {
    name: "bu",
    desc: "MEMORY CARD",
    flags: 0x14,
    block: 0x80,
    entries: &[
        (dcb::OPEN, crate::hle_bu::internal::OPEN),
        (dcb::READ, crate::hle_bu::internal::READ),
        (dcb::WRITE, crate::hle_bu::internal::WRITE),
        (dcb::CLOSE, crate::hle_bu::internal::CLOSE),
        (dcb::ERASE, crate::hle_bu::internal::ERASE),
        (dcb::UNDELETE, crate::hle_bu::internal::UNDELETE),
        (dcb::FIRSTFILE, crate::hle_bu::internal::FIRSTFILE),
        (dcb::NEXTFILE, crate::hle_bu::internal::NEXTFILE),
        (dcb::FORMAT, crate::hle_bu::internal::FORMAT),
        (dcb::RENAME, crate::hle_bu::internal::RENAME),
    ],
};

const CDROM: KernelDevice = KernelDevice {
    name: "cdrom",
    desc: "CD-ROM",
    flags: 0x14,
    block: 0x800,
    entries: &[
        (dcb::OPEN, internal::CD_OPEN),
        (dcb::READ, internal::CD_READ),
    ],
};

/// Boot: the TTY and CD-ROM devices (the memory card joins with the card
/// driver), standard input/output on the TTY, and the CD-ROM driver's IRQ
/// handlers and events, as the retail kernel sets them up before Exec.
pub fn install(bus: &mut Bus) {
    let mut s = STRINGS;
    for (slot, dev) in [TTY, CDROM, BU].iter().enumerate() {
        let d = dcb_addr(slot as u32);
        let name = s;
        s = write_cstr(bus, s, dev.name);
        let desc = s;
        s = write_cstr(bus, s, dev.desc);
        poke32(bus, d + dcb::NAME, name);
        poke32(bus, d + dcb::FLAGS, dev.flags);
        poke32(bus, d + dcb::BLOCK, dev.block);
        poke32(bus, d + dcb::DESC, desc);
        for off in (dcb::INIT..DCB_SIZE).step_by(4) {
            poke32(bus, d + off, stub_addr(3, internal::NOP));
        }
        for (off, func) in dev.entries {
            poke32(bus, d + off, stub_addr(3, *func));
        }
    }
    for fd in 0..FCB_COUNT {
        poke32(bus, fcb_addr(fd) + fcb::NUMBER, fd);
    }
    for (fd, mode) in [(0, 1), (1, 2)] {
        let f = fcb_addr(fd);
        poke32(bus, f + fcb::STATUS, mode);
        poke32(bus, f + fcb::DCB, dcb_addr(0));
        poke32(bus, f + fcb::DEVICE_FLAGS, TTY.flags);
    }
    cd_install(bus);
}

/// Class of the kernel's CD-ROM events (psx-spx "BIOS Event Summary").
const CD_EVENT_CLASS: u32 = 0xF000_0003;
/// Specs of the five events the kernel opens for the drive.
const CD_EVENT_SPECS: [u32; 5] = [0x10, 0x20, 0x40, 0x80, 0x8000];

/// Set up the kernel's CD-ROM driver: its DMA and I/O interrupt handlers go
/// at the head of priority chain 0, ahead of the SYSCALL handler (psx-spx
/// "Priority Chains": CdromDmaIrq, CdromIoIrq, SyscallException), and it
/// opens and enables its five events. Each handler is one function that
/// returns from the exception when the interrupt was its own.
fn cd_install(bus: &mut Bus) {
    use crate::hle_exceptions as ex;
    for (element, func) in [
        (HI_CD_IO, internal::CD_IO_IRQ),
        (HI_CD_DMA, internal::CD_DMA_IRQ),
    ] {
        poke32(bus, element, 0);
        poke32(bus, element + 4, 0);
        poke32(bus, element + 8, stub_addr(3, func));
        poke32(bus, element + 12, 0);
        ex::enq_int(bus, 0, element);
    }
    for (i, spec) in CD_EVENT_SPECS.iter().enumerate() {
        let event = ex::open_event(bus, CD_EVENT_CLASS, *spec, ex::EV_MODE_READY, 0);
        ex::set_event_enabled(bus, event, true);
        poke32(bus, kvar::CD_EVENTS + 4 * i as u32, event);
    }
    poke32(bus, crate::hle_kernel::kvar::CD_KERNEL_ACTIVE, 1);
}

/// A(54h)/A(71h) _96_init: install the driver, replacing a set that is
/// already there so there is never more than one.
pub fn cd_init(bus: &mut Bus) {
    cd_remove(bus);
    cd_install(bus);
}

/// A(56h)/A(72h) _96_remove: close the driver's events and take its
/// handlers out of the chain. psx-spx says the retail function does not
/// manage this (its dequeue cannot reach priority 0 elements reliably);
/// here the removal works, since [`crate::hle_exceptions::deq_int`] does.
pub fn cd_remove(bus: &mut Bus) {
    use crate::hle_exceptions as ex;
    if peek32(bus, crate::hle_kernel::kvar::CD_KERNEL_ACTIVE) == 0 {
        return;
    }
    for i in 0..CD_EVENT_SPECS.len() as u32 {
        ex::close_event(bus, peek32(bus, kvar::CD_EVENTS + 4 * i));
    }
    ex::deq_int(bus, 0, HI_CD_DMA);
    ex::deq_int(bus, 0, HI_CD_IO);
    poke32(bus, crate::hle_kernel::kvar::CD_KERNEL_ACTIVE, 0);
}

// ------------------------------------------------------------ file layer

fn set_errno(bus: &mut Bus, e: u32) {
    poke32(bus, kvar::ERRNO, e);
}

/// B(54h) _get_errno.
pub fn errno(bus: &Bus) -> u32 {
    peek32(bus, kvar::ERRNO)
}

/// B(55h) _get_error(fd): FCB error, or FFFFFFFFh for a bad handle.
pub fn file_error(bus: &Bus, fd: u32) -> u32 {
    match open_fcb(bus, fd) {
        Some(f) => peek32(bus, f + fcb::ERROR),
        None => u32::MAX,
    }
}

fn open_fcb(bus: &Bus, fd: u32) -> Option<u32> {
    (fd < FCB_COUNT)
        .then(|| fcb_addr(fd))
        .filter(|&f| peek32(bus, f + fcb::STATUS) != 0)
}

fn free_fcb(bus: &Bus) -> Option<u32> {
    (0..FCB_COUNT).find(|&fd| peek32(bus, fcb_addr(fd) + fcb::STATUS) == 0)
}

/// A guest byte; zero where nothing is mapped.
fn byte_at(bus: &Bus, addr: u32) -> u8 {
    bus.try_read8(addr).unwrap_or(0)
}

/// The DCB of the device called `name` (case-sensitive, psx-spx "Device
/// Names").
fn find_dcb(bus: &Bus, name: &[u8]) -> Option<u32> {
    (0..DCB_COUNT).map(dcb_addr).find(|&d| {
        let at = peek32(bus, d + dcb::NAME);
        at != 0
            && (0..=name.len() as u32)
                .all(|i| byte_at(bus, at + i) == name.get(i as usize).copied().unwrap_or(0))
    })
}

/// Split a path "name12:rest" (psx-spx "Device Names"): the port number is
/// the run of hexadecimal digits (either case) that ends the name before
/// the colon, so a device name must not end in a hex digit. Returns the DCB
/// of the device, the port number and the address of what follows the
/// colon. `None` when the path has no colon within 32 characters or names
/// no device.
pub fn find_device(bus: &Bus, path: u32) -> Option<(u32, u32, u32)> {
    let colon = (0..32).find(|&i| matches!(byte_at(bus, path + i), b':' | 0))?;
    if byte_at(bus, path + colon) != b':' {
        return None;
    }
    let digits = (0..colon)
        .rev()
        .take_while(|&i| byte_at(bus, path + i).is_ascii_hexdigit())
        .count() as u32;
    let name_len = colon - digits;
    let port = (name_len..colon).fold(0u32, |port, i| {
        let digit = char::from(byte_at(bus, path + i)).to_digit(16).unwrap_or(0);
        port << 4 | digit
    });
    let name: Vec<u8> = (0..name_len).map(|i| byte_at(bus, path + i)).collect();
    Some((find_dcb(bus, &name)?, port, path + colon + 1))
}

/// Set up FCB `f` for a file or search on `dcb`: access mode `mode`
/// (nonzero marks it used), the port, position 0 and the device's flags.
fn prepare_fcb(bus: &mut Bus, f: u32, dcb: u32, port: u32, mode: u32) {
    poke32(bus, f + fcb::STATUS, mode);
    poke32(bus, f + fcb::DEVICE_ID, port);
    poke32(bus, f + fcb::TADDR, 0);
    poke32(bus, f + fcb::TLEN, 0);
    poke32(bus, f + fcb::FPOS, 0);
    poke32(bus, f + fcb::DEVICE_FLAGS, peek32(bus, dcb + dcb::FLAGS));
    poke32(bus, f + fcb::ERROR, 0);
    poke32(bus, f + fcb::DCB, dcb);
    poke32(bus, f + fcb::SIZE, 0);
    poke32(bus, f + fcb::LBA, 0);
}

/// What a file function does next: return to its caller, or continue at a
/// guest address (a device function it calls for the caller).
pub enum FileCall {
    /// Return this value.
    Return(u32),
    /// Jump to the guest function; its return lands in the continuation.
    Jump(u32),
}

/// Set the error number and fail with -1.
fn fail(bus: &mut Bus, e: u32) -> FileCall {
    set_errno(bus, e);
    FileCall::Return(u32::MAX)
}

/// Bytes the file layer pushes on the caller's stack while a device
/// function runs: 16 for the callee to spill its register arguments into,
/// then the caller's `ra`, what the continuation is to do ([`then`]), the
/// FCB or DCB it is about, and a spare word.
const FRAME: u32 = 0x20;

/// What the continuation does with the device function's result.
mod then {
    /// open: v0 = 0 keeps the FCB and answers its number.
    pub const OPEN: u32 = 1;
    /// read, write, in_out: v0 is the byte count or negative.
    pub const TRANSFER: u32 = 2;
    /// close: the FCB goes back first, v0 = 0 answers its number.
    pub const CLOSE: u32 = 3;
    /// format, erase, undelete, rename on a temporary FCB: it goes back,
    /// v0 = 0 answers 1.
    pub const MAINTENANCE: u32 = 4;
    /// AddDrv after the device's init(): answer 1.
    pub const ADD_DEVICE: u32 = 5;
    /// DelDrv after the device's remove(): the DCB is free, answer 1.
    pub const REMOVE_DEVICE: u32 = 6;
    /// firstfile, nextfile: v0 goes back unchanged.
    pub const PASS: u32 = 7;
}

/// Call the device function `target` for a file function. The caller's `ra`
/// and what the continuation needs are kept in a frame on the caller's
/// stack and the function returns into the continuation trap, so all state
/// is in guest memory.
fn call_device(
    bus: &mut Bus,
    gprs: &mut [u32; 32],
    target: u32,
    args: [u32; 4],
    what: u32,
    about: u32,
) -> FileCall {
    let sp = gprs[29].wrapping_sub(FRAME);
    poke32(bus, sp + 0x10, gprs[31]);
    poke32(bus, sp + 0x14, what);
    poke32(bus, sp + 0x18, about);
    gprs[29] = sp;
    gprs[31] = stub_addr(3, internal::CONTINUE);
    gprs[4..8].copy_from_slice(&args);
    FileCall::Jump(target)
}

/// The device function at DCB offset `entry`.
fn device_function(bus: &Bus, dcb: u32, entry: u32) -> u32 {
    peek32(bus, dcb + entry)
}

/// A device function failed: its reason is in the FCB (a driver that left
/// none counts as a general error). Sets the error number.
fn failure_reason(bus: &mut Bus, f: u32) {
    let reason = peek32(bus, f + fcb::ERROR);
    set_errno(bus, if reason == 0 { errno::IO } else { reason });
}

fn release_fcb(bus: &mut Bus, f: u32) {
    poke32(bus, f + fcb::STATUS, 0);
}

/// The continuation: a device function the file layer called has returned
/// `v0`. Takes the frame off the caller's stack, restores its `ra` and
/// answers the caller.
pub fn continue_call(bus: &mut Bus, gprs: &mut [u32; 32], v0: u32) -> u32 {
    let sp = gprs[29];
    gprs[31] = peek32(bus, sp + 0x10);
    gprs[29] = sp.wrapping_add(FRAME);
    let what = peek32(bus, sp + 0x14);
    let about = peek32(bus, sp + 0x18);
    match what {
        then::OPEN if v0 == 0 => peek32(bus, about + fcb::NUMBER),
        then::OPEN => {
            release_fcb(bus, about);
            failure_reason(bus, about);
            u32::MAX
        }
        then::TRANSFER if (v0 as i32) < 0 => {
            failure_reason(bus, about);
            u32::MAX
        }
        then::CLOSE => {
            release_fcb(bus, about);
            if v0 == 0 {
                peek32(bus, about + fcb::NUMBER)
            } else {
                failure_reason(bus, about);
                u32::MAX
            }
        }
        then::MAINTENANCE => {
            release_fcb(bus, about);
            if v0 != 0 {
                failure_reason(bus, about);
            }
            u32::from(v0 == 0)
        }
        then::ADD_DEVICE => 1,
        then::REMOVE_DEVICE => {
            poke32(bus, about + dcb::NAME, 0);
            1
        }
        _ => v0,
    }
}

/// B(32h)/A(00h) open(path, mode): the device is named by the path, a free
/// FCB is set up for it and the device's open(fcb, name, mode) decides.
/// Returns the handle, or -1 with the error number set: 13h for an unknown
/// device, 18h when all 16 handles are taken, the device's own otherwise.
pub fn open(bus: &mut Bus, gprs: &mut [u32; 32], path: u32, mode: u32) -> FileCall {
    let Some((dcb, port, name)) = find_device(bus, path) else {
        return fail(bus, errno::NODEV);
    };
    let Some(fd) = free_fcb(bus) else {
        return fail(bus, errno::MFILE);
    };
    let f = fcb_addr(fd);
    prepare_fcb(bus, f, dcb, port, mode);
    let open = device_function(bus, dcb, dcb::OPEN);
    call_device(bus, gprs, open, [f, name, mode, 0], then::OPEN, f)
}

/// B(33h)/A(01h) lseek(fd, offset, whence): whence 0 sets the position and 1
/// moves it. psx-spx lists the end-relative whence 2 as broken, and here it
/// leaves the position where it is. Nothing checks the file's size. Returns
/// the position; -1 with 09h for an unused handle or 16h for another
/// whence.
pub fn lseek(bus: &mut Bus, fd: u32, offset: u32, whence: u32) -> u32 {
    let Some(f) = open_fcb(bus, fd) else {
        set_errno(bus, errno::BADF);
        return u32::MAX;
    };
    let pos = peek32(bus, f + fcb::FPOS);
    let new = match whence {
        0 => offset,
        1 => pos.wrapping_add(offset),
        2 => pos,
        _ => {
            set_errno(bus, errno::INVAL);
            return u32::MAX;
        }
    };
    poke32(bus, f + fcb::FPOS, new);
    new
}

/// B(34h)/A(02h) read(fd, dst, len) and B(35h)/A(03h) write(fd, src, len)
/// (`which` 1 or 2). A block device gets the call as its read or write
/// function; a character device gets in_out(fcb, which) with the buffer and
/// length in the FCB. The device moves the file position. Returns the byte
/// count, or -1: 09h for an unused handle, 16h for no length, the device's
/// error otherwise.
pub fn read_write(
    bus: &mut Bus,
    gprs: &mut [u32; 32],
    fd: u32,
    buf: u32,
    len: u32,
    which: u32,
) -> FileCall {
    let Some(f) = open_fcb(bus, fd) else {
        return fail(bus, errno::BADF);
    };
    if len == 0 {
        return fail(bus, errno::INVAL);
    }
    let dcb = peek32(bus, f + fcb::DCB);
    if peek32(bus, f + fcb::DEVICE_FLAGS) & DEV_BLOCK == 0 {
        poke32(bus, f + fcb::TADDR, buf);
        poke32(bus, f + fcb::TLEN, len);
        let in_out = device_function(bus, dcb, dcb::INOUT);
        return call_device(bus, gprs, in_out, [f, which, 0, 0], then::TRANSFER, f);
    }
    let entry = if which == 1 { dcb::READ } else { dcb::WRITE };
    let transfer = device_function(bus, dcb, entry);
    call_device(bus, gprs, transfer, [f, buf, len, 0], then::TRANSFER, f)
}

/// B(36h)/A(04h) close(fd): the device's close(fcb) runs and the handle is
/// free whatever it answers. Returns the handle, or -1 (09h for an unused
/// handle, the device's error if it refused).
pub fn close(bus: &mut Bus, gprs: &mut [u32; 32], fd: u32) -> FileCall {
    let Some(f) = open_fcb(bus, fd) else {
        return fail(bus, errno::BADF);
    };
    let dcb = peek32(bus, f + fcb::DCB);
    let close = device_function(bus, dcb, dcb::CLOSE);
    call_device(bus, gprs, close, [f, 0, 0, 0], then::CLOSE, f)
}

/// B(47h) AddDrv(info): the 50h-byte device description goes into the first
/// free DCB and the device's init() runs. Returns 1, or 0 when all DCBs are
/// taken.
pub fn add_device(bus: &mut Bus, gprs: &mut [u32; 32], info: u32) -> FileCall {
    let Some(slot) = (0..DCB_COUNT).find(|&i| peek32(bus, dcb_addr(i) + dcb::NAME) == 0) else {
        return FileCall::Return(0);
    };
    let d = dcb_addr(slot);
    for off in (0..DCB_SIZE).step_by(4) {
        let word = peek32(bus, info.wrapping_add(off));
        poke32(bus, d + off, word);
    }
    let init = device_function(bus, d, dcb::INIT);
    call_device(bus, gprs, init, [0; 4], then::ADD_DEVICE, d)
}

/// B(48h) DelDrv(name): the device's remove() runs and its DCB is free.
/// Returns 1, or 0 when there is no such device.
pub fn remove_device(bus: &mut Bus, gprs: &mut [u32; 32], name: u32) -> FileCall {
    let name: Vec<u8> = (0..16)
        .map(|i| byte_at(bus, name + i))
        .take_while(|&b| b != 0)
        .collect();
    let Some(d) = find_dcb(bus, &name) else {
        return FileCall::Return(0);
    };
    let remove = device_function(bus, d, dcb::DEINIT);
    call_device(bus, gprs, remove, [0; 4], then::REMOVE_DEVICE, d)
}

/// A(96h) AddCDROMDevice: install the kernel CD-ROM device when absent.
pub fn add_kernel_cdrom(bus: &mut Bus) -> u32 {
    add_kernel_device(bus, 1)
}

/// A(97h) AddMemCardDevice: install the kernel memory card device when
/// absent.
pub fn add_kernel_memcard(bus: &mut Bus) -> u32 {
    add_kernel_device(bus, 2)
}

/// Install boot device `which` (index into the boot list, whose name
/// strings were written at boot) in the first free DCB unless a device of
/// that name exists. Returns 1, or 0 when no DCB is free.
fn add_kernel_device(bus: &mut Bus, which: usize) -> u32 {
    let devices = [TTY, CDROM, BU];
    let dev = &devices[which];
    let present = (0..DCB_COUNT).map(dcb_addr).any(|d| {
        let n = peek32(bus, d + dcb::NAME);
        n != 0 && read_cstr(bus, n, 16) == dev.name
    });
    if present {
        return 1;
    }
    let Some(slot) = (0..DCB_COUNT).find(|&i| peek32(bus, dcb_addr(i) + dcb::NAME) == 0) else {
        return 0;
    };
    let d = dcb_addr(slot);
    let name = devices[..which].iter().fold(STRINGS, |at, k| {
        at + k.name.len() as u32 + 1 + k.desc.len() as u32 + 1
    });
    poke32(bus, d + dcb::NAME, name);
    poke32(bus, d + dcb::FLAGS, dev.flags);
    poke32(bus, d + dcb::BLOCK, dev.block);
    poke32(bus, d + dcb::DESC, name + dev.name.len() as u32 + 1);
    for off in (dcb::INIT..DCB_SIZE).step_by(4) {
        poke32(bus, d + off, stub_addr(3, internal::NOP));
    }
    for (off, func) in dev.entries {
        poke32(bus, d + off, stub_addr(3, *func));
    }
    1
}

/// B(42h) firstfile2(name, direntry): psx-spx says the search runs on an FCB
/// that is taken once, the first free one, kept for every later search and
/// never marked used; that is how it is done here. The error number is left
/// alone. Returns what the device's firstfile answers (the direntry, or 0);
/// 0 for an unknown device or no free FCB.
pub fn firstfile(bus: &mut Bus, gprs: &mut [u32; 32], name: u32, direntry: u32) -> FileCall {
    let Some((dcb, port, rest)) = find_device(bus, name) else {
        return FileCall::Return(0);
    };
    let mut f = peek32(bus, kvar::FIND_FCB);
    if f == 0 {
        let Some(fd) = free_fcb(bus) else {
            return FileCall::Return(0);
        };
        f = fcb_addr(fd);
        poke32(bus, kvar::FIND_FCB, f);
    }
    poke32(bus, f + fcb::DEVICE_ID, port);
    poke32(bus, f + fcb::DEVICE_FLAGS, peek32(bus, dcb + dcb::FLAGS));
    poke32(bus, f + fcb::DCB, dcb);
    let first = device_function(bus, dcb, dcb::FIRSTFILE);
    call_device(bus, gprs, first, [f, rest, direntry, 0], then::PASS, f)
}

/// B(43h) nextfile(direntry): the device's nextfile on the search FCB of the
/// last firstfile2. Returns the direntry, or 0.
pub fn nextfile(bus: &mut Bus, gprs: &mut [u32; 32], direntry: u32) -> FileCall {
    let f = peek32(bus, kvar::FIND_FCB);
    let dcb = peek32(bus, f + fcb::DCB);
    if f == 0 || dcb == 0 {
        return FileCall::Return(0);
    }
    let next = device_function(bus, dcb, dcb::NEXTFILE);
    call_device(bus, gprs, next, [f, direntry, 0, 0], then::PASS, f)
}

/// B(41h) format(name), B(44h) rename(old, new), B(45h) erase(name),
/// B(46h) undelete(name): the device function at DCB offset `entry` runs on
/// a temporary FCB (psx-spx: these calls need a free handle, and rename
/// uses one, not two). Returns 1 when the device answers 0, else 0; the
/// error number says why: 13h for an unknown device, 12h for a rename
/// between two devices, 18h without a free handle, the device's own.
pub fn device_call(
    bus: &mut Bus,
    gprs: &mut [u32; 32],
    entry: u32,
    name: u32,
    new_name: Option<u32>,
) -> FileCall {
    let none = |bus: &mut Bus, e: u32| {
        set_errno(bus, e);
        FileCall::Return(0)
    };
    let Some((dcb, port, rest)) = find_device(bus, name) else {
        return none(bus, errno::NODEV);
    };
    let mut new_rest = 0;
    if let Some(new_name) = new_name {
        let Some((new_dcb, _, rest)) = find_device(bus, new_name) else {
            return none(bus, errno::NODEV);
        };
        if new_dcb != dcb {
            return none(bus, errno::XDEV);
        }
        new_rest = rest;
    }
    let Some(fd) = free_fcb(bus) else {
        return none(bus, errno::MFILE);
    };
    let f = fcb_addr(fd);
    prepare_fcb(bus, f, dcb, port, 1);
    let function = device_function(bus, dcb, entry);
    call_device(
        bus,
        gprs,
        function,
        [f, rest, f, new_rest],
        then::MAINTENANCE,
        f,
    )
}
/// TTY in_out(fcb, cmd): writes go to the host console; reads return 0
/// (no console input).
pub fn tty_inout(bus: &mut Bus, f: u32, cmd: u32, out: &mut dyn FnMut(u8)) -> u32 {
    if cmd != 2 {
        return 0;
    }
    let (addr, len) = (peek32(bus, f + fcb::TADDR), peek32(bus, f + fcb::TLEN));
    for i in 0..len.min(0x10_0000) {
        out(bus.try_read8(addr.wrapping_add(i)).unwrap_or(0));
    }
    len
}

// ----------------------------------------------------------- CD-ROM driver

const CD_INDEX: u32 = 0x1F80_1800;
const CD_REG1: u32 = 0x1F80_1801;
const CD_REG2: u32 = 0x1F80_1802;
const CD_REG3: u32 = 0x1F80_1803;
const D3_MADR: u32 = 0x1F80_10B0;
const D3_BCR: u32 = 0x1F80_10B4;
const D3_CHCR: u32 = 0x1F80_10B8;
const DPCR: u32 = 0x1F80_10F0;
const DICR: u32 = 0x1F80_10F4;

mod phase {
    pub const IDLE: u32 = 0;
    pub const SETMODE: u32 = 1;
    pub const SETLOC: u32 = 2;
    pub const READ_ACK: u32 = 3;
    pub const DATA: u32 = 4;
    pub const DMA: u32 = 5;
    pub const PAUSE_ACK: u32 = 6;
    pub const PAUSE_DONE: u32 = 7;
}

fn bcd(v: u8) -> u8 {
    (v / 10) << 4 | (v % 10)
}

/// Pending drive interrupt type (0 = none); its response is drained.
fn cd_take_int(bus: &mut Bus) -> u8 {
    bus.write8(CD_INDEX, 1);
    let int = bus.read8(CD_REG3) & 7;
    if int != 0 {
        while bus.read8(CD_INDEX) & 0x20 != 0 {
            bus.read8(CD_REG1);
        }
        bus.write8(CD_INDEX, 1);
        bus.write8(CD_REG3, 0x07);
    }
    int
}

fn cd_command(bus: &mut Bus, cmd: u8, params: &[u8]) {
    bus.write8(CD_INDEX, 0);
    for p in params {
        bus.write8(CD_REG2, *p);
    }
    bus.write8(CD_INDEX, 0);
    bus.write8(CD_REG1, cmd);
}

fn cd_finish(bus: &mut Bus, ok: bool) -> Option<bool> {
    bus.write8(CD_INDEX, 0);
    bus.write8(CD_REG3, 0x00);
    let mask = peek32(bus, kvar::CD_SAVED_MASK) as u8;
    bus.write8(CD_INDEX, 1);
    bus.write8(CD_REG2, mask);
    poke32(bus, kvar::CD_PHASE, phase::IDLE);
    Some(ok)
}

/// One step of reading `count` 2048-byte sectors from `lba` to `dst`.
/// Returns `None` while in progress (call again), then whether it worked.
pub fn cd_read_step(bus: &mut Bus, lba: u32, count: u32, dst: u32) -> Option<bool> {
    let ph = peek32(bus, kvar::CD_PHASE);
    if ph == phase::IDLE {
        if count == 0 {
            return Some(true);
        }
        bus.write8(CD_INDEX, 0);
        let mask = bus.read8(CD_REG3) & 0x1F;
        poke32(bus, kvar::CD_SAVED_MASK, u32::from(mask));
        bus.write8(CD_INDEX, 1);
        bus.write8(CD_REG2, 0);
        bus.write8(CD_REG3, 0x1F);
        poke32(bus, kvar::CD_LBA, lba);
        poke32(bus, kvar::CD_COUNT, count);
        poke32(bus, kvar::CD_DST, dst);
        cd_command(bus, 0x0E, &[0x80]);
        poke32(bus, kvar::CD_PHASE, phase::SETMODE);
        return None;
    }
    if ph == phase::DMA {
        if bus.read32(D3_CHCR) & (1 << 24) != 0 {
            return None;
        }
        bus.write8(CD_INDEX, 0);
        bus.write8(CD_REG3, 0x00);
        // Acknowledge the channel 3 completion flag (write-1-to-clear),
        // keeping the enables.
        let dicr = bus.read32(DICR);
        bus.write32(DICR, (dicr & 0x00FF_FFFF) | (1 << 27));
        if bus.read32(DICR) & 0x7F00_0000 == 0 {
            bus.write32(0x1F80_1070, !(1 << 3));
        }
        let left = peek32(bus, kvar::CD_COUNT) - 1;
        poke32(bus, kvar::CD_COUNT, left);
        let next = peek32(bus, kvar::CD_DST) + 0x800;
        poke32(bus, kvar::CD_DST, next);
        if left == 0 {
            cd_command(bus, 0x09, &[]);
            poke32(bus, kvar::CD_PHASE, phase::PAUSE_ACK);
        } else {
            poke32(bus, kvar::CD_PHASE, phase::DATA);
        }
        return None;
    }
    let int = cd_take_int(bus);
    if int == 0 {
        return None;
    }
    if int == 5 && ph != phase::PAUSE_DONE {
        return cd_finish(bus, false);
    }
    match (ph, int) {
        (phase::SETMODE, 3) => {
            let (m, s, f) = psx_iso::lba_to_msf(peek32(bus, kvar::CD_LBA));
            cd_command(bus, 0x02, &[bcd(m), bcd(s), bcd(f)]);
            poke32(bus, kvar::CD_PHASE, phase::SETLOC);
        }
        (phase::SETLOC, 3) => {
            cd_command(bus, 0x06, &[]);
            poke32(bus, kvar::CD_PHASE, phase::READ_ACK);
        }
        (phase::READ_ACK, 3) => poke32(bus, kvar::CD_PHASE, phase::DATA),
        (phase::READ_ACK | phase::DATA, 1) => {
            bus.write8(CD_INDEX, 0);
            bus.write8(CD_REG3, 0x80);
            let dpcr = bus.read32(DPCR);
            bus.write32(DPCR, dpcr | 0x8000);
            bus.write32(D3_MADR, peek32(bus, kvar::CD_DST) & 0x00FF_FFFC);
            bus.write32(D3_BCR, 0x0001_0200);
            bus.write32(D3_CHCR, 0x1100_0000);
            poke32(bus, kvar::CD_PHASE, phase::DMA);
        }
        (phase::PAUSE_ACK, 3) => poke32(bus, kvar::CD_PHASE, phase::PAUSE_DONE),
        (phase::PAUSE_DONE, 2 | 5) => return cd_finish(bus, true),
        // Sectors still arriving before the pause takes effect.
        _ => {}
    }
    None
}

// ------------------------------------------------------ CD-ROM filesystem

fn buf8(bus: &Bus, off: u32) -> u8 {
    bus.try_read8(SECTOR_BUF + off).unwrap_or(0)
}

fn buf32(bus: &Bus, off: u32) -> u32 {
    peek32(bus, SECTOR_BUF + off)
}

/// Resolve `path` (after "cdrom:") to `(lba, size)`. `None` while reading;
/// `Some(None)` when the file does not exist or the disc cannot be read.
pub fn cd_lookup_step(bus: &mut Bus, path: u32) -> Option<Option<(u32, u32)>> {
    const START: u32 = 0;
    const PVD: u32 = 1;
    const DIR: u32 = 2;
    let comps = crate::system_cnf::normalize_path(&read_cstr(bus, path, 128));
    loop {
        match peek32(bus, kvar::FS_PHASE) {
            START => {
                if comps.is_empty() {
                    return Some(None);
                }
                if peek32(bus, kvar::FS_ROOT_LBA) == 0 {
                    poke32(bus, kvar::FS_PHASE, PVD);
                    continue;
                }
                poke32(bus, kvar::FS_COMP, 0);
                poke32(bus, kvar::FS_DIR_LBA, peek32(bus, kvar::FS_ROOT_LBA));
                poke32(bus, kvar::FS_DIR_LEFT, peek32(bus, kvar::FS_ROOT_SIZE));
                poke32(bus, kvar::FS_PHASE, DIR);
            }
            PVD => match cd_read_step(bus, 16, 1, SECTOR_BUF)? {
                true if buf8(bus, 0) == 1 && buf8(bus, 1) == b'C' => {
                    poke32(bus, kvar::FS_ROOT_LBA, buf32(bus, 156 + 2));
                    poke32(bus, kvar::FS_ROOT_SIZE, buf32(bus, 156 + 10));
                    poke32(bus, kvar::FS_PHASE, START);
                }
                _ => {
                    poke32(bus, kvar::FS_PHASE, START);
                    return Some(None);
                }
            },
            _ => {
                let lba = peek32(bus, kvar::FS_DIR_LBA);
                if !cd_read_step(bus, lba, 1, SECTOR_BUF)? {
                    poke32(bus, kvar::FS_PHASE, START);
                    return Some(None);
                }
                let comp = peek32(bus, kvar::FS_COMP) as usize;
                let want = comps[comp].split(';').next().unwrap_or("");
                match find_in_sector(bus, want) {
                    Some((ext, size, is_dir)) if comp + 1 == comps.len() => {
                        poke32(bus, kvar::FS_PHASE, START);
                        return Some((!is_dir).then_some((ext, size)));
                    }
                    Some((ext, size, true)) => {
                        poke32(bus, kvar::FS_COMP, comp as u32 + 1);
                        poke32(bus, kvar::FS_DIR_LBA, ext);
                        poke32(bus, kvar::FS_DIR_LEFT, size);
                    }
                    Some(_) => {
                        poke32(bus, kvar::FS_PHASE, START);
                        return Some(None);
                    }
                    None => {
                        let left = peek32(bus, kvar::FS_DIR_LEFT);
                        if left <= 0x800 {
                            poke32(bus, kvar::FS_PHASE, START);
                            return Some(None);
                        }
                        poke32(bus, kvar::FS_DIR_LEFT, left - 0x800);
                        poke32(bus, kvar::FS_DIR_LBA, lba + 1);
                    }
                }
            }
        }
    }
}

/// Find `name` among the directory records in the sector buffer.
fn find_in_sector(bus: &Bus, name: &str) -> Option<(u32, u32, bool)> {
    let mut off = 0u32;
    while off < 0x800 {
        let len = u32::from(buf8(bus, off));
        if len == 0 {
            break;
        }
        let nlen = u32::from(buf8(bus, off + 32));
        let ident: String = (0..nlen).map(|i| buf8(bus, off + 33 + i) as char).collect();
        if ident.to_ascii_uppercase().split(';').next() == Some(name) {
            return Some((
                buf32(bus, off + 2),
                buf32(bus, off + 10),
                buf8(bus, off + 25) & 2 != 0,
            ));
        }
        off += len;
    }
    None
}

/// CD-ROM open(fcb, name, mode): 0, or nonzero with FCB error 2.
pub fn cd_open_step(bus: &mut Bus, f: u32, name: u32) -> Option<u32> {
    Some(match cd_lookup_step(bus, name)? {
        Some((lba, size)) => {
            poke32(bus, f + fcb::LBA, lba);
            poke32(bus, f + fcb::SIZE, size);
            poke32(bus, f + fcb::DEVICE_ID, 0);
            0
        }
        None => {
            poke32(bus, f + fcb::ERROR, errno::NOENT);
            u32::MAX
        }
    })
}

/// CD-ROM read(fcb, dst, len): the file position must be sector aligned
/// and inside the file (else error 16h). Whole sectors go straight to
/// `dst`; a partial last sector goes through the sector buffer.
pub fn cd_read_file_step(bus: &mut Bus, f: u32, dst: u32, len: u32) -> Option<u32> {
    let pos = peek32(bus, f + fcb::FPOS);
    let size = peek32(bus, f + fcb::SIZE);
    let lba = peek32(bus, f + fcb::LBA) + pos / 0x800;
    match peek32(bus, kvar::RD_STAGE) {
        0 => {
            if !pos.is_multiple_of(0x800) || pos >= size {
                poke32(bus, f + fcb::ERROR, errno::INVAL);
                return Some(u32::MAX);
            }
            poke32(bus, kvar::RD_LEN, len.min(size - pos));
            poke32(bus, kvar::RD_STAGE, 1);
            None
        }
        1 => {
            let n = peek32(bus, kvar::RD_LEN);
            if !cd_read_step(bus, lba, n / 0x800, dst)? {
                poke32(bus, kvar::RD_STAGE, 0);
                poke32(bus, f + fcb::ERROR, errno::IO);
                return Some(u32::MAX);
            }
            if n.is_multiple_of(0x800) {
                return Some(finish_read(bus, f, n));
            }
            poke32(bus, kvar::RD_STAGE, 2);
            None
        }
        _ => {
            let n = peek32(bus, kvar::RD_LEN);
            let whole = n / 0x800;
            if !cd_read_step(bus, lba + whole, 1, SECTOR_BUF)? {
                poke32(bus, kvar::RD_STAGE, 0);
                poke32(bus, f + fcb::ERROR, errno::IO);
                return Some(u32::MAX);
            }
            for i in 0..n % 0x800 {
                let b = buf8(bus, i);
                bus.write8_safe(dst + whole * 0x800 + i, b);
            }
            Some(finish_read(bus, f, n))
        }
    }
}

fn finish_read(bus: &mut Bus, f: u32, n: u32) -> u32 {
    poke32(bus, kvar::RD_STAGE, 0);
    let pos = peek32(bus, f + fcb::FPOS);
    poke32(bus, f + fcb::FPOS, pos + n);
    n
}

// ------------------------------------------------------ executable loading

/// Progress of A(41h)/A(42h)/A(51h).
pub enum LoadStep {
    /// Still reading; call again.
    Pending,
    /// Not a file on the kernel CD-ROM device (other devices are not
    /// supported by the loader yet).
    Unsupported,
    /// Finished: 1 = loaded, 0 = failed.
    Done(u32),
}

/// A(41h) LoadTest (`body` false) / A(42h) Load (`body` true): read the
/// executable's 800h-byte header, copy bytes 10h..4Bh to `header`
/// (psx-spx), and for Load also read the body to its load address.
/// psx-spx gives no return value; callers test it as a flag, so it is 1 when
/// the file was loaded and 0 when it was not.
pub fn load_step(bus: &mut Bus, path: u32, header: u32, body: bool) -> LoadStep {
    const LOOKUP: u32 = 0;
    const HEADER: u32 = 1;
    const BODY: u32 = 2;
    match find_device(bus, path) {
        Some((d, _, _)) if peek32(bus, d + dcb::OPEN) == stub_addr(3, internal::CD_OPEN) => {}
        _ => return LoadStep::Unsupported,
    }
    let name = find_device(bus, path).map(|(_, _, n)| n).unwrap_or(path);
    let fail = |bus: &mut Bus| {
        poke32(bus, kvar::LD_STAGE, LOOKUP);
        LoadStep::Done(0)
    };
    match peek32(bus, kvar::LD_STAGE) {
        LOOKUP => match cd_lookup_step(bus, name) {
            None => LoadStep::Pending,
            Some(None) => fail(bus),
            Some(Some((lba, size))) => {
                poke32(bus, kvar::LD_LBA, lba);
                poke32(bus, kvar::LD_SIZE, size);
                poke32(bus, kvar::LD_STAGE, HEADER);
                LoadStep::Pending
            }
        },
        HEADER => {
            let lba = peek32(bus, kvar::LD_LBA);
            match cd_read_step(bus, lba, 1, SECTOR_BUF) {
                None => LoadStep::Pending,
                Some(false) => fail(bus),
                Some(true) => {
                    let magic: Vec<u8> = (0..8).map(|i| buf8(bus, i)).collect();
                    if magic != b"PS-X EXE" {
                        return fail(bus);
                    }
                    for off in (0..0x3C).step_by(4) {
                        let w = buf32(bus, 0x10 + off);
                        poke32(bus, header + off, w);
                    }
                    if !body {
                        poke32(bus, kvar::LD_STAGE, LOOKUP);
                        return LoadStep::Done(1);
                    }
                    poke32(bus, kvar::LD_STAGE, BODY);
                    LoadStep::Pending
                }
            }
        }
        _ => {
            let lba = peek32(bus, kvar::LD_LBA) + 1;
            let (dst, size) = (peek32(bus, header + 8), peek32(bus, header + 12));
            match cd_read_step(bus, lba, size.div_ceil(0x800), dst) {
                None => LoadStep::Pending,
                Some(false) => fail(bus),
                Some(true) => {
                    poke32(bus, kvar::LD_STAGE, LOOKUP);
                    LoadStep::Done(1)
                }
            }
        }
    }
}

// ------------------------------------------------------ CD-ROM IRQ handlers

/// Kernel CdromIoIrq verifier: a drive interrupt that reached the CPU is
/// acknowledged, its response kept in kernel RAM, and the matching
/// F0000003h event delivered (10h ack, 20h complete, 40h data ready, 80h
/// end, 8000h error). Returns the event spec, or `None` when not ours.
pub fn cd_io_irq(bus: &mut Bus) -> Option<u32> {
    let pending = bus.read32(0x1F80_1070) & bus.read32(0x1F80_1074);
    if pending & 4 == 0 {
        return None;
    }
    bus.write8(CD_INDEX, 1);
    let int = bus.read8(CD_REG3) & 7;
    let mut i = 0;
    while bus.read8(CD_INDEX) & 0x20 != 0 && i < 16 {
        let b = bus.read8(CD_REG1);
        bus.write8_safe(kvar::CD_RESPONSE + i, b);
        i += 1;
    }
    bus.write8(CD_INDEX, 1);
    bus.write8(CD_REG3, 0x07);
    bus.write32(0x1F80_1070, !4);
    poke32(bus, kvar::CD_LAST_INT, u32::from(int));
    Some(match int {
        1 => 0x40,
        2 => 0x20,
        3 => 0x10,
        4 => 0x80,
        _ => 0x8000,
    })
}

/// Kernel CdromDmaIrq verifier: acknowledges a channel 3 completion that
/// reached the CPU. Returns whether it handled one.
pub fn cd_dma_irq(bus: &mut Bus) -> bool {
    let pending = bus.read32(0x1F80_1070) & bus.read32(0x1F80_1074);
    let dicr = bus.read32(DICR);
    if pending & 8 == 0 || dicr & (1 << 27) == 0 {
        return false;
    }
    bus.write32(DICR, (dicr & 0x00FF_FFFF) | (1 << 27));
    if bus.read32(DICR) & 0x7F00_0000 == 0 {
        bus.write32(0x1F80_1070, !8);
    }
    true
}

#[cfg(test)]
mod tests {
    use crate::hle_asm::*;
    use crate::{Bus, Cpu};
    use psx_iso::{Disc, IsoBuilder};

    /// Assemble a guest program calling B-table functions in sequence.
    fn program(calls: &[(u32, [Option<u32>; 3])]) -> Vec<u32> {
        program_on(0xB0, calls)
    }

    fn program_on(vector: i16, calls: &[(u32, [Option<u32>; 3])]) -> Vec<u32> {
        let mut a = Asm::new(0x8001_0000);
        for (func, args) in calls {
            for (i, arg) in args.iter().enumerate() {
                match arg {
                    Some(v) => a.li(A0 + i as u32, *v),
                    // None: pass the previous result (v0) along.
                    None => a.mov(A0 + i as u32, V0),
                }
            }
            a.addiu(T2, ZERO, vector);
            a.jalr(T2);
            a.addiu(T1, ZERO, *func as i16);
            a.sw(V0, (4 * (*func & 0xF)) as i16, S0);
        }
        a.label("end");
        a.b("end");
        a.nop();
        a.finish()
    }

    fn run(bus: &mut Bus, words: &[u32], results: u32) -> Cpu {
        let mut cpu = Cpu::new();
        for (i, w) in words.iter().enumerate() {
            bus.write32(0x8001_0000 + 4 * i as u32, *w);
        }
        cpu.gprs_mut_for_test()[16] = results;
        cpu.gprs_mut_for_test()[29] = 0x801F_FF00;
        cpu.set_pc_for_test(0x8001_0000);
        let end = 0x8001_0000 + 4 * (words.len() as u32 - 2);
        for _ in 0..40_000_000u32 {
            if cpu.pc() == end {
                return cpu;
            }
            cpu.step(bus).unwrap();
            bus.run_spu_to_current_cycle();
            let _ = bus.spu.drain_audio();
        }
        panic!("program did not finish, pc={:#x}", cpu.pc());
    }

    #[test]
    fn cdrom_files_open_read_and_close_through_the_drive() {
        let data: Vec<u8> = (0..3000u32).map(|i| (i * 7) as u8).collect();
        let mut iso = IsoBuilder::new();
        iso.add_file("DATA.BIN", data.clone());
        let mut bus = Bus::new_without_bios();
        bus.enable_hle_bios();
        bus.cdrom.insert_disc(Some(Disc::from_bin(iso.build_bin())));
        let path = 0x8002_0000;
        for (i, b) in b"cdrom:\\DATA.BIN;1\0".iter().enumerate() {
            bus.write8_safe(path + i as u32, *b);
        }
        let dst = 0x8003_0000;
        let results = 0x8004_0000;
        // open -> [results+8], read -> [+0x10], close -> [+0x18]; fd comes
        // back in v0 and is passed on as a0.
        let words = program(&[
            (0x32, [Some(path), Some(1), Some(0)]),
            (0x34, [None, Some(dst), Some(3000)]),
            (0x36, [Some(2), Some(0), Some(0)]),
        ]);
        let cycles = bus.cycles();
        run(&mut bus, &words, results);
        assert_eq!(bus.read32(results + 8), 2, "fd 2: 0 and 1 are the TTY");
        assert_eq!(bus.read32(results + 0x10), 3000);
        assert_eq!(bus.read32(results + 0x18), 2);
        let got: Vec<u8> = (0..3000).map(|i| bus.try_read8(dst + i).unwrap()).collect();
        assert_eq!(got, data);
        // The drive's own seek/read timing passed, not an instant copy.
        assert!(bus.cycles() - cycles > 100_000, "{}", bus.cycles() - cycles);
    }

    /// A(71h)/A(54h) _96_init installs the kernel's CD-ROM handlers and
    /// events again (Nightmare Creatures calls it first thing; it used to
    /// be unimplemented and the game never drew a frame). Calling it twice
    /// or after _96_remove leaves one working set.
    #[test]
    fn cd_init_reinstalls_the_cd_driver_once() {
        let data: Vec<u8> = (0..3000u32).map(|i| (i * 5) as u8).collect();
        let mut iso = IsoBuilder::new();
        iso.add_file("DATA.BIN", data.clone());
        let mut bus = Bus::new_without_bios();
        bus.enable_hle_bios();
        bus.cdrom.insert_disc(Some(Disc::from_bin(iso.build_bin())));
        let results = 0x8004_0000;
        let init = program_on(
            0xA0,
            &[
                (0x72, [Some(0), Some(0), Some(0)]),
                (0x71, [Some(0), Some(0), Some(0)]),
                (0x54, [Some(0), Some(0), Some(0)]),
            ],
        );
        let cpu = run(&mut bus, &init, results);
        assert!(
            bus.hle_bios_first_unimplemented().is_none(),
            "_96_init is implemented"
        );
        // psx-spx says nothing about interrupts: the caller's state (off
        // here) is left alone, and the reads below poll the drive anyway.
        assert_eq!(cpu.cop0()[12] & 0x401, 0);
        let path = 0x8002_0000;
        for (i, b) in b"cdrom:\\DATA.BIN;1\0".iter().enumerate() {
            bus.write8_safe(path + i as u32, *b);
        }
        let dst = 0x8003_0000;
        let words = program(&[
            (0x32, [Some(path), Some(1), Some(0)]),
            (0x34, [None, Some(dst), Some(3000)]),
            (0x36, [Some(2), Some(0), Some(0)]),
        ]);
        run(&mut bus, &words, results);
        assert_eq!(bus.read32(results + 0x10), 3000);
        let got: Vec<u8> = (0..3000).map(|i| bus.try_read8(dst + i).unwrap()).collect();
        assert_eq!(got, data);
    }

    #[test]
    fn missing_files_and_devices_fail_with_errno() {
        let mut iso = IsoBuilder::new();
        iso.add_file("A.BIN", vec![1; 16]);
        let mut bus = Bus::new_without_bios();
        bus.enable_hle_bios();
        bus.cdrom.insert_disc(Some(Disc::from_bin(iso.build_bin())));
        for (i, b) in b"cdrom:\\NOPE.BIN\0nope:x\0".iter().enumerate() {
            bus.write8_safe(0x8002_0000 + i as u32, *b);
        }
        let words = program(&[
            (0x32, [Some(0x8002_0000), Some(1), Some(0)]),
            (0x54, [Some(0), Some(0), Some(0)]),
            (0x32, [Some(0x8002_0010), Some(1), Some(0)]),
            (0x55, [Some(9), Some(0), Some(0)]),
        ]);
        run(&mut bus, &words, 0x8004_0000);
        assert_eq!(bus.read32(0x8004_0008), u32::MAX);
        assert_eq!(bus.read32(0x8004_0010), super::errno::NOENT);
        assert_eq!(
            bus.read32(0x8004_0014),
            u32::MAX,
            "unknown device (and B(55h) of a free fd)"
        );
    }

    #[test]
    fn load_and_exec_run_a_second_executable_from_the_disc() {
        // CHILD.EXE: store a0+a1 at [0x80050000] and return.
        let mut child = vec![0u8; psx_iso::EXE_HEADER_BYTES];
        child[..8].copy_from_slice(b"PS-X EXE");
        child[0x10..0x14].copy_from_slice(&0x8006_0000u32.to_le_bytes());
        child[0x18..0x1C].copy_from_slice(&0x8006_0000u32.to_le_bytes());
        child[0x1C..0x20].copy_from_slice(&0x800u32.to_le_bytes());
        let mut body = vec![0u8; 0x800];
        for (i, w) in [0x3C08_8005u32, 0x0085_4821, 0xAD09_0000, 0x03E0_0008, 0]
            .iter()
            .enumerate()
        {
            body[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
        }
        child.extend_from_slice(&body);
        let mut iso = IsoBuilder::new();
        iso.add_file("CHILD.EXE", child);
        let mut bus = Bus::new_without_bios();
        bus.enable_hle_bios();
        bus.cdrom.insert_disc(Some(Disc::from_bin(iso.build_bin())));
        for (i, b) in b"cdrom:\\CHILD.EXE;1\0".iter().enumerate() {
            bus.write8_safe(0x8002_0000 + i as u32, *b);
        }
        let header = 0x8002_0100;
        let words = program_on(
            0xA0,
            &[
                (0x42, [Some(0x8002_0000), Some(header), Some(0)]),
                (0x43, [Some(header), Some(5), Some(6)]),
            ],
        );
        run(&mut bus, &words, 0x8004_0000);
        assert_eq!(bus.read32(0x8004_0008), 1, "Load");
        assert_eq!(bus.read32(header), 0x8006_0000, "header copied from 10h");
        assert_eq!(bus.read32(0x8005_0000), 11, "child ran with a0=5, a1=6");
        assert_eq!(bus.read32(0x8004_000C), 1, "Exec returned 1");
    }

    /// Like [`program`], but call `i`'s v0 goes to `results + 4 * i`, a
    /// negative vector selects A or B by its table, and `None` passes the
    /// previous result on.
    fn sequence(calls: &[(i16, u32, [Option<u32>; 3])], results: u32) -> Vec<u32> {
        let mut a = Asm::new(0x8001_0000);
        a.li(S0, results);
        for (vector, func, args) in calls {
            for (i, arg) in args.iter().enumerate() {
                match arg {
                    Some(v) => a.li(A0 + i as u32, *v),
                    None => a.mov(A0 + i as u32, V0),
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

    fn put_str(bus: &mut Bus, at: u32, s: &str) {
        for (i, b) in s.bytes().chain([0]).enumerate() {
            bus.write8_safe(at + i as u32, b);
        }
    }

    /// A device written in guest code, the way a game adds one: "dev" with
    /// character-device flags. Its open answers 0, in_out answers 55h,
    /// firstfile and nextfile answer their direntry argument, init and
    /// remove store a marker in RAM, and every other function does nothing
    /// and answers 0.
    fn install_guest_device(bus: &mut Bus) -> u32 {
        const CODE: u32 = 0x8005_0000;
        const INFO: u32 = 0x8005_1000;
        let mut a = Asm::new(CODE);
        a.label("open");
        a.jr(RA);
        a.mov(V0, ZERO);
        a.label("inout");
        a.li(V0, 0x55);
        a.jr(RA);
        a.nop();
        a.label("firstfile");
        a.jr(RA);
        a.mov(V0, A2);
        a.label("nextfile");
        a.jr(RA);
        a.mov(V0, A1);
        a.label("init");
        a.li(T0, 0x8005_2000);
        a.li(T1, 0x1234);
        a.jr(RA);
        a.sw(T1, 0, T0);
        a.label("remove");
        a.li(T0, 0x8005_2004);
        a.li(T1, 0x5678);
        a.jr(RA);
        a.sw(T1, 0, T0);
        let (open, inout, first, next, init, remove) = (
            a.addr("open"),
            a.addr("inout"),
            a.addr("firstfile"),
            a.addr("nextfile"),
            a.addr("init"),
            a.addr("remove"),
        );
        for (i, w) in a.finish().iter().enumerate() {
            bus.write32(CODE + 4 * i as u32, *w);
        }
        put_str(bus, 0x8005_1800, "dev");
        put_str(bus, 0x8005_1810, "GUEST");
        let mut info = [open; 20];
        info[0] = 0x8005_1800;
        info[1] = 1;
        info[2] = 1;
        info[3] = 0x8005_1810;
        info[4] = init;
        info[5] = open;
        info[6] = inout;
        info[13] = first;
        info[14] = next;
        info[18] = remove;
        for (i, w) in info.iter().enumerate() {
            bus.write32(INFO + 4 * i as u32, *w);
        }
        INFO
    }

    #[test]
    fn a_device_written_in_guest_code_is_added_used_and_removed() {
        let mut bus = Bus::new_without_bios();
        bus.enable_hle_bios();
        let info = install_guest_device(&mut bus);
        put_str(&mut bus, 0x8002_0000, "dev3:name");
        put_str(&mut bus, 0x8002_0020, "dev:*");
        put_str(&mut bus, 0x8002_0040, "dev");
        let (dirent, buf) = (0x8003_0000, 0x8003_1000);
        let words = sequence(
            &[
                (0xB0, 0x47, [Some(info), Some(0), Some(0)]), // 0 AddDrv
                (0xB0, 0x32, [Some(0x8002_0000), Some(3), Some(0)]), // 1 open
                (0xB0, 0x35, [None, Some(buf), Some(4)]),     // 2 write
                (0xB0, 0x42, [Some(0x8002_0020), Some(dirent), Some(0)]), // 3
                (0xB0, 0x32, [Some(0x8002_0000), Some(3), Some(0)]), // 4 open
                (0xB0, 0x43, [Some(dirent + 0x40), Some(0), Some(0)]), // 5
                (0xB0, 0x36, [Some(2), Some(0), Some(0)]),    // 6 close
                (0xB0, 0x48, [Some(0x8002_0040), Some(0), Some(0)]), // 7 DelDrv
                (0xB0, 0x32, [Some(0x8002_0000), Some(3), Some(0)]), // 8 open
                (0xB0, 0x54, [Some(0), Some(0), Some(0)]),    // 9 errno
            ],
            0x8004_0000,
        );
        run(&mut bus, &words, 0x8004_0000);
        let r = |bus: &mut Bus, i: u32| bus.read32(0x8004_0000 + 4 * i);
        assert_eq!(r(&mut bus, 0), 1, "added");
        assert_eq!(bus.read32(0x8005_2000), 0x1234, "init ran");
        assert_eq!(r(&mut bus, 1), 2, "open: handle 2");
        assert_eq!(r(&mut bus, 2), 0x55, "write goes to in_out");
        assert_eq!(r(&mut bus, 3), dirent, "firstfile answers its direntry");
        // The search took the first free FCB (3) without marking it used, so
        // the next open is given it as well (the bug psx-spx describes).
        assert_eq!(r(&mut bus, 4), 3, "search FCB is handed out again");
        assert_eq!(r(&mut bus, 5), dirent + 0x40, "nextfile");
        assert_eq!(r(&mut bus, 6), 2, "close returns the handle");
        assert_eq!(r(&mut bus, 7), 1, "removed");
        assert_eq!(bus.read32(0x8005_2004), 0x5678, "remove ran");
        assert_eq!(r(&mut bus, 8), u32::MAX, "device is gone");
        assert_eq!(r(&mut bus, 9), super::errno::NODEV);
    }

    #[test]
    fn lseek_and_bad_handles_follow_the_documented_errors() {
        let mut iso = IsoBuilder::new();
        iso.add_file("DATA.BIN", vec![7; 5000]);
        let mut bus = Bus::new_without_bios();
        bus.enable_hle_bios();
        bus.cdrom.insert_disc(Some(Disc::from_bin(iso.build_bin())));
        put_str(&mut bus, 0x8002_0000, "cdrom:\\DATA.BIN;1");
        let words = sequence(
            &[
                (0xB0, 0x32, [Some(0x8002_0000), Some(1), Some(0)]), // 0 open
                (0xB0, 0x33, [None, Some(0x800), Some(0)]),          // 1 set
                (0xB0, 0x33, [Some(2), Some(0x10), Some(1)]),        // 2 move
                (0xB0, 0x33, [Some(2), Some(0x99), Some(2)]),        // 3 from end
                (0xB0, 0x33, [Some(2), Some(0), Some(7)]),           // 4 bad type
                (0xB0, 0x54, [Some(0), Some(0), Some(0)]),           // 5 errno
                (0xB0, 0x33, [Some(7), Some(0), Some(0)]),           // 6 closed
                (0xB0, 0x54, [Some(0), Some(0), Some(0)]),           // 7 errno
                (0xB0, 0x34, [Some(2), Some(0x8003_0000), Some(0)]), // 8 no length
                (0xB0, 0x36, [Some(2), Some(0), Some(0)]),           // 9 close
                (0xB0, 0x36, [Some(2), Some(0), Some(0)]),           // 10 again
                (0xB0, 0x54, [Some(0), Some(0), Some(0)]),           // 11 errno
            ],
            0x8004_0000,
        );
        run(&mut bus, &words, 0x8004_0000);
        let r = |bus: &mut Bus, i: u32| bus.read32(0x8004_0000 + 4 * i);
        assert_eq!(r(&mut bus, 1), 0x800);
        assert_eq!(r(&mut bus, 2), 0x810);
        assert_eq!(r(&mut bus, 3), 0x810, "whence 2 does not move");
        assert_eq!(r(&mut bus, 4), u32::MAX);
        assert_eq!(r(&mut bus, 5), super::errno::INVAL);
        assert_eq!(r(&mut bus, 6), u32::MAX);
        assert_eq!(r(&mut bus, 7), super::errno::BADF);
        assert_eq!(r(&mut bus, 8), u32::MAX, "a read needs a length");
        assert_eq!(r(&mut bus, 9), 2);
        assert_eq!(r(&mut bus, 10), u32::MAX, "already closed");
        assert_eq!(r(&mut bus, 11), super::errno::BADF);
    }

    #[test]
    fn maintenance_calls_need_a_free_handle_and_one_device() {
        let mut iso = IsoBuilder::new();
        iso.add_file("A.BIN", vec![1; 16]);
        let mut bus = Bus::new_without_bios();
        bus.enable_hle_bios();
        bus.cdrom.insert_disc(Some(Disc::from_bin(iso.build_bin())));
        put_str(&mut bus, 0x8002_0000, "cdrom:\\A.BIN;1");
        put_str(&mut bus, 0x8002_0020, "bu00:B");
        let mut calls = vec![(
            0xB0i16,
            0x44u32,
            [Some(0x8002_0000), Some(0x8002_0020), Some(0)],
        )]; // 0 rename across devices
        calls.push((0xB0, 0x54, [Some(0), Some(0), Some(0)])); // 1 errno
        for _ in 0..14 {
            calls.push((0xB0, 0x32, [Some(0x8002_0000), Some(1), Some(0)]));
        } // 2..15 take every handle
        calls.push((0xB0, 0x32, [Some(0x8002_0000), Some(1), Some(0)])); // 16
        calls.push((0xB0, 0x54, [Some(0), Some(0), Some(0)])); // 17 errno
        calls.push((0xB0, 0x45, [Some(0x8002_0000), Some(0), Some(0)])); // 18
        calls.push((0xB0, 0x54, [Some(0), Some(0), Some(0)])); // 19 errno
        let words = sequence(&calls, 0x8004_0000);
        run(&mut bus, &words, 0x8004_0000);
        let r = |bus: &mut Bus, i: u32| bus.read32(0x8004_0000 + 4 * i);
        assert_eq!(r(&mut bus, 0), 0);
        assert_eq!(r(&mut bus, 1), super::errno::XDEV);
        assert_eq!(r(&mut bus, 15), 15, "the last handle");
        assert_eq!(r(&mut bus, 16), u32::MAX, "no handle left");
        assert_eq!(r(&mut bus, 17), super::errno::MFILE);
        assert_eq!(r(&mut bus, 18), 0, "erase needs a handle too");
        assert_eq!(r(&mut bus, 19), super::errno::MFILE);
    }
}
