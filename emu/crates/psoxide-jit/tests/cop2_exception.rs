//! Coprocessor enables may change after a block has compiled.
#![cfg(target_arch = "aarch64")]
use psoxide_jit::testgen::lockstep;

#[test]
fn compiled_cop2_memory_ops_trap_after_cu2_is_cleared() {
    for opcode in [0x32, 0x3a] {
        // LWC2, SWC2
        for delay in [false, true] {
            let memory = opcode << 26 | 9 << 21;
            let mut code = vec![0x3c08_4000, 0x4088_6000, 0x3c09_8010, 0x340a_0064];
            if delay {
                code.extend([0x1000_0001, memory]);
            } else {
                code.push(memory);
            }
            code.push(0x254a_ffff);
            let branch_index = code.len();
            let offset = 4i32 - (branch_index as i32 + 1);
            code.extend([0x1d40_0000 | (offset as u16 as u32), 0]);
            code.extend([0x4080_6000, 0x0800_4004, 0]);
            let handler = [0x0800_0020, 0];
            lockstep(&code, &handler, 20_000, 1, opcode as u64, true)
                .unwrap_or_else(|e| panic!("opcode {opcode:x}, delay {delay}: {e}"));
        }
    }
}
