// SPDX-License-Identifier: GPL-2.0-or-later
//! Load-shadow census (lane ps1-shadow): what the load-shadow rule
//! (`Cpu::hides_in_load_shadow`) does on a real guest. Opt-in through
//! `PSOXIDE_SHADOW_CENSUS=FILE`; inert otherwise. See `limits.rs`.
//!
//! The census counts instructions, loads, stores and shadows from the point
//! the limit oracles go live (`PSOXIDE_LIMIT_FROM_POLL` / `_FROM_CYCLE` /
//! `_FROM_GL`), skipping instructions inside `PSOXIDE_LIMIT_WAIT` ranges. A
//! shadow belongs to the load that opened it: when that load is inside a wait
//! range the shadow is not tracked.
//!
//! Reason precedence when a shadow ends, in the order the rule tests them: a
//! bus opcode (class by effective address), then a read of the loaded
//! register, then an I-cache miss, then expiry (the seventh instruction). An
//! exception or interrupt entered inside a shadow closes the census shadow
//! (the model itself keeps the shadow alive into the handler).

use std::path::PathBuf;

/// Reasons a shadow ends, in table order.
pub const REASONS: [&str; 11] = [
    "ram_load",
    "ram_store",
    "spad_load",
    "spad_store",
    "io_load",
    "io_store",
    "other_bus_op",
    "reads_loaded_reg",
    "icache_miss",
    "expiry",
    "exception",
];
const R_READS: usize = 7;
const R_ICACHE: usize = 8;
const R_EXPIRY: usize = 9;
const R_EXC: usize = 10;

/// Effective-address class of the instruction about to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Access {
    /// Opcode below 0x20: no bus access of the kind the rule tests.
    #[default]
    None,
    /// Main-RAM load.
    RamLoad,
    /// Main-RAM store.
    RamStore,
    /// Scratchpad load.
    SpadLoad,
    /// Scratchpad store.
    SpadStore,
    /// Load from I/O, BIOS or expansion space.
    IoLoad,
    /// Store to I/O, BIOS or expansion space.
    IoStore,
    /// Opcode 0x20 or above that is neither a load nor a store (coprocessor 0, 1, 3 memory ops).
    Other,
}

impl Access {
    /// Class of `instr` given the value of its base register.
    pub fn classify(instr: u32, base: u32) -> Self {
        let opcode = instr >> 26;
        let load = matches!(opcode, 0x20..=0x26 | 0x32);
        let store = matches!(opcode, 0x28..=0x2b | 0x2e | 0x3a);
        if !(load || store) {
            return if opcode >= 0x20 { Self::Other } else { Self::None };
        }
        let addr = base.wrapping_add(instr as u16 as i16 as i32 as u32);
        let phys = addr & 0x1fff_ffff;
        let ram = phys < 0x0080_0000;
        let spad = (addr >> 29) != 5 && (0x1f80_0000..0x1f80_0400).contains(&phys);
        match (ram, spad, load) {
            (true, _, true) => Self::RamLoad,
            (true, _, false) => Self::RamStore,
            (_, true, true) => Self::SpadLoad,
            (_, true, false) => Self::SpadStore,
            (_, _, true) => Self::IoLoad,
            (_, _, false) => Self::IoStore,
        }
    }

    fn reason(self) -> usize {
        match self {
            Self::RamLoad => 0,
            Self::RamStore => 1,
            Self::SpadLoad => 2,
            Self::SpadStore => 3,
            Self::IoLoad => 4,
            Self::IoStore => 5,
            Self::Other | Self::None => 6,
        }
    }
}

#[derive(Debug, Default)]
struct OpenShadow {
    func: Option<usize>,
    exceptions_at_open: u64,
}

/// The counters.
#[derive(Debug, Default)]
pub struct ShadowCensus {
    pub(crate) path: Option<PathBuf>,
    func_names: Vec<(String, u32, u32)>,
    pub(crate) written: bool,
    /// Bus cycle of the first counted instruction.
    pub first_cycle: Option<u64>,
    /// Bus cycle at the last instruction seen.
    pub end_cycle: u64,
    /// Instructions retired (outside wait ranges).
    pub instrs: u64,
    /// Instructions retired inside wait ranges (not counted above).
    pub wait_instrs: u64,
    /// Main-RAM loads, any code, any shadow state.
    pub ram_loads: u64,
    /// Of those, with $sp as the base register.
    pub ram_loads_sp: u64,
    /// Main-RAM stores.
    pub ram_stores: u64,
    /// Scratchpad loads.
    pub spad_loads: u64,
    /// Scratchpad stores.
    pub spad_stores: u64,
    /// Loads from I/O, BIOS or expansion space.
    pub io_loads: u64,
    /// Stores to I/O, BIOS or expansion space.
    pub io_stores: u64,
    /// LWC2 or SWC2 (counted again in the classes above).
    pub cop2_mem: u64,
    /// Shadows opened (a load from cached code that the rule treats as hiding work).
    pub opened: u64,
    /// Of those, main-RAM loads.
    pub opened_ram: u64,
    /// Of those, main-RAM loads with $sp as the base.
    pub opened_ram_sp: u64,
    /// Of those, on-die register loads (timers, I_STAT and the like).
    pub opened_io: u64,
    /// Instructions with issue cost 0.
    pub hidden: u64,
    /// Hidden by position (index 3..=6 used).
    pub hidden_by_pos: [u64; 8],
    /// Hidden while no census shadow was open (after an exception closed one).
    pub hidden_untracked: u64,
    /// Shadows closed by an exception entry.
    pub exception_closes: u64,
    /// Reason x position (1..=7).
    pub ends: [[u64; 8]; 11],
    /// Per profile range: shadows opened, instructions hidden.
    pub per_func: Vec<[u64; 2]>,
    /// Current access class (set by `pre`).
    acc: Access,
    sp_base: bool,
    counting: bool,
    open: Option<OpenShadow>,
}

impl ShadowCensus {
    pub(crate) fn new(path: Option<PathBuf>, funcs: Vec<(String, u32, u32)>) -> Self {
        let per_func = vec![[0; 2]; funcs.len()];
        Self {
            path,
            func_names: funcs,
            per_func,
            ..Self::default()
        }
    }

    /// Before an instruction: classify it and count it.
    pub(crate) fn pre(&mut self, instr: u32, base: u32, cycle: u64, in_wait: bool) {
        self.acc = Access::classify(instr, base);
        self.sp_base = (instr >> 21) & 31 == 29;
        self.counting = !in_wait;
        self.end_cycle = cycle;
        if in_wait {
            self.wait_instrs += 1;
            return;
        }
        self.first_cycle.get_or_insert(cycle);
        self.instrs += 1;
        let opcode = instr >> 26;
        if matches!(opcode, 0x32 | 0x3a) {
            self.cop2_mem += 1;
        }
        match self.acc {
            Access::RamLoad => {
                self.ram_loads += 1;
                self.ram_loads_sp += u64::from(self.sp_base);
            }
            Access::RamStore => self.ram_stores += 1,
            Access::SpadLoad => self.spad_loads += 1,
            Access::SpadStore => self.spad_stores += 1,
            Access::IoLoad => self.io_loads += 1,
            Access::IoStore => self.io_stores += 1,
            Access::Other | Access::None => {}
        }
    }

    /// The access class `pre` saw.
    pub(crate) fn access(&self) -> Access {
        self.acc
    }

    /// A shadow ends at `pos` (1..=7). `reads_loaded` and `icache_miss` are the rule's other tests.
    pub(crate) fn end(
        &mut self,
        pos: u8,
        touches_bus: bool,
        reads_loaded: bool,
        icache_miss: bool,
        exceptions_now: u64,
    ) {
        let Some(open) = self.open.take() else {
            return;
        };
        let reason = if open.exceptions_at_open != exceptions_now {
            self.exception_closes += 1;
            R_EXC
        } else if touches_bus {
            self.acc.reason()
        } else if reads_loaded {
            R_READS
        } else if icache_miss {
            R_ICACHE
        } else {
            R_EXPIRY
        };
        self.ends[reason][usize::from(pos).min(7)] += 1;
    }

    /// An exception was entered inside the open shadow: close it, position `pos`.
    pub(crate) fn close_for_exception(&mut self, pos: u8) {
        if self.open.take().is_some() {
            self.exception_closes += 1;
            self.ends[R_EXC][usize::from(pos).min(7)] += 1;
        }
    }

    /// Whether a tracked shadow is open, and the exception count it opened at.
    pub(crate) fn open_exceptions(&self) -> Option<u64> {
        self.open.as_ref().map(|o| o.exceptions_at_open)
    }

    /// An instruction hides at `pos`.
    pub(crate) fn hide(&mut self, pos: u8) {
        if !self.counting {
            // Hidden work is counted even when the instruction is inside a wait range only if
            // its shadow is tracked.
            if self.open.is_none() {
                return;
            }
        }
        self.hidden += 1;
        self.hidden_by_pos[usize::from(pos).min(7)] += 1;
        match self.open.as_ref().and_then(|o| o.func) {
            Some(f) => self.per_func[f][1] += 1,
            None if self.open.is_none() => self.hidden_untracked += 1,
            None => {}
        }
    }

    /// The instruction just run opened a shadow.
    pub(crate) fn opened(&mut self, func: Option<usize>, exceptions_now: u64) {
        self.open = None;
        if !self.counting {
            return;
        }
        self.opened += 1;
        match self.acc {
            Access::RamLoad => {
                self.opened_ram += 1;
                self.opened_ram_sp += u64::from(self.sp_base);
            }
            _ => self.opened_io += 1,
        }
        if let Some(f) = func {
            self.per_func[f][0] += 1;
        }
        self.open = Some(OpenShadow {
            func,
            exceptions_at_open: exceptions_now,
        });
    }

    /// Render the census as text (key lines, the reason table, then functions).
    pub fn render(&self, window_cycles: u64, wait_cycles: u64, funcs: &[(String, u32, u32, u64, u64)]) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        let _ = writeln!(s, "window_cycles {window_cycles}");
        let _ = writeln!(s, "wait_cycles {wait_cycles}");
        let _ = writeln!(s, "work_cycles {}", window_cycles.saturating_sub(wait_cycles));
        let _ = writeln!(s, "first_cycle {}", self.first_cycle.unwrap_or(0));
        let _ = writeln!(s, "end_cycle {}", self.end_cycle);
        let _ = writeln!(s, "instrs {}", self.instrs);
        let _ = writeln!(s, "wait_instrs_skipped {}", self.wait_instrs);
        let _ = writeln!(s, "ram_loads {}", self.ram_loads);
        let _ = writeln!(s, "ram_loads_sp_base {}", self.ram_loads_sp);
        let _ = writeln!(s, "ram_loads_other_base {}", self.ram_loads - self.ram_loads_sp);
        let _ = writeln!(s, "ram_stores {}", self.ram_stores);
        let _ = writeln!(s, "spad_loads {}", self.spad_loads);
        let _ = writeln!(s, "spad_stores {}", self.spad_stores);
        let _ = writeln!(s, "io_loads {}", self.io_loads);
        let _ = writeln!(s, "io_stores {}", self.io_stores);
        let _ = writeln!(s, "cop2_mem_ops {}", self.cop2_mem);
        let _ = writeln!(s, "shadows_opened {}", self.opened);
        let _ = writeln!(s, "shadows_opened_ram_load {}", self.opened_ram);
        let _ = writeln!(s, "shadows_opened_ram_load_sp_base {}", self.opened_ram_sp);
        let _ = writeln!(s, "shadows_opened_ram_load_other_base {}", self.opened_ram - self.opened_ram_sp);
        let _ = writeln!(s, "shadows_opened_ondie_io_load {}", self.opened_io);
        let _ = writeln!(s, "hidden_instrs {}", self.hidden);
        let _ = writeln!(s, "hidden_untracked {}", self.hidden_untracked);
        let _ = writeln!(s, "hidden_by_pos {:?}", &self.hidden_by_pos[3..=6]);
        let _ = writeln!(s, "exception_closes {}", self.exception_closes);
        let _ = writeln!(s, "#reason_by_position pos1 pos2 pos3 pos4 pos5 pos6 pos7");
        for (i, name) in REASONS.iter().enumerate() {
            let row = &self.ends[i][1..=7];
            let _ = writeln!(
                s,
                "end {name} {} {} {} {} {} {} {}",
                row[0], row[1], row[2], row[3], row[4], row[5], row[6]
            );
        }
        let _ = writeln!(s, "#func start end name charged_cycles instructions shadows_opened hidden unused_slots");
        for (i, (name, start, end, charged, instrs)) in funcs.iter().enumerate() {
            let [o, h] = self.per_func.get(i).copied().unwrap_or([0, 0]);
            if o == 0 && h == 0 {
                continue;
            }
            let _ = writeln!(
                s,
                "func {start:#010x} {end:#010x} {name} {charged} {instrs} {o} {h} {}",
                (4 * o).saturating_sub(h)
            );
        }
        s
    }

    pub(crate) fn func_names(&self) -> &[(String, u32, u32)] {
        &self.func_names
    }
}
