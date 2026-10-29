//! `psoxide-frametime`: frame-time pages from a `psoxide-gate run --tick-log` log.
//!
//! ```text
//! psoxide-frametime report  CONFIG TICKS.csv [--gate-log LOG] [--note TEXT]... --out DIR
//! psoxide-frametime compare CONFIG BEFORE.csv AFTER.csv [--names BEFORE,AFTER] --out DIR
//! psoxide-frametime gate-args CONFIG       (the --tick-log-ram value for the config)
//! ```

mod compare;
mod config;
mod log;
mod page;
mod report;
mod stats;
#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use config::Config;
use log::{Run, TickLog};

#[derive(Parser)]
#[command(version, about = "Frame-time pages from a psoxide-gate tick log")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// One route: data/*.csv and index.html in the output folder.
    Report {
        /// The game's frametime.toml.
        config: PathBuf,
        /// Tick log written by `psoxide-gate run --tick-log`; several, comma separated, are
        /// consecutive segments of one route.
        #[arg(value_delimiter = ',', num_args = 1..)]
        ticks: Vec<PathBuf>,
        /// The gate's `-v` output, for step names (one per tick log, comma separated).
        #[arg(long, value_delimiter = ',')]
        gate_log: Vec<PathBuf>,
        /// A paragraph for the page's method section (the disc used, for one).
        #[arg(long)]
        note: Vec<String>,
        #[arg(long)]
        out: PathBuf,
    },
    /// Two runs of the same route (before and after a change) on one page.
    Compare {
        config: PathBuf,
        /// Tick log of the run before, or its segments comma separated.
        before: String,
        /// Tick log of the run after.
        after: String,
        /// Names for the two runs, `before,after` by default.
        #[arg(long, value_delimiter = ',', default_values = ["before", "after"])]
        names: Vec<String>,
        #[arg(long)]
        out: PathBuf,
    },
    /// Print the value for `--tick-log-ram` that logs the config's words.
    GateArgs { config: PathBuf },
}

fn measure(config: &Config, ticks: &[PathBuf], gate_logs: &[PathBuf]) -> Result<Run, String> {
    let mut parts = Vec::new();
    for p in ticks {
        let text = std::fs::read_to_string(p).map_err(|e| format!("read {}: {e}", p.display()))?;
        parts.push(TickLog::parse(&text).map_err(|e| format!("{}: {e}", p.display()))?);
    }
    let (log, offsets) = TickLog::join(parts)?;
    let mut labels = std::collections::HashMap::new();
    for (p, off) in gate_logs.iter().zip(&offsets) {
        let text = std::fs::read_to_string(p).map_err(|e| format!("read {}: {e}", p.display()))?;
        labels.extend(log::step_labels(&text, *off));
    }
    Run::measure(&log, config, labels).map_err(|e| format!("{}: {e}", ticks[0].display()))
}

/// Split `a.csv,b.csv` into the segments of one run.
fn segments(list: &str) -> Vec<PathBuf> {
    list.split(',').map(PathBuf::from).collect()
}

fn run(cli: Cli) -> Result<(), String> {
    match cli.command {
        Cmd::Report {
            config,
            ticks,
            gate_log,
            note,
            out,
        } => {
            let cfg = Config::load(&config)?;
            let run = measure(&cfg, &ticks, &gate_log)?;
            let rows = report::write(&run, &cfg, &note, &out)?;
            if let Some(a) = rows.last() {
                println!(
                    "{}: {} play frames, mean {:.2} fps, 1% low {:.2}, over 2 vblanks {:.1}%, over 3 {:.1}%, {} loads {:.1} s",
                    cfg.title, a.frames, a.mean_fps, a.p1_low_fps, a.pct_over_2, a.pct_over_3, a.loads, a.load_seconds
                );
            }
            println!("wrote {}", out.join("index.html").display());
            Ok(())
        }
        Cmd::Compare {
            config,
            before,
            after,
            names,
            out,
        } => {
            if names.len() != 2 {
                return Err("--names takes two names, `before,after`".into());
            }
            let cfg = Config::load(&config)?;
            let a = measure(&cfg, &segments(&before), &[])?;
            let b = measure(&cfg, &segments(&after), &[])?;
            let line = compare::write(&a, &b, &cfg, (&names[0], &names[1]), &out)?;
            println!("{line}");
            println!("wrote {}", out.join("index.html").display());
            Ok(())
        }
        Cmd::GateArgs { config } => {
            let cfg = Config::load(&config)?;
            println!("{}", cfg.gate.ram.join(","));
            Ok(())
        }
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("psoxide-frametime: {e}");
            ExitCode::FAILURE
        }
    }
}
