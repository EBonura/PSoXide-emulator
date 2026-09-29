//! A minimal AArch64 instruction encoder: only the forms the block
//! compiler emits. Registers are plain numbers; 31 means WZR/XZR for the
//! data-processing forms used here and SP for the load/store pair forms.

/// Condition codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Cond {
    /// Equal.
    Eq = 0,
    /// Not equal.
    Ne = 1,
    /// Unsigned higher or same.
    Hs = 2,
    /// Unsigned lower.
    Lo = 3,
    /// Unsigned higher.
    Hi = 8,
    /// Signed greater or equal.
    Ge = 10,
    /// Signed less than.
    Lt = 11,
    /// Signed greater than.
    Gt = 12,
    /// Signed less or equal.
    Le = 13,
}

impl Cond {
    fn invert(self) -> u32 {
        (self as u32) ^ 1
    }
}

/// A forward branch waiting for its target.
#[derive(Clone, Copy, Debug)]
pub struct Fixup {
    at: usize,
    kind: FixupKind,
}

#[derive(Clone, Copy, Debug)]
enum FixupKind {
    /// CBZ/CBNZ/B.cond: imm19 at bits 5..24.
    Imm19,
    /// B: imm26.
    Imm26,
}

/// Instruction buffer.
#[derive(Default)]
pub struct Asm {
    /// Encoded words.
    pub code: Vec<u32>,
}

/// Zero register / stack pointer number.
pub const ZR: u8 = 31;
/// Stack pointer (pair forms only).
pub const SP: u8 = 31;

impl Asm {
    fn emit(&mut self, word: u32) {
        self.code.push(word);
    }

    /// Current position in instructions.
    pub fn pos(&self) -> usize {
        self.code.len()
    }

    /// `MOVZ wd, #imm16, LSL #shift`.
    pub fn movz_w(&mut self, rd: u8, imm16: u16, shift: u32) {
        self.emit(0x5280_0000 | (shift / 16) << 21 | (imm16 as u32) << 5 | rd as u32);
    }

    /// `MOVK wd, #imm16, LSL #shift`.
    pub fn movk_w(&mut self, rd: u8, imm16: u16, shift: u32) {
        self.emit(0x7280_0000 | (shift / 16) << 21 | (imm16 as u32) << 5 | rd as u32);
    }

    /// `MOVN wd, #imm16`.
    pub fn movn_w(&mut self, rd: u8, imm16: u16) {
        self.emit(0x1280_0000 | (imm16 as u32) << 5 | rd as u32);
    }

    /// Load a 32-bit constant into `wd` in one or two instructions.
    pub fn mov32(&mut self, rd: u8, value: u32) {
        if value >> 16 == 0 {
            self.movz_w(rd, value as u16, 0);
        } else if (!value) >> 16 == 0 {
            self.movn_w(rd, !value as u16);
        } else {
            self.movz_w(rd, value as u16, 0);
            self.movk_w(rd, (value >> 16) as u16, 16);
        }
    }

    /// Load a 64-bit constant into `xd`.
    pub fn mov64(&mut self, rd: u8, value: u64) {
        self.emit(0xD280_0000 | ((value & 0xFFFF) as u32) << 5 | rd as u32);
        for shift in [16u32, 32, 48] {
            let part = ((value >> shift) & 0xFFFF) as u32;
            if part != 0 {
                self.emit(0xF280_0000 | (shift / 16) << 21 | part << 5 | rd as u32);
            }
        }
    }

    /// `MOV xd, xm`.
    pub fn mov_x(&mut self, rd: u8, rm: u8) {
        self.emit(0xAA00_0000 | (rm as u32) << 16 | (ZR as u32) << 5 | rd as u32);
    }

    /// `MOV wd, wm`.
    pub fn mov_w(&mut self, rd: u8, rm: u8) {
        self.emit(0x2A00_0000 | (rm as u32) << 16 | (ZR as u32) << 5 | rd as u32);
    }

    fn rrr(&mut self, base: u32, rd: u8, rn: u8, rm: u8) {
        self.emit(base | (rm as u32) << 16 | (rn as u32) << 5 | rd as u32);
    }

    /// `ADD wd, wn, wm`.
    pub fn add_w(&mut self, rd: u8, rn: u8, rm: u8) {
        self.rrr(0x0B00_0000, rd, rn, rm);
    }
    /// `SUB wd, wn, wm`.
    pub fn sub_w(&mut self, rd: u8, rn: u8, rm: u8) {
        self.rrr(0x4B00_0000, rd, rn, rm);
    }
    /// `AND wd, wn, wm`.
    pub fn and_w(&mut self, rd: u8, rn: u8, rm: u8) {
        self.rrr(0x0A00_0000, rd, rn, rm);
    }
    /// `ORR wd, wn, wm`.
    pub fn orr_w(&mut self, rd: u8, rn: u8, rm: u8) {
        self.rrr(0x2A00_0000, rd, rn, rm);
    }
    /// `ORN wd, wn, wm` (`wn | !wm`).
    pub fn orn_w(&mut self, rd: u8, rn: u8, rm: u8) {
        self.rrr(0x2A20_0000, rd, rn, rm);
    }
    /// `EOR wd, wn, wm`.
    pub fn eor_w(&mut self, rd: u8, rn: u8, rm: u8) {
        self.rrr(0x4A00_0000, rd, rn, rm);
    }
    /// `LSLV wd, wn, wm`.
    pub fn lslv_w(&mut self, rd: u8, rn: u8, rm: u8) {
        self.rrr(0x1AC0_2000, rd, rn, rm);
    }
    /// `LSRV wd, wn, wm`.
    pub fn lsrv_w(&mut self, rd: u8, rn: u8, rm: u8) {
        self.rrr(0x1AC0_2400, rd, rn, rm);
    }
    /// `ASRV wd, wn, wm`.
    pub fn asrv_w(&mut self, rd: u8, rn: u8, rm: u8) {
        self.rrr(0x1AC0_2800, rd, rn, rm);
    }
    /// `CMP wn, wm`.
    pub fn cmp_w(&mut self, rn: u8, rm: u8) {
        self.rrr(0x6B00_0000, ZR, rn, rm);
    }
    /// `ADD wd, wn, #imm12`.
    pub fn add_w_imm(&mut self, rd: u8, rn: u8, imm12: u32) {
        debug_assert!(imm12 < 4096);
        self.emit(0x1100_0000 | imm12 << 10 | (rn as u32) << 5 | rd as u32);
    }
    /// `CMP wn, #imm12`.
    pub fn cmp_w_imm(&mut self, rn: u8, imm12: u32) {
        debug_assert!(imm12 < 4096);
        self.emit(0x7100_0000 | imm12 << 10 | (rn as u32) << 5 | ZR as u32);
    }
    /// `CSET wd, cond`.
    pub fn cset_w(&mut self, rd: u8, cond: Cond) {
        self.emit(0x1A9F_07E0 | cond.invert() << 12 | rd as u32);
    }

    /// `LSL wd, wn, #sh` (0 < sh < 32).
    pub fn lsl_w_imm(&mut self, rd: u8, rn: u8, sh: u32) {
        let immr = (32 - sh) & 31;
        let imms = 31 - sh;
        self.emit(0x5300_0000 | immr << 16 | imms << 10 | (rn as u32) << 5 | rd as u32);
    }
    /// `LSR wd, wn, #sh`.
    pub fn lsr_w_imm(&mut self, rd: u8, rn: u8, sh: u32) {
        self.emit(0x5300_0000 | sh << 16 | 31 << 10 | (rn as u32) << 5 | rd as u32);
    }
    /// `ASR wd, wn, #sh`.
    pub fn asr_w_imm(&mut self, rd: u8, rn: u8, sh: u32) {
        self.emit(0x1300_0000 | sh << 16 | 31 << 10 | (rn as u32) << 5 | rd as u32);
    }

    /// `LDR wt, [xn, #off]` (off a multiple of 4 below 16 KiB).
    pub fn ldr_w(&mut self, rt: u8, rn: u8, off: u32) {
        debug_assert!(off.is_multiple_of(4) && off < 16384);
        self.emit(0xB940_0000 | (off / 4) << 10 | (rn as u32) << 5 | rt as u32);
    }
    /// `STR wt, [xn, #off]`.
    pub fn str_w(&mut self, rt: u8, rn: u8, off: u32) {
        debug_assert!(off.is_multiple_of(4) && off < 16384);
        self.emit(0xB900_0000 | (off / 4) << 10 | (rn as u32) << 5 | rt as u32);
    }
    /// `LDR xt, [xn, #off]` (off a multiple of 8 below 32 KiB).
    pub fn ldr_x(&mut self, rt: u8, rn: u8, off: u32) {
        debug_assert!(off.is_multiple_of(8) && off < 32768);
        self.emit(0xF940_0000 | (off / 8) << 10 | (rn as u32) << 5 | rt as u32);
    }
    /// `STR xt, [xn, #off]`.
    pub fn str_x(&mut self, rt: u8, rn: u8, off: u32) {
        debug_assert!(off.is_multiple_of(8) && off < 32768);
        self.emit(0xF900_0000 | (off / 8) << 10 | (rn as u32) << 5 | rt as u32);
    }
    /// `ADD xd, xn, xm`.
    pub fn add_x(&mut self, rd: u8, rn: u8, rm: u8) {
        self.rrr(0x8B00_0000, rd, rn, rm);
    }
    /// `ADD xd, xn, #imm12`.
    pub fn add_x_imm(&mut self, rd: u8, rn: u8, imm12: u32) {
        debug_assert!(imm12 < 4096);
        self.emit(0x9100_0000 | imm12 << 10 | (rn as u32) << 5 | rd as u32);
    }
    /// `CMP xn, xm`.
    pub fn cmp_x(&mut self, rn: u8, rm: u8) {
        self.rrr(0xEB00_0000, ZR, rn, rm);
    }

    /// `LDRB wt, [xn]`.
    pub fn ldrb_w(&mut self, rt: u8, rn: u8) {
        self.emit(0x3940_0000 | (rn as u32) << 5 | rt as u32);
    }

    /// `LDRH wt, [xn]`.
    pub fn ldrh_w(&mut self, rt: u8, rn: u8) {
        self.emit(0x7940_0000 | (rn as u32) << 5 | rt as u32);
    }

    /// `LDR wt, [xn, xm, LSL #2]`.
    pub fn ldr_w_idx4(&mut self, rt: u8, rn: u8, rm: u8) {
        self.emit(0xB860_7800 | (rm as u32) << 16 | (rn as u32) << 5 | rt as u32);
    }

    /// `STR wt, [xn, xm, LSL #2]`.
    pub fn str_w_idx4(&mut self, rt: u8, rn: u8, rm: u8) {
        self.emit(0xB820_7800 | (rm as u32) << 16 | (rn as u32) << 5 | rt as u32);
    }

    /// `STP xt1, xt2, [sp, #off]!` (pre-index, off a multiple of 8).
    pub fn stp_x_pre(&mut self, rt1: u8, rt2: u8, off: i32) {
        let imm7 = ((off / 8) as u32) & 0x7F;
        self.emit(0xA980_0000 | imm7 << 15 | (rt2 as u32) << 10 | (SP as u32) << 5 | rt1 as u32);
    }
    /// `STP xt1, xt2, [sp, #off]`.
    pub fn stp_x(&mut self, rt1: u8, rt2: u8, off: i32) {
        let imm7 = ((off / 8) as u32) & 0x7F;
        self.emit(0xA900_0000 | imm7 << 15 | (rt2 as u32) << 10 | (SP as u32) << 5 | rt1 as u32);
    }
    /// `LDP xt1, xt2, [sp, #off]`.
    pub fn ldp_x(&mut self, rt1: u8, rt2: u8, off: i32) {
        let imm7 = ((off / 8) as u32) & 0x7F;
        self.emit(0xA940_0000 | imm7 << 15 | (rt2 as u32) << 10 | (SP as u32) << 5 | rt1 as u32);
    }
    /// `LDP xt1, xt2, [sp], #off` (post-index).
    pub fn ldp_x_post(&mut self, rt1: u8, rt2: u8, off: i32) {
        let imm7 = ((off / 8) as u32) & 0x7F;
        self.emit(0xA8C0_0000 | imm7 << 15 | (rt2 as u32) << 10 | (SP as u32) << 5 | rt1 as u32);
    }
    /// `MOV x29, sp`.
    pub fn mov_fp_sp(&mut self) {
        // ADD x29, sp, #0
        self.emit(0x9100_03FD);
    }

    /// `BLR xn`.
    pub fn blr(&mut self, rn: u8) {
        self.emit(0xD63F_0000 | (rn as u32) << 5);
    }
    /// `BR xn`.
    pub fn br(&mut self, rn: u8) {
        self.emit(0xD61F_0000 | (rn as u32) << 5);
    }
    /// `CBZ xt, <label>` to be bound later.
    pub fn cbz_x(&mut self, rt: u8) -> Fixup {
        let at = self.pos();
        self.emit(0xB400_0000 | rt as u32);
        Fixup {
            at,
            kind: FixupKind::Imm19,
        }
    }
    /// `RET`.
    pub fn ret(&mut self) {
        self.emit(0xD65F_03C0);
    }

    /// `CBZ wt, <label>` to be bound later.
    pub fn cbz_w(&mut self, rt: u8) -> Fixup {
        let at = self.pos();
        self.emit(0x3400_0000 | rt as u32);
        Fixup {
            at,
            kind: FixupKind::Imm19,
        }
    }
    /// `CBNZ wt, <label>` to be bound later.
    pub fn cbnz_w(&mut self, rt: u8) -> Fixup {
        let at = self.pos();
        self.emit(0x3500_0000 | rt as u32);
        Fixup {
            at,
            kind: FixupKind::Imm19,
        }
    }
    /// `B.cond <label>` to be bound later.
    pub fn b_cond(&mut self, cond: Cond) -> Fixup {
        let at = self.pos();
        self.emit(0x5400_0000 | cond as u32);
        Fixup {
            at,
            kind: FixupKind::Imm19,
        }
    }
    /// `B <label>` to be bound later.
    pub fn b(&mut self) -> Fixup {
        let at = self.pos();
        self.emit(0x1400_0000);
        Fixup {
            at,
            kind: FixupKind::Imm26,
        }
    }

    /// Point `fixup` at instruction index `target`.
    pub fn bind_to(&mut self, fixup: Fixup, target: usize) {
        let delta = target as i64 - fixup.at as i64;
        let word = &mut self.code[fixup.at];
        match fixup.kind {
            FixupKind::Imm19 => {
                assert!((-(1 << 18)..(1 << 18)).contains(&delta));
                *word |= ((delta as u32) & 0x7FFFF) << 5;
            }
            FixupKind::Imm26 => {
                assert!((-(1 << 25)..(1 << 25)).contains(&delta));
                *word |= (delta as u32) & 0x3FF_FFFF;
            }
        }
    }

    /// Point `fixup` at the current position.
    pub fn bind(&mut self, fixup: Fixup) {
        let here = self.pos();
        self.bind_to(fixup, here);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reference words produced by clang's AArch64 assembler for the
    // same instructions (see the comments).
    #[test]
    fn encodings_match_the_assembler() {
        let mut a = Asm::default();
        a.add_w(11, 9, 10); // add w11, w9, w10
        a.sub_w(11, 9, 10); // sub w11, w9, w10
        a.cmp_w(9, 10); // cmp w9, w10
        a.cset_w(11, Cond::Lt); // cset w11, lt
        a.lsl_w_imm(11, 10, 2); // lsl w11, w10, #2
        a.lsr_w_imm(11, 10, 3); // lsr w11, w10, #3
        a.asr_w_imm(11, 10, 4); // asr w11, w10, #4
        a.ldr_w(9, 22, 124); // ldr w9, [x22, #124]
        a.str_w_idx4(13, 22, 12); // str w13, [x22, x12, lsl #2]
        a.stp_x_pre(29, 30, -48); // stp x29, x30, [sp, #-48]!
        a.ldp_x_post(29, 30, 48); // ldp x29, x30, [sp], #48
        a.orn_w(11, ZR, 11); // mvn w11, w11
        a.movn_w(10, 0); // mov w10, #-1
        a.cmp_w_imm(9, 0); // cmp w9, #0
        a.movz_w(3, 5, 0); // movz w3, #5
        a.movk_w(3, 0x1234, 16); // movk w3, #0x1234, lsl #16
        a.mov_x(19, 0); // mov x19, x0
        a.mov_w(11, 10); // mov w11, w10
        a.lslv_w(11, 10, 9); // lslv w11, w10, w9
        a.lsrv_w(11, 10, 9); // lsrv w11, w10, w9
        a.asrv_w(11, 10, 9); // asrv w11, w10, w9
        a.orr_w(11, 9, 10); // orr w11, w9, w10
        a.and_w(11, 9, 10); // and w11, w9, w10
        a.eor_w(11, 9, 10); // eor w11, w9, w10
        a.str_w(11, 21, 8); // str w11, [x21, #8]
        a.stp_x(19, 20, 16); // stp x19, x20, [sp, #16]
        a.ldp_x(21, 22, 32); // ldp x21, x22, [sp, #32]
        a.blr(16); // blr x16
        a.ret(); // ret
        a.cset_w(11, Cond::Lo); // cset w11, lo
        a.cset_w(11, Cond::Eq); // cset w11, eq
        a.cset_w(11, Cond::Ge); // cset w11, ge
        a.cset_w(11, Cond::Gt); // cset w11, gt
        a.cset_w(11, Cond::Le); // cset w11, le
        a.cset_w(11, Cond::Ne); // cset w11, ne
        a.mov_fp_sp(); // mov x29, sp
        a.mov64(16, 0xdef0_9abc_5678_1234); // movz/movk x16
        a.ldr_x(9, 19, 40); // ldr x9, [x19, #40]
        a.str_x(23, 19, 56); // str x23, [x19, #56]
        a.add_x(9, 9, 23); // add x9, x9, x23
        a.add_x_imm(9, 9, 1); // add x9, x9, #1
        a.cmp_x(9, 22); // cmp x9, x22
        a.cmp_x(24, 25); // cmp x24, x25
        let f = a.b_cond(Cond::Hs); // b.hs .+8
        let here = a.pos() + 1;
        a.bind_to(f, here);
        a.add_x(23, 23, 0); // add x23, x23, x0
        a.mov_x(19, 0); // mov x19, x0
        a.stp_x(23, 24, 48); // stp x23, x24, [sp, #48]
        a.ldr_w(10, 19, 100); // ldr w10, [x19, #100]
        assert_eq!(
            a.code,
            [
                0x0b0a012b, 0x4b0a012b, 0x6b0a013f, 0x1a9fa7eb, 0x531e754b, 0x53037d4b, 0x13047d4b,
                0xb9407ec9, 0xb82c7acd, 0xa9bd7bfd, 0xa8c37bfd, 0x2a2b03eb, 0x1280000a, 0x7100013f,
                0x528000a3, 0x72a24683, 0xaa0003f3, 0x2a0a03eb, 0x1ac9214b, 0x1ac9254b, 0x1ac9294b,
                0x2a0a012b, 0x0a0a012b, 0x4a0a012b, 0xb9000aab, 0xa90153f3, 0xa9425bf5, 0xd63f0200,
                0xd65f03c0, 0x1a9f27eb, 0x1a9f17eb, 0x1a9fb7eb, 0x1a9fd7eb, 0x1a9fc7eb, 0x1a9f07eb,
                0x910003fd, 0xd2824690, 0xf2aacf10, 0xf2d35790, 0xf2fbde10, 0xf9401669, 0xf9001e77,
                0x8b170129, 0x91000529, 0xeb16013f, 0xeb19031f, 0x54000042, 0x8b0002f7, 0xaa0003f3,
                0xa90363f7, 0xb940666a,
            ]
        );
    }
}
