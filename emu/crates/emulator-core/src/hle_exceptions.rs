// SPDX-License-Identifier: GPL-2.0-or-later
//! Exception core of the HLE kernel: exception vector, handler, priority
//! chains, events, root-counter and default IRQ handlers, SYSCALL 1/2/3,
//! and threads.
//!
//! The parts that call guest code or are patched by games run as guest
//! MIPS code assembled at boot ([`crate::hle_asm`]): the vector at 80h,
//! the exception handler at C(06h), ReturnFromException, DeliverEvent,
//! the root-counter and default-IRQ verifiers. Their layout and register
//! protocol follow psx-spx "BIOS Interrupt/Exception Handling" and the
//! OpenBIOS handler (pcsx-redux src/mips/openbios/kernel/vectors.s, MIT),
//! which keeps the retail offsets that Psy-Q kernel patches write to.
//! Functions that only touch kernel structures are HLE traps. All state
//! lives in guest RAM: the ExCB chains, EvCBs and TCBs from the table of
//! tables, and the variables in [`kvar`].

use crate::hle_asm::*;
use crate::hle_kernel::{peek32, poke32, stub_addr, TOT};
use crate::Bus;

/// Exception vector (KSEG0 80000080h).
pub const VECTOR: u32 = 0x0000_0080;
/// Exception stack: 1000h bytes below this address (psx-spx: fixed
/// 1000h-byte stack in the first 64 KiB).
pub const EXCEPTION_STACK_TOP: u32 = 0x0000_2600;
/// Guest routines assembled at boot.
pub const KERNEL_CODE: u32 = 0x0000_2600;
/// End of the kernel code area.
pub const KERNEL_CODE_END: u32 = 0x0000_3000;
/// Kernel data: default exit buffer, handler descriptors, IRQ table.
pub const KERNEL_DATA: u32 = 0x0000_3000;

/// ResetEntryInt's default exit buffer (setjmp layout).
pub const DEFAULT_JMPBUF: u32 = KERNEL_DATA;
/// Handler descriptors: 16 bytes each (next, second function, first
/// function, unused).
const HI_SYSCALL: u32 = KERNEL_DATA + 0x40;
const HI_RCNT: u32 = KERNEL_DATA + 0x50;
const HI_DEFINT: u32 = KERNEL_DATA + 0x90;
/// Default IRQ handler table: (I_STAT bit, event class, auto-ack variable)
/// per entry, terminated by a zero bit.
const DEFINT_TABLE: u32 = KERNEL_DATA + 0xA0;

/// Kernel variables used by the exception core.
pub mod kvar {
    /// Exit buffer the handler longjmps to after the chains
    /// (HookEntryInt / ResetEntryInt).
    pub const EXIT_JMPBUF: u32 = 0x0A44;
    /// Exception stack pointer loaded on entry.
    pub const EXCEPTION_SP: u32 = 0x0A48;
    /// ChangeClearRCnt flags for timers 0..2 and VBlank (4 words).
    pub const RCNT_AUTOACK: u32 = 0x0A50;
    /// SetIrqAutoAck flags for IRQ 0..10 (11 words).
    pub const IRQ_AUTOACK: u32 = 0x0A60;
}

/// Kernel-internal trap functions used by the exception core.
pub mod internal {
    /// Default SYSCALL/exception verifier (priority 0).
    pub const SYSCALL_VERIFIER: u8 = 0x03;
    /// Root-counter handlers for timers 0..2 and VBlank (4 entries).
    pub const RCNT_HANDLER: u8 = 0x04;
}

const I_STAT: i16 = 0x1070;
const I_MASK: i16 = 0x1074;
const IO_HI: u16 = 0x1F80;

/// Addresses inside the assembled kernel code.
#[derive(Clone, Debug)]
pub struct KernelCode {
    /// Code words for [`KERNEL_CODE`].
    pub words: Vec<u32>,
    /// Exception handler words for [`crate::hle_kernel::EXCEPTION_HANDLER`].
    pub handler: Vec<u32>,
    /// B(17h) ReturnFromException.
    pub return_from_exception: u32,
    /// B(07h) DeliverEvent.
    pub deliver_event: u32,
    /// Root-counter verifiers for timers 0..2 and VBlank.
    pub rcnt_verifier: [u32; 4],
    /// Default IRQ verifier (InitDefInt).
    pub defint_verifier: u32,
    /// DeliverEvent(F0000010h, 1000h) continuation: calls A(40h).
    pub unresolved_glue: u32,
    /// `syscall; jr ra` used by ChangeTh.
    pub syscall_stub: u32,
    /// A(43h) Exec.
    pub exec: u32,
}

/// Assembled kernel code (deterministic; built once).
pub fn code() -> &'static KernelCode {
    static CODE: std::sync::OnceLock<KernelCode> = std::sync::OnceLock::new();
    CODE.get_or_init(assemble)
}

fn assemble() -> KernelCode {
    let mut a = Asm::new(KERNEL_CODE);

    // getCop0CauseAndEPC: v0 = CAUSE, v1 = EPC.
    a.label("cause_epc");
    a.mfc0(V0, 13);
    a.mfc0(V1, 14);
    a.jr(RA);
    a.nop();

    // ReturnFromException: restore the current TCB's frame and rfe.
    // r26/k0 is not restored (psx-spx); k1 is, last (OpenBIOS: games rely
    // on k1 surviving interrupts).
    a.label("rfe");
    a.addiu(K1, ZERO, TOT as i16);
    a.lw(K1, 8, K1);
    a.nop();
    a.lw(K1, 0, K1);
    a.nop();
    a.lw(V0, 0x90, K1);
    a.addiu(K1, K1, 8);
    a.mtlo(V0);
    a.lw(V1, 0x84, K1);
    a.lw(K0, 0x80, K1);
    a.mthi(V1);
    a.lw(A1, 0x8C, K1);
    a.lw(AT, 0x04, K1);
    a.mtc0(A1, 12);
    for (reg, off) in (2..=25).map(|r| (r, (4 * r) as i16)) {
        a.lw(reg, off, K1);
    }
    a.lw(GP, 0x70, K1);
    a.lw(SP, 0x74, K1);
    a.lw(FP, 0x78, K1);
    a.lw(RA, 0x7C, K1);
    a.lw(K1, 0x6C, K1);
    a.jr(K0);
    a.rfe();

    // DeliverEvent(class, spec): every EvCB that is enabled/busy (2000h)
    // with this class and spec becomes ready (mode 2000h) or has its
    // callback called (mode 1000h).
    a.label("deliver_event");
    a.addiu(SP, SP, -24);
    a.sw(RA, 0, SP);
    a.sw(S0, 4, SP);
    a.sw(S1, 8, SP);
    a.sw(S2, 12, SP);
    a.sw(S3, 16, SP);
    a.mov(S2, A0);
    a.mov(S3, A1);
    a.addiu(T0, ZERO, (TOT + 0x20) as i16);
    a.lw(S0, 0, T0);
    a.lw(S1, 4, T0);
    a.nop();
    a.addu(S1, S0, S1);
    a.label("de_loop");
    a.sltu(T0, S0, S1);
    a.beqz(T0, "de_done");
    a.nop();
    a.lw(T0, 4, S0);
    a.ori(T1, ZERO, 0x2000);
    a.bne(T0, T1, "de_next");
    a.nop();
    a.lw(T0, 0, S0);
    a.nop();
    a.bne(T0, S2, "de_next");
    a.nop();
    a.lw(T0, 8, S0);
    a.nop();
    a.bne(T0, S3, "de_next");
    a.nop();
    a.lw(T0, 12, S0);
    a.ori(T1, ZERO, 0x2000);
    a.bne(T0, T1, "de_callback");
    a.nop();
    a.ori(T1, ZERO, 0x4000);
    a.sw(T1, 4, S0);
    a.b("de_next");
    a.nop();
    a.label("de_callback");
    a.ori(T1, ZERO, 0x1000);
    a.bne(T0, T1, "de_next");
    a.nop();
    a.lw(T0, 16, S0);
    a.nop();
    a.beqz(T0, "de_next");
    a.nop();
    a.jalr(T0);
    a.nop();
    a.label("de_next");
    a.addiu(S0, S0, 0x1C);
    a.b("de_loop");
    a.nop();
    a.label("de_done");
    a.lw(RA, 0, SP);
    a.lw(S0, 4, SP);
    a.lw(S1, 8, SP);
    a.lw(S2, 12, SP);
    a.lw(S3, 16, SP);
    a.jr(RA);
    a.addiu(SP, SP, 24);

    // Root-counter verifiers (OpenBIOS T0..T3verifier): if the IRQ is
    // enabled and pending, DeliverEvent(F200000n, 2) and return 1.
    const RCNT_LABELS: [(&str, &str); 4] = [
        ("rcnt0", "rcnt0_no"),
        ("rcnt1", "rcnt1_no"),
        ("rcnt2", "rcnt2_no"),
        ("rcnt3", "rcnt3_no"),
    ];
    for (n, (label, no)) in RCNT_LABELS.iter().enumerate() {
        let bit = rcnt_irq_bit(n);
        a.label(label);
        a.lui(T1, IO_HI);
        a.lw(T2, I_MASK, T1);
        a.lw(T3, I_STAT, T1);
        a.andi(T2, T2, bit as u16);
        a.beqz(T2, no);
        a.andi(T3, T3, bit as u16);
        a.beqz(T3, no);
        a.nop();
        a.addiu(SP, SP, -8);
        a.sw(RA, 0, SP);
        a.lui(A0, 0xF200);
        a.ori(A0, A0, n as u16);
        a.jal("deliver_event");
        a.addiu(A1, ZERO, 2);
        a.lw(RA, 0, SP);
        a.addiu(SP, SP, 8);
        a.jr(RA);
        a.addiu(V0, ZERO, 1);
        a.label(no);
        a.jr(RA);
        a.mov(V0, ZERO);
    }

    // Default IRQ verifier (OpenBIOS IRQVerifier, lossless variant): for
    // each enabled and pending IRQ, DeliverEvent(class, 1000h) and, when
    // SetIrqAutoAck enabled it, acknowledge it. Always returns 0.
    a.label("defint");
    a.addiu(SP, SP, -8);
    a.sw(RA, 0, SP);
    a.sw(S0, 4, SP);
    a.li(S0, DEFINT_TABLE);
    a.label("di_loop");
    a.lw(T0, 0, S0);
    a.nop();
    a.beqz(T0, "di_done");
    a.lui(T1, IO_HI);
    a.lw(T2, I_STAT, T1);
    a.lw(T3, I_MASK, T1);
    a.nop();
    a.and(T2, T2, T3);
    a.and(T2, T2, T0);
    a.beqz(T2, "di_next");
    a.nop();
    a.lw(A0, 4, S0);
    a.jal("deliver_event");
    a.ori(A1, ZERO, 0x1000);
    a.lw(T1, 8, S0);
    a.lw(T0, 0, S0);
    a.lw(T1, 0, T1);
    a.nop();
    a.beqz(T1, "di_next");
    a.nor(T2, T0, ZERO);
    a.lui(T1, IO_HI);
    a.sw(T2, I_STAT, T1);
    a.label("di_next");
    a.b("di_loop");
    a.addiu(S0, S0, 12);
    a.label("di_done");
    a.lw(RA, 0, SP);
    a.lw(S0, 4, SP);
    a.addiu(SP, SP, 8);
    a.jr(RA);
    a.mov(V0, ZERO);

    // After DeliverEvent(F0000010h, 1000h) for an unresolved exception:
    // call A(40h) SystemErrorUnresolvedException through the A0 vector,
    // returning to ReturnFromException.
    a.label("unresolved");
    a.li(RA, 0); // patched below with the rfe address
    a.addiu(T1, ZERO, 0x40);
    a.addiu(T0, ZERO, 0xA0);
    a.jr(T0);
    a.nop();

    // ChangeTh helper: SYSCALL(3) with a1 = new TCB; the handler returns
    // past the syscall.
    a.label("syscall_stub");
    a.syscall();
    a.jr(RA);
    a.nop();

    // A(43h) Exec(header, a1, a2) (psx-spx; register protocol from
    // OpenBIOS psxexec.s): save s0/ra/sp/fp/gp in the header's reserved
    // words, zero-fill the memfill region, set sp=fp=base+offset when a
    // stack base is given, gp from the header, call the entry with
    // (a1, a2), then restore and return 1.
    a.label("exec");
    a.sw(S0, 0x38, A0);
    a.sw(RA, 0x34, A0);
    a.sw(SP, 0x28, A0);
    a.sw(FP, 0x2C, A0);
    a.sw(GP, 0x30, A0);
    a.lw(T0, 0x1C, A0);
    a.lw(T3, 0x20, A0);
    a.beqz(T0, "exec_nobss");
    a.mov(S0, A0);
    a.lw(T1, 0x18, A0);
    a.label("exec_bss");
    a.addi(T0, T0, -4);
    a.sw(ZERO, 0, T1);
    a.bgtz(T0, "exec_bss");
    a.addi(T1, T1, 4);
    a.label("exec_nobss");
    a.beqz(T3, "exec_nostack");
    a.lw(T2, 0x00, S0);
    a.lw(T1, 0x24, S0);
    a.nop();
    a.addu(SP, T3, T1);
    a.mov(FP, SP);
    a.label("exec_nostack");
    a.lw(GP, 0x04, S0);
    a.mov(A0, A1);
    a.jalr(T2);
    a.mov(A1, A2);
    a.lw(RA, 0x34, S0);
    a.lw(SP, 0x28, S0);
    a.lw(FP, 0x2C, S0);
    a.lw(GP, 0x30, S0);
    a.lw(S0, 0x38, S0);
    a.jr(RA);
    a.addiu(V0, ZERO, 1);

    let rfe = a.addr("rfe");
    let deliver_event = a.addr("deliver_event");
    let exec = a.addr("exec");
    let rcnt_verifier = [
        a.addr("rcnt0"),
        a.addr("rcnt1"),
        a.addr("rcnt2"),
        a.addr("rcnt3"),
    ];
    let defint_verifier = a.addr("defint");
    let unresolved_glue = a.addr("unresolved");
    let syscall_stub = a.addr("syscall_stub");
    let cause_epc = a.addr("cause_epc");
    let mut words = a.finish();
    // Fill in `li ra, rfe` in the unresolved glue.
    let glue = ((unresolved_glue - KERNEL_CODE) / 4) as usize;
    words[glue] |= rfe >> 16;
    words[glue + 1] |= rfe & 0xFFFF;
    assert!(KERNEL_CODE + 4 * words.len() as u32 <= KERNEL_CODE_END);

    KernelCode {
        handler: assemble_handler(cause_epc),
        words,
        return_from_exception: rfe,
        deliver_event,
        rcnt_verifier,
        defint_verifier,
        unresolved_glue,
        syscall_stub,
        exec,
    }
}

/// The exception handler at C(06h). Offsets up to the patch slots match
/// the retail/OpenBIOS layout: games patch +00h..+37h (_patch_gte), read
/// and write +70h.. (memory card and lightgun patches).
fn assemble_handler(cause_epc: u32) -> Vec<u32> {
    let base = crate::hle_kernel::EXCEPTION_HANDLER;
    let mut a = Asm::new(base);
    for _ in 0..4 {
        a.nop();
    }
    // k0 = &current TCB registers.
    a.addiu(K0, ZERO, TOT as i16);
    a.lw(K0, 8, K0);
    a.nop();
    a.lw(K0, 0, K0);
    a.nop();
    a.addi(K0, K0, 8);
    a.sw(AT, 0x04, K0);
    a.sw(V0, 0x08, K0);
    a.sw(V1, 0x0C, K0);
    a.sw(RA, 0x7C, K0);
    a.jal_abs(cause_epc);
    a.nop();
    // Interrupted in front of a GTE command: the command already ran, so
    // resume after it (psx-spx "Interrupts vs GTE Commands").
    a.andi(V0, V0, 0x3C);
    a.bnez(V0, "no_cop2");
    a.nop();
    a.lw(V0, 0, V1);
    a.nop();
    a.srl(V0, V0, 24);
    a.andi(V0, V0, 0xFE);
    a.addiu(AT, ZERO, 0x4A);
    a.bne(V0, AT, "no_cop2");
    a.nop();
    a.addi(V1, V1, 4);
    a.label("no_cop2");
    a.sw(V1, 0x80, K0);
    debug_assert_eq!(a.here(), base + 0x70);
    // Four 4-word patch slots (memory card, lightgun, ...).
    for _ in 0..16 {
        a.nop();
    }
    for (reg, off) in [(A0, 0x10), (5, 0x14), (6, 0x18), (7, 0x1C)] {
        a.sw(reg, off, K0);
    }
    a.mfc0(A0, 12);
    a.nop();
    a.sw(A0, 0x8C, K0);
    a.mfc0(A1, 13);
    a.nop();
    a.sw(A1, 0x90, K0);
    a.sw(K1, 0x6C, K0);
    for reg in 16..=23 {
        a.sw(reg, (4 * reg) as i16, K0);
    }
    for reg in 8..=15 {
        a.sw(reg, (4 * reg) as i16, K0);
    }
    a.sw(24, 0x60, K0);
    a.sw(25, 0x64, K0);
    a.sw(GP, 0x70, K0);
    a.sw(SP, 0x74, K0);
    a.sw(FP, 0x78, K0);
    a.mfhi(A0);
    a.nop();
    a.sw(A0, 0x84, K0);
    a.mflo(A0);
    a.nop();
    a.sw(A0, 0x88, K0);
    // Kernel stack; s3 walks the four ExCB priority slots.
    a.lw(SP, kvar::EXCEPTION_SP as i16, ZERO);
    a.addiu(S3, ZERO, TOT as i16);
    a.lw(S3, 0, S3);
    a.mov(GP, ZERO);
    a.mov(FP, SP);
    a.addi(S4, S3, 0x20);
    a.label("prio");
    a.lw(S6, 0, S3);
    a.nop();
    a.beqz(S6, "next_prio");
    a.nop();
    a.label("handlers");
    a.lw(S1, 8, S6);
    a.lw(S0, 4, S6);
    a.beqz(S1, "next_handler");
    a.nop();
    a.jalr(S1);
    a.nop();
    a.beqz(V0, "next_handler");
    a.nop();
    a.beqz(S0, "next_handler");
    a.mov(A0, V0);
    a.jalr(S0);
    a.nop();
    a.label("next_handler");
    a.lw(S6, 0, S6);
    a.nop();
    a.bnez(S6, "handlers");
    a.nop();
    a.label("next_prio");
    a.addi(S3, S3, 8);
    a.bne(S4, S3, "prio");
    a.nop();
    // Nobody returned from the exception: longjmp to the exit buffer
    // with r2 = 1 (psx-spx HookEntryInt).
    a.lw(A0, kvar::EXIT_JMPBUF as i16, ZERO);
    a.nop();
    a.lw(RA, 0x00, A0);
    a.lw(GP, 0x2C, A0);
    a.lw(SP, 0x04, A0);
    a.lw(FP, 0x08, A0);
    for reg in 16..=23 {
        a.lw(reg, (0x0C + 4 * (reg - 16)) as i16, A0);
    }
    a.jr(RA);
    a.addiu(V0, ZERO, 1);
    let words = a.finish();
    assert!(base + 4 * words.len() as u32 <= crate::hle_kernel::EXCEPTION_HANDLER_END);
    words
}

fn rcnt_irq_bit(n: usize) -> u32 {
    // Timers 0..2 are IRQ 4..6, "timer 3" is VBlank (IRQ 0).
    [1 << 4, 1 << 5, 1 << 6, 1][n]
}

/// Default IRQ handler order and events (OpenBIOS IRQVerifier; timer 2
/// reuses the timer 1 class, as the retail BIOS does).
const DEFINT: [(u32, u32); 11] = [
    (2, 0xF000_0003),  // CDROM
    (9, 0xF000_0009),  // SPU
    (1, 0xF000_0002),  // GPU
    (10, 0xF000_000A), // PIO / IRQ10
    (8, 0xF000_000B),  // SIO
    (0, 0xF000_0001),  // VBlank
    (4, 0xF000_0005),  // timer 0
    (5, 0xF000_0006),  // timer 1
    (6, 0xF000_0006),  // timer 2
    (7, 0xF000_0008),  // controller
    (3, 0xF000_0004),  // DMA
];

fn write_words(bus: &mut Bus, base: u32, words: &[u32]) {
    for (i, w) in words.iter().enumerate() {
        poke32(bus, base + 4 * i as u32, *w);
    }
}

/// C(07h) InstallExceptionHandlers: the four-word vector at 80h, and its
/// copy at 0 with the first word already smashed to 3 (psx-spx "Garbage
/// Area"; R-Types needs a nonzero halfword at 0).
pub fn install_vector(bus: &mut Bus) {
    let mut a = Asm::new(VECTOR);
    let h = crate::hle_kernel::EXCEPTION_HANDLER;
    a.lui(K0, (h >> 16) as u16);
    a.addiu(K0, K0, h as i16);
    a.jr(K0);
    a.nop();
    let words = a.finish();
    write_words(bus, VECTOR, &words);
    write_words(bus, 0, &words);
    poke32(bus, 0, 3);
}

/// Write the exception core into RAM and enqueue the default handlers.
pub fn install(bus: &mut Bus) {
    let code = code();
    install_vector(bus);
    write_words(bus, crate::hle_kernel::EXCEPTION_HANDLER, &code.handler);
    write_words(bus, KERNEL_CODE, &code.words);
    let b0 = crate::hle_kernel::B0_TABLE;
    poke32(bus, b0 + 4 * 0x07, code.deliver_event);
    poke32(bus, b0 + 4 * 0x17, code.return_from_exception);
    poke32(bus, crate::hle_kernel::A0_TABLE + 4 * 0x43, code.exec);

    // Default exit buffer: ReturnFromException on the exception stack
    // (stack top minus 4, psx-spx), other registers 0.
    poke32(bus, DEFAULT_JMPBUF, code.return_from_exception);
    poke32(bus, DEFAULT_JMPBUF + 4, EXCEPTION_STACK_TOP - 4);
    poke32(bus, kvar::EXIT_JMPBUF, DEFAULT_JMPBUF);
    poke32(bus, kvar::EXCEPTION_SP, EXCEPTION_STACK_TOP);

    // Handler descriptors.
    poke32(
        bus,
        HI_SYSCALL + 8,
        stub_addr(3, internal::SYSCALL_VERIFIER),
    );
    for n in 0..4u32 {
        let hi = HI_RCNT + 16 * n;
        poke32(bus, hi + 4, stub_addr(3, internal::RCNT_HANDLER + n as u8));
        poke32(bus, hi + 8, code.rcnt_verifier[n as usize]);
    }
    poke32(bus, HI_DEFINT + 8, code.defint_verifier);
    for (i, (irq, class)) in DEFINT.iter().enumerate() {
        let e = DEFINT_TABLE + 12 * i as u32;
        poke32(bus, e, 1 << irq);
        poke32(bus, e + 4, *class);
        poke32(bus, e + 8, kvar::IRQ_AUTOACK + 4 * irq);
    }
    enqueue_defaults(bus);
}

/// The kernel's default chain elements (psx-spx "Priority Chains"):
/// SYSCALL handler at priority 0, timers and VBlank at 1, the default IRQ
/// handler at 3. Boot leaves the timer hardware alone: its state at EXE
/// entry comes from the entry-state profile.
pub fn enqueue_defaults(bus: &mut Bus) {
    enqueue_syscall_handler(bus, 0);
    enqueue_rcnt(bus, 1, false);
    enqueue_defint(bus, 3);
}

fn excb(bus: &Bus) -> u32 {
    peek32(bus, TOT)
}

/// C(02h) SysEnqIntRP: insert at the head of the priority chain.
pub fn enq_int(bus: &mut Bus, prio: u32, handler: u32) {
    let slot = excb(bus) + 8 * (prio & 3);
    let first = peek32(bus, slot);
    poke32(bus, slot, handler);
    poke32(bus, handler, first);
}

/// C(03h) SysDeqIntRP: unlink `handler` from the chain. psx-spx documents
/// the retail function as able to remove only the first element; this
/// follows OpenBIOS and searches the whole chain. Returns the element or 0.
pub fn deq_int(bus: &mut Bus, prio: u32, handler: u32) -> u32 {
    let slot = excb(bus) + 8 * (prio & 3);
    let mut prev = slot;
    let mut cur = peek32(bus, slot);
    for _ in 0..0x1000 {
        if cur == 0 {
            return 0;
        }
        if cur == handler {
            let next = peek32(bus, cur);
            poke32(bus, prev, next);
            return cur;
        }
        prev = cur;
        cur = peek32(bus, cur);
    }
    0
}

/// C(01h) EnqueueSyscallHandler.
pub fn enqueue_syscall_handler(bus: &mut Bus, prio: u32) {
    enq_int(bus, prio, HI_SYSCALL);
}

/// C(00h) EnqueueTimerAndVblankIrqs: auto-ack on, four chain elements;
/// with `touch_hw` also masks the four IRQs and clears the timers
/// (OpenBIOS enqueueRCntIrqs).
pub fn enqueue_rcnt(bus: &mut Bus, prio: u32, touch_hw: bool) {
    if touch_hw {
        let mask = bus.read32(0x1F80_1074);
        bus.write32(0x1F80_1074, mask & !0x71);
    }
    for n in 0..4 {
        poke32(bus, kvar::RCNT_AUTOACK + 4 * n, 1);
        enq_int(bus, prio, HI_RCNT + 16 * n);
    }
    if touch_hw {
        for t in 0..3u32 {
            let base = 0x1F80_1100 + 0x10 * t;
            bus.write16(base + 4, 0);
            bus.write16(base + 8, 0);
            bus.write16(base, 0);
        }
    }
}

/// C(0Ch) InitDefInt: clear every IRQ auto-ack flag and enqueue the
/// default IRQ handler.
pub fn enqueue_defint(bus: &mut Bus, prio: u32) {
    for irq in 0..11 {
        poke32(bus, kvar::IRQ_AUTOACK + 4 * irq, 0);
    }
    enq_int(bus, prio, HI_DEFINT);
}

// ------------------------------------------------------------------ events

/// EvCB status values (psx-spx).
pub const EV_FREE: u32 = 0;
/// Disabled.
pub const EV_DISABLED: u32 = 0x1000;
/// Enabled, waiting (busy).
pub const EV_BUSY: u32 = 0x2000;
/// Enabled, delivered (ready).
pub const EV_READY: u32 = 0x4000;
/// Mode: mark ready instead of calling back.
pub const EV_MODE_READY: u32 = 0x2000;

fn evcb(bus: &Bus, index: u32) -> u32 {
    peek32(bus, TOT + 0x20) + 0x1C * index
}

fn evcb_count(bus: &Bus) -> u32 {
    peek32(bus, TOT + 0x24) / 0x1C
}

/// C(04h) get_free_EvCB_slot.
pub fn free_evcb(bus: &Bus) -> Option<u32> {
    (0..evcb_count(bus)).find(|&i| peek32(bus, evcb(bus, i) + 4) == EV_FREE)
}

/// B(08h) OpenEvent: returns F1000000h | slot, or FFFFFFFFh.
pub fn open_event(bus: &mut Bus, class: u32, spec: u32, mode: u32, func: u32) -> u32 {
    let Some(slot) = free_evcb(bus) else {
        return u32::MAX;
    };
    let e = evcb(bus, slot);
    poke32(bus, e, class);
    poke32(bus, e + 4, EV_DISABLED);
    poke32(bus, e + 8, spec);
    poke32(bus, e + 12, mode);
    poke32(bus, e + 16, func);
    0xF100_0000 | slot
}

fn event_addr(bus: &Bus, event: u32) -> u32 {
    evcb(bus, event & 0xFFFF)
}

/// B(09h) CloseEvent.
pub fn close_event(bus: &mut Bus, event: u32) {
    let e = event_addr(bus, event);
    poke32(bus, e + 4, EV_FREE);
}

/// B(0Ch)/B(0Dh) EnableEvent/DisableEvent: only an open event changes.
pub fn set_event_enabled(bus: &mut Bus, event: u32, enabled: bool) {
    let e = event_addr(bus, event);
    if peek32(bus, e + 4) != EV_FREE {
        poke32(bus, e + 4, if enabled { EV_BUSY } else { EV_DISABLED });
    }
}

/// B(0Bh) TestEvent: consumes a ready event.
pub fn test_event(bus: &mut Bus, event: u32) -> bool {
    let e = event_addr(bus, event);
    if peek32(bus, e + 4) == EV_READY {
        poke32(bus, e + 4, EV_BUSY);
        return true;
    }
    false
}

/// B(0Ah) WaitEvent state: `Some(result)` when it returns now, `None`
/// while an enabled event is still busy.
pub fn wait_event(bus: &mut Bus, event: u32) -> Option<u32> {
    let e = event_addr(bus, event);
    match peek32(bus, e + 4) {
        EV_READY => {
            poke32(bus, e + 4, EV_BUSY);
            Some(1)
        }
        EV_BUSY => None,
        _ => Some(0),
    }
}

/// B(20h) UnDeliverEvent: ready mark-ready events of this class and spec
/// go back to busy.
pub fn undeliver_event(bus: &mut Bus, class: u32, spec: u32) {
    for i in 0..evcb_count(bus) {
        let e = evcb(bus, i);
        if peek32(bus, e + 4) == EV_READY
            && peek32(bus, e) == class
            && peek32(bus, e + 8) == spec
            && peek32(bus, e + 12) == EV_MODE_READY
        {
            poke32(bus, e + 4, EV_BUSY);
        }
    }
}

// ----------------------------------------------------------------- threads

fn tcb(bus: &Bus, index: u32) -> u32 {
    peek32(bus, TOT + 0x10) + crate::hle_kernel::TCB_SIZE * index
}

fn tcb_count(bus: &Bus) -> u32 {
    peek32(bus, TOT + 0x14) / crate::hle_kernel::TCB_SIZE
}

/// C(05h) get_free_TCB_slot.
pub fn free_tcb(bus: &Bus) -> Option<u32> {
    (0..tcb_count(bus)).find(|&i| peek32(bus, tcb(bus, i)) == crate::hle_kernel::TCB_FREE)
}

/// B(0Eh) OpenTh(pc, sp, gp): returns FF000000h | slot or FFFFFFFFh.
/// SR is left as it was (psx-spx documents this).
pub fn open_thread(bus: &mut Bus, pc: u32, sp: u32, gp: u32) -> u32 {
    let Some(slot) = free_tcb(bus) else {
        return u32::MAX;
    };
    let t = tcb(bus, slot);
    poke32(bus, t, crate::hle_kernel::TCB_USED);
    poke32(bus, t + 4, 0x1000);
    poke32(bus, t + 8 + 4 * 29, sp);
    poke32(bus, t + 8 + 4 * 30, sp);
    poke32(bus, t + 8 + 4 * 28, gp);
    poke32(bus, t + crate::hle_bios::TCB_RETURN_PC, pc);
    0xFF00_0000 | slot
}

/// B(0Fh) CloseTh.
pub fn close_thread(bus: &mut Bus, thread: u32) {
    let t = tcb(bus, thread & 0xFFFF);
    poke32(bus, t, crate::hle_kernel::TCB_FREE);
}

/// TCB address for ChangeTh's SYSCALL(3).
pub fn thread_tcb(bus: &Bus, thread: u32) -> u32 {
    tcb(bus, thread & 0xFFFF)
}

// ------------------------------------------------------------ syscall path

/// What the default SYSCALL/exception verifier decided.
pub enum SyscallAction {
    /// Not ours (an interrupt): return 0 to the chain.
    Pass,
    /// Handled; leave through ReturnFromException.
    Return,
    /// Deliver (class, spec), then continue at `then`.
    Deliver(u32, u32, u32),
}

/// Default SYSCALL/exception verifier (OpenBIOS syscallVerifier), acting
/// on the frame saved in the current TCB.
pub fn syscall_verifier(bus: &mut Bus) -> SyscallAction {
    use crate::hle_bios::{TCB_CAUSE, TCB_REGISTERS, TCB_RETURN_PC, TCB_SR};
    let t = crate::hle_kernel::current_tcb(bus);
    let cause = peek32(bus, t + TCB_CAUSE);
    match cause & 0x3C {
        0x00 => SyscallAction::Pass,
        0x20 => {
            let epc = peek32(bus, t + TCB_RETURN_PC);
            poke32(bus, t + TCB_RETURN_PC, epc.wrapping_add(4));
            let reg = |r: u32| t + TCB_REGISTERS + 4 * r;
            match peek32(bus, reg(4)) {
                0 => SyscallAction::Return,
                1 => {
                    let sr = peek32(bus, t + TCB_SR);
                    poke32(bus, reg(2), u32::from(sr & 0x404 == 0x404));
                    poke32(bus, t + TCB_SR, sr & !0x404);
                    SyscallAction::Return
                }
                2 => {
                    let sr = peek32(bus, t + TCB_SR);
                    poke32(bus, t + TCB_SR, sr | 0x404);
                    SyscallAction::Return
                }
                3 => {
                    let new = peek32(bus, reg(5));
                    poke32(bus, reg(2), 1);
                    let pcb = peek32(bus, TOT + 8);
                    poke32(bus, pcb, new);
                    SyscallAction::Return
                }
                _ => SyscallAction::Deliver(0xF000_0010, 0x4000, code().return_from_exception),
            }
        }
        _ => SyscallAction::Deliver(0xF000_0010, 0x1000, code().unresolved_glue),
    }
}

/// Root-counter handler `n` (OpenBIOS T0..T3handler): with auto-ack on,
/// acknowledge the IRQ and return from the exception.
pub fn rcnt_handler(bus: &mut Bus, n: u8) -> bool {
    let n = u32::from(n & 3);
    if peek32(bus, kvar::RCNT_AUTOACK + 4 * n) == 0 {
        return false;
    }
    bus.write32(0x1F80_1070, !rcnt_irq_bit(n as usize));
    true
}

/// C(0Ah) ChangeClearRCnt(t, flag): returns the previous flag.
pub fn change_clear_rcnt(bus: &mut Bus, t: u32, flag: u32) -> u32 {
    let var = kvar::RCNT_AUTOACK + 4 * (t & 3);
    let old = peek32(bus, var);
    poke32(bus, var, flag);
    old
}

// ------------------------------------------------------------------ timers

fn timer_reg(t: u32, reg: u32) -> u32 {
    0x1F80_1100 + 0x10 * t + reg
}

/// B(02h) init_timer(t, reload, flags), psx-spx: for t = 0..2, mode 0,
/// target = reload, then mode 48h (49h when flags bit 4), OR 100h when
/// flags bit 0 is clear, OR 10h when flags bit 12 is set. Returns 1, or
/// 0 for t > 2. (OpenBIOS applies 100h when bit 0 is set; this follows
/// psx-spx.)
pub fn init_timer(bus: &mut Bus, t: u32, reload: u32, flags: u32) -> u32 {
    let t = t & 0xFFFF;
    if t > 2 {
        return 0;
    }
    bus.write16(timer_reg(t, 4), 0);
    bus.write16(timer_reg(t, 8), reload as u16);
    let mut mode: u16 = if flags & 0x10 != 0 { 0x49 } else { 0x48 };
    if flags & 1 == 0 {
        mode |= 0x100;
    }
    if flags & 0x1000 != 0 {
        mode |= 0x10;
    }
    bus.write16(timer_reg(t, 4), mode);
    1
}

/// B(03h) get_timer(t): current counter for t = 0..2, else 0.
pub fn get_timer(bus: &mut Bus, t: u32) -> u32 {
    if t > 2 {
        return 0;
    }
    u32::from(bus.read16(timer_reg(t, 0)))
}

/// B(04h) enable_timer_irq / B(05h) disable_timer_irq: I_MASK bit 4/5/6
/// for t = 0..2, bit 0 for t = 3. Enable returns 1 for t = 0..2 and 0 for
/// 3; disable always returns 1. Other t change nothing here (psx-spx:
/// "random/garbage bits").
pub fn set_timer_irq(bus: &mut Bus, t: u32, enable: bool) -> u32 {
    let bit = match t {
        0..=2 => 1 << (4 + t),
        3 => 1,
        _ => 0,
    };
    let mask = bus.read32(0x1F80_1074);
    bus.write32(0x1F80_1074, if enable { mask | bit } else { mask & !bit });
    if enable {
        u32::from(t <= 2)
    } else {
        1
    }
}

/// B(06h) restart_timer(t): counter to 0, returns 1 for t = 0..2.
pub fn restart_timer(bus: &mut Bus, t: u32) -> u32 {
    if t > 2 {
        return 0;
    }
    bus.write16(timer_reg(t, 0), 0);
    1
}

/// C(0Dh) SetIrqAutoAck(irq, flag).
pub fn set_irq_autoack(bus: &mut Bus, irq: u32, flag: u32) {
    if irq < 11 {
        poke32(bus, kvar::IRQ_AUTOACK + 4 * irq, flag);
    }
}

/// B(19h) HookEntryInt / B(18h) ResetEntryInt.
pub fn set_exit_jmpbuf(bus: &mut Bus, buf: u32) {
    poke32(bus, kvar::EXIT_JMPBUF, buf);
}

/// Current exit buffer.
pub fn exit_jmpbuf(bus: &Bus) -> u32 {
    peek32(bus, kvar::EXIT_JMPBUF)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hle_bus() -> Bus {
        let mut bus = Bus::new_without_bios();
        bus.enable_hle_bios();
        bus
    }

    #[test]
    fn exec_runs_the_entry_with_its_stack_and_returns_one() {
        use crate::Cpu;
        let mut bus = hle_bus();
        let mut cpu = Cpu::new();
        let header = 0x8003_0000;
        let entry = 0x8004_0000;
        // Entry: store sp to [0x80050000], a0/a1 after it, then return.
        for (i, w) in [
            0x3C08_8005u32,
            0xAD1D_0000,
            0xAD04_0004,
            0xAD05_0008,
            0x03E0_0008,
            0,
        ]
        .iter()
        .enumerate()
        {
            bus.write32(entry + 4 * i as u32, *w);
        }
        for (off, v) in [
            (0x00, entry),
            (0x04, 0x1234),
            (0x18, 0x8006_0000),
            (0x1C, 8),
            (0x20, 0x801F_0000),
            (0x24, 0x10),
        ] {
            bus.write32(header + off, v);
        }
        bus.write32(0x8006_0000, 0xFFFF_FFFF);
        bus.write32(0x8006_0004, 0xFFFF_FFFF);
        bus.write32(0x8006_0008, 0xFFFF_FFFF);
        // Call A(43h) through the vector, returning to a spin loop.
        bus.write32(0x8001_0000, 0x1000_FFFF);
        cpu.gprs_mut_for_test()[4] = header;
        cpu.gprs_mut_for_test()[5] = 7;
        cpu.gprs_mut_for_test()[6] = 9;
        cpu.gprs_mut_for_test()[9] = 0x43;
        cpu.gprs_mut_for_test()[29] = 0x801F_FF00;
        cpu.gprs_mut_for_test()[31] = 0x8001_0000;
        cpu.set_pc_for_test(0xA0);
        for _ in 0..500 {
            if cpu.pc() == 0x8001_0000 {
                break;
            }
            cpu.step(&mut bus).unwrap();
        }
        assert_eq!(cpu.pc(), 0x8001_0000);
        assert_eq!(cpu.gpr(2), 1);
        assert_eq!(cpu.gpr(29), 0x801F_FF00, "caller's sp restored");
        assert_eq!(
            bus.read32(0x8005_0000),
            0x801F_0010,
            "entry ran on base+offset"
        );
        assert_eq!((bus.read32(0x8005_0004), bus.read32(0x8005_0008)), (7, 9));
        assert_eq!(bus.read32(0x8006_0000), 0);
        assert_eq!(bus.read32(0x8006_0004), 0);
        assert_eq!(
            bus.read32(0x8006_0008),
            0xFFFF_FFFF,
            "only b_size bytes cleared"
        );
    }

    #[test]
    fn timer_helpers_program_the_documented_registers() {
        let mut bus = hle_bus();
        assert_eq!(init_timer(&mut bus, 1, 0x1234, 0x1000), 1);
        assert_eq!(bus.read16(0x1F80_1118), 0x1234);
        assert_eq!(bus.read16(0x1F80_1114) & 0x3FF, 0x158);
        assert_eq!(init_timer(&mut bus, 3, 1, 0), 0);
        assert_eq!(set_timer_irq(&mut bus, 2, true), 1);
        assert_eq!(set_timer_irq(&mut bus, 3, true), 0);
        assert_eq!(bus.read32(0x1F80_1074) & 0x41, 0x41);
        assert_eq!(set_timer_irq(&mut bus, 2, false), 1);
        assert_eq!(bus.read32(0x1F80_1074) & 0x40, 0);
        assert_eq!(restart_timer(&mut bus, 0), 1);
        assert_eq!(get_timer(&mut bus, 7), 0);
    }
}
