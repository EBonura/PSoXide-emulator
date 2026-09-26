//! What the block builder needs to know about each R3000A instruction.

use emulator_core::cpu::jit_abi::is_gte_command;

/// Register-only arithmetic the compiler emits natively.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alu {
    /// `rd = rt << sa` (SLL)
    Sll,
    /// SRL
    Srl,
    /// SRA
    Sra,
    /// SLLV
    Sllv,
    /// SRLV
    Srlv,
    /// SRAV
    Srav,
    /// ADDU
    Addu,
    /// SUBU
    Subu,
    /// AND
    And,
    /// OR
    Or,
    /// XOR
    Xor,
    /// NOR
    Nor,
    /// SLT
    Slt,
    /// SLTU
    Sltu,
    /// ADDIU
    Addiu,
    /// SLTI
    Slti,
    /// SLTIU
    Sltiu,
    /// ANDI
    Andi,
    /// ORI
    Ori,
    /// XORI
    Xori,
    /// LUI
    Lui,
}

/// Branches and jumps, all compiled natively.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Branch {
    /// J
    J,
    /// JAL
    Jal,
    /// JR
    Jr,
    /// JALR
    Jalr,
    /// BEQ
    Beq,
    /// BNE
    Bne,
    /// BLEZ
    Blez,
    /// BGTZ
    Bgtz,
    /// BLTZ
    Bltz,
    /// BGEZ
    Bgez,
    /// BLTZAL
    Bltzal,
    /// BGEZAL
    Bgezal,
}

/// How a block may contain an instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// Native register arithmetic.
    Alu(Alu),
    /// Native branch or jump (ends the block after its delay slot).
    Branch(Branch),
    /// Loads, stores, multiply/divide, HI/LO moves, trapping adds, MFC0 and
    /// GTE register moves: executed by the interpreter's own code through a
    /// helper, in the middle of a block.
    Helper,
    /// A GTE command: executed through the interpreter's full step, which
    /// handles the interrupt-versus-GTE hazard.
    GteCommand,
    /// SYSCALL, BREAK, MTC0, RFE and other COP0 or absent-coprocessor
    /// opcodes: allowed only as the last instruction of a block.
    Terminator,
    /// Anything the interpreter rejects: never compiled.
    Reject,
}

/// Classify one instruction word.
pub fn classify(word: u32) -> Class {
    let op = word >> 26;
    let rt = (word >> 16) & 0x1F;
    let rs = (word >> 21) & 0x1F;
    let funct = word & 0x3F;
    match op {
        0x00 => match funct {
            0x00 => Class::Alu(Alu::Sll),
            0x02 => Class::Alu(Alu::Srl),
            0x03 => Class::Alu(Alu::Sra),
            0x04 => Class::Alu(Alu::Sllv),
            0x06 => Class::Alu(Alu::Srlv),
            0x07 => Class::Alu(Alu::Srav),
            0x08 => Class::Branch(Branch::Jr),
            0x09 => Class::Branch(Branch::Jalr),
            0x0C | 0x0D => Class::Terminator,
            0x10..=0x13 | 0x18..=0x1B | 0x20 | 0x22 => Class::Helper,
            0x21 => Class::Alu(Alu::Addu),
            0x23 => Class::Alu(Alu::Subu),
            0x24 => Class::Alu(Alu::And),
            0x25 => Class::Alu(Alu::Or),
            0x26 => Class::Alu(Alu::Xor),
            0x27 => Class::Alu(Alu::Nor),
            0x2A => Class::Alu(Alu::Slt),
            0x2B => Class::Alu(Alu::Sltu),
            _ => Class::Reject,
        },
        0x01 => match rt {
            0x00 => Class::Branch(Branch::Bltz),
            0x01 => Class::Branch(Branch::Bgez),
            0x10 => Class::Branch(Branch::Bltzal),
            0x11 => Class::Branch(Branch::Bgezal),
            _ => Class::Reject,
        },
        0x02 => Class::Branch(Branch::J),
        0x03 => Class::Branch(Branch::Jal),
        0x04 => Class::Branch(Branch::Beq),
        0x05 => Class::Branch(Branch::Bne),
        0x06 => Class::Branch(Branch::Blez),
        0x07 => Class::Branch(Branch::Bgtz),
        0x08 => Class::Helper,
        0x09 => Class::Alu(Alu::Addiu),
        0x0A => Class::Alu(Alu::Slti),
        0x0B => Class::Alu(Alu::Sltiu),
        0x0C => Class::Alu(Alu::Andi),
        0x0D => Class::Alu(Alu::Ori),
        0x0E => Class::Alu(Alu::Xori),
        0x0F => Class::Alu(Alu::Lui),
        // COP0: MFC0 only reads; everything else may change SR.
        0x10 if rs == 0 => Class::Helper,
        0x10 => Class::Terminator,
        0x12 if is_gte_command(word) => Class::GteCommand,
        0x12 => Class::Helper,
        0x11 | 0x13 | 0x30 | 0x31 | 0x33 | 0x38 | 0x39 | 0x3B => Class::Terminator,
        0x20..=0x26 | 0x28..=0x2B | 0x2E | 0x32 | 0x3A => Class::Helper,
        _ => Class::Reject,
    }
}

/// Whether `word` stores to memory (the helper then checks the address).
pub fn is_store(word: u32) -> bool {
    matches!(word >> 26, 0x28..=0x2B | 0x2E | 0x3A)
}

/// The register an instruction writes through `set_gpr`, the path that
/// cancels a load in flight to the same register. Loads, MFC0/MFC2 and
/// other delayed writes are not included.
pub fn alu_dest(class: Class, word: u32) -> Option<u8> {
    let rt = ((word >> 16) & 0x1F) as u8;
    let rd = ((word >> 11) & 0x1F) as u8;
    match class {
        Class::Alu(alu) => Some(match alu {
            Alu::Addiu | Alu::Slti | Alu::Sltiu | Alu::Andi | Alu::Ori | Alu::Xori | Alu::Lui => rt,
            _ => rd,
        }),
        Class::Branch(Branch::Jal | Branch::Bltzal | Branch::Bgezal) => Some(31),
        Class::Branch(Branch::Jalr) => Some(rd),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_encodings_classify() {
        assert_eq!(classify(0x0000_0000), Class::Alu(Alu::Sll)); // nop
        assert_eq!(classify(0x27BD_FFE8), Class::Alu(Alu::Addiu)); // addiu sp, sp, -24
        assert_eq!(classify(0x8FBF_0010), Class::Helper); // lw ra, 16(sp)
        assert_eq!(classify(0x03E0_0008), Class::Branch(Branch::Jr)); // jr ra
        assert_eq!(classify(0x0C00_0000), Class::Branch(Branch::Jal));
        assert_eq!(classify(0x4A18_0001), Class::GteCommand); // RTPS
        assert_eq!(classify(0x4808_0000), Class::Helper); // mfc2
        assert_eq!(classify(0x4084_6000), Class::Terminator); // mtc0 a0, sr
        assert_eq!(classify(0x4000_6000), Class::Helper); // mfc0
        assert_eq!(classify(0x0000_000C), Class::Terminator); // syscall
        assert_eq!(classify(0xFC00_0000), Class::Reject);
    }
}
