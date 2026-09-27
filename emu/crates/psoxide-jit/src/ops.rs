//! Code generation for the register-only ops the native tier runs inline:
//! arithmetic and branch decisions over the guest register file, which
//! compiled code addresses through x22.

use crate::a64::{Asm, Cond, ZR};
use crate::decode::{Alu, Branch};

/// Host register holding the address of the guest register file.
pub(crate) const X_GPRS: u8 = 22;

/// Load guest register `guest` into w`host` (r0 reads as zero).
pub(crate) fn load_reg(a: &mut Asm, host: u8, guest: u32) {
    if guest == 0 {
        a.movz_w(host, 0, 0);
    } else {
        a.ldr_w(host, X_GPRS, guest * 4);
    }
}

/// Store w`host` into guest register `guest` (writes to r0 are dropped).
pub(crate) fn store_reg(a: &mut Asm, host: u8, guest: u32) {
    if guest != 0 {
        a.str_w(host, X_GPRS, guest * 4);
    }
}

/// A register-only op: reads its operands from the register file and
/// writes the result back (through w9..w13).
pub(crate) fn emit_alu(a: &mut Asm, alu: Alu, word: u32) {
    let rs = (word >> 21) & 0x1F;
    let rt = (word >> 16) & 0x1F;
    let rd = (word >> 11) & 0x1F;
    let sa = (word >> 6) & 0x1F;
    let imm = word & 0xFFFF;
    let simm = (word as i16) as i32 as u32;
    let dest = match alu {
        Alu::Addiu | Alu::Slti | Alu::Sltiu | Alu::Andi | Alu::Ori | Alu::Xori | Alu::Lui => rt,
        _ => rd,
    };
    // Every operation here is nontrapping. Its issue cycle, load shadow,
    // and delayed-load commit still run in the caller, even for a NOP.
    if dest == 0 {
        return;
    }
    match alu {
        Alu::Sll | Alu::Srl | Alu::Sra => {
            load_reg(a, 10, rt);
            if sa == 0 {
                a.mov_w(11, 10);
            } else {
                match alu {
                    Alu::Sll => a.lsl_w_imm(11, 10, sa),
                    Alu::Srl => a.lsr_w_imm(11, 10, sa),
                    _ => a.asr_w_imm(11, 10, sa),
                }
            }
            store_reg(a, 11, rd);
        }
        Alu::Sllv | Alu::Srlv | Alu::Srav => {
            load_reg(a, 9, rs);
            load_reg(a, 10, rt);
            match alu {
                Alu::Sllv => a.lslv_w(11, 10, 9),
                Alu::Srlv => a.lsrv_w(11, 10, 9),
                _ => a.asrv_w(11, 10, 9),
            }
            store_reg(a, 11, rd);
        }
        Alu::Addu | Alu::Subu | Alu::And | Alu::Or | Alu::Xor | Alu::Nor | Alu::Slt | Alu::Sltu => {
            load_reg(a, 9, rs);
            load_reg(a, 10, rt);
            match alu {
                Alu::Addu => a.add_w(11, 9, 10),
                Alu::Subu => a.sub_w(11, 9, 10),
                Alu::And => a.and_w(11, 9, 10),
                Alu::Or => a.orr_w(11, 9, 10),
                Alu::Xor => a.eor_w(11, 9, 10),
                Alu::Nor => {
                    a.orr_w(11, 9, 10);
                    a.orn_w(11, ZR, 11);
                }
                Alu::Slt => {
                    a.cmp_w(9, 10);
                    a.cset_w(11, Cond::Lt);
                }
                _ => {
                    a.cmp_w(9, 10);
                    a.cset_w(11, Cond::Lo);
                }
            }
            store_reg(a, 11, rd);
        }
        Alu::Addiu | Alu::Slti | Alu::Sltiu => {
            load_reg(a, 9, rs);
            a.mov32(10, simm);
            match alu {
                Alu::Addiu => a.add_w(11, 9, 10),
                Alu::Slti => {
                    a.cmp_w(9, 10);
                    a.cset_w(11, Cond::Lt);
                }
                _ => {
                    a.cmp_w(9, 10);
                    a.cset_w(11, Cond::Lo);
                }
            }
            store_reg(a, 11, rt);
        }
        Alu::Andi | Alu::Ori | Alu::Xori => {
            load_reg(a, 9, rs);
            a.mov32(10, imm);
            match alu {
                Alu::Andi => a.and_w(11, 9, 10),
                Alu::Ori => a.orr_w(11, 9, 10),
                _ => a.eor_w(11, 9, 10),
            }
            store_reg(a, 11, rt);
        }
        Alu::Lui => {
            a.mov32(11, imm << 16);
            store_reg(a, 11, rt);
        }
    }
}

/// A branch's decision: writes the link register (through the squashing
/// path, like `set_gpr`), and leaves w11 = taken (0/1), w12 = target.
pub(crate) fn emit_branch_decision(a: &mut Asm, branch: Branch, word: u32, pc: u32) {
    let rs = (word >> 21) & 0x1F;
    let rt = (word >> 16) & 0x1F;
    let rd = (word >> 11) & 0x1F;
    let jump_target = (pc.wrapping_add(4) & 0xF000_0000) | ((word & 0x03FF_FFFF) << 2);
    let branch_target = pc
        .wrapping_add(4)
        .wrapping_add((((word as i16) as i32) << 2) as u32);
    let link = pc.wrapping_add(8);
    // w11 = taken (0/1), w12 = target.
    match branch {
        Branch::J | Branch::Jal => {
            if branch == Branch::Jal {
                a.mov32(13, link);
                store_reg(a, 13, 31);
            }
            a.movz_w(11, 1, 0);
            a.mov32(12, jump_target);
        }
        Branch::Jr | Branch::Jalr => {
            load_reg(a, 12, rs);
            if branch == Branch::Jalr {
                a.mov32(13, link);
                store_reg(a, 13, rd);
            }
            a.movz_w(11, 1, 0);
        }
        Branch::Beq | Branch::Bne => {
            load_reg(a, 9, rs);
            load_reg(a, 10, rt);
            a.cmp_w(9, 10);
            a.cset_w(
                11,
                if branch == Branch::Beq {
                    Cond::Eq
                } else {
                    Cond::Ne
                },
            );
            a.mov32(12, branch_target);
        }
        Branch::Blez | Branch::Bgtz | Branch::Bltz | Branch::Bgez => {
            load_reg(a, 9, rs);
            a.cmp_w_imm(9, 0);
            a.cset_w(
                11,
                match branch {
                    Branch::Blez => Cond::Le,
                    Branch::Bgtz => Cond::Gt,
                    Branch::Bltz => Cond::Lt,
                    _ => Cond::Ge,
                },
            );
            a.mov32(12, branch_target);
        }
        Branch::Bltzal | Branch::Bgezal => {
            // The link is written before rs is read, as the interpreter does.
            a.mov32(13, link);
            store_reg(a, 13, 31);
            load_reg(a, 9, rs);
            a.cmp_w_imm(9, 0);
            a.cset_w(
                11,
                if branch == Branch::Bltzal {
                    Cond::Lt
                } else {
                    Cond::Ge
                },
            );
            a.mov32(12, branch_target);
        }
    }
}
