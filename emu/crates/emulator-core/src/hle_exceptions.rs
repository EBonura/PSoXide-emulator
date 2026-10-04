// SPDX-License-Identifier: GPL-2.0-or-later
//! Exception core of the HLE kernel: the exception vector, the handler at
//! C(06h), ReturnFromException, the priority chains, events, threads, the
//! root-counter and default IRQ handlers, and SYSCALL 0..3.
//!
//! Code that calls back into the game, or that games patch or decode, is
//! guest MIPS assembled at boot with [`crate::hle_asm`]: the vector, the
//! handler, ReturnFromException, DeliverEvent, the root-counter and
//! default-IRQ verifiers, Exec, GetConf and the early card routine. The
//! rest is host code behind HLE traps. All state lives in guest RAM: the
//! ExCB chains, EvCBs and TCBs reached through the table of tables, and
//! the variables in [`kvar`].
//!
//! Sources: psx-spx "BIOS Interrupt/Exception Handling", "BIOS Control
//! Blocks", "BIOS Event Functions", "BIOS Event Summary", "BIOS Thread
//! Functions", "BIOS Timer Functions", "BIOS File Execute and Flush Cache",
//! "BIOS Memory Map", "BIOS Patches" and "CPU Specifications" (interrupts
//! on GTE commands). Where psx-spx leaves a choice open, the choice is
//! noted where it is made.

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
/// Exec nesting depth, then [`EXEC_SLOTS`] header addresses.
const EXEC_DEPTH: u32 = KERNEL_DATA + 0x128;
const EXEC_STACK: u32 = KERNEL_DATA + 0x12C;
const EXEC_SLOTS: u32 = 5;

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
    /// Events queued by HLE handlers for the guest DeliverEvent: count,
    /// then the address to continue at when the queue is empty.
    pub const DQ_COUNT: u32 = 0x0C00;
    /// Continuation after the last queued delivery.
    pub const DQ_RETURN: u32 = 0x0C04;
    /// `v0` handed to that continuation.
    pub const DQ_V0: u32 = 0x0C48;
    /// Queued (class, spec) pairs, [`super::DQ_MAX`] of them.
    pub const DQ_ITEMS: u32 = 0x0C08;
}

/// Capacity of the delivery queue.
pub const DQ_MAX: u32 = 8;

/// Kernel-internal trap functions used by the exception core.
pub mod internal {
    /// Default SYSCALL/exception verifier (priority 0).
    pub const SYSCALL_VERIFIER: u8 = 0x03;
    /// Root-counter handlers for timers 0..2 and VBlank (4 entries).
    pub const RCNT_HANDLER: u8 = 0x04;
    /// Delivers the next queued event, or continues after the queue.
    pub const DELIVER_NEXT: u8 = 0x3E;
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
    /// A(9Dh) GetConf.
    pub get_conf: u32,
    /// Early memory card IRQ routine, called from exception handler slot
    /// 1 (installed by InitCARD2).
    pub card_early: u32,
    /// Return from the exception after the early card routine moved a
    /// data byte.
    pub card_fast_rfe: u32,
    /// Endless loop.
    pub hang: u32,
}

/// Assembled kernel code (deterministic; built once).
pub fn code() -> &'static KernelCode {
    static CODE: std::sync::OnceLock<KernelCode> = std::sync::OnceLock::new();
    CODE.get_or_init(assemble)
}

// ------------------------------------------------------------ guest code

/// Offsets inside a TCB (psx-spx "BIOS Control Blocks").
mod tcb {
    /// Status word (1000h free, 4000h used).
    pub const STATUS: u32 = 0x00;
    /// Second word, set to 1000h by OpenTh.
    pub const MODE: u32 = 0x04;
    /// r0..r31, four bytes each.
    pub const REGS: u32 = 0x08;
    /// Return PC.
    pub const EPC: u32 = 0x88;
    pub const HI: u32 = 0x8C;
    pub const LO: u32 = 0x90;
    pub const SR: u32 = 0x94;
    pub const CAUSE: u32 = 0x98;

    /// Offset of register `r`.
    pub const fn reg(r: u32) -> u32 {
        REGS + 4 * r
    }
}

/// Table-of-tables words the guest code reads: the ExCB base, the PCB
/// (whose first word is the current TCB) and the EvCB base and size.
const TOT_EXCB: i16 = TOT as i16;
const TOT_PCB: i16 = TOT as i16 + 0x08;
const TOT_EVCB: i16 = TOT as i16 + 0x20;
const TOT_EVCB_SIZE: i16 = TOT as i16 + 0x24;

/// Root counter `n` (0..2 timers, 3 VBlank): its I_STAT/I_MASK bit.
fn rcnt_irq_bit(n: u32) -> u32 {
    if n == 3 {
        1
    } else {
        0x10 << n
    }
}

/// Load the current TCB into `reg`: `[[100h + 08h]]`.
fn current_tcb_into(a: &mut Asm, reg: u32) {
    a.lw(reg, TOT_PCB, ZERO);
    a.nop();
    a.lw(reg, 0, reg);
    a.nop();
}

/// psx-spx "Interrupts vs GTE Commands": when the exception is an
/// interrupt and the opcode at EPC is a GTE command, that command has
/// already run, so the return address moves past it. `cause` and `epc`
/// hold the COP0 values; `epc` is adjusted in place, `tmp` is clobbered.
fn skip_interrupted_gte_command(a: &mut Asm, cause: u32, epc: u32, tmp: u32, done: &'static str) {
    a.andi(tmp, cause, 0x7C);
    a.bnez(tmp, done);
    a.nop();
    a.lw(tmp, 0, epc);
    a.nop();
    a.srl(tmp, tmp, 25);
    a.xori(tmp, tmp, 0x25);
    a.bnez(tmp, done);
    a.nop();
    a.addiu(epc, epc, 4);
    a.label(done);
}

/// Offset in the handler of the first of its four 4-word call slots.
pub const HANDLER_SLOTS: u32 = 0x70;

/// The exception handler at C(06h).
///
/// Its first 70h bytes are laid out the way psx-spx's "BIOS Patches"
/// listings show the retail kernel, because games rewrite them:
///
/// * +00h..+0Ch spare `nop`s;
/// * +10h..+24h `k0` = current TCB + 8 (the register area);
/// * +28h..+34h at, v0, v1 and ra saved there;
/// * +38h `v1` = EPC, +40h `v0` = cause.
///
/// The "missing cop0r13" patch rewrites +00h..+37h with an equivalent
/// prologue that leaves `v0` = cause; the variant that first checks
/// +28h..+3Bh finds the words it expects and rewrites +28h..+3Fh. Either
/// way the code from +40h on re-reads what it needs. Then come the four
/// call slots at +70h (slot 1 is the early card routine's, slot 2 the
/// lightgun hook's), which run with only at, v0, v1 and ra saved.
///
/// After the slots: save the rest of the registers, HI/LO, SR, cause and
/// EPC (moved past an interrupted GTE command), switch to the exception
/// stack and walk the four ExCB chains. For each element the first
/// function runs; when it returns nonzero the second runs with that value
/// in `a0`. Elements that finish the exception jump to
/// ReturnFromException. When all chains are done, the exit buffer
/// (HookEntryInt) is jumped through with `v0` = 1.
fn assemble_handler() -> Vec<u32> {
    let mut a = Asm::new(crate::hle_kernel::EXCEPTION_HANDLER);
    for _ in 0..4 {
        a.nop();
    }
    a.addiu(K0, ZERO, TOT_PCB - 8); // 100h
    a.lw(K0, 8, K0);
    a.nop();
    a.lw(K0, 0, K0);
    a.nop();
    a.addi(K0, K0, tcb::REGS as i16);
    debug_assert_eq!(a.here(), crate::hle_kernel::EXCEPTION_HANDLER + 0x28);
    a.sw(AT, 4 * AT as i16, K0);
    a.sw(V0, 4 * V0 as i16, K0);
    a.sw(V1, 4 * V1 as i16, K0);
    a.sw(RA, 4 * RA as i16, K0);
    a.mfc0(V1, 14);
    a.nop();
    a.mfc0(V0, 13);
    a.nop();
    while a.here() < crate::hle_kernel::EXCEPTION_HANDLER + HANDLER_SLOTS {
        a.nop();
    }
    for _ in 0..16 {
        a.nop();
    }

    // Full save. The slots may have used k0, so the TCB is looked up again.
    current_tcb_into(&mut a, K0);
    for r in (4..=25).chain(27..=31) {
        if r != RA {
            a.sw(r, tcb::reg(r) as i16, K0);
        }
    }
    a.mfhi(T0);
    a.mflo(T1);
    a.sw(T0, tcb::HI as i16, K0);
    a.sw(T1, tcb::LO as i16, K0);
    a.mfc0(T0, 12);
    a.mfc0(T1, 13);
    a.mfc0(T2, 14);
    a.sw(T0, tcb::SR as i16, K0);
    a.sw(T1, tcb::CAUSE as i16, K0);
    skip_interrupted_gte_command(&mut a, T1, T2, T3, "epc_ready");
    a.sw(T2, tcb::EPC as i16, K0);

    // Chains, priority 0 first, on the kernel's exception stack.
    a.lw(SP, kvar::EXCEPTION_SP as i16, ZERO);
    a.mov(S0, ZERO);
    a.label("prio");
    a.lw(T0, TOT_EXCB, ZERO);
    a.nop();
    a.addu(T0, T0, S0);
    a.lw(S1, 0, T0);
    a.nop();
    a.label("element");
    a.beqz(S1, "prio_done");
    a.nop();
    // The next pointer is read first, so an element may unlink itself.
    a.lw(S2, 0, S1);
    a.lw(T1, 8, S1);
    a.nop();
    a.beqz(T1, "skip");
    a.nop();
    a.jalr(T1);
    a.nop();
    a.beqz(V0, "skip");
    a.lw(T1, 4, S1);
    a.nop();
    a.beqz(T1, "skip");
    a.nop();
    a.jalr(T1);
    a.mov(A0, V0);
    a.label("skip");
    a.b("element");
    a.mov(S1, S2);
    a.label("prio_done");
    a.addiu(S0, S0, 8);
    a.addiu(T0, ZERO, 0x20);
    a.bne(S0, T0, "prio");
    a.nop();

    // Nothing ended the exception: leave through the exit buffer.
    a.lw(T0, kvar::EXIT_JMPBUF as i16, ZERO);
    a.nop();
    longjmp_through(&mut a, T0);
    a.finish()
}

/// Restore the setjmp-layout buffer at `buf` (psx-spx B(19h): ra, sp, fp,
/// s0..s7, gp) and return into it with `v0` = 1.
fn longjmp_through(a: &mut Asm, buf: u32) {
    a.lw(RA, 0x00, buf);
    a.lw(SP, 0x04, buf);
    a.lw(FP, 0x08, buf);
    for (k, r) in (S0..=S7).enumerate() {
        a.lw(r, 0x0C + 4 * k as i16, buf);
    }
    a.lw(GP, 0x2C, buf);
    a.jr(RA);
    a.addiu(V0, ZERO, 1);
}

/// Restore every register but r0 and k0 from the current TCB, then return
/// to its EPC with RFE in the jump's delay slot (psx-spx B(17h)).
fn return_from_exception(a: &mut Asm) {
    current_tcb_into(a, K0);
    a.lw(T0, tcb::HI as i16, K0);
    a.lw(T1, tcb::LO as i16, K0);
    a.lw(T2, tcb::SR as i16, K0);
    a.mthi(T0);
    a.mtlo(T1);
    a.mtc0(T2, 12);
    for r in (1..=25).chain(27..=31) {
        a.lw(r, tcb::reg(r) as i16, K0);
    }
    a.lw(K0, tcb::EPC as i16, K0);
    a.nop();
    a.jr(K0);
    a.rfe();
}

/// B(07h) DeliverEvent(class, spec): every EvCB that is enabled and busy
/// with this class and spec either becomes ready (mode 2000h) or has its
/// callback called (mode 1000h, staying busy).
fn deliver_event(a: &mut Asm) {
    a.addiu(SP, SP, -24);
    a.sw(RA, 20, SP);
    a.sw(S0, 16, SP);
    a.sw(S1, 12, SP);
    a.sw(S2, 8, SP);
    a.sw(S3, 4, SP);
    a.mov(S2, A0);
    a.mov(S3, A1);
    a.lw(S0, TOT_EVCB, ZERO);
    a.lw(S1, TOT_EVCB_SIZE, ZERO);
    a.nop();
    a.addu(S1, S0, S1);
    a.label("de_loop");
    a.sltu(T0, S0, S1);
    a.beqz(T0, "de_done");
    a.nop();
    a.lw(T0, 0x04, S0);
    a.addiu(T1, ZERO, EV_BUSY as i16);
    a.bne(T0, T1, "de_next");
    a.lw(T0, 0x00, S0);
    a.nop();
    a.bne(T0, S2, "de_next");
    a.lw(T0, 0x08, S0);
    a.nop();
    a.bne(T0, S3, "de_next");
    a.lw(T0, 0x0C, S0);
    a.addiu(T1, ZERO, EV_MODE_READY as i16);
    a.bne(T0, T1, "de_callback");
    a.addiu(T1, ZERO, EV_MODE_CALLBACK as i16);
    a.addiu(T2, ZERO, EV_READY as i16);
    a.b("de_next");
    a.sw(T2, 0x04, S0);
    a.label("de_callback");
    a.bne(T0, T1, "de_next");
    a.lw(T2, 0x10, S0);
    a.nop();
    a.beqz(T2, "de_next");
    a.nop();
    a.jalr(T2);
    a.nop();
    a.label("de_next");
    a.b("de_loop");
    a.addiu(S0, S0, EVCB_SIZE as i16);
    a.label("de_done");
    a.lw(RA, 20, SP);
    a.lw(S0, 16, SP);
    a.lw(S1, 12, SP);
    a.lw(S2, 8, SP);
    a.lw(S3, 4, SP);
    a.jr(RA);
    a.addiu(SP, SP, 24);
}

/// Root-counter verifier `n`: when the counter's IRQ is both enabled and
/// pending, deliver F2000000h+n / 0002h and return 1; otherwise 0.
fn rcnt_verifier(a: &mut Asm, n: u32, skip: &'static str) {
    a.lui(T0, IO_HI);
    a.lw(T1, I_STAT, T0);
    a.lw(T2, I_MASK, T0);
    a.nop();
    a.and(T1, T1, T2);
    a.andi(T1, T1, rcnt_irq_bit(n) as u16);
    a.beqz(T1, skip);
    a.mov(V0, ZERO);
    a.addiu(SP, SP, -8);
    a.sw(RA, 4, SP);
    a.li(A0, 0xF200_0000 + n);
    a.jal("deliver_event");
    a.addiu(A1, ZERO, 2);
    a.lw(RA, 4, SP);
    a.addiu(SP, SP, 8);
    a.addiu(V0, ZERO, 1);
    a.label(skip);
    a.jr(RA);
    a.nop();
}

/// InitDefInt's verifier: for each IRQ in [`DEFINT`] order that is enabled
/// and pending, deliver its class with spec 1000h (psx-spx "Default IRQ
/// Handler Events"). The IRQs whose SetIrqAutoAck flag is set are then
/// acknowledged together, after the scan. psx-spx does not say when the
/// acknowledge happens; doing it once at the end means a delivery's
/// callback still sees its IRQ pending. Always returns 0, so the chain
/// goes on.
fn defint_verifier(a: &mut Asm) {
    a.addiu(SP, SP, -24);
    a.sw(RA, 20, SP);
    a.sw(S0, 16, SP);
    a.sw(S1, 12, SP);
    a.sw(S2, 8, SP);
    a.lui(S1, IO_HI);
    a.mov(S2, ZERO);
    a.li(S0, DEFINT_TABLE);
    a.label("di_loop");
    a.lw(T0, 0, S0);
    a.nop();
    a.beqz(T0, "di_done");
    a.lw(T1, I_STAT, S1);
    a.lw(T2, I_MASK, S1);
    a.and(T1, T1, T0);
    a.nop();
    a.and(T1, T1, T2);
    a.beqz(T1, "di_next");
    a.lw(T3, 8, S0);
    a.nop();
    a.lw(T3, 0, T3);
    a.nop();
    a.beqz(T3, "di_deliver");
    a.nop();
    a.or(S2, S2, T0);
    a.label("di_deliver");
    a.lw(A0, 4, S0);
    a.jal("deliver_event");
    a.addiu(A1, ZERO, 0x1000);
    a.label("di_next");
    a.b("di_loop");
    a.addiu(S0, S0, 12);
    a.label("di_done");
    a.beqz(S2, "di_out");
    a.nor(T0, S2, ZERO);
    a.sw(T0, I_STAT, S1);
    a.label("di_out");
    a.lw(RA, 20, SP);
    a.lw(S0, 16, SP);
    a.lw(S1, 12, SP);
    a.lw(S2, 8, SP);
    a.addiu(SP, SP, 24);
    a.jr(RA);
    a.mov(V0, ZERO);
}

/// A(43h) Exec(header, param1, param2), psx-spx: the caller's ra, sp, fp,
/// gp and s0 go into the header's reserved words (+28h..+3Bh), the memfill region is cleared a word at a time, sp and fp
/// become stack base + offset when the base is nonzero, gp comes from the
/// header, and the entry runs with param1/param2 in a0/a1. If it returns,
/// the saved registers come back and Exec returns 1.
///
/// psx-spx names the five registers but not their slots. Formula One 2001
/// fixes them: its boot stub Execs each part of the game and, after one
/// returns, crashes unless the words read sp at +28h, fp at +2Ch, gp at
/// +30h and ra at +34h (s0 takes the last word). Other orders were tried
/// against its hle_compat run.
///
/// The executable owns every register while it runs, so the header
/// address is kept on a short kernel stack ([`EXEC_STACK`]) rather than in
/// a register; nested Execs push and pop it.
fn exec(a: &mut Asm) {
    a.sw(SP, 0x28, A0);
    a.sw(FP, 0x2C, A0);
    a.sw(GP, 0x30, A0);
    a.sw(RA, 0x34, A0);
    a.sw(S0, 0x38, A0);
    // Push the header; past the last slot the newest replaces the top.
    a.lw(T0, EXEC_DEPTH as i16, ZERO);
    a.addiu(T1, ZERO, EXEC_SLOTS as i16);
    a.sltu(T1, T0, T1);
    a.bnez(T1, "ex_push");
    a.nop();
    a.addiu(T0, ZERO, EXEC_SLOTS as i16 - 1);
    a.label("ex_push");
    a.sll(T1, T0, 2);
    a.sw(A0, EXEC_STACK as i16, T1);
    a.addiu(T0, T0, 1);
    a.sw(T0, EXEC_DEPTH as i16, ZERO);
    a.mov(S0, A0);
    a.lw(T0, 0x18, S0);
    a.lw(T1, 0x1C, S0);
    a.nop();
    a.beqz(T1, "ex_stack");
    a.addu(T1, T0, T1);
    a.label("ex_fill");
    a.sw(ZERO, 0, T0);
    a.addiu(T0, T0, 4);
    a.sltu(T2, T0, T1);
    a.bnez(T2, "ex_fill");
    a.nop();
    a.label("ex_stack");
    a.lw(T0, 0x20, S0);
    a.lw(T1, 0x24, S0);
    a.nop();
    a.beqz(T0, "ex_gp");
    a.addu(T0, T0, T1);
    a.mov(SP, T0);
    a.mov(FP, T0);
    a.label("ex_gp");
    a.lw(GP, 0x04, S0);
    a.lw(T2, 0x00, S0);
    a.mov(A0, A1);
    a.jalr(T2);
    a.mov(A1, A2);
    // Pop the header and restore the caller from it.
    a.lw(T0, EXEC_DEPTH as i16, ZERO);
    a.nop();
    a.addiu(T0, T0, -1);
    a.sw(T0, EXEC_DEPTH as i16, ZERO);
    a.sll(T1, T0, 2);
    a.lw(T2, EXEC_STACK as i16, T1);
    a.nop();
    a.lw(RA, 0x34, T2);
    a.lw(SP, 0x28, T2);
    a.lw(FP, 0x2C, T2);
    a.lw(GP, 0x30, T2);
    a.lw(S0, 0x38, T2);
    a.jr(RA);
    a.addiu(V0, ZERO, 1);
}

/// A(9Dh) GetConf(&events, &threads, &stacktop). The first two words are
/// a `lui`/`lw` pair whose immediates address the stacktop word: psx-spx
/// "set_conf_without_realloc" shows games decoding them to find the
/// three configuration words (threads, events, stacktop) and writing
/// them directly.
fn get_conf(a: &mut Asm) {
    use crate::hle_kernel::kvar::{CONF_EVENT, CONF_STACK, CONF_TCB};
    a.lui(T0, (CONF_STACK >> 16) as u16);
    a.lw(T1, CONF_STACK as u16 as i16, T0);
    a.lw(T2, CONF_EVENT as u16 as i16, T0);
    a.lw(T3, CONF_TCB as u16 as i16, T0);
    a.sw(T1, 0, A2);
    a.sw(T2, 0, A0);
    a.jr(RA);
    a.sw(T3, 0, A1);
}

/// Offset in the early card routine of the words games replace.
pub const CARD_EARLY_PATCHED: u32 = 0x28;
/// Offset in the early card routine where those replacements continue.
pub const CARD_EARLY_RESUME: u32 = 0x3C;

/// The early card IRQ routine that InitCARD2 calls from handler slot 1
/// (psx-spx "early_card_irq_patch"). It runs with only at, v0, v1 and ra
/// saved. When a sector's data phase is running and IRQ7 is enabled, it
/// checks IRQ7 is pending (+28h..+3Bh, with `v1` = 1F800000h: the part
/// games replace with a wait for the card's ACK line), then at +3Ch hands
/// the byte to the card driver, which returns from the exception. In
/// every other case it returns to the handler.
fn card_early(a: &mut Asm) {
    let start = a.here();
    a.lui(V1, IO_HI);
    a.lw(V0, crate::hle_card::kvar::DATA_PHASE as i16, ZERO);
    a.lw(AT, I_MASK, V1);
    a.beqz(V0, "ce_return");
    a.andi(AT, AT, 0x80);
    a.beqz(AT, "ce_return");
    a.nop();
    while a.here() < start + CARD_EARLY_PATCHED {
        a.nop();
    }
    a.lw(V0, I_STAT, V1);
    a.nop();
    a.andi(V0, V0, 0x80);
    a.beqz(V0, "ce_return");
    a.nop();
    debug_assert_eq!(a.here(), start + CARD_EARLY_RESUME);
    a.j_abs(stub_addr(3, crate::hle_card::internal::FAST));
    a.nop();
    a.label("ce_return");
    a.jr(RA);
    a.nop();
}

/// Where the card driver continues after moving an early data byte:
/// restore at, v0, v1 and ra from the TCB and return from the exception,
/// past an interrupted GTE command like the full handler.
fn card_fast_rfe(a: &mut Asm) {
    current_tcb_into(a, K0);
    a.mfc0(V0, 13);
    a.mfc0(V1, 14);
    a.nop();
    skip_interrupted_gte_command(a, V0, V1, AT, "cf_epc");
    a.sw(V1, tcb::EPC as i16, K0);
    a.lw(AT, tcb::reg(AT) as i16, K0);
    a.lw(V0, tcb::reg(V0) as i16, K0);
    a.lw(V1, tcb::reg(V1) as i16, K0);
    a.lw(RA, tcb::reg(RA) as i16, K0);
    a.lw(K0, tcb::EPC as i16, K0);
    a.nop();
    a.jr(K0);
    a.rfe();
}

fn assemble() -> KernelCode {
    let mut a = Asm::new(KERNEL_CODE);

    a.label("return_from_exception");
    return_from_exception(&mut a);

    a.label("deliver_event");
    deliver_event(&mut a);

    const VERIFIERS: [&str; 4] = ["rcnt0", "rcnt1", "rcnt2", "rcnt3"];
    const SKIPS: [&str; 4] = ["rcnt0_no", "rcnt1_no", "rcnt2_no", "rcnt3_no"];
    for n in 0..4 {
        a.label(VERIFIERS[n]);
        rcnt_verifier(&mut a, n as u32, SKIPS[n]);
    }

    a.label("defint");
    defint_verifier(&mut a);

    // After DeliverEvent(F0000010h, 1000h) for an unresolved exception:
    // A(40h) through the A0 vector, so a replaced entry is honoured, and
    // back to ReturnFromException if it returns.
    a.label("unresolved_glue");
    a.li(RA, 0);
    let ra_fix = a.here() - 8;
    a.addiu(T1, ZERO, 0x40);
    a.addiu(T0, ZERO, 0xA0);
    a.jr(T0);
    a.nop();

    // SYSCALL with a0 already set; the handler returns past it.
    a.label("syscall_stub");
    a.syscall();
    a.jr(RA);
    a.nop();

    a.label("exec");
    exec(&mut a);

    a.label("get_conf");
    get_conf(&mut a);

    a.label("card_early");
    card_early(&mut a);

    a.label("card_fast_rfe");
    card_fast_rfe(&mut a);

    a.label("hang");
    a.b("hang");
    a.nop();

    let rfe = a.addr("return_from_exception");
    let layout = KernelCode {
        words: Vec::new(),
        handler: assemble_handler(),
        return_from_exception: rfe,
        deliver_event: a.addr("deliver_event"),
        rcnt_verifier: VERIFIERS.map(|l| a.addr(l)),
        defint_verifier: a.addr("defint"),
        unresolved_glue: a.addr("unresolved_glue"),
        syscall_stub: a.addr("syscall_stub"),
        exec: a.addr("exec"),
        get_conf: a.addr("get_conf"),
        card_early: a.addr("card_early"),
        card_fast_rfe: a.addr("card_fast_rfe"),
        hang: a.addr("hang"),
    };
    let mut words = a.finish();
    // `li ra, ReturnFromException` in the glue, now that it is known.
    let at = ((ra_fix - KERNEL_CODE) / 4) as usize;
    words[at] |= rfe >> 16;
    words[at + 1] |= rfe & 0xFFFF;
    assert!(KERNEL_CODE + 4 * words.len() as u32 <= KERNEL_CODE_END);
    assert!(
        crate::hle_kernel::EXCEPTION_HANDLER + 4 * layout.handler.len() as u32
            <= crate::hle_kernel::EXCEPTION_HANDLER_END
    );
    KernelCode { words, ..layout }
}

// ---------------------------------------------------------------- install

/// Default IRQ events (psx-spx "Default IRQ Handler Events"), in the order
/// psx-spx lists them: (I_STAT bit, event class). IRQ6 shares timer 1's
/// class, as psx-spx records.
const DEFINT: [(u32, u32); 11] = [
    (0, 0xF000_0001),
    (1, 0xF000_0002),
    (2, 0xF000_0003),
    (3, 0xF000_0004),
    (4, 0xF000_0005),
    (5, 0xF000_0006),
    (6, 0xF000_0006),
    (7, 0xF000_0008),
    (9, 0xF000_0009),
    (10, 0xF000_000A),
    (8, 0xF000_000B),
];

fn write_words(bus: &mut Bus, base: u32, words: &[u32]) {
    for (i, word) in words.iter().enumerate() {
        poke32(bus, base + 4 * i as u32, *word);
    }
}

/// C(07h) InstallExceptionHandlers: the four-word jump to C(06h) at 80h,
/// and a copy at 0 whose first word then holds 3 (psx-spx "BIOS Memory
/// Map", garbage area: games read [0..9], and R-Type needs the halfword at
/// 0 to be nonzero).
pub fn install_vector(bus: &mut Bus) {
    let handler = crate::hle_kernel::EXCEPTION_HANDLER;
    let mut a = Asm::new(VECTOR);
    a.lui(K0, (handler >> 16) as u16);
    a.addiu(K0, K0, handler as u16 as i16);
    a.jr(K0);
    a.nop();
    let words = a.finish();
    write_words(bus, VECTOR, &words);
    write_words(bus, 0, &words);
    poke32(bus, 0, 3);
}

/// Lay out the exception core: guest code, table entries for the guest
/// routines, the default exit buffer, chain element descriptors, kernel
/// variables, the vector, and the default handlers.
pub fn install(bus: &mut Bus) {
    use crate::hle_kernel::{A0_TABLE, B0_TABLE, EXCEPTION_HANDLER};
    let code = code();
    write_words(bus, KERNEL_CODE, &code.words);
    write_words(bus, EXCEPTION_HANDLER, &code.handler);
    for (entry, target) in [
        (B0_TABLE + 4 * 0x07, code.deliver_event),
        (B0_TABLE + 4 * 0x17, code.return_from_exception),
        (A0_TABLE + 4 * 0x43, code.exec),
        (A0_TABLE + 4 * 0x9D, code.get_conf),
    ] {
        poke32(bus, entry, target);
    }

    // psx-spx B(18h): ReturnFromException, the exception stacktop minus 4,
    // and zero for fp, s0..s7 and gp.
    write_words(bus, DEFAULT_JMPBUF, &[0; 12]);
    poke32(bus, DEFAULT_JMPBUF, code.return_from_exception);
    poke32(bus, DEFAULT_JMPBUF + 4, EXCEPTION_STACK_TOP - 4);
    poke32(bus, kvar::EXIT_JMPBUF, DEFAULT_JMPBUF);
    poke32(bus, kvar::EXCEPTION_SP, EXCEPTION_STACK_TOP);

    descriptor(bus, HI_SYSCALL, 0, stub_addr(3, internal::SYSCALL_VERIFIER));
    for n in 0..4u32 {
        descriptor(
            bus,
            HI_RCNT + 16 * n,
            stub_addr(3, internal::RCNT_HANDLER + n as u8),
            code.rcnt_verifier[n as usize],
        );
        // ChangeClearRCnt is on for all four until a game turns it off.
        poke32(bus, kvar::RCNT_AUTOACK + 4 * n, 1);
    }
    descriptor(bus, HI_DEFINT, 0, code.defint_verifier);
    for (i, (irq, class)) in DEFINT.iter().enumerate() {
        let entry = DEFINT_TABLE + 12 * i as u32;
        poke32(bus, entry, 1 << irq);
        poke32(bus, entry + 4, *class);
        poke32(bus, entry + 8, kvar::IRQ_AUTOACK + 4 * irq);
    }
    poke32(bus, DEFINT_TABLE + 12 * DEFINT.len() as u32, 0);
    poke32(bus, kvar::DQ_COUNT, 0);
    write_words(bus, EXEC_DEPTH, &[0; 1 + EXEC_SLOTS as usize]);

    install_vector(bus);
    enqueue_defaults(bus);
}

/// Fill a chain element (psx-spx "Priority Chains"): next, second
/// function, first function, unused.
fn descriptor(bus: &mut Bus, at: u32, second: u32, first: u32) {
    poke32(bus, at, 0);
    poke32(bus, at + 4, second);
    poke32(bus, at + 8, first);
    poke32(bus, at + 12, 0);
}

/// The handlers psx-spx lists as present after boot that belong to the
/// exception core: the SYSCALL handler (priority 0), the timer and VBlank
/// handlers (priority 1) and the default IRQ handler (priority 3). The CD,
/// pad and card drivers add their own. Also used by SetConf, which
/// re-enqueues the defaults after reallocating the control blocks.
pub fn enqueue_defaults(bus: &mut Bus) {
    enqueue_syscall_handler(bus, 0);
    enqueue_rcnt(bus, 1, false);
    enqueue_defint(bus, 3);
}

// ----------------------------------------------------------------- chains

/// ExCB for `prio` (0..3), or `None`.
fn excb(bus: &Bus, prio: u32) -> Option<u32> {
    let base = peek32(bus, TOT);
    (base != 0 && prio < 4).then_some(base + 8 * prio)
}

/// Remove `element` from every chain, so enqueueing it again cannot make
/// a chain point at itself.
fn unlink_everywhere(bus: &mut Bus, element: u32) {
    for prio in 0..4 {
        deq_int(bus, prio, element);
    }
}

/// C(02h) SysEnqIntRP(prio, element): insert at the head of the chain
/// (psx-spx "Priority Chains").
pub fn enq_int(bus: &mut Bus, prio: u32, element: u32) {
    let Some(head) = excb(bus, prio) else {
        return;
    };
    poke32(bus, element, peek32(bus, head));
    poke32(bus, head, element);
}

/// C(03h) SysDeqIntRP(prio, element): unlink `element` wherever it is in
/// the chain. psx-spx documents that the retail function only removes
/// the first element reliably and reads garbage after that; the HLE
/// removes it anywhere, because the kernel's own drivers dequeue and
/// requeue their elements in chains games also use. Returns the element,
/// or 0 when it was not in the chain.
pub fn deq_int(bus: &mut Bus, prio: u32, element: u32) -> u32 {
    let Some(head) = excb(bus, prio) else {
        return 0;
    };
    let mut link = head;
    for _ in 0..256 {
        let current = peek32(bus, link);
        if current == 0 {
            return 0;
        }
        if current == element {
            poke32(bus, link, peek32(bus, element));
            return element;
        }
        link = current;
    }
    0
}

/// C(01h) EnqueueSyscallHandler(prio).
pub fn enqueue_syscall_handler(bus: &mut Bus, prio: u32) {
    unlink_everywhere(bus, HI_SYSCALL);
    enq_int(bus, prio, HI_SYSCALL);
}

/// C(00h) EnqueueTimerAndVblankIrqs(prio): the three timers and VBlank,
/// enqueued so the chain reads VBlank, timer 2, timer 1, timer 0 (psx-spx
/// "Priority Chains"). Called by a game (`touch_hw`), it also masks those
/// four IRQs and stops and clears the three timers.
pub fn enqueue_rcnt(bus: &mut Bus, prio: u32, touch_hw: bool) {
    if touch_hw {
        let mask = bus.read32(0x1F80_1074);
        bus.write32(0x1F80_1074, mask & !0x71);
        for t in 0..3 {
            for reg in [4, 8, 0] {
                bus.write16(timer_reg(t, reg), 0);
            }
        }
    }
    for n in 0..4 {
        let element = HI_RCNT + 16 * n;
        unlink_everywhere(bus, element);
        enq_int(bus, prio, element);
    }
}

/// C(0Ch) InitDefInt(prio): all SetIrqAutoAck flags off, then the default
/// IRQ verifier into the chain.
pub fn enqueue_defint(bus: &mut Bus, prio: u32) {
    for irq in 0..11 {
        poke32(bus, kvar::IRQ_AUTOACK + 4 * irq, 0);
    }
    unlink_everywhere(bus, HI_DEFINT);
    enq_int(bus, prio, HI_DEFINT);
}

// ----------------------------------------------------------------- events

/// EvCB status: free.
pub const EV_FREE: u32 = 0;
/// EvCB status: disabled.
pub const EV_DISABLED: u32 = 0x1000;
/// EvCB status: enabled, waiting.
pub const EV_BUSY: u32 = 0x2000;
/// EvCB status: enabled, delivered.
pub const EV_READY: u32 = 0x4000;
/// EvCB mode: mark the event ready.
pub const EV_MODE_READY: u32 = 0x2000;
/// EvCB mode: call the callback and stay busy.
pub const EV_MODE_CALLBACK: u32 = 0x1000;
/// Bytes per EvCB.
const EVCB_SIZE: u32 = crate::hle_kernel::EVCB_SIZE;
/// Event handles are F1000000h + index (psx-spx "BIOS Memory Map").
const EVENT_HANDLE: u32 = 0xF100_0000;

fn evcb_count(bus: &Bus) -> u32 {
    peek32(bus, TOT + 0x24) / EVCB_SIZE
}

fn evcb(bus: &Bus, index: u32) -> u32 {
    peek32(bus, TOT + 0x20) + EVCB_SIZE * index
}

/// The EvCB of `event`, if the handle names one.
fn evcb_of(bus: &Bus, event: u32) -> Option<u32> {
    let index = event & 0xFFFF;
    (index < evcb_count(bus)).then(|| evcb(bus, index))
}

/// EvCB address of `event` (0 for a handle that names none).
#[cfg(test)]
fn event_addr(bus: &Bus, event: u32) -> u32 {
    evcb_of(bus, event).unwrap_or(0)
}

/// C(04h) get_free_EvCB_slot.
pub fn free_evcb(bus: &Bus) -> Option<u32> {
    (0..evcb_count(bus)).find(|&i| peek32(bus, evcb(bus, i) + 4) == EV_FREE)
}

/// B(08h) OpenEvent: a free EvCB, disabled, or FFFFFFFFh.
pub fn open_event(bus: &mut Bus, class: u32, spec: u32, mode: u32, func: u32) -> u32 {
    let Some(index) = free_evcb(bus) else {
        return u32::MAX;
    };
    let e = evcb(bus, index);
    poke32(bus, e, class);
    poke32(bus, e + 4, EV_DISABLED);
    poke32(bus, e + 8, spec);
    poke32(bus, e + 12, mode);
    poke32(bus, e + 16, func);
    EVENT_HANDLE | index
}

/// B(09h) CloseEvent.
pub fn close_event(bus: &mut Bus, event: u32) {
    if let Some(e) = evcb_of(bus, event) {
        poke32(bus, e + 4, EV_FREE);
    }
}

/// B(0Ch) EnableEvent / B(0Dh) DisableEvent. A free EvCB stays free.
pub fn set_event_enabled(bus: &mut Bus, event: u32, enabled: bool) {
    if let Some(e) = evcb_of(bus, event) {
        if peek32(bus, e + 4) != EV_FREE {
            poke32(bus, e + 4, if enabled { EV_BUSY } else { EV_DISABLED });
        }
    }
}

/// B(0Bh) TestEvent: true once for a ready event, which goes back to busy.
pub fn test_event(bus: &mut Bus, event: u32) -> bool {
    match evcb_of(bus, event) {
        Some(e) if peek32(bus, e + 4) == EV_READY => {
            poke32(bus, e + 4, EV_BUSY);
            true
        }
        _ => false,
    }
}

/// B(0Ah) WaitEvent: `Some(1)` once the event is ready (it goes back to
/// busy), `Some(0)` when it is not enabled, `None` while it is waiting.
pub fn wait_event(bus: &mut Bus, event: u32) -> Option<u32> {
    let Some(e) = evcb_of(bus, event) else {
        return Some(0);
    };
    match peek32(bus, e + 4) {
        EV_READY => {
            poke32(bus, e + 4, EV_BUSY);
            Some(1)
        }
        EV_BUSY => None,
        _ => Some(0),
    }
}

/// Whether [`wait_event`] would keep waiting without changing anything.
pub(crate) fn wait_event_waiting(bus: &Bus, event: u32) -> bool {
    evcb_of(bus, event).is_some_and(|e| peek32(bus, e + 4) == EV_BUSY)
}

/// B(20h) UnDeliverEvent: ready mark-ready events with this class and spec
/// go back to busy.
pub fn undeliver_event(bus: &mut Bus, class: u32, spec: u32) {
    for i in 0..evcb_count(bus) {
        let e = evcb(bus, i);
        if peek32(bus, e + 4) == EV_READY
            && peek32(bus, e + 12) == EV_MODE_READY
            && peek32(bus, e) == class
            && peek32(bus, e + 8) == spec
        {
            poke32(bus, e + 4, EV_BUSY);
        }
    }
}

// ---------------------------------------------------------------- threads

/// Thread handles are FF000000h + index (psx-spx "BIOS Thread Functions").
const THREAD_HANDLE: u32 = 0xFF00_0000;

fn tcb_count(bus: &Bus) -> u32 {
    peek32(bus, TOT + 0x14) / crate::hle_kernel::TCB_SIZE
}

fn tcb_at(bus: &Bus, index: u32) -> u32 {
    peek32(bus, TOT + 0x10) + crate::hle_kernel::TCB_SIZE * index
}

/// C(05h) get_free_TCB_slot.
pub fn free_tcb(bus: &Bus) -> Option<u32> {
    (0..tcb_count(bus))
        .find(|&i| peek32(bus, tcb_at(bus, i) + tcb::STATUS) == crate::hle_kernel::TCB_FREE)
}

/// B(0Eh) OpenTh(pc, sp, gp): a free TCB marked used, with its return PC,
/// sp = fp and gp set; the other registers stay as they were. Returns the
/// handle or FFFFFFFFh.
pub fn open_thread(bus: &mut Bus, pc: u32, sp: u32, gp: u32) -> u32 {
    let Some(index) = free_tcb(bus) else {
        return u32::MAX;
    };
    let t = tcb_at(bus, index);
    poke32(bus, t + tcb::STATUS, crate::hle_kernel::TCB_USED);
    poke32(bus, t + tcb::MODE, 0x1000);
    poke32(bus, t + tcb::EPC, pc);
    poke32(bus, t + tcb::reg(SP), sp);
    poke32(bus, t + tcb::reg(FP), sp);
    poke32(bus, t + tcb::reg(GP), gp);
    THREAD_HANDLE | index
}

/// B(0Fh) CloseTh: the TCB becomes free.
pub fn close_thread(bus: &mut Bus, thread: u32) {
    let index = thread & 0xFFFF;
    if index < tcb_count(bus) {
        let t = tcb_at(bus, index);
        poke32(bus, t + tcb::STATUS, crate::hle_kernel::TCB_FREE);
    }
}

/// TCB address of a thread handle, for ChangeTh's SYSCALL(3).
pub fn thread_tcb(bus: &Bus, thread: u32) -> u32 {
    tcb_at(bus, thread & 0xFFFF)
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

/// SR bits SYSCALL 1/2 clear and set: IEp (bit 2, which RFE moves back to
/// IEc) and the IRQ mask bit IM2 (bit 10).
const CRITICAL_BITS: u32 = 0x0404;

/// The SYSCALL handler (priority 0), reading the frame the handler saved
/// in the current TCB (psx-spx "BIOS Function Summary", SYS functions, and
/// "Unresolved Exception Events"):
///
/// * an interrupt is not its business;
/// * a SYSCALL returns past the instruction: 0 does nothing, 1
///   (EnterCriticalSection) clears IEp and IM2 and returns whether both
///   were set, 2 (ExitCriticalSection) sets them, 3 makes the TCB in a1
///   current (the old thread gets v0 = 1), and any other number delivers
///   F0000010h/4000h;
/// * any other exception delivers F0000010h/1000h and goes on to A(40h).
pub fn syscall_verifier(bus: &mut Bus) -> SyscallAction {
    let t = crate::hle_kernel::current_tcb(bus);
    let excode = (peek32(bus, t + tcb::CAUSE) >> 2) & 0x1F;
    match excode {
        0 => SyscallAction::Pass,
        8 => {
            let epc = peek32(bus, t + tcb::EPC);
            poke32(bus, t + tcb::EPC, epc.wrapping_add(4));
            let sr = peek32(bus, t + tcb::SR);
            match peek32(bus, t + tcb::reg(A0)) {
                0 => {}
                1 => {
                    let was = u32::from(sr & CRITICAL_BITS == CRITICAL_BITS);
                    poke32(bus, t + tcb::SR, sr & !CRITICAL_BITS);
                    poke32(bus, t + tcb::reg(V0), was);
                }
                2 => poke32(bus, t + tcb::SR, sr | CRITICAL_BITS),
                3 => {
                    poke32(bus, t + tcb::reg(V0), 1);
                    let pcb = peek32(bus, TOT + 8);
                    if pcb != 0 {
                        poke32(bus, pcb, peek32(bus, t + tcb::reg(A1)));
                    }
                }
                _ => {
                    return SyscallAction::Deliver(
                        0xF000_0010,
                        0x4000,
                        code().return_from_exception,
                    )
                }
            }
            SyscallAction::Return
        }
        _ => SyscallAction::Deliver(0xF000_0010, 0x1000, code().unresolved_glue),
    }
}

/// Second function of root counter `n`'s chain element: with
/// ChangeClearRCnt on, acknowledge the IRQ and end the exception (true);
/// otherwise let the chain go on.
pub fn rcnt_handler(bus: &mut Bus, n: u8) -> bool {
    let n = u32::from(n);
    if n > 3 || peek32(bus, kvar::RCNT_AUTOACK + 4 * n) == 0 {
        return false;
    }
    bus.write32(0x1F80_1070, !rcnt_irq_bit(n));
    true
}

/// C(0Ah) ChangeClearRCnt(t, flag): returns the previous flag.
pub fn change_clear_rcnt(bus: &mut Bus, t: u32, flag: u32) -> u32 {
    if t > 3 {
        return 0;
    }
    let old = peek32(bus, kvar::RCNT_AUTOACK + 4 * t);
    poke32(bus, kvar::RCNT_AUTOACK + 4 * t, flag);
    old
}

/// Queue DeliverEvent(class, spec) for [`flush_events`]. HLE handlers use
/// this instead of delivering directly because a delivery can call a
/// guest callback. Deliveries past [`DQ_MAX`] are dropped (none of the
/// kernel's handlers queues more than three).
pub fn queue_event(bus: &mut Bus, class: u32, spec: u32) {
    let n = peek32(bus, kvar::DQ_COUNT);
    if n >= DQ_MAX {
        return;
    }
    poke32(bus, kvar::DQ_ITEMS + 8 * n, class);
    poke32(bus, kvar::DQ_ITEMS + 8 * n + 4, spec);
    poke32(bus, kvar::DQ_COUNT, n + 1);
}

/// Start delivering the queued events through the guest DeliverEvent,
/// continuing at `then` afterwards. Returns the address to jump to, with
/// `a0`, `a1` and `ra` set up, or `None` when nothing is queued.
pub fn flush_events(bus: &mut Bus, gprs: &mut [u32; 32], then: u32) -> Option<u32> {
    flush_events_returning(bus, gprs, then, gprs[2])
}

/// [`flush_events`] for a function returning `v0`: the value reaches
/// `then` in `v0` after the deliveries.
pub fn flush_events_returning(
    bus: &mut Bus,
    gprs: &mut [u32; 32],
    then: u32,
    v0: u32,
) -> Option<u32> {
    if peek32(bus, kvar::DQ_COUNT) == 0 {
        return None;
    }
    poke32(bus, kvar::DQ_RETURN, then);
    poke32(bus, kvar::DQ_V0, v0);
    Some(deliver_next(bus, gprs))
}

/// DELIVER_NEXT: pop the first queued event into DeliverEvent, or
/// continue at the saved address when the queue is empty.
pub fn deliver_next(bus: &mut Bus, gprs: &mut [u32; 32]) -> u32 {
    let n = peek32(bus, kvar::DQ_COUNT);
    if n == 0 {
        gprs[2] = peek32(bus, kvar::DQ_V0);
        return peek32(bus, kvar::DQ_RETURN);
    }
    gprs[4] = peek32(bus, kvar::DQ_ITEMS);
    gprs[5] = peek32(bus, kvar::DQ_ITEMS + 4);
    for i in 1..n {
        let class = peek32(bus, kvar::DQ_ITEMS + 8 * i);
        let spec = peek32(bus, kvar::DQ_ITEMS + 8 * i + 4);
        poke32(bus, kvar::DQ_ITEMS + 8 * (i - 1), class);
        poke32(bus, kvar::DQ_ITEMS + 8 * (i - 1) + 4, spec);
    }
    poke32(bus, kvar::DQ_COUNT, n - 1);
    gprs[31] = stub_addr(3, internal::DELIVER_NEXT);
    code().deliver_event
}

// ------------------------------------------------------------------ timers

fn timer_reg(t: u32, reg: u32) -> u32 {
    0x1F80_1100 + 0x10 * t + reg
}

/// B(02h) init_timer(t, reload, flags), psx-spx: for t = 0..2, mode 0,
/// target = reload, then mode 48h (49h when flags bit 4), OR 100h when
/// flags bit 0 is clear, OR 10h when flags bit 12 is set. Returns 1, or
/// 0 for t > 2.
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

    /// TestEvent through the B0 vector costs what the retail kernel's does
    /// (black-box timing, vector to return: 43 cycles for a busy event, 48
    /// for a ready one). Two cycles let Resident Evil 2 and 3 spin their
    /// 250,000-iteration card timeout four times faster than on a console,
    /// so the timeout fired before `_card_load` finished ("Access error"
    /// on an empty card, where the retail kernel shows "no files").
    #[test]
    fn test_event_costs_the_measured_retail_cycles() {
        use crate::Cpu;
        let mut bus = hle_bus();
        let handle = open_event(&mut bus, 0xF400_0001, 4, 0x2000, 0);
        set_event_enabled(&mut bus, handle, true);
        let mut cpu = Cpu::new();
        bus.write32(0x8001_0000, 0x1000_FFFF);
        let mut call = |bus: &mut Bus| {
            cpu.gprs_mut_for_test()[4] = handle;
            cpu.gprs_mut_for_test()[9] = 0x0B;
            cpu.gprs_mut_for_test()[31] = 0x8001_0000;
            cpu.set_pc_for_test(0xB0);
            let before = bus.cycles();
            cpu.step(bus).unwrap();
            assert_eq!(cpu.pc(), 0x8001_0000);
            (bus.cycles() - before, cpu.gpr(2))
        };
        assert_eq!(call(&mut bus), (43, 0));
        let status = event_addr(&bus, handle) + 4;
        poke32(&mut bus, status, EV_READY);
        assert_eq!(call(&mut bus), (48, 1));
    }

    /// memcpy, memset and bzero through the A0 vector cost what the retail
    /// ROM routines do for the length (black-box fits; see
    /// `hle_bios::call_cycles`).
    #[test]
    fn libc_copies_cost_the_measured_retail_cycles_per_byte() {
        use crate::Cpu;
        let mut bus = hle_bus();
        let mut cpu = Cpu::new();
        bus.write32(0x8001_0000, 0x1000_FFFF);
        let mut call = |bus: &mut Bus, func: u32, a: [u32; 3]| {
            cpu.gprs_mut_for_test()[4] = a[0];
            cpu.gprs_mut_for_test()[5] = a[1];
            cpu.gprs_mut_for_test()[6] = a[2];
            cpu.gprs_mut_for_test()[9] = func;
            cpu.gprs_mut_for_test()[31] = 0x8001_0000;
            cpu.set_pc_for_test(0xA0);
            let before = bus.cycles();
            cpu.step(bus).unwrap();
            assert_eq!(cpu.pc(), 0x8001_0000);
            bus.cycles() - before
        };
        // Measured: memcpy 20 bytes 4299, 92 bytes 18875; memset 80 bytes
        // 10887; bzero 480 bytes 63786.
        assert_eq!(call(&mut bus, 0x2A, [0x8002_0000, 0x8003_0000, 20]), 4293);
        assert_eq!(call(&mut bus, 0x2A, [0x8002_0000, 0x8003_0000, 92]), 18873);
        assert_eq!(call(&mut bus, 0x2B, [0x8002_0000, 0, 80]), 10874);
        assert_eq!(call(&mut bus, 0x28, [0x8002_0000, 480, 0]), 63682);
        // Nothing to copy: the dispatch only.
        assert_eq!(call(&mut bus, 0x2A, [0x8002_0000, 0x8003_0000, 0]), 2);
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
