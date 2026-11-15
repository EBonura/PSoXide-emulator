//! `stall-tools`: reports and gates for the stall-free engine work.
//!
//! * `report <run-dir> [--from-tick N]` prints the stall budget of a window.
//! * `hot-lines <datalog.csv> --bytes N [--window] [--out ranges.txt]` ranks the
//!   data-line log and writes a `PSOXIDE_LIMIT_FAST_DATA` range file.
//! * `stall-by-function <stall.csv> <game.map> [--cycles cycles.csv] [--from-tick N]
//!   [--kinds a,b] [--top N]` sums a `--stall-pc-log` over the functions of a
//!   linker map, and checks it against the cycle log.
//! * `stall-diff <stall-A.csv> <map-A> <stall-B.csv> <map-B> [--cycles-a F --cycles-b F]
//!   [--from-tick N] [--kinds a,b] [--top N]` ranks the functions whose cycles
//!   of each kind differ most between two runs.
//! * `spill-gate <image> <kernels.txt> [--max-bytes N]` fails when a registered
//!   kernel has stack traffic or a call inside a loop, or is over budget.
//!   `<image>` is a PSX-EXE or a 2 MB RAM dump.

mod csv;
mod functions;
mod gate;
mod lines;
mod mips;
mod report;

use std::path::Path;
use std::process::ExitCode;

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn number(args: &[String], name: &str, default: u64) -> Result<usize, String> {
    flag(args, name)
        .map_or(Ok(default as usize), str::parse)
        .map_err(|e| format!("{name}: {e}"))
}

fn kinds(args: &[String]) -> Vec<&str> {
    flag(args, "--kinds").map_or_else(
        || functions::DEFAULT_KINDS.to_vec(),
        |list| list.split(',').collect(),
    )
}

/// Reconcile a stall log with the cycle log named by `--flag`, when given.
fn check(
    args: &[String],
    name: &str,
    log: &functions::StallLog,
    from: usize,
) -> Result<bool, String> {
    let Some(path) = flag(args, name) else {
        return Ok(true);
    };
    let (text, ok) = functions::reconcile(log, Path::new(path), from as u64)?;
    println!(
        "reconciliation with {path}: {}",
        if ok { "equal" } else { "DIFFER" }
    );
    print!("{text}");
    Ok(ok)
}

fn run(args: &[String]) -> Result<bool, String> {
    match args.first().map(String::as_str) {
        Some("report") => {
            let dir = args
                .get(1)
                .ok_or("usage: report <run-dir> [--from-tick N]")?;
            let from = flag(args, "--from-tick")
                .map_or(Ok(0), str::parse)
                .map_err(|e| format!("{e}"))?;
            print!("{}", report::build(Path::new(dir), from)?.render());
            Ok(true)
        }
        Some("hot-lines") => {
            let path = args
                .get(1)
                .ok_or("usage: hot-lines <datalog.csv> --bytes N [--window] [--out FILE]")?;
            let bytes: u32 = flag(args, "--bytes")
                .ok_or("--bytes is required")?
                .parse()
                .map_err(|e| format!("{e}"))?;
            let log =
                lines::parse(&std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?);
            let (ranges, share) = if args.iter().any(|a| a == "--window") {
                let ((s, e), share) = lines::window(&log, bytes);
                (vec![(s, e)], share)
            } else {
                lines::top(&log, bytes)
            };
            let text: String = ranges
                .iter()
                .enumerate()
                .map(|(i, (s, e))| format!("{s:08x} {e:08x} r{i}\n"))
                .collect();
            match flag(args, "--out") {
                Some(out) => std::fs::write(out, &text).map_err(|e| format!("{out}: {e}"))?,
                None => print!("{text}"),
            }
            eprintln!("covers {share:.1}% of RAM-load stall cycles");
            Ok(true)
        }
        Some("stall-by-function") => {
            let usage = "usage: stall-by-function <stall.csv> <game.map> [--cycles F] [--from-tick N] [--kinds a,b] [--top N]";
            let log = functions::StallLog::read(Path::new(args.get(1).ok_or(usage)?))?;
            let map = args.get(2).ok_or(usage)?;
            let symbols = functions::parse_map(
                &std::fs::read_to_string(map).map_err(|e| format!("{map}: {e}"))?,
            );
            let kinds = kinds(args);
            let top = number(args, "--top", 25)?;
            print!(
                "{}",
                functions::by_function_report(&log, &symbols, &kinds, top)?
            );
            check(args, "--cycles", &log, number(args, "--from-tick", 0)?)
        }
        Some("stall-diff") => {
            let usage = "usage: stall-diff <stall-A.csv> <map-A> <stall-B.csv> <map-B> [--cycles-a F --cycles-b F] [--from-tick N] [--kinds a,b] [--top N]";
            let load =
                |log: usize,
                 map: usize|
                 -> Result<(functions::StallLog, Vec<functions::Symbol>), String> {
                    let log = functions::StallLog::read(Path::new(args.get(log).ok_or(usage)?))?;
                    let map = args.get(map).ok_or(usage)?;
                    let text = std::fs::read_to_string(map).map_err(|e| format!("{map}: {e}"))?;
                    Ok((log, functions::parse_map(&text)))
                };
            let (a, a_map) = load(1, 2)?;
            let (b, b_map) = load(3, 4)?;
            print!(
                "{}",
                functions::diff_report(
                    (&a, &a_map),
                    (&b, &b_map),
                    &kinds(args),
                    number(args, "--top", 25)?
                )?
            );
            let from = number(args, "--from-tick", 0)?;
            let first = check(args, "--cycles-a", &a, from)?;
            let second = check(args, "--cycles-b", &b, from)?;
            Ok(first && second)
        }
        Some("spill-gate") => {
            let image = args
                .get(1)
                .ok_or("usage: spill-gate <image> <kernels.txt> [--max-bytes N]")?;
            let kernels = gate::parse_ranges(
                &std::fs::read_to_string(args.get(2).ok_or("kernels file required")?)
                    .map_err(|e| format!("{e}"))?,
            )?;
            let budget: u32 = flag(args, "--max-bytes")
                .map_or(Ok(u32::MAX), str::parse)
                .map_err(|e| format!("{e}"))?;
            let bytes = std::fs::read(image).map_err(|e| format!("{image}: {e}"))?;
            // A PSX-EXE starts with "PS-X EXE"; its text loads at the address in the header.
            let (base, data): (u32, &[u8]) = if bytes.starts_with(b"PS-X EXE") {
                let load = u32::from_le_bytes(bytes[0x18..0x1c].try_into().unwrap());
                (load, &bytes[0x800..])
            } else {
                (0x8000_0000, &bytes[..])
            };
            let word = |a: u32| {
                let o = (a.wrapping_sub(base)) as usize;
                data.get(o..o + 4)
                    .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            };
            let mut ok = true;
            for k in &kernels {
                let findings = gate::check(k, budget, &word);
                println!(
                    "{:<24} {:5} bytes  {}",
                    k.name,
                    k.end - k.start,
                    if findings.is_empty() { "ok" } else { "FAIL" }
                );
                for f in &findings {
                    println!("    {f:?}");
                }
                ok &= findings.is_empty();
            }
            Ok(ok)
        }
        _ => Err(
            "usage: stall-tools report|hot-lines|stall-by-function|stall-diff|spill-gate ..."
                .into(),
        ),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("stall-tools: {e}");
            ExitCode::from(2)
        }
    }
}
