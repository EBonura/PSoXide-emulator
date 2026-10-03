// SPDX-License-Identifier: GPL-2.0-or-later
//! Psy-Q kernel patches, the way the HLE kernel lets them happen.
//!
//! psx-spx "BIOS Patches" lists the routines games run right after
//! B(56h) GetC0Table or B(57h) GetB0Table: they read C(06h) or B(5Bh) out of
//! the returned table and write code or data at fixed offsets from it, or
//! take the address of a kernel function at a fixed offset.
//!
//! The HLE keeps both entries at their retail addresses with the documented
//! room behind them (see the [`crate::hle_kernel`] layout), so a routine can
//! run exactly as the game wrote it. Nothing in the game is rewritten and no
//! routine has to be recognised for the game to work:
//!
//! * C(06h) is guest code. Its prologue, the four call slots at +70h and
//!   the early card routine those slots call keep the layout the patches
//!   expect, so their writes have the documented effect on real code.
//! * B(5Bh) is a host function, and the pad and card drivers behind it are
//!   host code. Each place psx-spx documents behind it holds a marker word
//!   ([`INTACT`]) until a game overwrites it, and the drivers ask
//!   [`pad_error_reselect`], [`pad_vblank_ack`], [`pad_output_clipped`] and
//!   [`card_info_extra_byte`] instead of keeping flags. The function
//!   addresses games take from the zone hold trap words for the matching
//!   kernel functions.
//!
//! [`identify`] decodes the routine after a call for diagnostics and for the
//! disc scanner. It names routines by what they touch, with psx-spx's
//! section names; it never decides what the kernel does.

use crate::hle_kernel::{internal, peek32, poke32, trap_word, PAD_CARD_ENTRY};
use crate::Bus;

/// Offsets from B(5Bh) that psx-spx's patch descriptions write to or take.
pub mod site {
    /// Start of the pad output byte handling; overwritten by
    /// "patch_optional_pad_output" to stop the 00h/01h clipping.
    pub const PAD_OUTPUT_CLIP: u32 = 0x3D8;
    /// Second copy of the pad output handling, also overwritten by that
    /// patch.
    pub const PAD_OUTPUT_CLIP_2: u32 = 0x4DC;
    /// Pad error handling (select the other slot, then delay); zeroed by
    /// "patch_pad_error_handling_and_get_pad_enable_functions".
    pub const PAD_ERROR_RESELECT: u32 = 0x594;
    /// VBlank acknowledge in the pad/card handler; zeroed by
    /// "patch_no_pad_card_auto_ack".
    pub const PAD_VBLANK_ACK: u32 = 0x62C;
    /// SetPadOutput(src1, blah1, src2, blah2), taken by
    /// "patch_optional_pad_output".
    pub const PAD_OUTPUT_FN: u32 = 0x7A0;
    /// SetPadEnableFlag(), taken by the pad error handling patch.
    pub const PAD_ENABLE_FN: u32 = 0x884;
    /// ClearPadEnableFlag(), taken by the pad error handling patch.
    pub const PAD_DISABLE_FN: u32 = 0x894;
    /// Card command/status byte handling; overwritten by
    /// "patch_card_specific_delay" with a call to the game's delay loop.
    pub const CARD_IRQ_DELAY: u32 = 0x9C8;
    /// The opcode that sends one byte too many in card_info; zeroed by
    /// "patch_card_info_step4".
    pub const CARD_INFO_EXTRA_BYTE: u32 = 0x1988;
}

/// What an untouched site holds: `addiu zero, zero, 0`. It is not zero, so
/// the patches that clear words are visible, and it does nothing if a game
/// ever jumps to it.
pub const INTACT: u32 = 0x2400_0000;

/// Data sites, filled with [`INTACT`] by [`install`].
const DATA_SITES: [u32; 6] = [
    site::PAD_OUTPUT_CLIP,
    site::PAD_OUTPUT_CLIP_2,
    site::PAD_ERROR_RESELECT,
    site::PAD_VBLANK_ACK,
    site::CARD_IRQ_DELAY,
    site::CARD_INFO_EXTRA_BYTE,
];

/// Fill the B(5Bh) zone's documented sites. Called by
/// [`crate::hle_kernel::install`] after the zone is cleared.
pub fn install(bus: &mut Bus) {
    for offset in DATA_SITES {
        poke32(bus, PAD_CARD_ENTRY + offset, INTACT);
    }
    for (offset, func) in [
        (site::PAD_OUTPUT_FN, internal::SET_PAD_OUTPUT_DATA),
        (site::PAD_ENABLE_FN, internal::START_PAD),
        (site::PAD_DISABLE_FN, internal::STOP_PAD),
    ] {
        poke32(bus, PAD_CARD_ENTRY + offset, trap_word(3, func));
    }
}

fn intact(bus: &Bus, offset: u32) -> bool {
    peek32(bus, PAD_CARD_ENTRY + offset) == INTACT
}

/// Whether a failed pad transfer still selects the other slot for a moment
/// (true until a game clears that handling).
pub fn pad_error_reselect(bus: &Bus) -> bool {
    intact(bus, site::PAD_ERROR_RESELECT)
}

/// Whether the pad/card handler may acknowledge VBlank when auto-ack is on
/// (true until a game clears that code).
pub fn pad_vblank_ack(bus: &Bus) -> bool {
    intact(bus, site::PAD_VBLANK_ACK)
}

/// Whether bytes sent to a controller are clipped to 00h/01h (true until a
/// game replaces that code).
pub fn pad_output_clipped(bus: &Bus) -> bool {
    intact(bus, site::PAD_OUTPUT_CLIP)
}

/// Whether card_info sends the extra byte after the last one (true until a
/// game clears that opcode).
pub fn card_info_extra_byte(bus: &Bus) -> bool {
    intact(bus, site::CARD_INFO_EXTRA_BYTE)
}

// ---------------------------------------------------------------- identify

/// The table a routine received.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Got {
    /// B(56h) GetC0Table.
    C0,
    /// B(57h) GetB0Table.
    B0,
}

impl Got {
    /// The table a B-function number hands out, if it is one of the two.
    pub fn from_function(func: u32) -> Option<Self> {
        match func {
            0x56 => Some(Got::C0),
            0x57 => Some(Got::B0),
            _ => None,
        }
    }
}

/// A patch routine, named after its psx-spx section.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Routine {
    /// Rewrites the exception handler's first 70h bytes (the cause read
    /// that old kernels missed).
    ExceptionPrologue,
    /// Reads the call in slot 1 to find the early card routine and patches
    /// it to wait for the card.
    EarlyCardWait,
    /// Overwrites slot 1 (the early card call).
    EarlyCardUninstall,
    /// Writes a call into slot 2 at +80h.
    LightgunHandler,
    /// Overwrites the card command/status byte handling.
    CardDelay,
    /// Clears card_info's extra byte.
    CardInfoStep4,
    /// Clears the pad error handling, or takes the pad enable functions.
    PadErrorHandling,
    /// Unclips pad output, or takes SetPadOutput.
    PadOutput,
    /// Clears the pad/card handler's VBlank acknowledge.
    PadNoAutoAck,
}

impl Routine {
    /// The psx-spx section name.
    pub fn name(self) -> &'static str {
        match self {
            Routine::ExceptionPrologue => "patch_missing_cop0r13_in_exception_handler",
            Routine::EarlyCardWait => "early_card_irq_patch",
            Routine::EarlyCardUninstall => "patch_uninstall_early_card_irq_handler",
            Routine::LightgunHandler => "patch_install_lightgun_irq_handler",
            Routine::CardDelay => "patch_card_specific_delay",
            Routine::CardInfoStep4 => "patch_card_info_step4",
            Routine::PadErrorHandling => "patch_pad_error_handling_and_get_pad_enable_functions",
            Routine::PadOutput => "patch_optional_pad_output",
            Routine::PadNoAutoAck => "patch_no_pad_card_auto_ack",
        }
    }
}

/// C(06h) and B(5Bh) table indices.
const C0_EXCEPTION: u32 = 0x06;
const B0_CLEAR_PAD: u32 = 0x5B;

/// What decoding knows about a register.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Val {
    Unknown,
    Const(u32),
    /// The table base plus a byte offset.
    Table(u32),
    /// Table entry `index` plus a byte offset.
    Entry(u32, u32),
}

impl Val {
    fn add(self, n: u32) -> Val {
        match self {
            Val::Const(c) => Val::Const(c.wrapping_add(n)),
            Val::Table(o) => Val::Table(o.wrapping_add(n)),
            Val::Entry(i, o) => Val::Entry(i, o.wrapping_add(n)),
            Val::Unknown => Val::Unknown,
        }
    }

    fn derived(self) -> bool {
        matches!(self, Val::Table(_) | Val::Entry(..))
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Access {
    Load,
    Store,
    /// The entry-relative address was stored somewhere else.
    Take,
}

/// Longest stretch decoded after the call.
pub const WINDOW: usize = 64;

/// Decode the code after a B(56h)/B(57h) call (`words[0]` is the word at
/// the return address) and name the patch routines in it, in the order
/// their first access appears. Empty when nothing documented is touched.
///
/// The decode tracks the returned table through registers, follows loops
/// once in program order (so a pointer bumped before a delay-slot store is
/// accounted for), and ends at the first jump or call (after its delay
/// slot), when no register holds a table-derived value any more, or after
/// [`WINDOW`] words.
pub fn identify(got: Got, words: &[u32]) -> Vec<Routine> {
    let mut regs = [Val::Unknown; 32];
    regs[0] = Val::Const(0);
    regs[2] = Val::Table(0);
    let mut seen: Vec<(u32, u32, Access)> = Vec::new();
    let mut end_after: Option<usize> = None;
    for (n, &w) in words.iter().take(WINDOW).enumerate() {
        step(w, &mut regs, &mut seen, &mut end_after, n);
        regs[0] = Val::Const(0);
        if end_after == Some(n) || !regs.iter().any(|v| v.derived()) {
            break;
        }
    }
    let mut out = Vec::new();
    for (index, offset, access) in seen {
        if let Some(routine) = classify(got, index, offset, access) {
            if !out.contains(&routine) {
                out.push(routine);
            }
        }
    }
    out
}

fn classify(got: Got, index: u32, offset: u32, access: Access) -> Option<Routine> {
    match (got, index) {
        (Got::C0, C0_EXCEPTION) => match (access, offset) {
            (Access::Store, 0x00..=0x6F) => Some(Routine::ExceptionPrologue),
            (Access::Load, 0x70..=0x77) => Some(Routine::EarlyCardWait),
            (Access::Store, 0x70..=0x7F) => Some(Routine::EarlyCardUninstall),
            (Access::Store, 0x80..=0x8F) => Some(Routine::LightgunHandler),
            _ => None,
        },
        (Got::B0, B0_CLEAR_PAD) => match (access, offset) {
            (Access::Store, 0x9C0..=0x9DF) => Some(Routine::CardDelay),
            (Access::Store, site::CARD_INFO_EXTRA_BYTE) => Some(Routine::CardInfoStep4),
            (Access::Store, 0x594..=0x5BF) => Some(Routine::PadErrorHandling),
            (Access::Take, site::PAD_ENABLE_FN | site::PAD_DISABLE_FN) => {
                Some(Routine::PadErrorHandling)
            }
            (Access::Store, 0x3D8..=0x3E7 | 0x4DC..=0x4EF) => Some(Routine::PadOutput),
            (Access::Take, site::PAD_OUTPUT_FN) => Some(Routine::PadOutput),
            (Access::Store, 0x62C..=0x64F) => Some(Routine::PadNoAutoAck),
            _ => None,
        },
        _ => None,
    }
}

fn step(
    w: u32,
    regs: &mut [Val; 32],
    seen: &mut Vec<(u32, u32, Access)>,
    end_after: &mut Option<usize>,
    n: usize,
) {
    let op = w >> 26;
    let rs = ((w >> 21) & 31) as usize;
    let rt = ((w >> 16) & 31) as usize;
    let rd = ((w >> 11) & 31) as usize;
    let simm = w as u16 as i16 as i32 as u32;
    let zimm = w & 0xFFFF;
    match op {
        0x00 => match w & 0x3F {
            // addu/add: register plus a known constant keeps the tracking.
            0x20 | 0x21 => {
                regs[rd] = match (regs[rs], regs[rt]) {
                    (v, Val::Const(c)) | (Val::Const(c), v) => v.add(c),
                    _ => Val::Unknown,
                }
            }
            // or with zero is a move.
            0x25 => {
                regs[rd] = match (regs[rs], regs[rt]) {
                    (v, Val::Const(0)) | (Val::Const(0), v) => v,
                    _ => Val::Unknown,
                }
            }
            // jr/jalr end the routine after the delay slot.
            0x08 | 0x09 => {
                if w & 0x3F == 0x09 {
                    regs[rd] = Val::Unknown;
                }
                *end_after = Some(n + 1);
            }
            // Shifts, logic, multiply results: no longer an address.
            0x00..=0x07 | 0x10 | 0x12 | 0x22..=0x2B => regs[rd] = Val::Unknown,
            _ => {}
        },
        // j/jal.
        0x02 | 0x03 => {
            if op == 0x03 {
                regs[31] = Val::Unknown;
            }
            *end_after = Some(n + 1);
        }
        // addi/addiu.
        0x08 | 0x09 => regs[rt] = regs[rs].add(simm),
        // ori on a constant (lui/ori pairs).
        0x0D => {
            regs[rt] = match regs[rs] {
                Val::Const(c) => Val::Const(c | zimm),
                _ => Val::Unknown,
            }
        }
        // lui.
        0x0F => regs[rt] = Val::Const(zimm << 16),
        // Other immediate ALU ops.
        0x0A..=0x0C | 0x0E => regs[rt] = Val::Unknown,
        // Loads.
        0x20..=0x26 => {
            let value = match regs[rs].add(simm) {
                Val::Table(offset) if op == 0x23 && offset % 4 == 0 => Val::Entry(offset / 4, 0),
                Val::Entry(index, offset) => {
                    seen.push((index, offset, Access::Load));
                    Val::Unknown
                }
                _ => Val::Unknown,
            };
            regs[rt] = value;
        }
        // Stores.
        0x28 | 0x29 | 0x2B => match regs[rs].add(simm) {
            Val::Entry(index, offset) => seen.push((index, offset, Access::Store)),
            _ => {
                if let Val::Entry(index, offset) = regs[rt] {
                    seen.push((index, offset, Access::Take));
                }
            }
        },
        // COP0/COP2 moves to a GPR.
        0x10 | 0x12 if (w >> 21) & 0x1F == 0 => regs[rt] = Val::Unknown,
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hle_asm::*;

    // Every routine below is written for these tests: same effect as the
    // psx-spx descriptions, different instructions.

    fn ends_with_call(a: &mut Asm) {
        a.jal_abs(0x8001_2340);
        a.nop();
    }

    #[test]
    fn exception_prologue_copy_is_recognised() {
        let mut a = Asm::new(0x8003_0000);
        a.lw(V0, 0x18, V0); // C(06h)
        a.lui(T3, 0x8004);
        a.ori(T3, T3, 0x1000);
        a.addiu(T2, T3, 0x38);
        a.label("copy");
        a.lw(A0, 0, T3);
        a.addiu(T3, T3, 4);
        a.sw(A0, 0, V0);
        a.bne(T3, T2, "copy");
        a.addiu(V0, V0, 4);
        ends_with_call(&mut a);
        assert_eq!(
            identify(Got::C0, &a.finish()),
            vec![Routine::ExceptionPrologue]
        );
    }

    #[test]
    fn verify_then_copy_at_plus_28h_is_the_prologue_patch() {
        let mut a = Asm::new(0x8003_0000);
        a.lw(V0, 0x18, V0);
        a.nop();
        a.addiu(V0, V0, 0x28);
        a.mov(S1, V0);
        a.label("verify");
        a.lw(T3, 0, V0);
        a.bne(T3, ZERO, "out");
        a.addiu(V0, V0, 4);
        a.mov(V0, S1);
        a.sw(ZERO, 0, V0);
        a.label("out");
        ends_with_call(&mut a);
        assert_eq!(
            identify(Got::C0, &a.finish()),
            vec![Routine::ExceptionPrologue]
        );
    }

    #[test]
    fn reading_slot_one_finds_the_early_card_routine() {
        let mut a = Asm::new(0x8003_0000);
        a.lw(V0, 0x18, V0);
        a.nop();
        a.lw(V1, 0x70, V0);
        a.lw(A1, 0x74, V0);
        a.andi(V1, V1, 0xFFFF);
        a.sll(V1, V1, 16);
        a.andi(A1, A1, 0xFFFF);
        a.addu(V1, V1, A1);
        a.addiu(V0, V1, 0x28);
        ends_with_call(&mut a);
        assert_eq!(identify(Got::C0, &a.finish()), vec![Routine::EarlyCardWait]);
    }

    #[test]
    fn slot_writes_tell_uninstall_from_lightgun() {
        for (slot, want) in [
            (0x70, Routine::EarlyCardUninstall),
            (0x80, Routine::LightgunHandler),
        ] {
            let mut a = Asm::new(0x8003_0000);
            a.lw(T0, 0x18, V0);
            a.li(A2, 3);
            a.label("fill");
            a.sw(ZERO, slot, T0);
            a.addiu(A2, A2, -1);
            a.bnez(A2, "fill");
            a.addiu(T0, T0, 4);
            ends_with_call(&mut a);
            assert_eq!(identify(Got::C0, &a.finish()), vec![want]);
        }
    }

    #[test]
    fn b5bh_sites_name_their_routines() {
        let cases: [(i16, Routine); 5] = [
            (0x9C8, Routine::CardDelay),
            (0x594, Routine::PadErrorHandling),
            (0x3D8, Routine::PadOutput),
            (0x62C, Routine::PadNoAutoAck),
            (0x1988, Routine::CardInfoStep4),
        ];
        for (offset, want) in cases {
            let mut a = Asm::new(0x8003_0000);
            a.lw(V0, 0x16C, V0); // B(5Bh)
            a.nop();
            a.addiu(A3, V0, offset);
            a.jal_abs(0x8001_2340);
            a.sw(ZERO, 0, A3);
            assert_eq!(identify(Got::B0, &a.finish()), vec![want], "{offset:#x}");
        }
    }

    #[test]
    fn taking_kernel_function_addresses_is_recognised() {
        let mut a = Asm::new(0x8003_0000);
        a.lw(V0, 0x16C, V0);
        a.lui(AT, 0x8005);
        a.addiu(V1, V0, 0x7A0);
        a.sw(V1, 0x10, AT);
        ends_with_call(&mut a);
        assert_eq!(identify(Got::B0, &a.finish()), vec![Routine::PadOutput]);

        let mut a = Asm::new(0x8003_0000);
        a.lw(V0, 0x16C, V0);
        a.addiu(T0, V0, 0x884);
        a.lui(AT, 0x8005);
        a.sw(T0, 0x20, AT);
        ends_with_call(&mut a);
        assert_eq!(
            identify(Got::B0, &a.finish()),
            vec![Routine::PadErrorHandling]
        );
    }

    #[test]
    fn unrelated_code_and_the_wrong_table_are_not_patches() {
        let words: Vec<u32> = (0..16).map(|i| 0x2400_0000 | i).collect();
        assert!(identify(Got::B0, &words).is_empty());
        let mut a = Asm::new(0x8003_0000);
        a.lw(V0, 0x16C, V0);
        a.sw(ZERO, 0x594, V0);
        ends_with_call(&mut a);
        assert!(identify(Got::C0, &a.finish()).is_empty());
    }

    #[test]
    fn decoding_stops_at_the_first_call() {
        let mut a = Asm::new(0x8003_0000);
        a.lw(V0, 0x16C, V0);
        a.jal_abs(0x8001_2340);
        a.nop();
        a.sw(ZERO, 0x62C, V0);
        assert!(identify(Got::B0, &a.finish()).is_empty());
    }

    #[test]
    fn games_see_the_sites_and_the_drivers_see_their_writes() {
        let mut bus = Bus::new_without_bios();
        bus.enable_hle_bios();
        assert!(pad_error_reselect(&bus));
        assert!(pad_vblank_ack(&bus));
        assert!(pad_output_clipped(&bus));
        assert!(card_info_extra_byte(&bus));
        let entry = peek32(&bus, crate::hle_kernel::B0_TABLE + 4 * 0x5B);
        assert_eq!(entry, PAD_CARD_ENTRY);
        assert_eq!(
            crate::hle_kernel::decode_trap(peek32(&bus, entry + site::PAD_ENABLE_FN)),
            Some((3, internal::START_PAD))
        );
        assert_eq!(
            crate::hle_kernel::decode_trap(peek32(&bus, entry + site::PAD_OUTPUT_FN)),
            Some((3, internal::SET_PAD_OUTPUT_DATA))
        );
        for offset in [
            site::PAD_ERROR_RESELECT,
            site::PAD_VBLANK_ACK,
            site::PAD_OUTPUT_CLIP,
            site::CARD_INFO_EXTRA_BYTE,
        ] {
            poke32(&mut bus, entry + offset, 0);
        }
        assert!(!pad_error_reselect(&bus));
        assert!(!pad_vblank_ack(&bus));
        assert!(!pad_output_clipped(&bus));
        assert!(!card_info_extra_byte(&bus));
    }
}
