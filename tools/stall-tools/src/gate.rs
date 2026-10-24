//! Spill gate: checks registered kernels in a linked image.
//!
//! A kernel is a code range (`START END NAME`, the same file format as the
//! emulator's `PSOXIDE_LIMIT_FREE`). The gate fails a kernel when
//!
//! * any loop in it (a backward branch and the code it jumps back over) holds
//!   a load or store through `$sp`, which is a spill or a reload on every
//!   iteration, or a call (JAL/JALR), or
//! * it is larger than the byte budget.
//!
//! Prologue and epilogue stack traffic outside loops is allowed. The image is
//! a PSX-EXE or a RAM dump (2 MB, KSEG0 addresses).

use crate::mips;

pub struct Kernel {
    pub start: u32,
    pub end: u32,
    pub name: String,
}

pub fn parse_ranges(text: &str) -> Result<Vec<Kernel>, String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut f = line.split_whitespace();
        let hex = |s: Option<&str>| {
            u32::from_str_radix(
                s.ok_or(format!("line {}: missing field", n + 1))?
                    .trim_start_matches("0x"),
                16,
            )
            .map_err(|e| format!("line {}: {e}", n + 1))
        };
        let start = hex(f.next())?;
        let end = hex(f.next())?;
        let name = f
            .next()
            .map_or_else(|| format!("{start:#010x}"), str::to_string);
        out.push(Kernel { start, end, name });
    }
    Ok(out)
}

#[derive(PartialEq, Eq)]
pub enum Finding {
    TooBig { bytes: u32, budget: u32 },
    StackInLoop { pc: u32 },
    CallInLoop { pc: u32 },
}

impl std::fmt::Debug for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Finding::TooBig { bytes, budget } => {
                write!(f, "TooBig {{ bytes: {bytes}, budget: {budget} }}")
            }
            Finding::StackInLoop { pc } => write!(f, "stack access in a loop at {pc:#010x}"),
            Finding::CallInLoop { pc } => write!(f, "call in a loop at {pc:#010x}"),
        }
    }
}

pub fn check(kernel: &Kernel, budget: u32, word: &dyn Fn(u32) -> u32) -> Vec<Finding> {
    let mut findings = Vec::new();
    let bytes = kernel.end - kernel.start;
    if bytes > budget {
        findings.push(Finding::TooBig { bytes, budget });
    }
    let mut in_loop = std::collections::BTreeSet::new();
    let mut pc = kernel.start;
    while pc < kernel.end {
        let w = word(pc);
        if mips::is_cond_branch(w) {
            let t = mips::branch_target(pc, w);
            if t <= pc && t >= kernel.start {
                // The loop body runs from the target through the delay slot.
                let mut x = t;
                while x <= pc + 4 && x < kernel.end {
                    in_loop.insert(x);
                    x += 4;
                }
            }
        }
        pc += 4;
    }
    for &pc in &in_loop {
        let w = word(pc);
        if (mips::is_load(w) || mips::is_store(w)) && mips::rs(w) == mips::SP {
            findings.push(Finding::StackInLoop { pc });
        }
        if mips::is_call(w) {
            findings.push(Finding::CallInLoop { pc });
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    const LW_SP: u32 = 0x8fa8_0010; // lw t0, 16(sp)
    const LW_A0: u32 = 0x8c88_0000; // lw t0, 0(a0)
    const ADDIU: u32 = 0x2508_0001;
    const JAL: u32 = 0x0c00_0000;
    const BNE_BACK3: u32 = 0x1500_fffc; // bne t0, zero, -4 words (relative to pc+4)

    fn run(code: &[u32], budget: u32) -> Vec<Finding> {
        let k = Kernel {
            start: 0x8001_0000,
            end: 0x8001_0000 + 4 * code.len() as u32,
            name: "k".into(),
        };
        check(&k, budget, &|a| code[((a - 0x8001_0000) / 4) as usize])
    }

    #[test]
    fn clean_loop_passes() {
        // prologue with a stack store is fine; the loop has a load through a0 only.
        let code = [0xafb0_0000, LW_A0, ADDIU, ADDIU, BNE_BACK3, 0];
        // bne at index 4 jumps back to index 1 (-3): use -4 words from pc+4 => index 1.
        assert!(run(&code, 64).is_empty());
    }

    #[test]
    fn stack_load_in_loop_fails_and_outside_does_not() {
        let code = [LW_SP, LW_SP, ADDIU, ADDIU, BNE_BACK3, 0];
        let f = run(&code, 64);
        assert_eq!(f, vec![Finding::StackInLoop { pc: 0x8001_0004 }]);
    }

    #[test]
    fn call_in_loop_and_size_budget_are_reported() {
        let code = [ADDIU, JAL, ADDIU, ADDIU, BNE_BACK3, 0];
        let f = run(&code, 16);
        assert!(f.contains(&Finding::CallInLoop { pc: 0x8001_0004 }));
        assert!(f.contains(&Finding::TooBig {
            bytes: 24,
            budget: 16
        }));
    }
}
