// SPDX-License-Identifier: GPL-2.0-or-later
//! Kernel RAM for the HLE BIOS: layout, trap stubs, heaps and kernel-patch
//! handling.
//!
//! Every piece of kernel state lives in guest RAM (the first 64 KiB), so it
//! is covered by save states, visible to debuggers, and deterministic. The
//! host keeps only diagnostics.
//!
//! Layout (see docs/hle-bios-provenance.md for sources):
//!
//! | Address | Contents |
//! |---|---|
//! | `0x0060..0x006B` | RAM size (MB), `0`, `0xFF` (psx-spx) |
//! | `0x0100..0x0157` | Table of tables (psx-spx) |
//! | `0x0180..0x01FF` | `BOOT` argument (written by disc boot) |
//! | `0x0200..0x04FF` | A0 table, 0xC0 entries (retail address) |
//! | `0x0674..0x06F3` | C0 table, 0x20 entries (retail address) |
//! | `0x0874..0x09F3` | B0 table, 0x60 entries (retail address) |
//! | `0x0A00..0x0A7F` | HLE kernel variables ([`kvar`]) |
//! | `0x0C80..0x0E7F` | C(06h) entry and patch zone (retail address) |
//! | `0x1000..0x15FF` | trap stubs, one word per function |
//! | `0x43D0..0x5DFF` | B(5Bh) entry and patch zone (retail address) |
//! | `0x6EE0..0x71FF` | DCBs, `0x8648..0x8907` FCBs (retail addresses) |
//! | `0xDF80..0xDFFF` | left to games (psx-spx: "used for BIOS patches") |
//! | `0xE000..0xFFFF` | kernel heap: ExCB, EvCB, PCB, TCB, then free |
//!
//! Table, entry and patch-zone addresses match the retail kernel because
//! games read table entries and write at fixed offsets from them (Psy-Q
//! kernel patches); keeping them lets those writes land where they do on a
//! console instead of on HLE state.

use crate::Bus;

/// A0 table base.
pub const A0_TABLE: u32 = 0x0000_0200;
/// B0 table base.
pub const B0_TABLE: u32 = 0x0000_0874;
/// C0 table base.
pub const C0_TABLE: u32 = 0x0000_0674;
/// Entries per table.
pub const A0_LEN: u32 = 0xC0;
/// Entries per table.
pub const B0_LEN: u32 = 0x60;
/// Entries per table.
pub const C0_LEN: u32 = 0x20;

/// First word of the trap stub array. Stub for `(table, func)` is at
/// `STUBS + 4 * (table_offset + func)`.
pub const STUBS: u32 = 0x0000_1000;
const STUB_OFFSET: [u32; 4] = [0, 0xC0, 0x120, 0x140];
/// Number of kernel-internal functions reachable through stubs.
pub const INTERNAL_LEN: u32 = 0x40;

/// C(06h), the exception handler entry. Games read `C0[6]` and patch at
/// fixed offsets from it.
pub const EXCEPTION_HANDLER: u32 = 0x0000_0C80;
/// End of the region reserved at [`EXCEPTION_HANDLER`].
pub const EXCEPTION_HANDLER_END: u32 = 0x0000_0E80;
/// B(5Bh), the pad/card auto-ack function. Games read `B0[5Bh]` and
/// patch at fixed offsets from it (up to +1988h is documented).
pub const PAD_CARD_ENTRY: u32 = 0x0000_43D0;
/// End of the region reserved at [`PAD_CARD_ENTRY`].
pub const PAD_CARD_ENTRY_END: u32 = 0x0000_5E00;

/// Device control blocks (fixed; psx-spx 0x150).
pub const DCB_BASE: u32 = 0x0000_6EE0;
/// DCB area size: 10 blocks of 50h.
pub const DCB_SIZE: u32 = 0x320;
/// File control blocks (fixed; psx-spx 0x140).
pub const FCB_BASE: u32 = 0x0000_8648;
/// FCB area size: 16 blocks of 2Ch.
pub const FCB_SIZE: u32 = 0x2C0;

/// Kernel heap managed by SysInitMemory / alloc_kernel_memory.
pub const KERNEL_HEAP: u32 = 0xA000_E000;
/// Kernel heap size.
pub const KERNEL_HEAP_SIZE: u32 = 0x2000;

/// Table of tables.
pub const TOT: u32 = 0x0000_0100;
/// ExCB: 4 priorities of 8 bytes.
pub const EXCB_SIZE: u32 = 0x20;
/// Bytes per EvCB.
pub const EVCB_SIZE: u32 = 0x1C;
/// Bytes per TCB.
pub const TCB_SIZE: u32 = 0xC0;
/// TCB status: free.
pub const TCB_FREE: u32 = 0x1000;
/// TCB status: in use.
pub const TCB_USED: u32 = 0x4000;

/// Kernel variables owned by the HLE, at `0x0A00`.
pub mod kvar {
    /// User heap: first block header address (0 = no InitHeap yet).
    pub const USER_HEAP_START: u32 = 0x0A00;
    /// User heap: end address (exclusive).
    pub const USER_HEAP_END: u32 = 0x0A04;
    /// Kernel heap: first block header address.
    pub const KERNEL_HEAP_START: u32 = 0x0A08;
    /// Kernel heap: end address (exclusive).
    pub const KERNEL_HEAP_END: u32 = 0x0A0C;
    /// Number of EvCBs (SYSTEM.CNF EVENT / SetConf).
    pub const CONF_EVENT: u32 = 0x0A10;
    /// Number of TCBs (SYSTEM.CNF TCB / SetConf).
    pub const CONF_TCB: u32 = 0x0A14;
    /// Stack top (SYSTEM.CNF STACK / SetConf).
    pub const CONF_STACK: u32 = 0x0A18;
    /// B(5Bh) pad/card VBlank auto-acknowledge flag.
    pub const SIO0_AUTO_ACK: u32 = 0x0A1C;
    /// Nonzero while the kernel's CD-ROM handlers are installed
    /// (A(71h)/A(54h) set it, A(72h)/A(56h) clear it).
    pub const CD_KERNEL_ACTIVE: u32 = 0x0A20;
    /// Kernel patches applied, one bit per [`super::Patch`].
    pub const PATCH_FLAGS: u32 = 0x0A24;
    /// Memory card handler delay requested by `_patch_card2`.
    pub const MC_HANDLER_DELAY: u32 = 0x0A28;
    /// Pad driver enable toggled by the injected startPad/stopPad.
    pub const PAD_STARTED: u32 = 0x0A2C;
    /// setPadOutputData arguments: pad1 buffer, size, pad2 buffer, size.
    pub const PAD_OUTPUT: u32 = 0x0A30;
}

/// Kernel-internal functions reachable through stubs in table 3.
pub mod internal {
    /// Pad driver enable, injected by the `_patch_pad` counterpatch.
    pub const START_PAD: u8 = 0x00;
    /// Pad driver disable, injected by the `_patch_pad` counterpatch.
    pub const STOP_PAD: u8 = 0x01;
    /// Pad output buffers, injected by the `_send_pad` counterpatch.
    pub const SET_PAD_OUTPUT_DATA: u8 = 0x02;
}

/// BREAK code prefix that marks an HLE trap word. Psy-Q only emits small
/// BREAK codes (6 and 7 for divide checks), so this range is free.
const TRAP_MAGIC: u32 = 0xB_0000;

/// Trap word for `(table, func)`; table 0..=2 = A/B/C, 3 = internal.
pub fn trap_word(table: u8, func: u8) -> u32 {
    let code = TRAP_MAGIC | (u32::from(table & 3) << 8) | u32::from(func);
    (code << 6) | 0x0D
}

/// Decode a trap word into `(table, func)`.
pub fn decode_trap(word: u32) -> Option<(u8, u8)> {
    if word & 0xFC00_003F != 0x0D {
        return None;
    }
    let code = (word >> 6) & 0xF_FFFF;
    if code & 0xF_FC00 != TRAP_MAGIC {
        return None;
    }
    Some((((code >> 8) & 3) as u8, code as u8))
}

/// Address of the stub for `(table, func)`.
pub fn stub_addr(table: u8, func: u8) -> u32 {
    STUBS + 4 * (STUB_OFFSET[usize::from(table & 3)] + u32::from(func))
}

/// Table base and length for table 0..=2.
pub fn table(table: u8) -> (u32, u32) {
    match table {
        0 => (A0_TABLE, A0_LEN),
        1 => (B0_TABLE, B0_LEN),
        _ => (C0_TABLE, C0_LEN),
    }
}

/// Side-effect-free RAM/scratchpad word read (any alignment); unmapped
/// bytes read as 0.
pub fn peek32(bus: &Bus, addr: u32) -> u32 {
    (0..4).fold(0, |acc, k| {
        acc | u32::from(bus.try_read8(addr.wrapping_add(k)).unwrap_or(0)) << (8 * k)
    })
}

/// Side-effect-free RAM/scratchpad word write (any alignment); writes to
/// unmapped bytes are dropped.
pub fn poke32(bus: &mut Bus, addr: u32, value: u32) {
    for (k, byte) in value.to_le_bytes().into_iter().enumerate() {
        bus.write8_safe(addr.wrapping_add(k as u32), byte);
    }
}

fn fill(bus: &mut Bus, start: u32, end: u32, value: u8) {
    for addr in start..end {
        bus.write8_safe(addr, value);
    }
}

/// Kernel sizing taken from SYSTEM.CNF (or its defaults).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct KernelConfig {
    /// Thread control blocks.
    pub tcb: u32,
    /// Event control blocks.
    pub event: u32,
    /// Stack top.
    pub stack: u32,
}

impl Default for KernelConfig {
    fn default() -> Self {
        Self {
            tcb: crate::system_cnf::DEFAULT_TCB,
            event: crate::system_cnf::DEFAULT_EVENT,
            stack: crate::system_cnf::DEFAULT_STACK,
        }
    }
}

/// Lay out the kernel in guest RAM. Leaves `0x0000..0x00FF` and the `BOOT`
/// argument at `0x0180` alone apart from the documented variables.
pub fn install(bus: &mut Bus, cfg: KernelConfig) {
    fill(bus, TOT, 0x180, 0);
    fill(bus, 0x200, 0x1_0000, 0);
    poke32(bus, crate::hle_bios::RAM_SIZE_MB_VAR, 2);
    poke32(bus, 0x64, 0);
    poke32(bus, 0x68, 0xFF);

    for t in 0..3u8 {
        let (base, len) = table(t);
        for func in 0..len as u8 {
            let stub = stub_addr(t, func);
            poke32(bus, stub, trap_word(t, func));
            poke32(bus, base + 4 * u32::from(func), stub);
        }
    }
    for func in 0..INTERNAL_LEN as u8 {
        poke32(bus, stub_addr(3, func), trap_word(3, func));
    }
    poke32(bus, EXCEPTION_HANDLER, trap_word(2, 0x06));
    poke32(bus, C0_TABLE + 4 * 0x06, EXCEPTION_HANDLER);
    poke32(bus, PAD_CARD_ENTRY, trap_word(1, 0x5B));
    poke32(bus, B0_TABLE + 4 * 0x5B, PAD_CARD_ENTRY);

    poke32(bus, kvar::SIO0_AUTO_ACK, 1);
    poke32(bus, TOT + 0x40, FCB_BASE);
    poke32(bus, TOT + 0x44, FCB_SIZE);
    poke32(bus, TOT + 0x50, DCB_BASE);
    poke32(bus, TOT + 0x54, DCB_SIZE);
    allocate_control_blocks(bus, cfg);
}

/// Initialise the kernel heap and allocate ExCB, EvCB, PCB and TCB in the
/// retail order, then point the table of tables and the PCB at them. With
/// the retail 4-byte block header this reproduces the census addresses
/// (ExCB A000E004h, EvCB A000E028h, ...).
fn allocate_control_blocks(bus: &mut Bus, cfg: KernelConfig) {
    init_heap(bus, Heap::Kernel, KERNEL_HEAP, KERNEL_HEAP_SIZE);
    let excb = malloc(bus, Heap::Kernel, EXCB_SIZE);
    let evcb_size = cfg.event.saturating_mul(EVCB_SIZE);
    let evcb = malloc(bus, Heap::Kernel, evcb_size);
    let pcb = malloc(bus, Heap::Kernel, 4);
    let tcb_size = cfg.tcb.saturating_mul(TCB_SIZE);
    let tcb = malloc(bus, Heap::Kernel, tcb_size);
    for (addr, size) in [
        (excb, EXCB_SIZE),
        (evcb, evcb_size),
        (pcb, 4),
        (tcb, tcb_size),
    ] {
        if addr != 0 {
            fill(bus, addr, addr + size, 0);
        }
    }
    for (slot, (addr, size)) in [(excb, EXCB_SIZE), (pcb, 4), (tcb, tcb_size)]
        .into_iter()
        .enumerate()
    {
        poke32(bus, TOT + 8 * slot as u32, addr);
        poke32(bus, TOT + 8 * slot as u32 + 4, size);
    }
    poke32(bus, TOT + 0x20, evcb);
    poke32(bus, TOT + 0x24, evcb_size);
    if pcb != 0 {
        poke32(bus, pcb, tcb);
    }
    for i in 0..cfg.tcb {
        let status = if i == 0 { TCB_USED } else { TCB_FREE };
        poke32(bus, tcb + i * TCB_SIZE, status);
    }
    poke32(bus, kvar::CONF_EVENT, cfg.event);
    poke32(bus, kvar::CONF_TCB, cfg.tcb);
    poke32(bus, kvar::CONF_STACK, cfg.stack);
}

/// A(9Ch) SetConf: drop every kernel allocation and allocate new control
/// blocks. The current thread's TCB contents move to the new TCB 0.
pub fn set_conf(bus: &mut Bus, event: u32, tcb: u32, stack: u32) {
    let old = current_tcb(bus);
    let saved: Vec<u32> = (0..TCB_SIZE / 4)
        .map(|i| peek32(bus, old + 4 * i))
        .collect();
    allocate_control_blocks(bus, KernelConfig { tcb, event, stack });
    let new = current_tcb(bus);
    if old != 0 && new != 0 {
        for (i, word) in saved.into_iter().enumerate() {
            poke32(bus, new + 4 * i as u32, word);
        }
        poke32(bus, new, TCB_USED);
    }
}

/// A(9Dh) GetConf values: `(event, tcb, stack)`.
pub fn get_conf(bus: &Bus) -> (u32, u32, u32) {
    (
        peek32(bus, kvar::CONF_EVENT),
        peek32(bus, kvar::CONF_TCB),
        peek32(bus, kvar::CONF_STACK),
    )
}

/// Current thread's TCB: `[[0x108]]`, or 0 when the chain is broken.
pub fn current_tcb(bus: &Bus) -> u32 {
    let pcb = peek32(bus, TOT + 8);
    if pcb == 0 {
        return 0;
    }
    peek32(bus, pcb)
}

// ---------------------------------------------------------------- heaps

/// Which heap an allocation call addresses.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Heap {
    /// A(33h)..A(39h), set up by InitHeap.
    User,
    /// B(00h)/B(01h), set up by SysInitMemory.
    Kernel,
}

impl Heap {
    fn vars(self) -> (u32, u32) {
        match self {
            Heap::User => (kvar::USER_HEAP_START, kvar::USER_HEAP_END),
            Heap::Kernel => (kvar::KERNEL_HEAP_START, kvar::KERNEL_HEAP_END),
        }
    }
}

// Block format: a 4-byte header before each block holds the block size in
// bytes (a multiple of 4); bit 0 set marks a free block. psx-spx documents
// free() as `[buf-4] |= 1`, and the census kernel heap shows the size
// headers (E000h holds 20h for the ExCB block).
const FREE: u32 = 1;

/// InitHeap / SysInitMemory: the whole region becomes one free block.
pub fn init_heap(bus: &mut Bus, heap: Heap, addr: u32, size: u32) {
    let (start_var, end_var) = heap.vars();
    let start = (addr.wrapping_add(3)) & !3;
    let end = addr.wrapping_add(size) & !3;
    if end <= start.wrapping_add(4) || end < start {
        poke32(bus, start_var, 0);
        poke32(bus, end_var, 0);
        return;
    }
    poke32(bus, start_var, start);
    poke32(bus, end_var, end);
    poke32(bus, start, (end - start - 4) | FREE);
}

/// malloc / alloc_kernel_memory: first fit, size rounded up to 4, free
/// neighbours merged while scanning, remainder split off when it can hold
/// a header plus 4 bytes. Returns 0 when nothing fits or no heap is set.
pub fn malloc(bus: &mut Bus, heap: Heap, size: u32) -> u32 {
    let (start_var, end_var) = heap.vars();
    let (start, end) = (peek32(bus, start_var), peek32(bus, end_var));
    if start == 0 || end <= start || size > end - start {
        return 0;
    }
    let want = size.max(4).wrapping_add(3) & !3;
    let mut p = start;
    while p.wrapping_add(4) <= end {
        let header = peek32(bus, p);
        let mut len = header & !3;
        if header & FREE != 0 {
            loop {
                let next = p + 4 + len;
                if next + 4 > end || peek32(bus, next) & FREE == 0 {
                    break;
                }
                len += 4 + (peek32(bus, next) & !3);
            }
            len = len.min(end - p - 4);
            if len >= want {
                if len - want >= 8 {
                    poke32(bus, p, want);
                    poke32(bus, p + 4 + want, (len - want - 4) | FREE);
                } else {
                    poke32(bus, p, len);
                }
                return p + 4;
            }
            poke32(bus, p, len | FREE);
        }
        p = p.wrapping_add(4).wrapping_add(len);
    }
    0
}

/// free / free_kernel_memory: `[buf-4] |= 1`, no checking (psx-spx).
pub fn free(bus: &mut Bus, buf: u32) {
    if buf == 0 {
        return;
    }
    let header = buf.wrapping_sub(4);
    let value = peek32(bus, header);
    poke32(bus, header, value | FREE);
}

/// calloc: malloc(x*y) then zero fill.
pub fn calloc(bus: &mut Bus, x: u32, y: u32) -> u32 {
    let size = x.wrapping_mul(y);
    let p = malloc(bus, Heap::User, size);
    if p != 0 {
        fill(bus, p, p.wrapping_add(size), 0);
    }
    p
}

/// realloc as psx-spx documents it: malloc(new), copy `new_size` bytes
/// from the old block (the retail over-read), free the old block.
pub fn realloc(bus: &mut Bus, old: u32, new_size: u32) -> u32 {
    if old == 0 {
        return malloc(bus, Heap::User, new_size);
    }
    if new_size == 0 {
        free(bus, old);
        return 0;
    }
    let new = malloc(bus, Heap::User, new_size);
    if new == 0 {
        return 0;
    }
    for i in 0..new_size.min(0x20_0000) {
        let b = bus.try_read8(old.wrapping_add(i)).unwrap_or(0);
        bus.write8_safe(new.wrapping_add(i), b);
    }
    free(bus, old);
    new
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trap_words_round_trip_and_ignore_ordinary_breaks() {
        for (t, f) in [(0u8, 0u8), (1, 0x5B), (2, 0x1F), (3, 0x3F)] {
            assert_eq!(decode_trap(trap_word(t, f)), Some((t, f)));
        }
        // break 7 (divide by zero) and break 0.
        assert_eq!(decode_trap((7 << 16) | 0x0D), None);
        assert_eq!(decode_trap(0x0D), None);
        assert_eq!(decode_trap(0), None);
    }

    #[test]
    fn install_reproduces_the_retail_control_block_addresses() {
        let mut bus = Bus::new_without_bios();
        install(
            &mut bus,
            KernelConfig {
                tcb: 4,
                event: 0x16,
                stack: 0x801F_FFF0,
            },
        );
        // Census (SCPH1001, EVENT = 16h): ExCB A000E004h/20h, PCB A000E294h,
        // TCB A000E29Ch/300h, EvCB A000E028h/268h.
        let tot: Vec<u32> = (0..10).map(|i| peek32(&bus, TOT + 4 * i)).collect();
        assert_eq!(
            tot,
            [
                0xA000_E004,
                0x20,
                0xA000_E294,
                4,
                0xA000_E29C,
                0x300,
                0,
                0,
                0xA000_E028,
                0x268
            ]
        );
        assert_eq!(peek32(&bus, 0xE000), 0x20);
        assert_eq!(current_tcb(&bus), 0xA000_E29C);
        assert_eq!(peek32(&bus, 0xA000_E29C), TCB_USED);
        assert_eq!(peek32(&bus, 0xA000_E29C + TCB_SIZE), TCB_FREE);
        assert_eq!(peek32(&bus, TOT + 0x40), FCB_BASE);
        assert_eq!(peek32(&bus, TOT + 0x50), DCB_BASE);
        assert_eq!((peek32(&bus, 0x60), peek32(&bus, 0x68)), (2, 0xFF));
        // Table entries point at stubs, except the two patch targets.
        assert_eq!(peek32(&bus, A0_TABLE + 4 * 0x3C), stub_addr(0, 0x3C));
        assert_eq!(
            decode_trap(peek32(&bus, stub_addr(0, 0x3C))),
            Some((0, 0x3C))
        );
        assert_eq!(peek32(&bus, C0_TABLE + 0x18), EXCEPTION_HANDLER);
        assert_eq!(peek32(&bus, B0_TABLE + 0x16C), PAD_CARD_ENTRY);
        assert_eq!(decode_trap(peek32(&bus, PAD_CARD_ENTRY)), Some((1, 0x5B)));
        assert_eq!(get_conf(&bus), (0x16, 4, 0x801F_FFF0));
    }

    #[test]
    fn heap_follows_the_documented_block_rules() {
        let mut bus = Bus::new_without_bios();
        assert_eq!(malloc(&mut bus, Heap::User, 16), 0, "no InitHeap yet");
        init_heap(&mut bus, Heap::User, 0x8010_0002, 0x100);
        let a = malloc(&mut bus, Heap::User, 5);
        assert_eq!(a, 0x8010_0008, "aligned start plus header");
        assert_eq!(peek32(&bus, a - 4), 8, "size rounded up to 4");
        let b = malloc(&mut bus, Heap::User, 8);
        assert_eq!(b, a + 8 + 4);
        free(&mut bus, a);
        assert_eq!(peek32(&bus, a - 4), 8 | 1);
        // A later request that fits reuses the freed block.
        assert_eq!(malloc(&mut bus, Heap::User, 4), a);
        free(&mut bus, a);
        free(&mut bus, b);
        // Freed neighbours merge: 8 + 4 + 8 bytes are available at a.
        assert_eq!(malloc(&mut bus, Heap::User, 20), a);
        assert_eq!(malloc(&mut bus, Heap::User, 0x1000), 0);

        let c = calloc(&mut bus, 4, 4);
        assert!((0..16).all(|i| bus.try_read8(c + i) == Some(0)));
        poke32(&mut bus, c, 0x1122_3344);
        let d = realloc(&mut bus, c, 8);
        assert_eq!(peek32(&bus, d), 0x1122_3344);
        assert_eq!(peek32(&bus, c - 4) & 1, 1, "old block freed");
        assert_eq!(realloc(&mut bus, d, 0), 0);
    }
}
