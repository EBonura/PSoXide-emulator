//! `psoxide-gate`: the fleet test gate.
//!
//! Plays each game's player journey headlessly, captures every checkpoint on
//! the CPU renderer and on the hardware renderer at several scales, and judges
//! frames, RAM state, presentation and audio. See `TESTING.md`.

#![allow(missing_docs)]

mod exec;
mod fleet;
mod img;
mod journey;
mod machine;
mod render;
mod report;
mod symbols;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use clap::{Args, Parser, Subcommand};

use exec::{JourneyResult, RunOptions, Status};
use fleet::{Entry, Fleet};
use journey::Journey;

#[derive(Parser)]
#[command(version, about = "Fleet test gate: play each game's journey on the CPU and hardware renderers")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run journeys and write the HTML contact sheet. Exit code 1 on any failure.
    Run(RunArgs),
    /// Run a journey and write its frames as the new goldens, with a diff report.
    Bless(BlessArgs),
    /// List the fleet: each game, its journey and whether it exists.
    List(CommonArgs),
}

#[derive(Args, Clone)]
struct CommonArgs {
    /// Fleet manifest to use instead of the bundled one.
    #[arg(long)]
    fleet: Option<PathBuf>,
    /// Use this checkout for a game instead of the manifest's: `name=/path`.
    #[arg(long = "repo", value_parser = parse_repo)]
    repos: Vec<(String, PathBuf)>,
}

#[derive(Args, Clone)]
struct RunArgs {
    /// `all`, a game name from the fleet, or a path to a journey.toml.
    target: String,
    #[command(flatten)]
    common: CommonArgs,
    /// Boot this disc (.cue/.bin) instead of the journey's or the library's.
    #[arg(long)]
    disc: Option<PathBuf>,
    /// Boot the disc the repo's normal build produced (the journey's `disc`)
    /// rather than the library copy. Default for a journey path.
    #[arg(long, conflicts_with = "library")]
    build: bool,
    /// Boot the library copy. Default for a fleet name and for `all`.
    #[arg(long)]
    library: bool,
    /// Report directory.
    #[arg(long, default_value = "gate-report")]
    out: PathBuf,
    /// Read goldens from this directory instead of `tests/golden/<name>`.
    #[arg(long)]
    golden_dir: Option<PathBuf>,
    /// Skip the hardware-renderer matrix (CPU frame and asserts only).
    #[arg(long)]
    no_hw: bool,
    /// Hardware scales to capture, e.g. `1,3`.
    #[arg(long, value_delimiter = ',')]
    scales: Option<Vec<u32>>,
    /// Journeys running at once for `all` (each is one emulator run).
    #[arg(long, default_value_t = 1)]
    jobs: usize,
    /// A game with no journey counts as a failure.
    #[arg(long)]
    strict: bool,
    /// Print each step as it starts.
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Args, Clone)]
struct BlessArgs {
    /// A game name from the fleet or a path to a journey.toml.
    target: String,
    #[command(flatten)]
    common: CommonArgs,
    #[arg(long)]
    disc: Option<PathBuf>,
    #[arg(long, conflicts_with = "library")]
    build: bool,
    #[arg(long)]
    library: bool,
    #[arg(long, default_value = "gate-report")]
    out: PathBuf,
    /// Write goldens to this directory instead of `tests/golden/<name>`.
    #[arg(long)]
    golden_dir: Option<PathBuf>,
    #[arg(long, value_delimiter = ',')]
    scales: Option<Vec<u32>>,
    #[arg(long)]
    no_hw: bool,
    /// Bless even though asserts failed.
    #[arg(long)]
    force: bool,
    #[arg(short, long)]
    verbose: bool,
}

fn parse_repo(s: &str) -> Result<(String, PathBuf), String> {
    let (name, path) = s.split_once('=').ok_or("expected name=/path")?;
    Ok((name.to_string(), PathBuf::from(path)))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Cmd::Run(a) => cmd_run(&a),
        Cmd::Bless(a) => cmd_bless(&a),
        Cmd::List(a) => cmd_list(&a),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("psoxide-gate: {e}");
            ExitCode::from(2)
        }
    }
}

/// What to run for one target.
struct Job {
    name: String,
    journey_path: PathBuf,
    repo_root: PathBuf,
    library_disc: Option<PathBuf>,
}

enum Planned {
    Run(Job),
    Missing { name: String, why: String },
}

fn plan(target: &str, common: &CommonArgs) -> Result<(Vec<Planned>, Fleet), String> {
    let fleet = Fleet::load(common.fleet.as_deref())?;
    let mut planned = Vec::new();
    let from_entry = |e: &Entry| -> Planned {
        let repo = fleet.repo_dir(e, &common.repos);
        let jp = repo.join(&e.journey);
        if jp.exists() {
            Planned::Run(Job {
                name: e.name.clone(),
                journey_path: jp,
                repo_root: repo,
                library_disc: Some(fleet.library_disc(e)),
            })
        } else {
            Planned::Missing {
                name: e.name.clone(),
                why: format!("no journey at {}", jp.display()),
            }
        }
    };
    if target == "all" {
        planned.extend(fleet.games.iter().map(from_entry));
    } else if target.ends_with(".toml") || Path::new(target).is_file() {
        let jp = PathBuf::from(target);
        let jp = std::fs::canonicalize(&jp).map_err(|e| format!("{}: {e}", jp.display()))?;
        let repo_root = Journey::repo_root(&jp);
        let name = jp.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        planned.push(Planned::Run(Job { name, journey_path: jp, repo_root, library_disc: None }));
    } else if let Some(e) = fleet.find(target) {
        planned.push(from_entry(e));
    } else {
        return Err(format!(
            "unknown target `{target}`: use `all`, a path to a journey.toml, or one of: {}",
            fleet.games.iter().map(|g| g.name.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    Ok((planned, fleet))
}

fn pick_disc(
    journey: &Journey,
    job: &Job,
    fleet: &Fleet,
    explicit: Option<&Path>,
    build: bool,
    library: bool,
) -> Result<PathBuf, String> {
    if let Some(d) = explicit {
        return Ok(d.to_path_buf());
    }
    let from_build = || -> Option<PathBuf> {
        journey
            .disc
            .list()
            .iter()
            .map(|rel| job.repo_root.join(rel))
            .find(|p| p.exists())
    };
    let from_library = || -> Option<PathBuf> {
        job.library_disc
            .clone()
            .or_else(|| fleet.find(&journey.name).map(|e| fleet.library_disc(e)))
            .filter(|p| p.exists())
    };
    // A fleet name defaults to the library disc, a journey path to the build.
    let prefer_library = library || (!build && job.library_disc.is_some());
    let picked = if prefer_library {
        from_library().or_else(from_build)
    } else {
        from_build().or_else(from_library)
    };
    picked.ok_or_else(|| {
        format!(
            "no disc for `{}`: looked in the journey's disc list {:?} and the library; pass --disc",
            journey.name,
            journey.disc.list()
        )
    })
}

/// Everything that varies between invocations of one journey.
struct Opts {
    disc: Option<PathBuf>,
    build: bool,
    library: bool,
    use_hw: bool,
    scales: Option<Vec<u32>>,
    verbose: bool,
    golden_dir: Option<PathBuf>,
}

impl Opts {
    fn from_run(a: &RunArgs) -> Opts {
        Opts {
            disc: a.disc.clone(),
            build: a.build,
            library: a.library,
            use_hw: !a.no_hw,
            scales: a.scales.clone(),
            verbose: a.verbose,
            golden_dir: a.golden_dir.clone(),
        }
    }
}

fn golden_dir_for(job: &Job, name: &str, o: &Opts) -> PathBuf {
    o.golden_dir.clone().unwrap_or_else(|| {
        job.journey_path
            .parent()
            .unwrap_or(Path::new("."))
            .join("golden")
            .join(name)
    })
}

fn run_planned(job: &Job, fleet: &Fleet, o: &Opts) -> JourneyResult {
    let journey = match Journey::load(&job.journey_path) {
        Ok(j) => j,
        Err(e) => return aborted(&job.name, e),
    };
    let disc = match pick_disc(&journey, job, fleet, o.disc.as_deref(), o.build, o.library) {
        Ok(d) => d,
        Err(e) => return aborted(&journey.name, e),
    };
    exec::run_journey(
        &journey,
        &RunOptions {
            disc,
            repo_root: job.repo_root.clone(),
            golden_dir: golden_dir_for(job, &journey.name, o),
            use_hw: o.use_hw,
            scales: o.scales.clone(),
            verbose: o.verbose,
        },
    )
}

fn aborted(name: &str, why: String) -> JourneyResult {
    JourneyResult {
        name: name.to_string(),
        title: name.to_string(),
        disc: PathBuf::new(),
        disc_id: "-".into(),
        groups: Vec::new(),
        abort: Some(why),
        ticks: 0,
        emu_secs: 0.0,
        wall: std::time::Duration::ZERO,
        hw_adapter: None,
        notes: Vec::new(),
    }
}

fn print_failures(r: &JourneyResult, all: bool) {
    if let Some(a) = &r.abort {
        println!("  ABORT  {}: {a}", r.name);
    }
    for g in &r.groups {
        for c in &g.checks {
            if all || matches!(c.status, Status::Fail | Status::XPass) {
                println!("  {}  {}/{}: {}: {}", c.status.label(), r.name, g.name, c.name, c.detail);
            }
        }
    }
}

fn cmd_run(a: &RunArgs) -> Result<ExitCode, String> {
    let (planned, fleet) = plan(&a.target, &a.common)?;
    if a.disc.is_some() && planned.len() != 1 {
        return Err("--disc needs a single target".into());
    }
    let jobs: Vec<&Job> = planned
        .iter()
        .filter_map(|p| if let Planned::Run(j) = p { Some(j) } else { None })
        .collect();
    let opts = Opts::from_run(a);
    let slots: Vec<Mutex<Option<JourneyResult>>> = jobs.iter().map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..a.jobs.clamp(1, 2) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                let Some(job) = jobs.get(i) else { break };
                let r = run_planned(job, &fleet, &opts);
                println!("{}", r.summary());
                print_failures(&r, a.verbose);
                *slots[i].lock().expect("slot") = Some(r);
            });
        }
    });
    let results: Vec<JourneyResult> = slots
        .into_iter()
        .filter_map(|m| m.into_inner().expect("slot"))
        .collect();
    let mut missing = 0;
    for p in &planned {
        if let Planned::Missing { name, why } = p {
            println!("MISSING {name:<14} {why}");
            missing += 1;
        }
    }
    let index = report::write_report(&a.out, &results, "PSoXide fleet gate")?;
    println!("report: {}", index.display());
    let failed = results.iter().filter(|r| !r.passed()).count();
    println!(
        "gate: {} journeys, {} failed, {} without a journey",
        results.len(),
        failed,
        missing
    );
    let bad = failed > 0 || (a.strict && missing > 0);
    Ok(if bad { ExitCode::from(1) } else { ExitCode::SUCCESS })
}

fn cmd_bless(a: &BlessArgs) -> Result<ExitCode, String> {
    let (planned, fleet) = plan(&a.target, &a.common)?;
    if a.target == "all" || planned.len() != 1 {
        return Err("bless takes one game at a time: goldens are reviewed, not bulk-regenerated".into());
    }
    let Planned::Run(job) = &planned[0] else {
        let Planned::Missing { why, .. } = &planned[0] else { unreachable!() };
        return Err(why.clone());
    };
    let opts = Opts {
        disc: a.disc.clone(),
        build: a.build,
        library: a.library,
        use_hw: !a.no_hw,
        scales: a.scales.clone(),
        verbose: a.verbose,
        golden_dir: a.golden_dir.clone(),
    };
    let r = run_planned(job, &fleet, &opts);
    println!("{}", r.summary());
    let blocking: Vec<_> = r
        .groups
        .iter()
        .flat_map(|g| g.checks.iter().map(move |c| (g, c)))
        .filter(|(_, c)| c.status.is_failure() && c.name != "golden" && !c.name.starts_with("hw ") && !c.name.starts_with("strict"))
        .collect();
    let index = report::write_report(&a.out, std::slice::from_ref(&r), "PSoXide gate: golden review")?;
    println!("review report (old golden | new | diff): {}", index.display());
    if (r.abort.is_some() || !blocking.is_empty()) && !a.force {
        if let Some(e) = &r.abort {
            println!("  ABORT  {e}");
        }
        for (g, c) in &blocking {
            println!("  FAIL  {}/{}: {}: {}", r.name, g.name, c.name, c.detail);
        }
        println!("refusing to bless a journey whose asserts fail (--force to override)");
        return Ok(ExitCode::from(1));
    }
    let golden_dir = golden_dir_for(job, &r.name, &opts);
    let (mut new, mut changed, mut same) = (0, 0, 0);
    for g in &r.groups {
        let Some(cap) = &g.capture else { continue };
        if !g.wants_golden {
            continue;
        }
        let unchanged = cap
            .golden
            .as_ref()
            .is_some_and(|old| (old.w, old.h) == (cap.cpu.w, cap.cpu.h) && old.rgba == cap.cpu.rgba);
        if unchanged {
            same += 1;
            continue;
        }
        let verb = if cap.golden.is_some() {
            changed += 1;
            "changed"
        } else {
            new += 1;
            "new"
        };
        let detail = match &cap.golden {
            Some(old) if (old.w, old.h) == (cap.cpu.w, cap.cpu.h) => cap.cpu.diff(old, 0, 1, &[], None).describe(),
            Some(old) => format!("size {}x{} -> {}x{}", old.w, old.h, cap.cpu.w, cap.cpu.h),
            None => "first golden".into(),
        };
        cap.cpu.save_png(&golden_dir.join(format!("{}.png", g.name)))?;
        println!("  {verb:<7} {}: {detail}", g.name);
    }
    println!(
        "blessed {} goldens in {}: {new} new, {changed} changed, {same} unchanged",
        new + changed + same,
        golden_dir.display()
    );
    Ok(ExitCode::SUCCESS)
}

fn cmd_list(a: &CommonArgs) -> Result<ExitCode, String> {
    let fleet = Fleet::load(a.fleet.as_deref())?;
    for e in &fleet.games {
        let repo = fleet.repo_dir(e, &a.repos);
        let jp = repo.join(&e.journey);
        let state = if jp.exists() {
            match Journey::load(&jp) {
                Ok(j) => format!("journey {} ({} steps, {} checkpoints)", jp.display(), j.steps.len(), j.checkpoints().count()),
                Err(err) => format!("INVALID journey: {err}"),
            }
        } else {
            "no journey".to_string()
        };
        let disc = if fleet.library_disc(e).exists() { "" } else { " [library disc missing]" };
        println!("{:<15} {state}{disc}", e.name);
    }
    Ok(ExitCode::SUCCESS)
}
