//! The few MIPS-I facts the stall tools need, decoded straight from words.

pub const SP: u32 = 29;

pub fn opcode(word: u32) -> u32 {
    word >> 26
}

pub fn rs(word: u32) -> u32 {
    (word >> 21) & 31
}

/// Loads (LB, LH, LWL, LW, LBU, LHU, LWR).
pub fn is_load(word: u32) -> bool {
    matches!(opcode(word), 0x20..=0x26)
}

/// Stores (SB, SH, SWL, SW, SWR).
pub fn is_store(word: u32) -> bool {
    matches!(opcode(word), 0x28 | 0x29 | 0x2a | 0x2b | 0x2e)
}

/// JAL or JALR.
pub fn is_call(word: u32) -> bool {
    opcode(word) == 3 || (opcode(word) == 0 && word & 0x3f == 9)
}

/// MULT, MULTU, DIV, DIVU, or a COP2 instruction.
pub fn is_muldiv_or_gte(word: u32) -> bool {
    (opcode(word) == 0 && matches!(word & 0x3f, 0x18..=0x1b)) || opcode(word) == 0x12
}

/// A conditional branch (BEQ, BNE, BLEZ, BGTZ, REGIMM).
pub fn is_cond_branch(word: u32) -> bool {
    matches!(opcode(word), 1 | 4..=7)
}

/// Target of a PC-relative branch at `pc`.
pub fn branch_target(pc: u32, word: u32) -> u32 {
    let imm = (word & 0xffff) as u16 as i16 as i32;
    pc.wrapping_add(4).wrapping_add((imm << 2) as u32)
}

/// A polling loop body: at least one load, and no store, call, multiply,
/// divide or GTE instruction. Waiting for a vblank counter or a status
/// register looks like this; a copy or search loop with stores does not.
pub fn is_pure_poll(words: &[u32]) -> bool {
    words.iter().any(|&w| is_load(w))
        && words
            .iter()
            .all(|&w| !is_store(w) && !is_call(w) && !is_muldiv_or_gte(w) && opcode(w) != 0x3a)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LW_T0_SP: u32 = 0x8fa8_0010; // lw t0, 16(sp)
    const SW_T0_A0: u32 = 0xac88_0000; // sw t0, 0(a0)
    const ADDIU: u32 = 0x2508_0001; // addiu t0, t0, 1

    #[test]
    fn classifies_loads_stores_and_stack_base() {
        assert!(is_load(LW_T0_SP) && rs(LW_T0_SP) == SP);
        assert!(is_store(SW_T0_A0) && rs(SW_T0_A0) != SP);
        assert!(!is_load(ADDIU) && !is_store(ADDIU));
    }

    #[test]
    fn branch_target_handles_backward_offsets() {
        // bne t0, zero, -3 words at 0x8001_0010 -> 0x8001_0008.
        let bne = 0x1500_fffd;
        assert_eq!(branch_target(0x8001_0010, bne), 0x8001_0008);
        assert!(is_cond_branch(bne));
    }

    #[test]
    fn poll_loops_have_a_load_and_no_stores() {
        assert!(is_pure_poll(&[LW_T0_SP, ADDIU, 0x1500_fffd, 0]));
        assert!(!is_pure_poll(&[LW_T0_SP, SW_T0_A0, ADDIU, 0x1500_fffd]));
        assert!(!is_pure_poll(&[ADDIU, ADDIU]));
    }
}
