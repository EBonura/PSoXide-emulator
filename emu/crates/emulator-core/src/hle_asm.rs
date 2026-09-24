// SPDX-License-Identifier: GPL-2.0-or-later
//! A minimal MIPS I assembler for the HLE kernel's guest-side routines.
//!
//! The exception handler, ReturnFromException and DeliverEvent have to
//! run as guest code: they call guest functions (chain verifiers and
//! handlers, event callbacks) and are patched by games. This builds their
//! machine code from instruction mnemonics; the only encodings it knows
//! are the ones those routines use.

#![allow(missing_docs)]

pub const ZERO: u32 = 0;
pub const AT: u32 = 1;
pub const V0: u32 = 2;
pub const V1: u32 = 3;
pub const A0: u32 = 4;
pub const A1: u32 = 5;
pub const A2: u32 = 6;
pub const A3: u32 = 7;
pub const T0: u32 = 8;
pub const T1: u32 = 9;
pub const T2: u32 = 10;
pub const T3: u32 = 11;
pub const S0: u32 = 16;
pub const S1: u32 = 17;
pub const S2: u32 = 18;
pub const S3: u32 = 19;
pub const S4: u32 = 20;
pub const S6: u32 = 22;
pub const K0: u32 = 26;
pub const K1: u32 = 27;
pub const GP: u32 = 28;
pub const SP: u32 = 29;
pub const FP: u32 = 30;
pub const RA: u32 = 31;

enum Fix {
    Branch,
    Jump,
}

/// Assembles words starting at `base`, with forward and backward labels.
pub struct Asm {
    base: u32,
    words: Vec<u32>,
    labels: Vec<(&'static str, u32)>,
    fixups: Vec<(usize, &'static str, Fix)>,
}

impl Asm {
    pub fn new(base: u32) -> Self {
        Self {
            base,
            words: Vec::new(),
            labels: Vec::new(),
            fixups: Vec::new(),
        }
    }

    /// Address of the next word.
    pub fn here(&self) -> u32 {
        self.base + 4 * self.words.len() as u32
    }

    pub fn label(&mut self, name: &'static str) {
        assert!(self.lookup(name).is_none(), "duplicate label {name}");
        self.labels.push((name, self.here()));
    }

    fn lookup(&self, name: &str) -> Option<u32> {
        self.labels
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, a)| *a)
    }

    pub fn word(&mut self, w: u32) {
        self.words.push(w);
    }

    fn r(&mut self, rs: u32, rt: u32, rd: u32, sh: u32, funct: u32) {
        self.word((rs << 21) | (rt << 16) | (rd << 11) | (sh << 6) | funct);
    }

    fn i(&mut self, op: u32, rs: u32, rt: u32, imm: u32) {
        self.word((op << 26) | (rs << 21) | (rt << 16) | (imm & 0xFFFF));
    }

    pub fn nop(&mut self) {
        self.word(0);
    }
    pub fn sll(&mut self, rd: u32, rt: u32, sh: u32) {
        self.r(0, rt, rd, sh, 0x00);
    }
    pub fn srl(&mut self, rd: u32, rt: u32, sh: u32) {
        self.r(0, rt, rd, sh, 0x02);
    }
    pub fn jr(&mut self, rs: u32) {
        self.r(rs, 0, 0, 0, 0x08);
    }
    pub fn jalr(&mut self, rs: u32) {
        self.r(rs, 0, RA, 0, 0x09);
    }
    pub fn syscall(&mut self) {
        self.word(0x0C);
    }
    pub fn mfhi(&mut self, rd: u32) {
        self.r(0, 0, rd, 0, 0x10);
    }
    pub fn mthi(&mut self, rs: u32) {
        self.r(rs, 0, 0, 0, 0x11);
    }
    pub fn mflo(&mut self, rd: u32) {
        self.r(0, 0, rd, 0, 0x12);
    }
    pub fn mtlo(&mut self, rs: u32) {
        self.r(rs, 0, 0, 0, 0x13);
    }
    pub fn addu(&mut self, rd: u32, rs: u32, rt: u32) {
        self.r(rs, rt, rd, 0, 0x21);
    }
    pub fn and(&mut self, rd: u32, rs: u32, rt: u32) {
        self.r(rs, rt, rd, 0, 0x24);
    }
    pub fn nor(&mut self, rd: u32, rs: u32, rt: u32) {
        self.r(rs, rt, rd, 0, 0x27);
    }
    pub fn sltu(&mut self, rd: u32, rs: u32, rt: u32) {
        self.r(rs, rt, rd, 0, 0x2B);
    }
    pub fn mov(&mut self, rd: u32, rs: u32) {
        self.addu(rd, rs, ZERO);
    }
    pub fn addi(&mut self, rt: u32, rs: u32, imm: i16) {
        self.i(0x08, rs, rt, imm as u16 as u32);
    }
    pub fn addiu(&mut self, rt: u32, rs: u32, imm: i16) {
        self.i(0x09, rs, rt, imm as u16 as u32);
    }
    pub fn andi(&mut self, rt: u32, rs: u32, imm: u16) {
        self.i(0x0C, rs, rt, u32::from(imm));
    }
    pub fn ori(&mut self, rt: u32, rs: u32, imm: u16) {
        self.i(0x0D, rs, rt, u32::from(imm));
    }
    pub fn lui(&mut self, rt: u32, imm: u16) {
        self.i(0x0F, 0, rt, u32::from(imm));
    }
    pub fn lw(&mut self, rt: u32, offset: i16, base: u32) {
        self.i(0x23, base, rt, offset as u16 as u32);
    }
    pub fn sw(&mut self, rt: u32, offset: i16, base: u32) {
        self.i(0x2B, base, rt, offset as u16 as u32);
    }
    /// `lui rt, hi(value); ori rt, rt, lo(value)`.
    pub fn li(&mut self, rt: u32, value: u32) {
        self.lui(rt, (value >> 16) as u16);
        self.ori(rt, rt, value as u16);
    }
    pub fn mfc0(&mut self, rt: u32, rd: u32) {
        self.word((0x10 << 26) | (rt << 16) | (rd << 11));
    }
    pub fn mtc0(&mut self, rt: u32, rd: u32) {
        self.word((0x10 << 26) | (4 << 21) | (rt << 16) | (rd << 11));
    }
    pub fn rfe(&mut self) {
        self.word(0x4200_0010);
    }

    fn branch(&mut self, op: u32, rs: u32, rt: u32, target: &'static str) {
        self.fixups.push((self.words.len(), target, Fix::Branch));
        self.i(op, rs, rt, 0);
    }
    pub fn beq(&mut self, rs: u32, rt: u32, target: &'static str) {
        self.branch(0x04, rs, rt, target);
    }
    pub fn bne(&mut self, rs: u32, rt: u32, target: &'static str) {
        self.branch(0x05, rs, rt, target);
    }
    pub fn beqz(&mut self, rs: u32, target: &'static str) {
        self.beq(rs, ZERO, target);
    }
    pub fn bnez(&mut self, rs: u32, target: &'static str) {
        self.bne(rs, ZERO, target);
    }
    pub fn b(&mut self, target: &'static str) {
        self.beq(ZERO, ZERO, target);
    }
    /// `jal` to an absolute address.
    pub fn jal_abs(&mut self, target: u32) {
        self.word((0x03 << 26) | ((target >> 2) & 0x03FF_FFFF));
    }
    /// `jal` to a label in this block.
    pub fn jal(&mut self, target: &'static str) {
        self.fixups.push((self.words.len(), target, Fix::Jump));
        self.word(0x03 << 26);
    }

    /// Address of `label` (panics when undefined).
    pub fn addr(&self, label: &str) -> u32 {
        self.lookup(label)
            .unwrap_or_else(|| panic!("undefined label {label}"))
    }

    /// Resolve labels and return the machine words.
    pub fn finish(mut self) -> Vec<u32> {
        let fixups = std::mem::take(&mut self.fixups);
        for (index, name, fix) in fixups {
            let target = self.addr(name);
            let pc = self.base + 4 * index as u32;
            match fix {
                Fix::Branch => {
                    let offset = (target.wrapping_sub(pc + 4) as i32) >> 2;
                    assert!(
                        (-0x8000..0x8000).contains(&offset),
                        "branch to {name} out of range"
                    );
                    self.words[index] |= offset as u32 & 0xFFFF;
                }
                Fix::Jump => self.words[index] |= (target >> 2) & 0x03FF_FFFF,
            }
        }
        self.words
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodings_match_known_words() {
        let mut a = Asm::new(0x80);
        a.lui(K0, 0);
        a.addiu(K0, K0, 0x0C80);
        a.jr(K0);
        a.nop();
        a.rfe();
        a.mfc0(V0, 13);
        a.mtc0(A1, 12);
        a.jalr(S1);
        a.sw(RA, 0x7C, K0);
        a.lw(K0, 8, K0);
        // psx-spx lists the retail exception vector words.
        assert_eq!(
            a.finish(),
            [
                0x3C1A_0000,
                0x275A_0C80,
                0x0340_0008,
                0,
                0x4200_0010,
                0x4002_6800,
                0x4085_6000,
                0x0220_F809,
                0xAF5F_007C,
                0x8F5A_0008
            ]
        );
    }

    #[test]
    fn branches_resolve_both_directions() {
        let mut a = Asm::new(0x1000);
        a.label("top");
        a.nop();
        a.bnez(T0, "top");
        a.nop();
        a.beqz(T0, "end");
        a.nop();
        a.label("end");
        a.jal("top");
        let w = a.finish();
        assert_eq!(w[1], 0x1500_FFFE);
        assert_eq!(w[3], 0x1100_0001);
        assert_eq!(w[5], 0x0C00_0400);
    }
}
