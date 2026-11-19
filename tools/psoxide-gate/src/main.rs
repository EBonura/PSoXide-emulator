//! `psoxide-gate`: the fleet test gate.
//!
//! Plays each game's player journey headlessly, captures every checkpoint on
//! the CPU renderer and on the hardware renderer at several scales, and judges
//! frames, RAM state, presentation and audio. See `TESTING.md`.

#![allow(missing_docs)]

mod behaviour;
mod card;
mod compare;
mod contact;
mod exec;
mod expr;
mod fetch_trace;
mod fleet;
mod img;
mod journey;
mod machine;
mod receipt;
mod refresh;
mod render;
mod report;
mod symbols;
mod tick_log;
mod trace;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use clap::{Args, Parser, Subcommand};

use exec::{Check, Group, JourneyResult, RunOptions, Status};
use fleet::{Entry, Fleet};
use journey::Journey;

#[derive(Parser)]
#[command(
    version,
    about = "Fleet test gate: play each game's journey on the CPU and hardware renderers"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run journeys and write the HTML contact sheet. Exit code 1 on any failure.
    Run(Box<RunArgs>),
    /// Recompose hash-verified raw capture evidence without starting an emulator.
    RefreshReport {
        manifest: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Write a labeled 3x before and after evidence PNG.
    BeforeAfter { manifest: PathBuf },
    /// Preserve a retained frontend PPM capture as lossless PNG data.
    ConvertCapture { input: PathBuf, output: PathBuf },
    /// Resolve a traced native VRAM pixel to its hash-verified CPU packet snapshot.
    TracePixel { checkpoint: PathBuf, x: u16, y: u16 },
    /// Run a journey and write its frames as the new goldens, with a diff report.
    Bless(BlessArgs),
    /// List the fleet: each game, its journey and whether it exists.
    List(CommonArgs),
    /// Print this gate's behaviour versions: `VERSIONS cpu=1 gte=1 ... rules=<hex16> journey=1`
    /// (every area, exercised or not; see `behaviour.rs`).
    Versions,
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
    /// Release gate: require a journey and CPU1, HW1 and HW3 at every checkpoint.
    #[arg(long)]
    strict: bool,
    /// Retain bounded GP0 packets, CPU ownership and guest state on failed checkpoints.
    #[arg(long)]
    trace_failures: bool,
    /// Extra named RAM state, e.g. DEFERRED_OVERLAYS:128 (at most 32, 256 bytes each).
    #[arg(long, requires = "trace_failures")]
    trace_state: Vec<String>,
    /// Capture this named checkpoint even when its comparisons pass.
    #[arg(long, requires = "trace_failures")]
    trace_checkpoint: Vec<String>,
    /// Write a per-tick CSV (cycles, instructions, display flips, journey step) to this file.
    /// One target only. `tools/psoxide-frametime` turns it into frame-time pages.
    #[arg(long, value_name = "PATH")]
    tick_log: Option<PathBuf>,
    /// Write the tick log with one row per pad poll (as it starts) instead of one per tick: the
    /// state each simulation tick of a polling game is about to act on, which the same journey
    /// under another boot phase repeats row for row when its steps are `sync = "poll"`.
    #[arg(long, requires = "tick_log")]
    tick_log_polls: bool,
    /// RAM words to add to the tick log as columns: `NAME=SYMBOL+OFFSET` or `NAME=0xADDR`
    /// (symbols come from the journey). Read off the bus, so the run is unchanged.
    #[arg(long, requires = "tick_log", value_delimiter = ',')]
    tick_log_ram: Vec<String>,
    /// Write the order in which every instruction retired over port-1 polls `FROM..TO`
    /// (`--fetch-trace PATH FROM..TO`), as the frontend's `launch --fetch-trace-log` does.
    /// One target only. The traced stretch runs one instruction at a time, which is
    /// slower and changes nothing in the run.
    #[arg(long, num_args = 2, value_names = ["PATH", "FROM..TO"])]
    fetch_trace: Vec<String>,
    /// Check every port-1 pad poll for a torn read (the d-pad byte of one pad state with the
    /// face-button byte of another, which a pad written between two bytes of a poll gives) and
    /// fail the journey if there is one. Needs a single target. A step with `sync = "poll"` cannot tear.
    #[arg(long)]
    audit_pad: bool,
    /// Fail a journey whose guest broke a conformance rule: something commercial software
    /// never does and another emulator or console revision does not accept (a CD interrupt
    /// acknowledge other than 07h/1Fh, GP0(1Fh); docs/conformance.md). `--conformance` alone
    /// enforces every rule; `--conformance=cd-ack-partial,cd-ack-param-reset` only those.
    /// Broken rules are printed (`CONFORMANCE` lines) with or without the flag, enforced or not.
    #[arg(
        long,
        value_name = "RULES",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "all"
    )]
    conformance: Option<String>,
    /// Entry state the boot hands each program: `default`, `openbios` or
    /// `hostile[:SEED]`, with `,skip=CATEGORY` and `,dpcr=`/`,dicr=`/`,imask=`
    /// options (docs/entry-states.md). Same as `PSOXIDE_ENTRY_STATE`. A
    /// golden mismatch under a non-default state is the finding, not a
    /// regression; the default run stays the reference.
    #[arg(long, value_name = "SPEC")]
    entry_state: Option<String>,
    /// Hardware target: `real` (default) or `mister` (the MiSTer PSX core on a
    /// SuperStation One), with `gpu-irq=`, `seed=` and `trace=` options
    /// (docs/hardware-targets.md). Same as `PSOXIDE_HARDWARE`. Goldens are
    /// recorded on `real`; a difference under `mister` is a finding about the
    /// program on that core.
    #[arg(long, value_name = "SPEC")]
    hardware: Option<String>,
    /// Treat a store to Expansion 2 beyond its configured window as a data bus
    /// error, as a console does. Same as `PSOXIDE_STRICT_EXP2=1`; off by default
    /// because `0x1F802080` is the emulator's debug text port.
    #[arg(long)]
    strict_exp2: bool,
    /// Print each step as it starts (the tick-log tool reads the step names from this).
    #[arg(short, long)]
    verbose: bool,
    /// Play every journey with this port-1 pad instead of the journey's own `pad`: `digital`
    /// (original pad), `dualshock` (a DualShock that starts digital) or `analog` (analog forced
    /// on). Port 2 is unchanged. The summary line says `pad=MODE` and the receipt key holds it.
    #[arg(long, value_enum, value_name = "MODE")]
    pad: Option<journey::PadMode>,
    /// Keep a receipt for every journey that passes, in this directory, and skip a journey
    /// whose receipt (same disc, journey, goldens, symbols, tapes, build, hardware target,
    /// flags and environment) already says PASS. A hit prints `RECEIPT HIT`; a failure is
    /// never kept. See USAGE.md.
    #[arg(long, value_name = "DIR")]
    receipts: Option<PathBuf>,
    /// Play every journey again even where a receipt exists and fail loudly if the fresh
    /// verdict, tick count or GP0(1Fh) count differs from it. The second model then runs
    /// in full instead of borrowing the first one's result. The daily full run uses this.
    #[arg(long, requires = "receipts")]
    receipts_verify: bool,
    /// Also run under this hardware target (same spec as `--hardware`), after the first.
    /// A journey whose guest issued no GP0(1Fh) behaves the same on both targets, so its
    /// result stands for the second without playing it (`MODEL EQUIVALENT`). The second
    /// target's report goes to `<out>-<name>`.
    #[arg(long, value_name = "SPEC")]
    second_hardware: Option<String>,
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

/// Diagnostic guards that are on for every gate run unless the caller set the
/// variable (`=0` turns one off). Each makes the emulator fail on a pattern
/// that freezes silicon and that the emulator would otherwise run happily:
/// - `PSOXIDE_CHECK_GPU_LL_FIFO`, see `docs/gpu-linked-list-diagnostic.md`: a
///   list node over the GPU's 16-word FIFO (WipEout's 41-word title node,
///   Celeste 0.2.3);
/// - `PSOXIDE_CHECK_CC_DMA`, see `emu/crates/emulator-core/src/bus/cc_dma.rs`:
///   a `cache_control` bus-bit change or `SR.IsC` set (the BIOS `FlushCache`)
///   while a DMA channel is busy, which made the next instruction fetch return
///   the DMA's word on a console (Quake PSX).
const GATE_GUARDS: [&str; 2] = ["PSOXIDE_CHECK_GPU_LL_FIFO", "PSOXIDE_CHECK_CC_DMA"];

/// What to set a guard variable to: `Some("1")` when the caller left it unset,
/// `None` when the caller already chose.
fn ll_guard_default(current: Option<&std::ffi::OsStr>) -> Option<&'static str> {
    current.is_none().then_some("1")
}

fn main() -> ExitCode {
    for guard in GATE_GUARDS {
        if let Some(value) = ll_guard_default(std::env::var_os(guard).as_deref()) {
            // Before any thread exists: the guards are read when a bus is built.
            std::env::set_var(guard, value);
        }
    }
    let cli = Cli::parse();
    let result = match cli.command {
        Cmd::Run(a) => cmd_run(&a),
        Cmd::TracePixel { checkpoint, x, y } => trace::inspect(&checkpoint, x, y).map(|s| {
            println!("{s}");
            ExitCode::SUCCESS
        }),
        Cmd::RefreshReport { manifest, out } => {
            refresh::run(&manifest, &out).map(|()| ExitCode::SUCCESS)
        }
        Cmd::ConvertCapture { input, output } => {
            if output.exists() {
                Err("capture output already exists".into())
            } else {
                img::Img::load_png(&input)
                    .and_then(|image| image.save_png(&output))
                    .map(|()| ExitCode::SUCCESS)
            }
        }
        Cmd::BeforeAfter { manifest } => {
            refresh::before_after(&manifest).map(|()| ExitCode::SUCCESS)
        }
        Cmd::Bless(a) => cmd_bless(&a),
        Cmd::List(a) => cmd_list(&a),
        Cmd::Versions => {
            println!("{}", versions_line());
            Ok(ExitCode::SUCCESS)
        }
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("psoxide-gate: {e}");
            ExitCode::from(2)
        }
    }
}

/// `VERSIONS area=version ...` for every behaviour area.
fn versions_line() -> String {
    let all: Vec<String> = behaviour::table()
        .into_iter()
        .map(|(area, version)| format!("{area}={version}"))
        .collect();
    format!("VERSIONS {}", all.join(" "))
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
        let name = jp
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        planned.push(Planned::Run(Job {
            name,
            journey_path: jp,
            repo_root,
            library_disc: None,
        }));
    } else if let Some(e) = fleet.find(target) {
        planned.push(from_entry(e));
    } else {
        return Err(format!(
            "unknown target `{target}`: use `all`, a path to a journey.toml, or one of: {}",
            fleet
                .games
                .iter()
                .map(|g| g.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
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
    strict: bool,
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
    let picked = if library || (strict && job.library_disc.is_some() && !build) {
        from_library()
    } else if build {
        from_build()
    } else if prefer_library {
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
    strict: bool,
    trace_failures: bool,
    trace_state: Vec<String>,
    trace_checkpoint: Vec<String>,
    tick_log: Option<PathBuf>,
    tick_log_ram: Vec<tick_log::WordSpec>,
    tick_log_polls: bool,
    fetch_trace: Option<(PathBuf, u64, u64)>,
    audit_pad: bool,
    /// Conformance rules enforced; empty: none.
    conformance: Vec<emulator_core::conformance::Rule>,
    out: PathBuf,
    receipts: Option<ReceiptMode>,
    /// Port-1 pad replacing every journey's own (`--pad`).
    pad: Option<journey::PadMode>,
}

/// Receipts in use for this run.
struct ReceiptMode {
    store: receipt::Store,
    verify: bool,
}

impl Opts {
    fn from_run(a: &RunArgs) -> Result<Opts, String> {
        let words = a
            .tick_log_ram
            .iter()
            .map(|w| tick_log::WordSpec::parse(w))
            .collect::<Result<Vec<_>, _>>()?;
        let fetch_trace = match a.fetch_trace.as_slice() {
            [] => None,
            [path, polls] => {
                let (from, to) = polls
                    .split_once("..")
                    .and_then(|(from, to)| Some((from.parse().ok()?, to.parse().ok()?)))
                    .ok_or_else(|| format!("--fetch-trace wants FROM..TO polls, not {polls}"))?;
                Some((PathBuf::from(path), from, to))
            }
            _ => return Err("--fetch-trace wants PATH FROM..TO".into()),
        };
        Ok(Opts {
            disc: a.disc.clone(),
            build: a.build,
            library: a.library,
            use_hw: !a.no_hw,
            scales: a.scales.clone(),
            verbose: a.verbose,
            golden_dir: a.golden_dir.clone(),
            strict: a.strict,
            trace_failures: a.trace_failures,
            trace_state: a.trace_state.clone(),
            trace_checkpoint: a.trace_checkpoint.clone(),
            tick_log: a.tick_log.clone(),
            tick_log_ram: words,
            tick_log_polls: a.tick_log_polls,
            fetch_trace,
            audit_pad: a.audit_pad,
            conformance: a
                .conformance
                .as_deref()
                .map(exec::parse_conformance)
                .transpose()?
                .unwrap_or_default(),
            out: a.out.clone(),
            receipts: a.receipts.as_deref().map(|dir| ReceiptMode {
                store: receipt::Store::new(dir),
                verify: a.receipts_verify,
            }),
            pad: a.pad,
        })
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
    let mut journey = match Journey::load(&job.journey_path) {
        Ok(j) => j,
        Err(e) => return aborted(&job.name, e),
    };
    if let Some(pad) = o.pad {
        // Both boots (the first and a journey's reboot) read the journey's pad.
        journey.pad = pad;
    }
    let disc = match pick_disc(
        &journey,
        job,
        fleet,
        o.disc.as_deref(),
        o.build,
        o.library,
        o.strict,
    ) {
        Ok(d) => d,
        Err(e) => return aborted(&journey.name, e),
    };
    let golden_dir = golden_dir_for(job, &journey.name, o);
    let mut state = receipt::State::Off;
    let mut keyed = None;
    let mut prior = None;
    if let Some(mode) = &o.receipts {
        match receipt_key(o, job, &journey, &disc, &golden_dir) {
            Err(why) => {
                println!("RECEIPT REFUSED {}: {why}", journey.name);
                state = receipt::State::Refused(why);
            }
            Ok(key) => {
                prior = mode.store.find(&journey.name, &key);
                if let (Some(kept), false) = (&prior, mode.verify) {
                    println!("{}", kept.hit_line());
                    let mut hit = receipt_hit(&journey, &disc, kept);
                    hit.facts.pad_override = o.pad;
                    return hit;
                }
                state = receipt::State::Ran;
                keyed = Some(key);
            }
        }
    }
    let mut result = exec::run_journey(
        &journey,
        &RunOptions {
            disc,
            repo_root: job.repo_root.clone(),
            golden_dir,
            use_hw: o.use_hw,
            scales: o.scales.clone(),
            verbose: o.verbose,
            strict: o.strict,
            trace_failures: o.trace_failures,
            trace_state: o.trace_state.clone(),
            trace_checkpoint: o.trace_checkpoint.clone(),
            tick_log: o.tick_log.clone(),
            tick_log_ram: o.tick_log_ram.clone(),
            tick_log_polls: o.tick_log_polls,
            fetch_trace: o.fetch_trace.clone(),
            audit_pad: o.audit_pad,
            conformance: o.conformance.clone(),
            journey_path: job.journey_path.clone(),
            artifact_dir: o.out.join(&journey.name).join("cards"),
        },
    );
    result.facts.pad_override = o.pad;
    result.receipt = match (&o.receipts, &keyed) {
        (Some(mode), Some(key)) => settle_receipt(&mode.store, key, prior.as_ref(), &mut result),
        _ => state,
    };
    result
}

/// The key of a journey about to run, or why no receipt can cover it.
fn receipt_key(
    o: &Opts,
    job: &Job,
    journey: &Journey,
    disc: &Path,
    golden_dir: &Path,
) -> Result<receipt::Key, String> {
    if o.tick_log.is_some() || o.fetch_trace.is_some() {
        return Err(
            "--tick-log and --fetch-trace write files a receipt hit would not produce".into(),
        );
    }
    let flags = key_flags(o, journey);
    receipt::compute_key(&receipt::Context {
        journey,
        journey_path: &job.journey_path,
        repo_root: &job.repo_root,
        disc,
        golden_dir,
        flags: &flags,
    })
}

/// Every flag and setting of this run that alters a journey's behaviour, as receipt key inputs.
fn key_flags(o: &Opts, journey: &Journey) -> Vec<(String, String)> {
    let scales = o
        .scales
        .clone()
        .unwrap_or_else(|| journey.render.scales.clone());
    let flag = |name: &str, value: String| (name.to_string(), value);
    vec![
        flag("hardware renderer", o.use_hw.to_string()),
        flag("scales", format!("{scales:?}")),
        flag("strict", o.strict.to_string()),
        flag("trace-failures", o.trace_failures.to_string()),
        flag("trace-state", format!("{:?}", o.trace_state)),
        flag("trace-checkpoint", format!("{:?}", o.trace_checkpoint)),
        flag("audit-pad", o.audit_pad.to_string()),
        flag(
            "pad",
            o.pad.map_or("journey".into(), |p| p.name().to_string()),
        ),
        flag(
            "conformance",
            o.conformance
                .iter()
                .map(|r| r.id())
                .collect::<Vec<_>>()
                .join(","),
        ),
        flag("entry-state", emulator_core::EntryState::from_env().spec()),
        flag(
            "hardware",
            format!("{:?}", emulator_core::HardwareTarget::from_env()),
        ),
    ]
}

/// The result of a journey a receipt answered for: the recorded counts, no frames.
fn receipt_hit(journey: &Journey, disc: &Path, kept: &receipt::Receipt) -> JourneyResult {
    let mut r = aborted(&journey.name, String::new());
    r.title = journey
        .title
        .clone()
        .unwrap_or_else(|| journey.name.clone());
    r.disc = disc.to_path_buf();
    r.disc_id = kept.disc_id.clone();
    r.abort = None;
    r.ticks = kept.ticks;
    r.emu_secs = kept.emu_secs;
    r.gp0_irq_commands = Some(kept.gp0_1f);
    r.facts.gp0_irq_writes = kept.gp0_1f_written;
    r.facts.mdec_command_writes = kept.mdec_command_writes;
    r.facts.golden_on = journey.golden_on_anywhere();
    // The receipt keeps only the CD acknowledge count; a clean one is clean of those rules,
    // which is all `model_equivalent` asks. Anything else stays uncounted.
    r.conformance = (kept.cd_ack == 0).then(Vec::new);
    r.receipt = receipt::State::Hit;
    if !kept.digest.is_empty() {
        r.facts.areas = Some(kept.areas.clone());
        r.facts.digest = Some(kept.digest.clone());
    }
    r.notes.push(
        "receipt hit: not played, so no frames, cards or traces were written for it here; \
         delete the receipt to play it again"
            .into(),
    );
    r.groups.push(Group {
        name: "receipt".into(),
        label: None,
        tick: kept.ticks,
        capture: None,
        checks: vec![Check {
            name: "receipt".into(),
            status: Status::Pass,
            detail: kept.hit_line(),
        }],
        wants_golden: false,
    });
    r
}

/// After a played journey: check it against the receipt it was asked to verify, or keep a new
/// receipt for a PASS.
fn settle_receipt(
    store: &receipt::Store,
    key: &receipt::Key,
    prior: Option<&receipt::Receipt>,
    result: &mut JourneyResult,
) -> receipt::State {
    if let Some(kept) = prior {
        let Some(why) = kept.differs_from(result) else {
            println!("RECEIPT VERIFIED {} {}", result.name, key.prefix());
            return receipt::State::Verified;
        };
        let detail = format!("RECEIPT MISMATCH {} {}: {why}", result.name, key.prefix());
        println!("{detail}");
        result.groups.push(Group {
            name: "receipt".into(),
            label: None,
            tick: result.ticks,
            capture: None,
            checks: vec![Check {
                name: "receipt".into(),
                status: Status::Fail,
                detail,
            }],
            wants_golden: false,
        });
        return receipt::State::Mismatch;
    }
    if let Some(fresh) = receipt::Receipt::of(result, key) {
        if let Err(e) = store.write(&fresh) {
            eprintln!("[gate] receipt for {} not kept: {e}", result.name);
        }
    }
    receipt::State::Ran
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
        cards: Vec::new(),
        gp0_irq_commands: None,
        conformance: None,
        cd_acknowledges: None,
        mdec_commands: None,
        receipt: receipt::State::Off,
        facts: Default::default(),
    }
}

fn print_failures(r: &JourneyResult, all: bool) {
    if let Some(a) = &r.abort {
        println!("  ABORT  {}: {a}", r.name);
    }
    for g in &r.groups {
        for c in &g.checks {
            if all
                || matches!(
                    c.status,
                    Status::Fail | Status::XPass | Status::GoldenSkipped
                )
            {
                println!(
                    "  {}  {}/{}: {}: {}",
                    c.status.label(),
                    r.name,
                    g.name,
                    c.name,
                    c.detail
                );
            }
        }
    }
}

fn cmd_run(a: &RunArgs) -> Result<ExitCode, String> {
    if let Some(spec) = &a.entry_state {
        emulator_core::EntryState::from_spec(spec)?;
        // Before any journey thread starts: the boot reads it.
        std::env::set_var(emulator_core::entry_state::ENV_VAR, spec);
    }
    if let Some(spec) = &a.hardware {
        emulator_core::HardwareTarget::from_spec(spec)?;
        // Before any journey thread starts: every GPU reads it when it is built.
        std::env::set_var(emulator_core::hardware_target::ENV_VAR, spec);
    }
    emulator_core::check_strict_exp2(a.strict_exp2)?;
    if a.strict_exp2 {
        // Before any journey thread starts: every bus reads it when it is built.
        std::env::set_var("PSOXIDE_STRICT_EXP2", "1");
    }
    if emulator_core::HardwareTarget::from_env().differs_from_real() {
        eprintln!(
            "[gate] hardware target {:?}: differences from the goldens are findings about the program on that core",
            emulator_core::HardwareTarget::from_env()
        );
    }
    if emulator_core::EntryState::from_env() != emulator_core::EntryState::Default {
        eprintln!(
            "[gate] entry state {}: differences from the goldens are findings about the program",
            emulator_core::EntryState::from_env().spec()
        );
    }
    if a.strict && a.no_hw {
        return Err("--strict requires hardware rendering; remove --no-hw".into());
    }
    if a.strict
        && a.scales
            .as_ref()
            .is_some_and(|s| !s.contains(&1) || !s.contains(&3))
    {
        return Err("--strict requires both hardware scales 1 and 3".into());
    }
    let (planned, fleet) = plan(&a.target, &a.common)?;
    if a.disc.is_some() && planned.len() != 1 {
        return Err("--disc needs a single target".into());
    }
    let jobs: Vec<&Job> = planned
        .iter()
        .filter_map(|p| {
            if let Planned::Run(j) = p {
                Some(j)
            } else {
                None
            }
        })
        .collect();
    if a.tick_log.is_some() && planned.len() != 1 {
        return Err("--tick-log needs a single target".into());
    }
    if let Some(spec) = &a.second_hardware {
        emulator_core::HardwareTarget::from_spec(spec)?;
        if a.tick_log.is_some() || !a.fetch_trace.is_empty() {
            return Err(
                "--second-hardware cannot be combined with --tick-log or --fetch-trace".into(),
            );
        }
    }
    let first_model = std::env::var(emulator_core::hardware_target::ENV_VAR)
        .ok()
        .filter(|spec| !spec.is_empty())
        .unwrap_or_else(|| "real".into());
    let opts = Opts::from_run(a)?;
    let first = play(&jobs, &fleet, &opts, a.jobs);
    let mut missing_jobs = Vec::new();
    for p in &planned {
        if let Planned::Missing { name, why } = p {
            println!("MISSING {name:<14} {why}");
            missing_jobs.push((name.clone(), why.clone()));
        }
    }
    let mut failed = finish(&a.out, &first, &missing_jobs, 0)?;
    if let Some(second_model) = &a.second_hardware {
        println!("[gate] second model {second_model} (first was {first_model})");
        let mut needed = Vec::new();
        let mut equivalent = 0;
        // The shortcut below holds only while both models read the MDEC status the same way, or
        // the guest never wrote an MDEC command (the request bits then never rise).
        let mdec_req = |spec: &str| {
            emulator_core::HardwareTarget::from_spec(spec)
                .map(|t| t.mdec_req)
                .ok()
        };
        let same_mdec = mdec_req(&first_model) == mdec_req(second_model);
        for (job, result) in jobs.iter().zip(&first) {
            let mdec_alike = same_mdec || result.facts.mdec_command_writes == Some(0);
            if !a.receipts_verify && mdec_alike && receipt::model_equivalent(result) {
                println!(
                    "MODEL EQUIVALENT {}: no GP0(1Fh), {first_model} result stands for {second_model}",
                    job.name
                );
                equivalent += 1;
            } else {
                needed.push(*job);
            }
        }
        // Before any journey thread starts: every GPU reads it when it is built.
        std::env::set_var(emulator_core::hardware_target::ENV_VAR, second_model);
        let mut opts = Opts::from_run(a)?;
        opts.out = sibling_dir(&a.out, &model_slug(second_model));
        let second = play(&needed, &fleet, &opts, a.jobs);
        failed += finish(&opts.out, &second, &[], equivalent)?;
    }
    let bad = failed > 0 || (!missing_jobs.is_empty() && (a.strict || a.target != "all"));
    Ok(if bad {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// Run `jobs` on up to two threads, results in job order.
fn play(jobs: &[&Job], fleet: &Fleet, opts: &Opts, threads: usize) -> Vec<JourneyResult> {
    let slots: Vec<Mutex<Option<JourneyResult>>> = jobs.iter().map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads.clamp(1, 2) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                let Some(job) = jobs.get(i) else { break };
                let r = run_planned(job, fleet, opts);
                println!("{}", r.summary());
                if let (Some(areas), Some(digest)) = (&r.facts.areas, &r.facts.digest) {
                    for line in behaviour::lines(&r.name, &behaviour::model(), areas, digest) {
                        println!("{line}");
                    }
                }
                if let Some(line) = behaviour::signals_line(&r, &behaviour::model()) {
                    println!("{line}");
                }
                if let Some(line) = r.conformance_line(&opts.conformance) {
                    println!("{line}");
                }
                print_failures(&r, opts.verbose);
                *slots[i].lock().expect("slot") = Some(r);
            });
        }
    });
    slots
        .into_iter()
        .filter_map(|m| m.into_inner().expect("slot"))
        .collect()
}

/// Write the report and print the summary lines; the number of failed journeys.
fn finish(
    out: &Path,
    results: &[JourneyResult],
    missing: &[(String, String)],
    equivalent: usize,
) -> Result<usize, String> {
    let index = report::write_report(out, results, missing, "PSoXide fleet gate")?;
    println!("report: {}", index.display());
    let failed = results.iter().filter(|r| !r.passed()).count();
    println!(
        "gate: {} journeys, {} failed, {} without a journey",
        results.len(),
        failed,
        missing.len()
    );
    use receipt::State;
    let count = |f: fn(&State) -> bool| results.iter().filter(|r| f(&r.receipt)).count();
    if results.iter().any(|r| r.receipt != State::Off) || equivalent > 0 {
        println!(
            "receipts: {} hits, {} runs ({} verified, {} MISMATCHED, {} refused), {} model-equivalent",
            count(|s| *s == State::Hit),
            count(|s| !matches!(s, State::Hit | State::Off)),
            count(|s| *s == State::Verified),
            count(|s| *s == State::Mismatch),
            count(|s| matches!(s, State::Refused(_))),
            equivalent
        );
    }
    Ok(failed)
}

/// `<dir>-<suffix>` next to `dir`.
fn sibling_dir(dir: &Path, suffix: &str) -> PathBuf {
    let mut name = dir.as_os_str().to_os_string();
    name.push("-");
    name.push(suffix);
    PathBuf::from(name)
}

/// A hardware spec as a file name part: `mister,gpu-irq=never` is `mister-gpu-irq-never`.
fn model_slug(spec: &str) -> String {
    spec.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn cmd_bless(a: &BlessArgs) -> Result<ExitCode, String> {
    if emulator_core::EntryState::from_env() != emulator_core::EntryState::Default {
        return Err("bless runs on the default entry state: unset PSOXIDE_ENTRY_STATE".into());
    }
    if emulator_core::HardwareTarget::from_env().differs_from_real() {
        return Err("bless runs on the real hardware target: unset PSOXIDE_HARDWARE".into());
    }
    let (planned, fleet) = plan(&a.target, &a.common)?;
    if a.target == "all" || planned.len() != 1 {
        return Err(
            "bless takes one game at a time: goldens are reviewed, not bulk-regenerated".into(),
        );
    }
    let Planned::Run(job) = &planned[0] else {
        let Planned::Missing { why, .. } = &planned[0] else {
            unreachable!()
        };
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
        strict: false,
        trace_failures: false,
        trace_state: Vec::new(),
        trace_checkpoint: Vec::new(),
        tick_log: None,
        tick_log_ram: Vec::new(),
        tick_log_polls: false,
        fetch_trace: None,
        audit_pad: false,
        conformance: Vec::new(),
        out: a.out.clone(),
        receipts: None,
        pad: None,
    };
    let r = run_planned(job, &fleet, &opts);
    println!("{}", r.summary());
    let blocking: Vec<_> = r
        .groups
        .iter()
        .flat_map(|g| g.checks.iter().map(move |c| (g, c)))
        .filter(|(_, c)| {
            c.status.is_failure()
                && c.name != "golden"
                && !c.name.starts_with("hw ")
                && !c.name.starts_with("strict")
        })
        .collect();
    let index = report::write_report(
        &a.out,
        std::slice::from_ref(&r),
        &[],
        "PSoXide gate: golden review",
    )?;
    println!(
        "review report (old golden | new | diff): {}",
        index.display()
    );
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
        let unchanged = cap.golden.as_ref().is_some_and(|old| {
            (old.w, old.h) == (cap.cpu.w, cap.cpu.h) && old.rgba == cap.cpu.rgba
        });
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
            Some(old) if (old.w, old.h) == (cap.cpu.w, cap.cpu.h) => {
                cap.cpu.diff(old, 0, 1, &[], None).describe()
            }
            Some(old) => format!("size {}x{} -> {}x{}", old.w, old.h, cap.cpu.w, cap.cpu.h),
            None => "first golden".into(),
        };
        cap.cpu
            .save_png(&golden_dir.join(format!("{}.png", g.name)))?;
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
                Ok(j) => format!(
                    "journey {} ({} steps, {} checkpoints)",
                    jp.display(),
                    j.steps.len(),
                    j.checkpoints().count()
                ),
                Err(err) => format!("INVALID journey: {err}"),
            }
        } else {
            "no journey".to_string()
        };
        let disc = if fleet.library_disc(e).exists() {
            ""
        } else {
            " [library disc missing]"
        };
        println!("{:<15} {state}{disc}", e.name);
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `run --conformance...` parses to: `None` without the flag.
    fn conformance_arg(extra: &[&str]) -> Result<Option<String>, String> {
        let mut argv = vec!["psoxide-gate", "run", "nitroxide"];
        argv.extend_from_slice(extra);
        match Cli::try_parse_from(argv)
            .map_err(|e| e.to_string())?
            .command
        {
            Cmd::Run(a) => Ok(a.conformance.clone()),
            _ => Err("not run".into()),
        }
    }

    fn run_args(extra: &[&str]) -> Result<RunArgs, String> {
        let mut argv = vec!["psoxide-gate", "run", "nitroxide"];
        argv.extend_from_slice(extra);
        match Cli::try_parse_from(argv)
            .map_err(|e| e.to_string())?
            .command
        {
            Cmd::Run(a) => Ok(*a),
            _ => Err("not run".into()),
        }
    }

    /// `--pad` takes the journey file's three names, nothing else, and is absent by default.
    #[test]
    fn pad_flag_takes_the_journey_pad_names() {
        use journey::PadMode;
        assert_eq!(run_args(&[]).unwrap().pad, None);
        for (word, mode) in [
            ("digital", PadMode::Digital),
            ("dualshock", PadMode::Dualshock),
            ("analog", PadMode::Analog),
        ] {
            assert_eq!(run_args(&["--pad", word]).unwrap().pad, Some(mode));
            assert_eq!(mode.name(), word);
        }
        assert!(run_args(&["--pad", "mouse"]).is_err());
    }

    /// The pad is a receipt key input: no pad override, and each override, key differently,
    /// so a digital run never answers from a DualShock receipt.
    #[test]
    fn pad_override_is_part_of_the_receipt_key_flags() {
        let journey: Journey = toml::from_str("name = \"j\"\ndisc = \"x.cue\"\n").expect("journey");
        let flags = |extra: &[&str]| {
            key_flags(
                &Opts::from_run(&run_args(extra).unwrap()).unwrap(),
                &journey,
            )
        };
        let pad = |f: Vec<(String, String)>| f.into_iter().find(|(k, _)| k == "pad").unwrap().1;
        assert_eq!(pad(flags(&[])), "journey");
        assert_eq!(pad(flags(&["--pad", "digital"])), "digital");
        assert_ne!(flags(&["--pad", "digital"]), flags(&["--pad", "dualshock"]));
        assert_ne!(flags(&[]), flags(&["--pad", "analog"]));
    }

    #[test]
    fn conformance_flag_alone_means_every_rule_and_takes_a_list_after_equals() {
        use emulator_core::conformance::Rule;
        assert_eq!(conformance_arg(&[]).unwrap(), None);
        let all = conformance_arg(&["--conformance"]).unwrap().unwrap();
        assert_eq!(exec::parse_conformance(&all).unwrap(), Rule::ALL);
        let cd = conformance_arg(&["--conformance=cd-ack-partial,cd-ack-param-reset"])
            .unwrap()
            .unwrap();
        assert_eq!(
            exec::parse_conformance(&cd).unwrap(),
            [Rule::CdAckPartial, Rule::CdAckWithParamReset]
        );
        // Without `=` the next word stays the next argument, not a rule list.
        assert_eq!(
            conformance_arg(&["--conformance", "--no-hw"])
                .unwrap()
                .as_deref(),
            Some("all")
        );
        let bad = conformance_arg(&["--conformance=gpu"]).unwrap().unwrap();
        assert!(exec::parse_conformance(&bad).is_err());
    }

    #[test]
    fn the_list_guard_defaults_on_and_yields_to_the_caller() {
        use std::ffi::OsStr;
        assert_eq!(ll_guard_default(None), Some("1"));
        assert_eq!(ll_guard_default(Some(OsStr::new("0"))), None);
        assert_eq!(ll_guard_default(Some(OsStr::new("1"))), None);
        assert_eq!(ll_guard_default(Some(OsStr::new(""))), None);
    }

    #[test]
    fn the_gate_turns_on_the_list_and_the_cache_control_dma_guards() {
        assert!(GATE_GUARDS.contains(&"PSOXIDE_CHECK_GPU_LL_FIFO"));
        assert!(GATE_GUARDS.contains(&"PSOXIDE_CHECK_CC_DMA"));
    }

    #[test]
    fn release_gate_never_substitutes_a_build_for_the_library_disc() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("game");
        std::fs::create_dir_all(repo.join("dist")).unwrap();
        let build_disc = repo.join("dist/build.cue");
        std::fs::write(&build_disc, "").unwrap();
        let library_disc = dir.path().join("library.cue");
        let journey: Journey = toml::from_str(
            "name = 'gate'\ndisc = 'dist/build.cue'\n[[step]]\ncheckpoint = 'menu'\n",
        )
        .unwrap();
        let job = Job {
            name: "gate".into(),
            journey_path: repo.join("tests/journey.toml"),
            repo_root: repo,
            library_disc: Some(library_disc.clone()),
        };
        let fleet = Fleet {
            games_dir: String::new(),
            repos_dir: String::new(),
            games: Vec::new(),
        };
        assert!(pick_disc(&journey, &job, &fleet, None, false, false, true).is_err());
        assert!(pick_disc(&journey, &job, &fleet, None, false, true, false).is_err());
        assert_eq!(
            pick_disc(&journey, &job, &fleet, None, true, false, true).unwrap(),
            build_disc
        );
    }
}
