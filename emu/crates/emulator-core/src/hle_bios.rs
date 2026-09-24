//! High-level emulation of the PS1 BIOS syscall tables.
//!
//! The BIOS publishes three entry points -- at physical addresses
//! `0xA0`, `0xB0`, and `0xC0` -- that dispatch to a table of service
//! functions. Each caller does:
//!
//! ```text
//!     la $t0, 0xA0       # or 0xB0 / 0xC0
//!     jr $t0
//!     li $t1, <func>     # in the branch delay slot
//! ```
//!
//! and the BIOS dispatcher reads `$t1`, calls the right handler, and
//! returns to `$ra`.
//!
//! When we side-load a PSX-EXE we bypass the BIOS boot sequence, so
//! the dispatcher stubs at those RAM addresses aren't populated. This
//! module fills the gap by intercepting the instruction fetch when
//! `PC` hits one of the three entry addresses, running the requested
//! service in host Rust, and "returning" by setting `PC = $ra`.
//!
//! Scope so far: TTY output, `FlushCache`, the stateless memory and
//! string helpers, `SetMem`, and the event system in "always-ready" mode
//! so homebrew that polls `TestEvent` doesn't spin forever. Games that
//! use richer BIOS facilities (file I/O, memory cards, controllers) land
//! their handlers here as the kernel model grows.

use crate::Bus;
use psx_hw::memory::to_physical;

/// A0 table slot for A(40h) SystemErrorUnresolvedException. Homebrew
/// replaces this entry to hook unresolved exceptions.
pub(crate) const UNRESOLVED_HANDLER_PTR: u32 = crate::hle_kernel::A0_TABLE + 4 * 0x40;
/// Return address given to a guest unresolved-exception handler; reaching
/// it restores the saved thread frame.
pub(crate) const EXCEPTION_RETURN_STUB: u32 = 0x8000_00D0;

/// Offsets inside a TCB (psx-spx "Thread Control Blocks"): registers r0..r31
/// from +08h, then return PC, HI, LO, SR and CAUSE.
pub(crate) const TCB_REGISTERS: u32 = 0x08;
pub(crate) const TCB_RETURN_PC: u32 = 0x88;
pub(crate) const TCB_HI: u32 = 0x8C;
pub(crate) const TCB_LO: u32 = 0x90;
pub(crate) const TCB_SR: u32 = 0x94;
pub(crate) const TCB_CAUSE: u32 = 0x98;

/// One of the three BIOS dispatcher tables, or the HLE kernel's own
/// internal functions (reached only through pointers the kernel hands out).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Table {
    /// Entry point at physical `0xA0`.
    A,
    /// Entry point at physical `0xB0`.
    B,
    /// Entry point at physical `0xC0`.
    C,
    /// Kernel-internal functions ([`crate::hle_kernel::internal`]).
    Kernel,
}

impl Table {
    /// Table index as used by [`crate::bios_names`]: 0 = A, 1 = B, 2 = C,
    /// 3 = kernel-internal.
    pub fn index(self) -> u8 {
        match self {
            Table::A => 0,
            Table::B => 1,
            Table::C => 2,
            Table::Kernel => 3,
        }
    }

    /// Single-letter label, `'A'`, `'B'`, `'C'`, or `'K'` for internal.
    pub fn letter(self) -> char {
        match self {
            Table::Kernel => 'K',
            other => (b'A' + other.index()) as char,
        }
    }

    fn from_index(index: u8) -> Self {
        match index {
            0 => Table::A,
            1 => Table::B,
            2 => Table::C,
            _ => Table::Kernel,
        }
    }

    fn from_phys(phys: u32) -> Option<Self> {
        match phys {
            0xA0 => Some(Table::A),
            0xB0 => Some(Table::B),
            0xC0 => Some(Table::C),
            _ => None,
        }
    }
}

/// Conventional name of `(table, func)`, including the internal functions.
pub fn function_name(table: Table, func: u8) -> &'static str {
    match table {
        Table::Kernel => match func {
            crate::hle_kernel::internal::START_PAD => "startPad",
            crate::hle_kernel::internal::STOP_PAD => "stopPad",
            crate::hle_kernel::internal::SET_PAD_OUTPUT_DATA => "setPadOutputData",
            _ => "?",
        },
        t => crate::bios_names::function_name(t.index(), u32::from(func)),
    }
}

/// How a BIOS function was serviced.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Implemented with the documented semantics.
    Done,
    /// Accepted without the real effect (events are always ready, device
    /// registration is ignored, ...). The guest proceeds, but may rely on
    /// state the kernel never produced.
    Stub,
    /// Not implemented. Returns 0 without touching guest state, logs once
    /// per function, and stops the CPU when strict mode is on.
    Unimplemented,
}

/// First occurrence of a stubbed or unimplemented BIOS function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallRecord {
    /// Dispatch table.
    pub table: Table,
    /// Function number.
    pub func: u8,
    /// Conventional name from [`crate::bios_names`], or `"?"`.
    pub name: &'static str,
    /// How the call was serviced.
    pub outcome: Outcome,
    /// `$a0..$a3` at the first call.
    pub args: [u32; 4],
    /// Caller's `$ra` at the first call.
    pub ra: u32,
    /// Bus cycle of the first call.
    pub cycle: u64,
    /// Calls so far.
    pub count: u64,
}

impl std::fmt::Display for CallRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}({:02X}h) {} a0={:#010x} a1={:#010x} a2={:#010x} a3={:#010x} ra={:#010x} cycle={}",
            self.table.letter(),
            self.func,
            self.name,
            self.args[0],
            self.args[1],
            self.args[2],
            self.args[3],
            self.ra,
            self.cycle
        )
    }
}

/// Result of one HLE dispatch.
#[derive(Copy, Clone, Debug)]
pub struct Hle {
    /// Value for `$v0`, or `None` when the table entry was guest code and
    /// the CPU only jumps there.
    pub v0: Option<u32>,
    /// PC to continue at: `$ra` after an HLE call (as left by the handler,
    /// so longjmp can redirect it), or the guest entry for a guest jump.
    pub next_pc: u32,
    /// Which table was called.
    pub table: Table,
    /// Function number.
    pub func: u8,
    /// How the call was serviced.
    pub outcome: Outcome,
    /// The handler changed code in RAM; the instruction cache must be
    /// invalidated (FlushCache, kernel patch counterpatches).
    pub flush_icache: bool,
}

/// Intercept a fetch at `pc` when it is a BIOS call.
///
/// Two forms are recognised, both only below 64 KiB of RAM:
///
/// * the `0xA0`/`0xB0`/`0xC0` vectors: the function number is in `$t1`;
///   the RAM table entry is looked up like the retail dispatcher does. An
///   entry pointing at an HLE trap word runs that function; an entry the
///   guest replaced with its own code is jumped to.
/// * a trap word itself (see [`crate::hle_kernel::trap_word`]), reached by
///   calling a table entry directly or through a pointer the kernel handed
///   out.
///
/// `gprs` is the CPU register file: handlers read their arguments from it
/// and may change callee-saved registers (longjmp).
pub fn dispatch(pc: u32, bus: &mut Bus, gprs: &mut [u32; 32]) -> Option<Hle> {
    let phys = to_physical(pc);
    if phys >= 0x1_0000 {
        return None;
    }
    let (table, func) = if let Some(vector) = Table::from_phys(phys) {
        let t1 = gprs[9];
        let (base, len) = crate::hle_kernel::table(vector.index());
        if t1 >= len {
            return Some(finish(
                bus,
                gprs,
                vector,
                t1 as u8,
                Ret::Unimplemented,
                false,
            ));
        }
        let entry = crate::hle_kernel::peek32(bus, base + 4 * t1);
        match crate::hle_kernel::decode_trap(crate::hle_kernel::peek32(bus, entry)) {
            Some((t, f)) => (Table::from_index(t), f),
            None => {
                return Some(Hle {
                    v0: None,
                    next_pc: entry,
                    table: vector,
                    func: t1 as u8,
                    outcome: Outcome::Done,
                    flush_icache: false,
                })
            }
        }
    } else {
        let (t, f) = crate::hle_kernel::decode_trap(bus.peek_instruction(pc)?)?;
        (Table::from_index(t), f)
    };
    let mut flush = table == Table::A && func == 0x44;
    let ret = run(table, func, bus, gprs, &mut flush);
    if table != Table::Kernel {
        bus.hle_bios_log_call(table, func);
    }
    Some(finish(bus, gprs, table, func, ret, flush))
}

fn finish(bus: &mut Bus, gprs: &[u32; 32], table: Table, func: u8, ret: Ret, flush: bool) -> Hle {
    let (outcome, v0) = match ret {
        Ret::Done(v0) => (Outcome::Done, v0),
        Ret::Stub(v0) => (Outcome::Stub, v0),
        Ret::Unimplemented => (Outcome::Unimplemented, 0),
    };
    if outcome != Outcome::Done {
        let args = [gprs[4], gprs[5], gprs[6], gprs[7]];
        bus.hle_bios_record_call(table, func, outcome, args, gprs[31]);
    }
    Hle {
        v0: Some(v0),
        next_pc: gprs[31],
        table,
        func,
        outcome,
        flush_icache: flush,
    }
}

/// Handler result. `Unimplemented` arms must not have side effects, so
/// strict mode can stop before the call changes anything.
enum Ret {
    Done(u32),
    Stub(u32),
    Unimplemented,
}

fn run(table: Table, func: u8, bus: &mut Bus, gprs: &mut [u32; 32], flush: &mut bool) -> Ret {
    use crate::hle_kernel::{self as k, Heap};
    use Ret::{Done, Stub, Unimplemented};
    let args = [gprs[4], gprs[5], gprs[6], gprs[7]];
    let sp = gprs[29];
    match (table, func) {
        // --- A-table ---
        //
        // Numbering and semantics follow the OpenBIOS `romA0table`
        // (pcsx-redux src/mips/openbios/kernel/handlers.c, MIT, used as a
        // specification only) after its `patchA0table`, which aliases
        // A(00h..09h) to B(32h..3Bh) and A(3Bh..3Eh) to B(3Ch..3Fh), and the
        // psx-spx "BIOS Memory Fill/Copy/Compare" and "BIOS String Functions"
        // descriptions. Stateful libc (malloc, rand, strtok) and the
        // functions psx-spx documents as buggy (memcmp/bcmp, memmove,
        // strstr, strpbrk) are deliberately absent until the kernel RAM
        // layout exists; they fall through to the unimplemented arm.

        // A(0Eh) abs / A(0Fh) labs.
        (Table::A, 0x0E) | (Table::A, 0x0F) => Done((args[0] as i32).wrapping_abs() as u32),

        // A(13h) setjmp(buf) / A(14h) longjmp(buf, value). Buffer layout
        // (psx-spx): ra, sp, fp, s0..s7, gp. longjmp returns `value`
        // unchanged (0 is not bumped to 1) at the restored ra.
        (Table::A, 0x13) => {
            let buf = args[0];
            for (slot, reg) in JMPBUF_REGS.iter().enumerate() {
                k::poke32(bus, buf.wrapping_add(4 * slot as u32), gprs[*reg]);
            }
            Done(0)
        }
        (Table::A, 0x14) => {
            let buf = args[0];
            for (slot, reg) in JMPBUF_REGS.iter().enumerate() {
                gprs[*reg] = k::peek32(bus, buf.wrapping_add(4 * slot as u32));
            }
            Done(args[1])
        }

        // A(15h) strcat(dst, src).
        (Table::A, 0x15) => Done(libc::strcat(bus, args[0], args[1])),
        // A(17h) strcmp(s1, s2) / A(18h) strncmp(s1, s2, maxlen).
        (Table::A, 0x17) => Done(libc::strncmp(bus, args[0], args[1], None)),
        (Table::A, 0x18) => Done(libc::strncmp(bus, args[0], args[1], Some(args[2]))),
        // A(19h) strcpy(dst, src) / A(1Ah) strncpy(dst, src, maxlen).
        (Table::A, 0x19) => Done(libc::strcpy(bus, args[0], args[1])),
        (Table::A, 0x1A) => Done(libc::strncpy(bus, args[0], args[1], args[2])),
        // A(1Bh) strlen(s).
        (Table::A, 0x1B) => Done(libc::strlen(bus, args[0])),
        // A(1Ch) index / A(1Eh) strchr, A(1Dh) rindex / A(1Fh) strrchr.
        (Table::A, 0x1C) | (Table::A, 0x1E) => {
            Done(libc::strchr(bus, args[0], args[1] as u8, false))
        }
        (Table::A, 0x1D) | (Table::A, 0x1F) => {
            Done(libc::strchr(bus, args[0], args[1] as u8, true))
        }
        // A(25h) toupper / A(26h) tolower. psx-spx documents 00h..7Fh only;
        // bytes 80h..FFh come back unchanged here.
        (Table::A, 0x25) => Done(u32::from((args[0] as u8).to_ascii_uppercase())),
        (Table::A, 0x26) => Done(u32::from((args[0] as u8).to_ascii_lowercase())),

        // A(27h) bcopy(src, dst, len) / A(2Ah) memcpy(dst, src, len).
        (Table::A, 0x27) => {
            libc::memcpy(bus, args[1], args[0], args[2], args[0]);
            Done(args[0])
        }
        (Table::A, 0x2A) => {
            libc::memcpy(bus, args[0], args[1], args[2], args[0]);
            Done(args[0])
        }
        // A(28h) bzero(dst, len) / A(2Bh) memset(dst, fill, len).
        (Table::A, 0x28) => Done(libc::memset(bus, args[0], 0, args[1])),
        (Table::A, 0x2B) => Done(libc::memset(bus, args[0], args[1] as u8, args[2])),
        // A(2Eh) memchr(src, byte, len).
        (Table::A, 0x2E) => Done(libc::memchr(bus, args[0], args[1] as u8, args[2])),

        // A(33h) malloc / A(34h) free / A(37h) calloc / A(38h) realloc /
        // A(39h) InitHeap (psx-spx "BIOS Memory Allocation").
        (Table::A, 0x33) => Done(k::malloc(bus, Heap::User, args[0])),
        (Table::A, 0x34) => {
            k::free(bus, args[0]);
            Done(0)
        }
        (Table::A, 0x37) => Done(k::calloc(bus, args[0], args[1])),
        (Table::A, 0x38) => Done(k::realloc(bus, args[0], args[1])),
        (Table::A, 0x39) => {
            k::init_heap(bus, Heap::User, args[0], args[1]);
            Done(0)
        }

        // A(3Ch) putchar.
        (Table::A, 0x3C) => {
            write_byte_to_stdout(args[0] as u8);
            Done(0)
        }
        // A(3Eh) puts(s) / A(3Fh) printf.
        (Table::A, 0x3E) => {
            write_puts_to_stdout(bus, args[0]);
            Done(0)
        }
        (Table::A, 0x3F) => {
            // printf varargs follow the MIPS o32 ABI: a1-a3 first, then the
            // caller's reserved argument area beginning at sp+16. Reading
            // both sources lets public hardware suites print complete rows
            // instead of losing their fourth and later values.
            hle_printf(bus, args[0], &[args[1], args[2], args[3]], sp);
            Done(0)
        }

        // A(44h) FlushCache -- the CPU intercept invalidates its
        // instruction cache before this HLE handler returns.
        (Table::A, 0x44) => Done(0),

        // A(56h)/A(72h) _96_remove: the kernel's CD-ROM handlers and
        // events are removed. Only the kernel flag exists so far; the
        // handler chains and events arrive with the exception core.
        (Table::A, 0x56) | (Table::A, 0x72) => {
            k::poke32(bus, k::kvar::CD_KERNEL_ACTIVE, 0);
            Done(0)
        }

        // A(70h) _bu_init (memcard filesystem init) -- accept.
        (Table::A, 0x70) => Stub(0),

        // A(96h) AddCDROMDevice / A(97h) AddMemCardDevice -- games
        // call these during init to register filesystem drivers.
        // We don't model the device table; accept so the game moves on.
        (Table::A, 0x96) | (Table::A, 0x97) => Stub(0),

        // A(9Ch) SetConf(events, threads, stacktop): reallocate the
        // control blocks. A(9Dh) GetConf(&events, &threads, &stacktop).
        (Table::A, 0x9C) => {
            k::set_conf(bus, args[0], args[1], args[2]);
            Done(0)
        }
        (Table::A, 0x9D) => {
            let (event, tcb, stack) = k::get_conf(bus);
            k::poke32(bus, args[0], event);
            k::poke32(bus, args[1], tcb);
            k::poke32(bus, args[2], stack);
            Done(0)
        }

        // A(9Fh) SetMem(megabytes): 2 clears RAM_SIZE bits 8-9, 8 sets them,
        // and the size is recorded at [0x60] (psx-spx; OpenBIOS
        // kernel/misc.c setMemSize). Other values change nothing.
        (Table::A, 0x9F) => {
            set_mem_size(bus, args[0]);
            Done(0)
        }

        // --- B-table ---

        // B(00h) alloc_kernel_memory / B(01h) free_kernel_memory: the same
        // allocator over the kernel heap set up by SysInitMemory.
        (Table::B, 0x00) => Done(k::malloc(bus, Heap::Kernel, args[0])),
        (Table::B, 0x01) => {
            k::free(bus, args[0]);
            Done(0)
        }

        // B(07h) DeliverEvent -- accept; our event system is always-
        // ready so there's nothing to deliver.
        (Table::B, 0x07) => Stub(0),

        // B(08h) OpenEvent: return a synthetic handle. We accept
        // everything; the handle encodes table + slot for debug.
        (Table::B, 0x08) => Stub(0xF400_0000 | (args[0] & 0xFFFF)),

        // B(09h) CloseEvent, B(0Ah) WaitEvent, B(0Bh) TestEvent,
        // B(0Ch) EnableEvent, B(0Dh) DisableEvent -- always-ready.
        (Table::B, 0x09)
        | (Table::B, 0x0A)
        | (Table::B, 0x0B)
        | (Table::B, 0x0C)
        | (Table::B, 0x0D) => Stub(1),

        // B(12h) InitPad(buf1, siz1, buf2, siz2): tell the kernel
        // where to stash pad state. Since we poll the hardware
        // directly via psx-pad there's nothing for us to do.
        (Table::B, 0x12) => Stub(1),

        // B(13h) StartPad, B(14h) StopPad -- accept.
        (Table::B, 0x13) | (Table::B, 0x14) => Stub(1),

        // B(17h) ReturnFromException is completed by Cpu::execute_one:
        // when a side-loaded guest IRQ hook is active it restores the
        // interrupted CPU frame instead of returning to this call's `$ra`.
        (Table::B, 0x17) => Done(0),

        // B(18h) ResetEntryInt / B(19h) HookEntryInt. The latter receives
        // a BIOS-compatible JumpBuffer pointer (ra, sp, fp, s0..s7, gp).
        // Retaining it lets side-loaded EXEs use their real guest ISR rather
        // than relying on a synthetic VBlank callback.
        (Table::B, 0x18) => {
            bus.set_hle_irq_jump_buffer(None);
            Done(0)
        }
        (Table::B, 0x19) => {
            bus.set_hle_irq_jump_buffer(Some(args[0]));
            Done(0)
        }

        // B(3Dh) putchar -- same as A(3Ch).
        (Table::B, 0x3D) => {
            write_byte_to_stdout(args[0] as u8);
            Done(0)
        }
        // B(3Fh) puts -- same as A(3Eh).
        (Table::B, 0x3F) => {
            write_puts_to_stdout(bus, args[0]);
            Done(0)
        }

        // B(56h) GetC0Table / B(57h) GetB0Table. Psy-Q libraries call
        // these only to patch the kernel; the code after the call is
        // hashed and known variants are handled as OpenBIOS does.
        (Table::B, 0x56) | (Table::B, 0x57) => {
            let (patch_table, base) = if func == 0x56 {
                (2, k::C0_TABLE)
            } else {
                (1, k::B0_TABLE)
            };
            let (site, rewrote) = k::handle_patch_site(bus, patch_table, gprs[31]);
            *flush |= rewrote;
            bus.hle_bios_record_patch(site, gprs[31]);
            Done(base)
        }

        // B(5Bh) ChangeClearPAD(flag): pad/card handler VBlank auto-ack.
        // Returns the previous setting (OpenBIOS setSIO0AutoAck).
        (Table::B, 0x5B) => {
            let previous = k::peek32(bus, k::kvar::SIO0_AUTO_ACK);
            k::poke32(bus, k::kvar::SIO0_AUTO_ACK, args[0]);
            Done(previous)
        }

        // B(4Ah) InitCard, B(4Bh) StartCard, B(4Ch) StopCard.
        (Table::B, 0x4A) | (Table::B, 0x4B) | (Table::B, 0x4C) => Stub(1),

        // --- C-table (kernel interrupt handlers) ---

        // C(00h) EnqueueTimerAndVblankIrqs / C(01h) EnqueueSyscallHandler /
        // C(02h) SysEnqIntRP / C(03h) SysDeqIntRP. Registration is
        // accepted but the chains never run.
        (Table::C, 0x00) | (Table::C, 0x01) | (Table::C, 0x02) | (Table::C, 0x03) => Stub(0),

        // C(08h) SysInitMemory(addr, size): new kernel heap.
        (Table::C, 0x08) => {
            k::init_heap(bus, Heap::Kernel, args[0], args[1]);
            Done(0)
        }

        // --- Kernel-internal functions handed out by counterpatches ---
        (Table::Kernel, k::internal::START_PAD) => {
            k::poke32(bus, k::kvar::PAD_STARTED, 1);
            Done(0)
        }
        (Table::Kernel, k::internal::STOP_PAD) => {
            k::poke32(bus, k::kvar::PAD_STARTED, 0);
            Done(0)
        }
        (Table::Kernel, k::internal::SET_PAD_OUTPUT_DATA) => {
            for (i, value) in args.iter().enumerate() {
                k::poke32(bus, k::kvar::PAD_OUTPUT + 4 * i as u32, *value);
            }
            Done(0)
        }

        // C(0Ah) ChangeClearRCnt -- affects how the kernel's
        // root-counter handler clears flags. No-op.
        (Table::C, 0x0A) => Stub(args[1]),

        // Everything else is loud: logged once per function with its
        // arguments and caller, and a stop in strict mode.
        _ => Unimplemented,
    }
}

/// Registers saved by setjmp, in buffer order: ra, sp, fp, s0..s7, gp.
const JMPBUF_REGS: [usize; 12] = [31, 29, 30, 16, 17, 18, 19, 20, 21, 22, 23, 28];

/// RAM_SIZE memory-control register.
const RAM_SIZE_PORT: u32 = 0x1F80_1060;
/// Kernel variable holding the effective RAM size in megabytes.
pub(crate) const RAM_SIZE_MB_VAR: u32 = 0x0000_0060;

fn set_mem_size(bus: &mut Bus, megabytes: u32) {
    let current = bus.read32(RAM_SIZE_PORT);
    let value = match megabytes {
        2 => current & !0x300,
        8 => current | 0x300,
        _ => return,
    };
    bus.write32(RAM_SIZE_PORT, value);
    bus.write32(RAM_SIZE_MB_VAR, megabytes);
}

/// Stateless BIOS memory and string helpers. Behaviour, including the
/// null-pointer and length refusals, follows psx-spx. Every loop is capped
/// at the size of main RAM so a bad guest pointer cannot hang the host.
mod libc {
    use crate::Bus;

    /// Largest transfer or scan one call performs (2 MiB, the RAM size).
    const MAX_BYTES: u32 = 0x20_0000;
    /// Lengths above this are refused by the BIOS memory functions.
    const MAX_LEN: u32 = 0x7FFF_FFFF;

    fn rd(bus: &Bus, addr: u32) -> u8 {
        bus.try_read8(addr).unwrap_or(0)
    }

    /// Forward byte copy shared by memcpy and bcopy. `guard` is the pointer
    /// the BIOS refuses when null: `dst` for memcpy, `src` for bcopy.
    pub(super) fn memcpy(bus: &mut Bus, dst: u32, src: u32, len: u32, guard: u32) {
        if guard == 0 || len > MAX_LEN {
            return;
        }
        for i in 0..len.min(MAX_BYTES) {
            let b = rd(bus, src.wrapping_add(i));
            let _ = bus.write8_safe(dst.wrapping_add(i), b);
        }
    }

    /// memset/bzero: returns `dst`, or 0 when the fill is refused or empty.
    pub(super) fn memset(bus: &mut Bus, dst: u32, fill: u8, len: u32) -> u32 {
        if dst == 0 || len == 0 || len > MAX_LEN {
            return 0;
        }
        for i in 0..len.min(MAX_BYTES) {
            let _ = bus.write8_safe(dst.wrapping_add(i), fill);
        }
        dst
    }

    pub(super) fn memchr(bus: &Bus, src: u32, byte: u8, len: u32) -> u32 {
        if src == 0 || len > MAX_LEN {
            return 0;
        }
        (0..len.min(MAX_BYTES))
            .map(|i| src.wrapping_add(i))
            .find(|&addr| rd(bus, addr) == byte)
            .unwrap_or(0)
    }

    pub(super) fn strlen(bus: &Bus, src: u32) -> u32 {
        if src == 0 {
            return 0;
        }
        (0..MAX_BYTES)
            .find(|&i| rd(bus, src.wrapping_add(i)) == 0)
            .unwrap_or(MAX_BYTES)
    }

    /// strcmp (`maxlen` = None) and strncmp. Mismatching bytes are
    /// sign-extended before subtracting; null pointers give 0 (both), -1
    /// (first) or +1 (second).
    pub(super) fn strncmp(bus: &Bus, s1: u32, s2: u32, maxlen: Option<u32>) -> u32 {
        match (s1, s2) {
            (0, 0) => return 0,
            (0, _) => return u32::MAX,
            (_, 0) => return 1,
            _ => {}
        }
        for i in 0..maxlen.unwrap_or(MAX_BYTES).min(MAX_BYTES) {
            let a = rd(bus, s1.wrapping_add(i));
            let b = rd(bus, s2.wrapping_add(i));
            if a != b {
                return (i32::from(a as i8) - i32::from(b as i8)) as u32;
            }
            if a == 0 {
                break;
            }
        }
        0
    }

    /// strcpy: copies up to and including the terminator; returns `dst`,
    /// or 0 when either pointer is null.
    pub(super) fn strcpy(bus: &mut Bus, dst: u32, src: u32) -> u32 {
        if dst == 0 || src == 0 {
            return 0;
        }
        for i in 0..MAX_BYTES {
            let b = rd(bus, src.wrapping_add(i));
            let _ = bus.write8_safe(dst.wrapping_add(i), b);
            if b == 0 {
                break;
            }
        }
        dst
    }

    /// strncpy: at most `maxlen` bytes. A source of `maxlen` or more
    /// characters gets no terminator; a shorter one is zero padded.
    pub(super) fn strncpy(bus: &mut Bus, dst: u32, src: u32, maxlen: u32) -> u32 {
        if dst == 0 || src == 0 {
            return 0;
        }
        let limit = maxlen.min(MAX_BYTES);
        let mut i = 0;
        while i < limit {
            let b = rd(bus, src.wrapping_add(i));
            let _ = bus.write8_safe(dst.wrapping_add(i), b);
            i += 1;
            if b == 0 {
                break;
            }
        }
        while i < limit {
            let _ = bus.write8_safe(dst.wrapping_add(i), 0);
            i += 1;
        }
        dst
    }

    /// strcat: appends `src` at the terminator of `dst`; returns `dst`, or
    /// 0 when either pointer is null.
    pub(super) fn strcat(bus: &mut Bus, dst: u32, src: u32) -> u32 {
        if dst == 0 || src == 0 {
            return 0;
        }
        let end = dst.wrapping_add(strlen(bus, dst));
        strcpy(bus, end, src);
        dst
    }

    /// index/strchr (`last` = false) and rindex/strrchr. Returns an
    /// address, never an offset; searching for 0 finds the terminator.
    pub(super) fn strchr(bus: &Bus, src: u32, ch: u8, last: bool) -> u32 {
        if src == 0 {
            return 0;
        }
        let mut found = 0;
        for i in 0..MAX_BYTES {
            let addr = src.wrapping_add(i);
            let b = rd(bus, addr);
            if b == ch {
                found = addr;
                if !last {
                    break;
                }
            }
            if b == 0 {
                break;
            }
        }
        found
    }
}

fn write_byte_to_stdout(byte: u8) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(&[byte]);
    let _ = out.flush();
}

/// A(3Eh)/B(3Fh) puts: a null pointer prints `<NULL>` (psx-spx).
fn write_puts_to_stdout(bus: &mut Bus, addr: u32) {
    if addr == 0 {
        for &b in b"<NULL>" {
            write_byte_to_stdout(b);
        }
        return;
    }
    write_cstring_to_stdout(bus, addr);
}

fn write_cstring_to_stdout(bus: &mut Bus, addr: u32) {
    let mut p = addr;
    // Bound at 4 KiB per call so a bogus pointer can't hang us.
    for _ in 0..4096 {
        let b = bus.try_read8(p).unwrap_or(0);
        if b == 0 {
            break;
        }
        write_byte_to_stdout(b);
        p = p.wrapping_add(1);
    }
}

/// Minimal printf for A(0x3F): %% %c %s %d %i %u %x %X with field width,
/// left alignment, zero padding, and alternate hexadecimal form (for example
/// `%-10s`, `%08x`, and `%#10x`). Register and stack varargs follow o32.
fn hle_printf(bus: &mut Bus, fmt_addr: u32, varargs: &[u32; 3], sp: u32) {
    let mut out: Vec<u8> = Vec::with_capacity(128);
    let mut next_arg = 0usize;
    let mut p = fmt_addr;
    let mut budget = 4096;
    while budget > 0 {
        budget -= 1;
        let b = bus.try_read8(p).unwrap_or(0);
        p = p.wrapping_add(1);
        if b == 0 {
            break;
        }
        if b != b'%' {
            out.push(b);
            continue;
        }
        // Accept flags on either side of the width. The latter is unusual,
        // but JaCzekanski's public access-time test uses `%2-d` and the real
        // BIOS accepts it.
        let mut zero_pad = false;
        let mut left_align = false;
        let mut alternate = false;
        let mut width = 0usize;
        let conv;
        loop {
            let c = bus.try_read8(p).unwrap_or(0);
            p = p.wrapping_add(1);
            match c {
                b'-' => left_align = true,
                b'#' => alternate = true,
                b'0' if width == 0 && !zero_pad => zero_pad = true,
                b'0'..=b'9' => width = width * 10 + (c - b'0') as usize,
                b'l' => {} // longs are 32-bit here; ignore the modifier
                _ => {
                    conv = c;
                    break;
                }
            }
        }
        match conv {
            b'%' => out.push(b'%'),
            b'c' => {
                if let Some(v) = next_printf_arg(bus, varargs, sp, &mut next_arg) {
                    append_padded(&mut out, &[v as u8], width, b' ', left_align);
                }
            }
            b's' => {
                if let Some(v) = next_printf_arg(bus, varargs, sp, &mut next_arg) {
                    let start = out.len();
                    let mut sp = v;
                    for _ in 0..4096 {
                        let sb = bus.try_read8(sp).unwrap_or(0);
                        if sb == 0 {
                            break;
                        }
                        out.push(sb);
                        sp = sp.wrapping_add(1);
                    }
                    pad_existing_field(&mut out, start, width, b' ', left_align);
                }
            }
            b'd' | b'i' => {
                if let Some(v) = next_printf_arg(bus, varargs, sp, &mut next_arg) {
                    let field = format!("{}", v as i32);
                    append_padded(
                        &mut out,
                        field.as_bytes(),
                        width,
                        if zero_pad { b'0' } else { b' ' },
                        left_align,
                    );
                }
            }
            b'u' => {
                if let Some(v) = next_printf_arg(bus, varargs, sp, &mut next_arg) {
                    let field = format!("{v}");
                    append_padded(
                        &mut out,
                        field.as_bytes(),
                        width,
                        if zero_pad { b'0' } else { b' ' },
                        left_align,
                    );
                }
            }
            b'x' | b'X' => {
                if let Some(v) = next_printf_arg(bus, varargs, sp, &mut next_arg) {
                    let digits = if conv == b'x' {
                        format!("{v:x}")
                    } else {
                        format!("{v:X}")
                    };
                    let s = if alternate && v != 0 {
                        format!("{}{}", if conv == b'x' { "0x" } else { "0X" }, digits)
                    } else {
                        digits
                    };
                    append_padded(
                        &mut out,
                        s.as_bytes(),
                        width,
                        if zero_pad { b'0' } else { b' ' },
                        left_align,
                    );
                }
            }
            // Unknown / out-of-register conversion: emit verbatim so the
            // reader at least sees what the guest meant.
            other => {
                out.push(b'%');
                if zero_pad {
                    out.push(b'0');
                }
                if left_align {
                    out.push(b'-');
                }
                if alternate {
                    out.push(b'#');
                }
                if width > 0 {
                    out.extend_from_slice(format!("{width}").as_bytes());
                }
                out.push(other);
            }
        }
    }
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(&out);
    let _ = stdout.flush();
}

fn next_printf_arg(bus: &Bus, registers: &[u32; 3], sp: u32, next: &mut usize) -> Option<u32> {
    let index = *next;
    *next += 1;
    if let Some(value) = registers.get(index) {
        return Some(*value);
    }
    // o32 reserves four argument words at the caller's stack pointer. The
    // fixed format pointer occupies slot 0; a1-a3 occupy slots 1-3, and the
    // fourth vararg begins at slot 4 (sp+16).
    let addr = sp.wrapping_add(16 + ((index - registers.len()) as u32) * 4);
    let bytes = [
        bus.try_read8(addr)?,
        bus.try_read8(addr.wrapping_add(1))?,
        bus.try_read8(addr.wrapping_add(2))?,
        bus.try_read8(addr.wrapping_add(3))?,
    ];
    Some(u32::from_le_bytes(bytes))
}

fn append_padded(out: &mut Vec<u8>, field: &[u8], width: usize, pad: u8, left_align: bool) {
    let padding = width.saturating_sub(field.len());
    if !left_align {
        out.extend(std::iter::repeat_n(pad, padding));
    }
    out.extend_from_slice(field);
    if left_align {
        out.extend(std::iter::repeat_n(pad, padding));
    }
}

fn pad_existing_field(out: &mut Vec<u8>, start: usize, width: usize, pad: u8, left_align: bool) {
    let field_len = out.len().saturating_sub(start);
    let padding = width.saturating_sub(field_len);
    if left_align {
        out.extend(std::iter::repeat_n(pad, padding));
    } else if padding != 0 {
        out.splice(start..start, std::iter::repeat_n(pad, padding));
    }
}

#[cfg(test)]
mod tests {
    use super::{append_padded, dispatch, pad_existing_field, Outcome, Table};
    use crate::Bus;

    const RA: u32 = 0x8001_0100;

    fn hle_bus() -> Bus {
        let mut bus = Bus::new_without_bios();
        bus.enable_hle_bios();
        bus
    }

    fn call(bus: &mut Bus, vector: u32, func: u32, args: [u32; 4]) -> u32 {
        let mut gprs = [0u32; 32];
        gprs[4..8].copy_from_slice(&args);
        gprs[9] = func;
        gprs[29] = 0x801F_FF00;
        gprs[31] = RA;
        dispatch(vector, bus, &mut gprs)
            .expect("BIOS vector dispatch")
            .v0
            .expect("HLE function, not a guest jump")
    }

    fn put_str(bus: &mut Bus, addr: u32, bytes: &[u8]) {
        for (i, &b) in bytes.iter().enumerate() {
            bus.write8_safe(addr + i as u32, b);
        }
    }

    fn get_bytes(bus: &Bus, addr: u32, len: u32) -> Vec<u8> {
        (0..len).map(|i| bus.try_read8(addr + i).unwrap()).collect()
    }

    #[test]
    fn memcpy_and_bcopy_return_their_guarded_pointer() {
        let mut bus = hle_bus();
        put_str(&mut bus, 0x8002_0000, b"abcd");
        assert_eq!(
            call(&mut bus, 0xA0, 0x2A, [0x8003_0000, 0x8002_0000, 4, 0]),
            0x8003_0000
        );
        assert_eq!(get_bytes(&bus, 0x8003_0000, 4), b"abcd");
        // bcopy swaps the operands and returns src.
        assert_eq!(
            call(&mut bus, 0xA0, 0x27, [0x8002_0000, 0x8003_1000, 3, 0]),
            0x8002_0000
        );
        assert_eq!(get_bytes(&bus, 0x8003_1000, 3), b"abc");
        // memcpy refuses dst=0 and huge lengths but still returns dst.
        assert_eq!(call(&mut bus, 0xA0, 0x2A, [0, 0x8002_0000, 4, 0]), 0);
        assert_eq!(
            call(
                &mut bus,
                0xA0,
                0x2A,
                [0x8003_2000, 0x8002_0000, 0x8000_0000, 0]
            ),
            0x8003_2000
        );
        assert_eq!(get_bytes(&bus, 0x8003_2000, 1), [0]);
    }

    #[test]
    fn memset_and_bzero_follow_the_documented_return_values() {
        let mut bus = hle_bus();
        assert_eq!(
            call(&mut bus, 0xA0, 0x2B, [0x8002_0000, 0x5A, 3, 0]),
            0x8002_0000
        );
        assert_eq!(get_bytes(&bus, 0x8002_0000, 4), [0x5A, 0x5A, 0x5A, 0]);
        assert_eq!(call(&mut bus, 0xA0, 0x2B, [0x8002_0000, 0x11, 0, 0]), 0);
        assert_eq!(
            call(&mut bus, 0xA0, 0x28, [0x8002_0000, 2, 0, 0]),
            0x8002_0000
        );
        assert_eq!(get_bytes(&bus, 0x8002_0000, 3), [0, 0, 0x5A]);
    }

    #[test]
    fn malloc_slot_no_longer_writes_memory() {
        let mut bus = hle_bus();
        put_str(&mut bus, 0x8002_0000, b"keep");
        // A(33h) is malloc; it used to run memset over its arguments.
        call(&mut bus, 0xA0, 0x33, [0x8002_0000, 0, 4, 0]);
        assert_eq!(get_bytes(&bus, 0x8002_0000, 4), b"keep");
    }

    #[test]
    fn string_compare_sign_extends_and_handles_null_pointers() {
        let mut bus = hle_bus();
        put_str(&mut bus, 0x8002_0000, b"abc\0");
        put_str(&mut bus, 0x8002_0100, b"abd\0");
        put_str(&mut bus, 0x8002_0200, &[0x80, 0]);
        assert_eq!(
            call(&mut bus, 0xA0, 0x17, [0x8002_0000, 0x8002_0000, 0, 0]),
            0
        );
        assert_eq!(
            call(&mut bus, 0xA0, 0x17, [0x8002_0000, 0x8002_0100, 0, 0]) as i32,
            -1
        );
        assert_eq!(
            call(&mut bus, 0xA0, 0x18, [0x8002_0000, 0x8002_0100, 2, 0]),
            0
        );
        // 0x80 sign-extends to -128, so it sorts below 'a'.
        assert_eq!(
            call(&mut bus, 0xA0, 0x17, [0x8002_0200, 0x8002_0000, 0, 0]) as i32,
            -128 - 0x61
        );
        assert_eq!(call(&mut bus, 0xA0, 0x17, [0, 0, 0, 0]), 0);
        assert_eq!(
            call(&mut bus, 0xA0, 0x17, [0, 0x8002_0000, 0, 0]) as i32,
            -1
        );
        assert_eq!(call(&mut bus, 0xA0, 0x17, [0x8002_0000, 0, 0, 0]), 1);
    }

    #[test]
    fn string_copy_length_and_search() {
        let mut bus = hle_bus();
        put_str(&mut bus, 0x8002_0000, b"hello\0");
        assert_eq!(call(&mut bus, 0xA0, 0x1B, [0x8002_0000, 0, 0, 0]), 5);
        assert_eq!(call(&mut bus, 0xA0, 0x1B, [0, 0, 0, 0]), 0);
        assert_eq!(
            call(&mut bus, 0xA0, 0x19, [0x8003_0000, 0x8002_0000, 0, 0]),
            0x8003_0000
        );
        assert_eq!(get_bytes(&bus, 0x8003_0000, 6), b"hello\0");
        assert_eq!(call(&mut bus, 0xA0, 0x19, [0, 0x8002_0000, 0, 0]), 0);

        // strncpy: short source is zero padded, long source gets no terminator.
        put_str(&mut bus, 0x8003_1000, &[0xEE; 8]);
        call(&mut bus, 0xA0, 0x1A, [0x8003_1000, 0x8002_0000, 7, 0]);
        assert_eq!(get_bytes(&bus, 0x8003_1000, 8), b"hello\0\0\xEE");
        put_str(&mut bus, 0x8003_2000, &[0xEE; 4]);
        call(&mut bus, 0xA0, 0x1A, [0x8003_2000, 0x8002_0000, 3, 0]);
        assert_eq!(get_bytes(&bus, 0x8003_2000, 4), b"hel\xEE");

        put_str(&mut bus, 0x8003_3000, b"ab\0");
        call(&mut bus, 0xA0, 0x15, [0x8003_3000, 0x8002_0000, 0, 0]);
        assert_eq!(get_bytes(&bus, 0x8003_3000, 8), b"abhello\0");

        assert_eq!(
            call(&mut bus, 0xA0, 0x1E, [0x8002_0000, u32::from(b'l'), 0, 0]),
            0x8002_0002
        );
        assert_eq!(
            call(&mut bus, 0xA0, 0x1F, [0x8002_0000, u32::from(b'l'), 0, 0]),
            0x8002_0003
        );
        assert_eq!(
            call(&mut bus, 0xA0, 0x1C, [0x8002_0000, 0, 0, 0]),
            0x8002_0005
        );
        assert_eq!(
            call(&mut bus, 0xA0, 0x1C, [0x8002_0000, u32::from(b'z'), 0, 0]),
            0
        );
        assert_eq!(
            call(&mut bus, 0xA0, 0x2E, [0x8002_0000, u32::from(b'o'), 5, 0]),
            0x8002_0004
        );
        assert_eq!(
            call(&mut bus, 0xA0, 0x2E, [0x8002_0000, u32::from(b'o'), 4, 0]),
            0
        );
    }

    #[test]
    fn set_mem_updates_ram_size_port_and_kernel_variable() {
        let mut bus = hle_bus();
        call(&mut bus, 0xA0, 0x9F, [8, 0, 0, 0]);
        assert_eq!(bus.read32(0x1F80_1060) & 0x300, 0x300);
        assert_eq!(bus.read32(0x60), 8);
        call(&mut bus, 0xA0, 0x9F, [2, 0, 0, 0]);
        assert_eq!(bus.read32(0x1F80_1060) & 0x300, 0);
        assert_eq!(bus.read32(0x60), 2);
        // Anything else is ignored.
        call(&mut bus, 0xA0, 0x9F, [4, 0, 0, 0]);
        assert_eq!(bus.read32(0x60), 2);
    }

    #[test]
    fn abs_and_case_conversion() {
        let mut bus = hle_bus();
        assert_eq!(call(&mut bus, 0xA0, 0x0E, [(-5i32) as u32, 0, 0, 0]), 5);
        assert_eq!(call(&mut bus, 0xA0, 0x0F, [7, 0, 0, 0]), 7);
        assert_eq!(
            call(&mut bus, 0xA0, 0x25, [u32::from(b'q'), 0, 0, 0]),
            u32::from(b'Q')
        );
        assert_eq!(
            call(&mut bus, 0xA0, 0x26, [0x141, 0, 0, 0]),
            u32::from(b'a')
        );
    }

    #[test]
    fn printf_field_padding_handles_both_alignments() {
        let mut out = Vec::new();
        append_padded(&mut out, b"RAM", 5, b' ', true);
        append_padded(&mut out, b"7", 3, b'0', false);
        assert_eq!(out, b"RAM  007");

        let start = out.len();
        out.extend_from_slice(b"BIOS");
        pad_existing_field(&mut out, start, 6, b' ', false);
        assert_eq!(&out[start..], b"  BIOS");
    }

    #[test]
    fn unimplemented_and_stubbed_calls_are_recorded_once_per_function() {
        let mut bus = hle_bus();
        assert!(bus.hle_bios_first_unimplemented().is_none());
        // B(0Bh) TestEvent is a stub; A(43h) exec is unimplemented.
        assert_eq!(call(&mut bus, 0xB0, 0x0B, [1, 0, 0, 0]), 1);
        assert_eq!(call(&mut bus, 0xA0, 0x43, [0x40, 0, 0, 0]), 0);
        assert_eq!(call(&mut bus, 0xA0, 0x43, [0x80, 0, 0, 0]), 0);
        // Implemented calls leave no record.
        call(&mut bus, 0xA0, 0x1B, [0, 0, 0, 0]);

        let records = bus.hle_bios_records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].outcome, Outcome::Stub);
        assert_eq!(records[0].name, "testEvent");
        let first = bus.hle_bios_first_unimplemented().unwrap();
        assert_eq!((first.table, first.func), (Table::A, 0x43));
        assert_eq!(first.args[0], 0x40);
        assert_eq!(first.ra, RA);
        assert_eq!(first.count, 2);
        assert_eq!(
            first.to_string().split(" a1=").next(),
            Some("A(43h) exec a0=0x00000040")
        );
    }

    #[test]
    fn hook_entry_int_tracks_and_resets_guest_jump_buffer() {
        let mut bus = hle_bus();
        let hook = 0x8001_4000;
        call(&mut bus, 0xB0, 0x19, [hook, 0, 0, 0]);
        assert_eq!(bus.hle_irq_jump_buffer(), Some(hook));
        call(&mut bus, 0xB0, 0x18, [0; 4]);
        assert_eq!(bus.hle_irq_jump_buffer(), None);
    }

    #[test]
    fn get_table_calls_return_the_retail_table_addresses_and_report_patches() {
        let mut bus = hle_bus();
        // Code after the call that matches no known patch routine.
        for i in 0..16u32 {
            crate::hle_kernel::poke32(&mut bus, RA + 4 * i, 0x2400_0000 | i);
        }
        assert_eq!(call(&mut bus, 0xB0, 0x56, [0; 4]), 0x674);
        assert_eq!(call(&mut bus, 0xB0, 0x57, [0; 4]), 0x874);
        assert_eq!(bus.hle_bios_patches().len(), 2);
        assert!(bus.hle_bios_patches()[0].0.starts_with("unknown:"));
    }

    #[test]
    fn user_heap_calls_and_conf() {
        let mut bus = hle_bus();
        assert_eq!(call(&mut bus, 0xA0, 0x33, [16, 0, 0, 0]), 0, "no heap yet");
        call(&mut bus, 0xA0, 0x39, [0x8010_0000, 0x1000, 0, 0]);
        let p = call(&mut bus, 0xA0, 0x33, [16, 0, 0, 0]);
        assert_eq!(p, 0x8010_0004);
        call(
            &mut bus,
            0xA0,
            0x9D,
            [0x8002_0000, 0x8002_0004, 0x8002_0008, 0],
        );
        let conf: Vec<u32> = (0..3)
            .map(|i| crate::hle_kernel::peek32(&bus, 0x8002_0000 + 4 * i))
            .collect();
        assert_eq!(conf, [0x10, 4, 0x801F_FF00]);
        // B(5Bh) returns the previous auto-ack setting.
        assert_eq!(call(&mut bus, 0xB0, 0x5B, [0, 0, 0, 0]), 1);
        assert_eq!(call(&mut bus, 0xB0, 0x5B, [1, 0, 0, 0]), 0);
        // B(00h) allocates from the kernel heap set up at boot.
        let k = call(&mut bus, 0xB0, 0x00, [8, 0, 0, 0]);
        assert!((0xA000_E000..0xA001_0000).contains(&k), "{k:#x}");
    }
}
