// SPDX-License-Identifier: GPL-2.0-or-later
//! File and device layer of the HLE kernel, the TTY and CD-ROM devices, and
//! the kernel's CD-ROM driver.
//!
//! Layout and behaviour follow psx-spx "BIOS File Functions", "BIOS CDROM
//! Functions" and "BIOS Control Blocks" (FCB 2Ch bytes at 8648h, DCB 50h
//! bytes at 6EE0h), with OpenBIOS `fileio/` and `cdrom/` (pcsx-redux, MIT)
//! as the specification for the call protocol between the file functions
//! and device drivers.
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
//! interrupt output and polls the flags, as OpenBIOS does for its blocking
//! operations; a waiting call is retried with interrupts serviced.

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

/// Device flag: filesystem device (read/write go to the driver directly).
pub const DEV_FS: u32 = 0x10;
/// Device flag: block device (in_out counts are in blocks).
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
}

/// Sector buffer for directory reads and partial sectors (2 KiB).
pub const SECTOR_BUF: u32 = 0x0000_3200;
/// Device name strings.
const STRINGS: u32 = 0x0000_3180;
/// Header buffer for A(42h)/A(51h) into Exec.
pub const EXEC_HEADER: u32 = 0x0000_3A00;

/// Kernel-internal trap functions of the file layer and devices.
pub mod internal {
    /// Continuation after a driver open(): FCB result.
    pub const CONT_OPEN: u8 = 0x10;
    /// Continuation after a filesystem read()/write(): error bookkeeping.
    pub const CONT_FS_RW: u8 = 0x11;
    /// Continuation after in_out(): advance the file position.
    pub const CONT_INOUT: u8 = 0x12;
    /// Continuation after close(): free the FCB.
    pub const CONT_CLOSE: u8 = 0x13;
    /// Continuation returning 1 (AddDevice after init()).
    pub const CONT_ONE: u8 = 0x14;
    /// Continuation returning the driver's v0 unchanged.
    pub const CONT_PASS: u8 = 0x15;
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
    for (slot, dev) in [TTY, CDROM].iter().enumerate() {
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
    // CD-ROM IRQ handlers at priority 0, in front of the SYSCALL handler
    // (psx-spx order: CdromDmaIrq, CdromIoIrq, SyscallException), and the
    // five CD-ROM events the kernel opens and enables for itself.
    for (hi, func) in [
        (HI_CD_IO, internal::CD_IO_IRQ),
        (HI_CD_DMA, internal::CD_DMA_IRQ),
    ] {
        poke32(bus, hi + 4, 0);
        poke32(bus, hi + 8, stub_addr(3, func));
        crate::hle_exceptions::enq_int(bus, 0, hi);
    }
    for (i, spec) in [0x10u32, 0x20, 0x40, 0x80, 0x8000].into_iter().enumerate() {
        let ev = crate::hle_exceptions::open_event(bus, 0xF000_0003, spec, 0x2000, 0);
        crate::hle_exceptions::set_event_enabled(bus, ev, true);
        poke32(bus, kvar::CD_EVENTS + 4 * i as u32, ev);
    }
    poke32(bus, crate::hle_kernel::kvar::CD_KERNEL_ACTIVE, 1);
}

/// A(72h)/A(56h) _96_remove: close the kernel's CD events and remove its
/// CD-ROM IRQ handlers.
pub fn cd_remove(bus: &mut Bus) {
    if peek32(bus, crate::hle_kernel::kvar::CD_KERNEL_ACTIVE) == 0 {
        return;
    }
    for i in 0..5 {
        let ev = peek32(bus, kvar::CD_EVENTS + 4 * i);
        crate::hle_exceptions::close_event(bus, ev);
    }
    crate::hle_exceptions::deq_int(bus, 0, HI_CD_DMA);
    crate::hle_exceptions::deq_int(bus, 0, HI_CD_IO);
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

/// Split "dev12:rest" into the DCB of "dev", the device number (digits
/// read as hex, OpenBIOS splitFilepathAndFindDevice) and the address of
/// "rest". Leading spaces are skipped.
pub fn find_device(bus: &Bus, path: u32) -> Option<(u32, u32, u32)> {
    let mut p = path;
    while bus.try_read8(p) == Some(b' ') {
        p += 1;
    }
    let full = read_cstr(bus, p, 64);
    let colon = full.find(':')?;
    let dev = &full[..colon];
    let digits = dev.find(|c: char| c.is_ascii_digit()).unwrap_or(dev.len());
    let (name, num) = dev.split_at(digits);
    let id = num.bytes().fold(0u32, |acc, c| {
        let d = if c.is_ascii_digit() { c - b'0' } else { 0 };
        acc * 0x10 + u32::from(d)
    });
    let d = (0..DCB_COUNT).map(dcb_addr).find(|&d| {
        let n = peek32(bus, d + dcb::NAME);
        n != 0 && read_cstr(bus, n, 16) == name
    })?;
    Some((d, id, p + colon as u32 + 1))
}

/// Push a continuation frame and call `target(args...)`, returning into
/// the internal continuation `cont` with `saved` available to it.
pub fn call_then(
    bus: &mut Bus,
    gprs: &mut [u32; 32],
    target: u32,
    args: &[u32],
    cont: u8,
    saved: u32,
) -> u32 {
    let sp = gprs[29].wrapping_sub(0x18);
    poke32(bus, sp + 0x10, gprs[31]);
    poke32(bus, sp + 0x14, saved);
    gprs[29] = sp;
    for (i, a) in args.iter().enumerate() {
        gprs[4 + i] = *a;
    }
    gprs[31] = stub_addr(3, cont);
    target
}

/// Pop the frame pushed by [`call_then`]; returns the saved word and
/// restores `$ra` and `$sp` of the original caller.
pub fn pop_frame(bus: &Bus, gprs: &mut [u32; 32]) -> u32 {
    let sp = gprs[29];
    gprs[31] = peek32(bus, sp + 0x10);
    let saved = peek32(bus, sp + 0x14);
    gprs[29] = sp.wrapping_add(0x18);
    saved
}

/// What a file function asks the dispatcher to do.
pub enum FileCall {
    /// Return this value now.
    Return(u32),
    /// Jump to a driver (after [`call_then`] set up the frame).
    Jump(u32),
}

/// B(32h) open(path, mode).
pub fn open(bus: &mut Bus, gprs: &mut [u32; 32], path: u32, mode: u32) -> FileCall {
    let Some(fd) = free_fcb(bus) else {
        set_errno(bus, errno::MFILE);
        return FileCall::Return(u32::MAX);
    };
    let Some((d, id, name)) = find_device(bus, path) else {
        set_errno(bus, errno::NODEV);
        return FileCall::Return(u32::MAX);
    };
    let f = fcb_addr(fd);
    poke32(bus, f + fcb::STATUS, mode);
    poke32(bus, f + fcb::DEVICE_ID, id);
    poke32(bus, f + fcb::DCB, d);
    poke32(bus, f + fcb::DEVICE_FLAGS, peek32(bus, d + dcb::FLAGS));
    poke32(bus, f + fcb::ERROR, 0);
    let target = peek32(bus, d + dcb::OPEN);
    FileCall::Jump(call_then(
        bus,
        gprs,
        target,
        &[f, name, mode],
        internal::CONT_OPEN,
        fd,
    ))
}

/// B(33h) lseek(fd, offset, whence): 0 set, 1 relative, 2 (end) leaves
/// the position unchanged as the retail kernel does.
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
            poke32(bus, f + fcb::ERROR, errno::INVAL);
            set_errno(bus, errno::INVAL);
            return u32::MAX;
        }
    };
    poke32(bus, f + fcb::FPOS, new);
    new
}

/// B(34h) read / B(35h) write (`cmd` 1 / 2).
pub fn read_write(
    bus: &mut Bus,
    gprs: &mut [u32; 32],
    fd: u32,
    buf: u32,
    len: u32,
    cmd: u32,
) -> FileCall {
    let Some(f) = open_fcb(bus, fd) else {
        set_errno(bus, errno::BADF);
        return FileCall::Return(u32::MAX);
    };
    let d = peek32(bus, f + fcb::DCB);
    if peek32(bus, f + fcb::DEVICE_FLAGS) & DEV_FS != 0 {
        let target = peek32(bus, d + if cmd == 1 { dcb::READ } else { dcb::WRITE });
        return FileCall::Jump(call_then(
            bus,
            gprs,
            target,
            &[f, buf, len],
            internal::CONT_FS_RW,
            fd,
        ));
    }
    let mut count = len;
    if peek32(bus, d + dcb::FLAGS) & DEV_BLOCK != 0 {
        let block = peek32(bus, d + dcb::BLOCK).max(1);
        if !peek32(bus, f + fcb::FPOS).is_multiple_of(block) {
            return FileCall::Return(u32::MAX);
        }
        count /= block;
    }
    poke32(bus, f + fcb::TADDR, buf);
    poke32(bus, f + fcb::TLEN, count);
    let target = peek32(bus, d + dcb::INOUT);
    FileCall::Jump(call_then(
        bus,
        gprs,
        target,
        &[f, cmd],
        internal::CONT_INOUT,
        fd,
    ))
}

/// B(36h) close(fd).
pub fn close(bus: &mut Bus, gprs: &mut [u32; 32], fd: u32) -> FileCall {
    let Some(f) = open_fcb(bus, fd) else {
        set_errno(bus, errno::BADF);
        return FileCall::Return(u32::MAX);
    };
    let target = peek32(bus, peek32(bus, f + fcb::DCB) + dcb::CLOSE);
    FileCall::Jump(call_then(bus, gprs, target, &[f], internal::CONT_CLOSE, fd))
}

/// Continuations: `v0` is the driver's result, `saved` the fd.
pub fn continuation(bus: &mut Bus, which: u8, v0: u32, saved: u32) -> u32 {
    let f = fcb_addr(saved);
    match which {
        internal::CONT_OPEN => {
            if v0 != 0 {
                set_errno(bus, peek32(bus, f + fcb::ERROR));
                poke32(bus, f + fcb::STATUS, 0);
                return u32::MAX;
            }
            poke32(bus, f + fcb::FPOS, 0);
            saved
        }
        internal::CONT_FS_RW => {
            if (v0 as i32) < 0 {
                set_errno(bus, peek32(bus, f + fcb::ERROR));
            }
            v0
        }
        internal::CONT_INOUT => {
            if (v0 as i32) > 0 {
                let pos = peek32(bus, f + fcb::FPOS);
                poke32(bus, f + fcb::FPOS, pos.wrapping_add(v0));
            } else if (v0 as i32) < 0 {
                set_errno(bus, peek32(bus, f + fcb::ERROR));
            }
            v0
        }
        internal::CONT_CLOSE => {
            poke32(bus, f + fcb::STATUS, 0);
            if v0 != 0 {
                set_errno(bus, peek32(bus, f + fcb::ERROR));
                return u32::MAX;
            }
            saved
        }
        internal::CONT_ONE => 1,
        _ => v0,
    }
}

/// B(47h) AddDevice(dcb): copy into the first free DCB and call its
/// init(); returns 1, or 0 when all ten DCBs are used.
pub fn add_device(bus: &mut Bus, gprs: &mut [u32; 32], src: u32) -> FileCall {
    let Some(d) = (0..DCB_COUNT)
        .map(dcb_addr)
        .find(|&d| peek32(bus, d + dcb::NAME) == 0)
    else {
        return FileCall::Return(0);
    };
    for off in (0..DCB_SIZE).step_by(4) {
        let w = peek32(bus, src + off);
        poke32(bus, d + off, w);
    }
    let init = peek32(bus, d + dcb::INIT);
    FileCall::Jump(call_then(bus, gprs, init, &[], internal::CONT_ONE, 0))
}

/// B(48h) RemoveDevice(name): call deinit and free the DCB; 1 or 0.
pub fn remove_device(bus: &mut Bus, gprs: &mut [u32; 32], name: u32) -> FileCall {
    let wanted = read_cstr(bus, name, 16);
    let Some(d) = (0..DCB_COUNT).map(dcb_addr).find(|&d| {
        let n = peek32(bus, d + dcb::NAME);
        n != 0 && read_cstr(bus, n, 16) == wanted
    }) else {
        return FileCall::Return(0);
    };
    poke32(bus, d + dcb::NAME, 0);
    let deinit = peek32(bus, d + dcb::DEINIT);
    FileCall::Jump(call_then(bus, gprs, deinit, &[], internal::CONT_ONE, 0))
}

/// A(96h) AddCDROMDevice: install the kernel CD-ROM device when absent.
pub fn add_kernel_cdrom(bus: &mut Bus) -> u32 {
    let present = (0..DCB_COUNT).map(dcb_addr).any(|d| {
        let n = peek32(bus, d + dcb::NAME);
        n != 0 && read_cstr(bus, n, 16) == "cdrom"
    });
    if present {
        return 1;
    }
    let Some(slot) = (0..DCB_COUNT).find(|&i| peek32(bus, dcb_addr(i) + dcb::NAME) == 0) else {
        return 0;
    };
    let d = dcb_addr(slot);
    // Reuse the strings written at boot (second device name).
    let name = STRINGS + TTY.name.len() as u32 + 1 + TTY.desc.len() as u32 + 1;
    poke32(bus, d + dcb::NAME, name);
    poke32(bus, d + dcb::FLAGS, CDROM.flags);
    poke32(bus, d + dcb::BLOCK, CDROM.block);
    poke32(bus, d + dcb::DESC, name + CDROM.name.len() as u32 + 1);
    for off in (dcb::INIT..DCB_SIZE).step_by(4) {
        poke32(bus, d + off, stub_addr(3, internal::NOP));
    }
    for (off, func) in CDROM.entries {
        poke32(bus, d + off, stub_addr(3, *func));
    }
    1
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
/// Returns 1 on success, 0 on failure (OpenBIOS).
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
}
