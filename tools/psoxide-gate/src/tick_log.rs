//! Per-tick CSV of a journey run, for frame-time studies (`psoxide-gate run --tick-log`).
//!
//! One row per emulated vblank tick: bus cycles, retired instructions, whether the display
//! start flipped, the journey step, and any guest RAM words the caller asked for. The words are
//! read straight out of RAM, never through the bus, so the log does not change the run. With no
//! log requested the machine holds a `None` and pays one branch per tick.
//!
//! `tools/psoxide-frametime` turns the CSV into the frame-time pages.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use emulator_core::telemetry::{stage, GuestTelemetryKind};
use emulator_core::Bus;

use crate::symbols::Symbols;

/// One RAM word to log: a column name and where it lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WordSpec {
    /// Column header (`ram_xxxxxxxx` when the spec gave no name).
    pub name: String,
    /// Address, or a symbol name resolved when the log is created.
    pub place: Place,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    Addr(u32),
    Symbol { name: String, offset: u32 },
}

impl WordSpec {
    /// Parse `NAME=SYMBOL+OFFSET`, `NAME=0xADDR`, `SYMBOL`, `SYMBOL+OFFSET` or `0xADDR`.
    /// The offset is decimal or `0x` hex.
    pub fn parse(text: &str) -> Result<WordSpec, String> {
        let (name, target) = match text.split_once('=') {
            Some((n, t)) => (Some(n.trim()), t.trim()),
            None => (None, text.trim()),
        };
        if target.is_empty() || name.is_some_and(str::is_empty) {
            return Err(format!("bad RAM word `{text}`"));
        }
        let place = match parse_num(target) {
            Some(addr) => Place::Addr(addr),
            None => {
                let (sym, off) = match target.split_once('+') {
                    Some((s, o)) => (
                        s.trim(),
                        parse_num(o.trim()).ok_or_else(|| format!("bad offset in `{text}`"))?,
                    ),
                    None => (target, 0),
                };
                Place::Symbol {
                    name: sym.to_string(),
                    offset: off,
                }
            }
        };
        let name = match (name, &place) {
            (Some(n), _) => n.to_string(),
            (None, Place::Addr(a)) => format!("ram_{a:08x}"),
            (None, Place::Symbol { name, offset: 0 }) => name.clone(),
            (None, Place::Symbol { name, offset }) => format!("{name}+{offset}"),
        };
        if name.contains(',') || name.contains('\n') {
            return Err(format!("RAM word name `{name}` may not hold a comma"));
        }
        Ok(WordSpec { name, place })
    }
}

fn parse_num(s: &str) -> Option<u32> {
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return u32::from_str_radix(h, 16).ok();
    }
    s.parse().ok()
}

/// Guest telemetry stages summed per tick, as the last columns: how many telemetry events the
/// guest wrote, and the bus cycles spent inside its update, render and present stages. All
/// zero unless the game was built to emit telemetry (a profile build, not the shipping disc).
const STAGES: [(u16, &str); 3] = [
    (stage::UPDATE, "tel_update"),
    (stage::RENDER, "tel_render"),
    (stage::PRESENT, "tel_present"),
];

/// Open log file with the columns it writes.
pub struct TickLog {
    out: BufWriter<File>,
    addrs: Vec<usize>,
    boot: u64,
    step: u64,
    last_cycles: u64,
    last_cpu: u64,
    /// Bus cycle at which each stage in `STAGES` began, while it is open.
    open: [Option<u64>; STAGES.len()],
}

impl TickLog {
    /// Create the file and write the header. Symbols are resolved here, so an unknown name fails
    /// before the first tick.
    pub fn create(path: &Path, words: &[WordSpec], symbols: &Symbols) -> Result<TickLog, String> {
        let mut head =
            String::from("boot,tick,step,bus_cycles,cycle_delta,cpu_delta,flipped,polls");
        let mut addrs = Vec::new();
        for w in words {
            let addr = match &w.place {
                Place::Addr(a) => *a,
                Place::Symbol { name, offset } => symbols.resolve(name)?.wrapping_add(*offset),
            };
            addrs.push((addr & 0x1f_ffff) as usize);
            head.push(',');
            head.push_str(&w.name);
        }
        head.push_str(",tel_events");
        for (_, name) in STAGES {
            head.push(',');
            head.push_str(name);
        }
        let file = File::create(path).map_err(|e| format!("tick log {}: {e}", path.display()))?;
        let mut out = BufWriter::new(file);
        writeln!(out, "{head}").map_err(|e| format!("tick log {}: {e}", path.display()))?;
        Ok(TickLog {
            out,
            addrs,
            boot: 1,
            step: 0,
            last_cycles: 0,
            last_cpu: 0,
            open: [None; STAGES.len()],
        })
    }

    /// The journey step that is about to run (1-based, as the `-v` output counts).
    pub fn set_step(&mut self, step: u64) {
        self.step = step;
    }

    /// The machine was rebooted (power-cut steps): deltas restart, `boot` counts up.
    pub fn reboot(&mut self) {
        self.boot += 1;
        self.last_cycles = 0;
        self.last_cpu = 0;
        self.open = [None; STAGES.len()];
    }

    /// Append the row for the tick that just ended.
    pub fn record(&mut self, tick: u64, bus: &mut Bus, cpu_tick: u64, flipped: bool) {
        let cycles = bus.cycles();
        let mut row = format!(
            "{},{tick},{},{cycles},{},{},{},{}",
            self.boot,
            self.step,
            cycles.saturating_sub(self.last_cycles),
            cpu_tick.saturating_sub(self.last_cpu),
            u8::from(flipped),
            bus.port1_completed_polls(),
        );
        let ram = bus.ram();
        for &off in &self.addrs {
            let v = ram
                .get(off..off + 4)
                .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
            row.push(',');
            row.push_str(&v.to_string());
        }
        let events = bus.telemetry.drain_events();
        let mut spent = [0u64; STAGES.len()];
        for e in &events {
            let Some(i) = STAGES.iter().position(|(id, _)| *id == e.id) else {
                continue;
            };
            match e.kind {
                GuestTelemetryKind::StageBegin => self.open[i] = Some(e.cycles),
                GuestTelemetryKind::StageEnd => {
                    if let Some(begin) = self.open[i].take() {
                        spent[i] += e.cycles.saturating_sub(begin);
                    }
                }
                _ => {}
            }
        }
        row.push_str(&format!(",{}", events.len()));
        for s in spent {
            row.push_str(&format!(",{s}"));
        }
        self.last_cycles = cycles;
        self.last_cpu = cpu_tick;
        // Flushed per row: the process may exit without dropping the log, and the last
        // rows (often the last load) are the ones a study needs.
        let _ = writeln!(self.out, "{row}");
        let _ = self.out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_word_specs() {
        let w = WordSpec::parse("map=WORLD_ARENA+4").unwrap();
        assert_eq!(w.name, "map");
        assert_eq!(
            w.place,
            Place::Symbol {
                name: "WORLD_ARENA".into(),
                offset: 4
            }
        );
        let w = WordSpec::parse("0x800e64cc").unwrap();
        assert_eq!(w.name, "ram_800e64cc");
        assert_eq!(w.place, Place::Addr(0x800e_64cc));
        assert_eq!(WordSpec::parse("SIM_NOW").unwrap().name, "SIM_NOW");
        assert!(WordSpec::parse("a=").is_err());
        assert!(WordSpec::parse("X+zz").is_err());
    }

    #[test]
    fn unknown_symbol_fails_before_the_first_tick() {
        let dir = std::env::temp_dir().join(format!("tick-log-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let words = [WordSpec::parse("NOPE").unwrap()];
        let err = TickLog::create(&dir.join("t.csv"), &words, &Symbols::default());
        assert!(err.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
