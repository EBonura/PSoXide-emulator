//! `--stall-pc-log`: the CPU cycle profile, split by the instruction that
//! spent each cycle.
//!
//! `--cpu-cycle-profile-log` says how many cycles of each kind a route tick
//! cost; the per-line stall logs each carry one kind. This log carries all of
//! them at once, per program counter, so a script with a link map can sum
//! every kind per function and compare two builds of the same game.
//!
//! The frontend reads the profile counters after every instruction and adds
//! the difference to the row of the PC the instruction was fetched from, so
//! the core has no hook for it and the log can only exist in the per-step
//! loop that the other per-PC logs already use. Over any window the rows add
//! up to the cycle profile's totals exactly (see [`StallPcLog::finish`]).
//!
//! Rows are keyed like `--pc-line-log`: by 16-byte I-cache line (column
//! `line_pc`), or by 4-byte instruction word (column `pc`) with
//! `--pc-log-words`. Use words to sum by function: a line can hold the tail of
//! one function and the head of the next. KUSEG, KSEG0 and KSEG1 aliases of
//! one location share a row.
//!
//! Format: a header line, one row per key that cost anything, in address
//! order, then one final row whose key is `open_tick`. Columns, all but the
//! key as decimal integers:
//!
//! `pc|line_pc,instructions,issue_cycles,ram_load_stall_cycles,`
//! `stack_ram_load_stall_cycles,ram_store_stall_cycles,mmio_stall_cycles,`
//! `icache_refill_stall_cycles,uncached_fetch_stall_cycles,`
//! `gte_busy_stall_cycles,muldiv_interlock_stall_cycles,other_stall_cycles,`
//! `total_cycles`
//!
//! The cycle names are `--cpu-cycle-profile-log`'s. `stack_ram_load_stall_cycles`
//! is the part of `ram_load_stall_cycles` from loads addressed from `$sp`; it
//! is not added into `total_cycles`. The `open_tick` row repeats the cycles
//! of the route tick that was still open when the run ended (a run that stops
//! on a poll stops inside a tick), which `--cpu-cycle-profile-log` has no row
//! for yet; the rows above include them, so subtract it to compare with the
//! cycle log (`stall-tools stall-by-function` does).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use emulator_core::CpuCycleProfileSnapshot;
use psx_hw::memory;

use super::{PC_LINE_SHIFT, PC_WORD_SHIFT};

/// Counters per key: instructions, then the profile's nine buckets.
const COUNTERS: usize = 11;

const HEADER_TAIL: &str = "instructions,issue_cycles,ram_load_stall_cycles,\
stack_ram_load_stall_cycles,ram_store_stall_cycles,mmio_stall_cycles,\
icache_refill_stall_cycles,uncached_fetch_stall_cycles,gte_busy_stall_cycles,\
muldiv_interlock_stall_cycles,other_stall_cycles,total_cycles";

type Counters = [u64; COUNTERS];

/// One instruction: itself, and the cycles the profile gained while it ran.
fn counters(delta: CpuCycleProfileSnapshot) -> Counters {
    [
        1,
        delta.issue_cycles,
        delta.ram_load_stall_cycles,
        delta.stack_ram_load_stall_cycles,
        delta.ram_store_stall_cycles,
        delta.mmio_stall_cycles,
        delta.icache_refill_stall_cycles,
        delta.uncached_fetch_stall_cycles,
        delta.gte_busy_stall_cycles,
        delta.muldiv_interlock_stall_cycles,
        delta.other_stall_cycles,
    ]
}

fn add(into: &mut Counters, from: &Counters) {
    for (to, from) in into.iter_mut().zip(from) {
        *to = to.saturating_add(*from);
    }
}

pub(super) struct StallPcLog {
    shift: u32,
    ram: Box<[Counters]>,
    bios: Box<[Counters]>,
    other: BTreeMap<u32, Counters>,
    /// The profile after the previous instruction.
    last: CpuCycleProfileSnapshot,
    /// What the open route tick has added so far.
    open_tick: Counters,
}

impl StallPcLog {
    pub(super) fn new(words: bool, start: CpuCycleProfileSnapshot) -> Self {
        let shift = if words { PC_WORD_SHIFT } else { PC_LINE_SHIFT };
        Self {
            shift,
            ram: vec![[0; COUNTERS]; emulator_core::ram::SIZE >> shift].into_boxed_slice(),
            bios: vec![[0; COUNTERS]; memory::bios::SIZE >> shift].into_boxed_slice(),
            other: BTreeMap::new(),
            last: start,
            open_tick: [0; COUNTERS],
        }
    }

    /// The instruction fetched from `pc` has retired and the profile now reads
    /// `now`. With `inside` false the cycles are consumed but not recorded
    /// (before the window starts).
    #[inline]
    pub(super) fn record(&mut self, pc: u32, now: CpuCycleProfileSnapshot, inside: bool) {
        let delta = now.delta_since(self.last);
        self.last = now;
        if !inside {
            return;
        }
        let row = counters(delta);
        add(&mut self.open_tick, &row);
        let physical = memory::to_physical(pc);
        let shift = self.shift;
        let slot = if physical < memory::ram::MIRROR_END {
            &mut self.ram[(physical as usize % emulator_core::ram::SIZE) >> shift]
        } else if (memory::bios::BASE..memory::bios::BASE + memory::bios::SIZE as u32)
            .contains(&physical)
        {
            &mut self.bios[(physical - memory::bios::BASE) as usize >> shift]
        } else {
            self.other
                .entry(pc >> shift << shift)
                .or_insert([0; COUNTERS])
        };
        add(slot, &row);
    }

    /// A route tick ended: the cycle log has a row for it now.
    #[inline]
    pub(super) fn close_tick(&mut self) {
        self.open_tick = [0; COUNTERS];
    }

    pub(super) fn finish(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("mkdir {}: {error}", parent.display()))?;
        }
        let file = std::fs::File::create(path)
            .map_err(|error| format!("create stall log {}: {error}", path.display()))?;
        let mut writer = std::io::BufWriter::new(file);
        let key = if self.shift == PC_WORD_SHIFT {
            "pc"
        } else {
            "line_pc"
        };
        let io = |error: std::io::Error| format!("write stall log {}: {error}", path.display());
        writeln!(writer, "{key},{HEADER_TAIL}").map_err(io)?;
        let shift = self.shift;
        let ram = self
            .ram
            .iter()
            .enumerate()
            .map(|(bucket, row)| (0x8000_0000 | ((bucket as u32) << shift), row));
        let bios = self
            .bios
            .iter()
            .enumerate()
            .map(|(bucket, row)| (0xbfc0_0000 | ((bucket as u32) << shift), row));
        let other = self.other.iter().map(|(&pc, row)| (pc, row));
        for (pc, row) in ram.chain(bios).chain(other) {
            if row[0] != 0 {
                write_row(&mut writer, &format!("0x{pc:08x}"), row).map_err(io)?;
            }
        }
        write_row(&mut writer, "open_tick", &self.open_tick).map_err(io)?;
        writer.flush().map_err(io)
    }
}

fn write_row(writer: &mut impl Write, key: &str, row: &Counters) -> std::io::Result<()> {
    // The stack column is a part of the RAM-load column, not a bucket.
    let total = row[1].saturating_add(row[2]).saturating_add(
        row[4..COUNTERS]
            .iter()
            .fold(0u64, |a, &v| a.saturating_add(v)),
    );
    write!(writer, "{key}")?;
    for value in row {
        write!(writer, ",{value}")?;
    }
    writeln!(writer, ",{total}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(f: impl FnOnce(&mut CpuCycleProfileSnapshot)) -> CpuCycleProfileSnapshot {
        let mut snapshot = CpuCycleProfileSnapshot::default();
        f(&mut snapshot);
        snapshot
    }

    fn read(path: &Path) -> Vec<Vec<String>> {
        let text = std::fs::read_to_string(path).expect("read stall log");
        let _ = std::fs::remove_file(path);
        text.lines()
            .map(|line| line.split(',').map(str::to_string).collect())
            .collect()
    }

    #[test]
    fn rows_carry_each_kind_to_the_pc_that_spent_it() {
        let path = std::env::temp_dir().join(format!("stall-pc-{}.csv", std::process::id()));
        let mut log = StallPcLog::new(true, CpuCycleProfileSnapshot::default());
        // A stack load: issue 2, 5 stall cycles, all of them from `$sp`.
        let after_load = profile(|p| {
            p.issue_cycles = 2;
            p.ram_load_stall_cycles = 5;
            p.stack_ram_load_stall_cycles = 5;
        });
        log.record(0x8001_0004, after_load, true);
        // The same word again through another alias, with a store and a refill.
        let after_store = profile(|p| {
            *p = after_load;
            p.issue_cycles += 2;
            p.ram_store_stall_cycles = 3;
            p.icache_refill_stall_cycles = 7;
        });
        log.record(0xa001_0004, after_store, true);
        // A multiply read right behind the multiply, next word.
        let after_mfhi = profile(|p| {
            *p = after_store;
            p.issue_cycles += 2;
            p.muldiv_interlock_stall_cycles = 11;
        });
        log.record(0x8001_0008, after_mfhi, true);
        log.finish(&path).expect("write stall log");
        let rows = read(&path);

        assert_eq!(
            rows[0].join(","),
            format!("pc,{HEADER_TAIL}"),
            "header names the cycle log's columns"
        );
        // instructions, issue, ram_load, stack_load, ram_store, mmio, icache,
        // uncached, gte, muldiv, other, total
        assert_eq!(
            rows[1].join(","),
            "0x80010004,2,4,5,5,3,0,7,0,0,0,0,19",
            "two aliases share a row; the stack column is not added into total"
        );
        assert_eq!(rows[2].join(","), "0x80010008,1,2,0,0,0,0,0,0,0,11,0,13");
        assert_eq!(rows[3][0], "open_tick");
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn line_keys_group_four_words_and_the_open_tick_resets() {
        let path = std::env::temp_dir().join(format!("stall-pc-line-{}.csv", std::process::id()));
        let mut log = StallPcLog::new(false, CpuCycleProfileSnapshot::default());
        let mut now = CpuCycleProfileSnapshot::default();
        for pc in [0x8000_1000u32, 0x8000_1004, 0x8000_100c] {
            now.issue_cycles += 2;
            log.record(pc, now, true);
        }
        log.close_tick();
        now.issue_cycles += 2;
        log.record(0x8000_1010, now, true);
        // Before the window: consumed, not recorded.
        now.issue_cycles += 2;
        log.record(0x8000_2000, now, false);
        log.finish(&path).expect("write stall log");
        let rows = read(&path);

        assert!(rows[0][0] == "line_pc");
        assert_eq!(rows[1].join(","), "0x80001000,3,6,0,0,0,0,0,0,0,0,0,6");
        assert_eq!(rows[2].join(","), "0x80001010,1,2,0,0,0,0,0,0,0,0,0,2");
        assert_eq!(
            rows[3].join(","),
            "open_tick,1,2,0,0,0,0,0,0,0,0,0,2",
            "only what came after the last route tick"
        );
        assert_eq!(rows.len(), 4, "nothing recorded outside the window");
    }
}
