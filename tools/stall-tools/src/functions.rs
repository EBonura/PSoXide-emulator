//! Cycles by kind per function: `--stall-pc-log` rows summed over the symbols
//! of a linker map (`stall-by-function`), and two such runs side by side
//! (`stall-diff`).
//!
//! The map is the one `lld` writes with `-Map` (`VMA LMA Size Align Out In
//! Symbol`); a symbol is a line with alignment 1 and a size, owning
//! `[VMA, VMA + Size)`. Rows whose PC no symbol covers are summed under `?`.
//! Counts for one name are added together, so a generic function that was
//! instantiated twice is one entry.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use crate::csv::Table;

/// The stall log's counter columns, in file order (after the key).
pub const COLUMNS: [&str; 12] = [
    "instructions",
    "issue_cycles",
    "ram_load_stall_cycles",
    "stack_ram_load_stall_cycles",
    "ram_store_stall_cycles",
    "mmio_stall_cycles",
    "icache_refill_stall_cycles",
    "uncached_fetch_stall_cycles",
    "gte_busy_stall_cycles",
    "muldiv_interlock_stall_cycles",
    "other_stall_cycles",
    "total_cycles",
];

/// The kinds a stall comparison looks at, unless `--kinds` says otherwise.
pub const DEFAULT_KINDS: [&str; 6] = [
    "stack_ram_load_stall_cycles",
    "ram_load_stall_cycles",
    "ram_store_stall_cycles",
    "icache_refill_stall_cycles",
    "mmio_stall_cycles",
    "muldiv_interlock_stall_cycles",
];

pub type Counters = [u64; COLUMNS.len()];

pub struct Symbol {
    pub start: u32,
    pub size: u32,
    pub name: String,
}

/// Symbols of a linker map, sorted by address.
pub fn parse_map(text: &str) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    for line in text.lines() {
        let mut rest = line;
        let mut field = [""; 4];
        for slot in &mut field {
            rest = rest.trim_start();
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            *slot = &rest[..end];
            rest = &rest[end..];
        }
        let name = rest.trim();
        let (Ok(start), Ok(size)) = (
            u32::from_str_radix(field[0], 16),
            u32::from_str_radix(field[2], 16),
        ) else {
            continue;
        };
        if field[0].len() != 8 || size == 0 || field[3] != "1" || name.is_empty() {
            continue;
        }
        // Input-section lines name an object file; assignments name no symbol.
        if name.starts_with('/') || name.contains(" = ") || name.contains(":(") {
            continue;
        }
        symbols.push(Symbol {
            start,
            size,
            name: name.to_string(),
        });
    }
    symbols.sort_by(|a, b| a.start.cmp(&b.start).then(a.size.cmp(&b.size)));
    symbols
}

fn lookup(symbols: &[Symbol], pc: u32) -> Option<&Symbol> {
    // The last symbol that starts at or below `pc`; an enclosing symbol with
    // a smaller start is not searched for, which is right for a function map.
    let after = symbols.partition_point(|s| s.start <= pc);
    let symbol = symbols.get(after.checked_sub(1)?)?;
    (pc - symbol.start < symbol.size).then_some(symbol)
}

/// One stall log, read.
pub struct StallLog {
    /// Per key (a word or a line address): the counters.
    pub rows: BTreeMap<u32, Counters>,
    /// What the route tick open at the end of the run spent.
    pub open_tick: Counters,
    pub by_line: bool,
}

impl StallLog {
    pub fn read(path: &Path) -> Result<StallLog, String> {
        let table = Table::read(path)?;
        let by_line = table.col("line_pc").is_ok();
        let key = table.col(if by_line { "line_pc" } else { "pc" })?;
        let columns: Vec<usize> = COLUMNS
            .iter()
            .map(|name| table.col(name))
            .collect::<Result<_, _>>()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let counters = |row: &[String]| -> Counters {
            let mut out = [0u64; COLUMNS.len()];
            for (slot, &column) in out.iter_mut().zip(&columns) {
                *slot = row.get(column).and_then(|v| v.parse().ok()).unwrap_or(0);
            }
            out
        };
        let mut log = StallLog {
            rows: BTreeMap::new(),
            open_tick: [0; COLUMNS.len()],
            by_line,
        };
        for row in &table.rows {
            let name = row.get(key).map_or("", String::as_str);
            if name == "open_tick" {
                log.open_tick = counters(row);
            } else if let Ok(pc) = u32::from_str_radix(name.trim_start_matches("0x"), 16) {
                log.rows.insert(pc, counters(row));
            }
        }
        Ok(log)
    }

    /// Per-kind totals over every row, less the still-open tick: what the
    /// cycle log (which has rows for closed ticks only) holds.
    pub fn closed_totals(&self) -> Counters {
        let mut total = [0u64; COLUMNS.len()];
        for row in self.rows.values() {
            for (sum, v) in total.iter_mut().zip(row) {
                *sum += v;
            }
        }
        for (sum, v) in total.iter_mut().zip(&self.open_tick) {
            *sum = sum.saturating_sub(*v);
        }
        total
    }

    /// Counters per function name.
    pub fn by_function(&self, symbols: &[Symbol]) -> BTreeMap<String, Counters> {
        let mut out: BTreeMap<String, Counters> = BTreeMap::new();
        for (&pc, row) in &self.rows {
            let name = lookup(symbols, pc).map_or("?", |s| s.name.as_str());
            let slot = out.entry(name.to_string()).or_insert([0; COLUMNS.len()]);
            for (sum, v) in slot.iter_mut().zip(row) {
                *sum += v;
            }
        }
        out
    }
}

fn index(kind: &str) -> Result<usize, String> {
    COLUMNS
        .iter()
        .position(|c| *c == kind)
        .ok_or_else(|| format!("unknown kind {kind}; one of {}", COLUMNS.join(", ")))
}

/// Compare a log's totals with a `--cpu-cycle-profile-log` file over the same
/// window (`from_tick` is the `--stall-pc-start-route-tick` it was run with).
/// Returns one line per kind and whether every kind is equal.
pub fn reconcile(log: &StallLog, cycles: &Path, from_tick: u64) -> Result<(String, bool), String> {
    let table = Table::read(cycles)?;
    let closed = log.closed_totals();
    let mut text = String::new();
    let mut ok = true;
    for (i, name) in COLUMNS.iter().enumerate() {
        if *name == "instructions" {
            continue;
        }
        // A tick's row is labelled with the tick that starts after it.
        let want: u64 = table
            .rows
            .iter()
            .filter(|r| table.num(r, "route_tick") > from_tick)
            .map(|r| {
                table.num(
                    r,
                    if *name == "total_cycles" {
                        "profiled_cpu_cycles"
                    } else {
                        name
                    },
                )
            })
            .sum();
        let verdict = if want == closed[i] { "equal" } else { "DIFFER" };
        ok &= want == closed[i];
        let _ = writeln!(
            text,
            "  {name:<30} stall log {:>13}  cycle log {want:>13}  {verdict}",
            closed[i]
        );
    }
    Ok((text, ok))
}

fn short(name: &str) -> String {
    const MAX: usize = 88;
    if name.chars().count() <= MAX {
        return name.to_string();
    }
    let cut: String = name.chars().take(MAX - 3).collect();
    format!("{cut}...")
}

/// `stall-by-function`: the heaviest functions for each kind.
pub fn by_function_report(
    log: &StallLog,
    symbols: &[Symbol],
    kinds: &[&str],
    top: usize,
) -> Result<String, String> {
    let functions = log.by_function(symbols);
    let mut text = String::new();
    if log.by_line {
        let _ = writeln!(
            text,
            "note: the log is keyed by 16-byte line; a line is placed by its first word. \
             Rerun with --pc-log-words for exact sums."
        );
    }
    for kind in kinds {
        let k = index(kind)?;
        let total: u64 = functions.values().map(|c| c[k]).sum();
        let mut ranked: Vec<(&String, u64)> = functions.iter().map(|(n, c)| (n, c[k])).collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let _ = writeln!(
            text,
            "{kind}: {total} in {} functions (includes {} from the tick open at the end)",
            functions.len(),
            log.open_tick[k]
        );
        for (name, value) in ranked.iter().take(top).filter(|(_, v)| *v > 0) {
            let share = 100.0 * *value as f64 / total.max(1) as f64;
            let _ = writeln!(text, "  {value:>13} {share:5.1}%  {}", short(name));
        }
    }
    Ok(text)
}

/// `stall-diff`: per kind, the functions whose counts differ most.
pub fn diff_report(
    a: (&StallLog, &[Symbol]),
    b: (&StallLog, &[Symbol]),
    kinds: &[&str],
    top: usize,
) -> Result<String, String> {
    let (fa, fb) = (a.0.by_function(a.1), b.0.by_function(b.1));
    let names: std::collections::BTreeSet<&String> = fa.keys().chain(fb.keys()).collect();
    let zero = [0u64; COLUMNS.len()];
    let mut text = String::new();
    for kind in kinds {
        let k = index(kind)?;
        let (ta, tb): (u64, u64) = (
            fa.values().map(|c| c[k]).sum(),
            fb.values().map(|c| c[k]).sum(),
        );
        let _ = writeln!(
            text,
            "{kind}: A {ta}  B {tb}  B-A {:+}  ({:+.1}%)  (open tick at the end: A {}, B {})",
            tb as i64 - ta as i64,
            100.0 * (tb as f64 - ta as f64) / ta.max(1) as f64,
            a.0.open_tick[k],
            b.0.open_tick[k]
        );
        let mut ranked: Vec<(i64, u64, u64, &String)> = names
            .iter()
            .map(|&n| {
                let (x, y) = (fa.get(n).unwrap_or(&zero)[k], fb.get(n).unwrap_or(&zero)[k]);
                (y as i64 - x as i64, x, y, n)
            })
            .filter(|r| r.0 != 0)
            .collect();
        ranked.sort_by(|p, q| q.0.abs().cmp(&p.0.abs()).then(p.3.cmp(q.3)));
        let _ = writeln!(text, "  {:>12} {:>12} {:>13}  function", "A", "B", "B-A");
        for (delta, x, y, name) in ranked.iter().take(top) {
            let _ = writeln!(text, "  {x:>12} {y:>12} {delta:>+13}  {}", short(name));
        }
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAP: &str = "     VMA      LMA     Size Align Out     In      Symbol
       0        0        0     1 RAM_BASE = 0x80000000
80010000 80010000      100     4 .text
80010000 80010000       40     4         /a/b/c.o:(.text.alpha)
80010000 80010000       40     1                 alpha
80010040 80010040       80     4         /a/b/c.o:(.text.beta)
80010040 80010040       80     1                 <T>::beta::<closure#0, U>
";

    fn log(rows: &[(u32, [u64; 12])], open: [u64; 12]) -> StallLog {
        StallLog {
            rows: rows.iter().copied().collect(),
            open_tick: open,
            by_line: false,
        }
    }

    #[test]
    fn map_yields_symbols_with_spaces_and_skips_sections() {
        let symbols = parse_map(MAP);
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[1].name, "<T>::beta::<closure#0, U>");
        assert_eq!(lookup(&symbols, 0x8001003c).unwrap().name, "alpha");
        assert_eq!(
            lookup(&symbols, 0x80010040).unwrap().name,
            "<T>::beta::<closure#0, U>"
        );
        assert!(lookup(&symbols, 0x800100c0).is_none());
        assert!(lookup(&symbols, 0x8000f000).is_none());
    }

    #[test]
    fn functions_sum_their_words_and_unmapped_pcs_go_to_question_mark() {
        let symbols = parse_map(MAP);
        let mut one = [0u64; 12];
        one[3] = 7;
        let mut two = [0u64; 12];
        two[3] = 5;
        let log = log(
            &[(0x8001_0000, one), (0x8001_0004, two), (0xbfc0_0000, one)],
            [0; 12],
        );
        let f = log.by_function(&symbols);
        assert_eq!(f["alpha"][3], 12);
        assert_eq!(f["?"][3], 7);
        let text = by_function_report(&log, &symbols, &["stack_ram_load_stall_cycles"], 5).unwrap();
        assert!(text.contains("stack_ram_load_stall_cycles: 19 in 2 functions"));
        assert!(text.contains("12"));
    }

    #[test]
    fn diff_ranks_by_size_of_change_and_open_tick_is_not_a_closed_total() {
        let symbols = parse_map(MAP);
        let mut a = [0u64; 12];
        a[10] = 100;
        let mut b1 = [0u64; 12];
        b1[10] = 130;
        let mut b2 = [0u64; 12];
        b2[10] = 10;
        let la = log(&[(0x8001_0000, a)], [0; 12]);
        let mut open = [0u64; 12];
        open[10] = 4;
        let lb = log(&[(0x8001_0000, b1), (0x8001_0040, b2)], open);
        let text =
            diff_report((&la, &symbols), (&lb, &symbols), &["other_stall_cycles"], 5).unwrap();
        assert!(text.contains("A 100  B 140  B-A +40"));
        assert!(text.contains("open tick at the end: A 0, B 4"));
        let alpha = text.find("alpha").unwrap();
        let beta = text.find("beta").unwrap();
        assert!(alpha < beta, "+30 outranks +10");
        assert_eq!(lb.closed_totals()[10], 136);
    }

    #[test]
    fn reconcile_compares_closed_ticks_after_the_start() {
        let dir = std::env::temp_dir().join(format!("stall-fn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cycles = dir.join("cycles.csv");
        std::fs::write(
            &cycles,
            "route_tick,issue_cycles,profiled_cpu_cycles\n0,0,0\n1,10,12\n2,20,25\n",
        )
        .unwrap();
        let mut row = [0u64; 12];
        row[1] = 30;
        row[11] = 37;
        let mut open = [0u64; 12];
        open[1] = 3;
        open[11] = 3;
        let mut tail = row;
        tail[1] += 3;
        tail[11] += 3;
        let ok = log(&[(0x8001_0000, tail)], open);
        let (text, same) = reconcile(&ok, &cycles, 0).unwrap();
        assert!(same, "{text}");
        let (_, same) = reconcile(&ok, &cycles, 1).unwrap();
        assert!(!same, "starting after tick 1 drops its row");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
