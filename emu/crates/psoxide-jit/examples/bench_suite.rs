//! Speed and exactness scoreboard: runs a fixed set of games through
//! `jit_run bench` (interpreter `run` and native `tier`) and, optionally, a
//! patched headless DuckStation regtest runner, then prints one table.
//!
//! usage:
//!   bench_suite [--manifest F] [--games DIR] [--tapes DIR] [--reps N]
//!               [--modes run,tier] [--only a,b] [--reference F] [--bless]
//!               [--md OUT.md] [--duckstation BIN [--ds-modes Interpreter,CachedInterpreter,Recompiler]]
//!
//! The manifest (default `bench/suite.txt` beside this crate) has one game per
//! line: `name | disc | from | frames | input | ds_shift`. `disc` is relative to
//! `--games` (default `~/Downloads/ps1 games`) unless absolute. `from..frames`
//! is the timed window (frames counted from the EXE entry; the run starts at
//! frame 0). `input` is `pulses:common`, `pulses:none`, `pulses:<spec>` or
//! `tape:<file>` (relative to `--tapes`, default
//! `~/Library/Application Support/PSoXide-perf/tapes-manny`). `ds_shift` is
//! added to the window for DuckStation, which counts frames from power-on.
//!
//! Exactness: each run's end state (state hash, display hash, guest cycles,
//! instructions) must equal `--reference` (default `bench/reference.txt`) and
//! the other mode's. A mismatch exits non-zero. A reference entry is bound to
//! a disc fingerprint; a rebuilt disc is reported as "disc changed" and is not
//! compared. `--bless` rewrites the reference from this run (use it only for a
//! deliberate timing-model change, never to make a speed change pass).
//!
//! Speed: frames per host CPU-second and per billion host cycles over the
//! window, median of `--reps`. The cycle figure moves far less than CPU time
//! when other work shares the machine. DuckStation numbers need a runner built
//! with the bench patch (it prints `BENCH mark`/`BENCH end` lines); it has no
//! tape replay, so only `pulses:` games are run on it.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[repr(C)]
struct RusageInfoV4 {
    uuid: [u8; 16],
    counters: [u64; 40],
}

extern "C" {
    fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut RusageInfoV4) -> i32;
}

/// (instructions, cycles) retired so far by another process, all threads.
fn host_counters(pid: u32) -> (u64, u64) {
    let mut info = RusageInfoV4 {
        uuid: [0; 16],
        counters: [0; 40],
    };
    // SAFETY: the kernel fills the buffer for flavour 4 (RUSAGE_INFO_V4).
    if unsafe { proc_pid_rusage(pid as i32, 4, &mut info) } != 0 {
        return (0, 0);
    }
    (info.counters[29], info.counters[30])
}

struct Game {
    name: String,
    disc: PathBuf,
    from: u64,
    frames: u64,
    input: String,
    ds_shift: u64,
}

fn common_pulses() -> String {
    (0..20)
        .map(|i| format!("0x{:04x}@{}+8", if i % 2 == 0 { 0x0008 } else { 0x4000 }, 600 + 150 * i))
        .collect::<Vec<_>>()
        .join(",")
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME"))
}

fn parse_manifest(path: &Path, games: &Path) -> Vec<Game> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let f: Vec<&str> = l.split('|').map(str::trim).collect();
            assert!(f.len() >= 5, "manifest line needs 5+ fields: {l}");
            let disc = if Path::new(f[1]).is_absolute() {
                PathBuf::from(f[1])
            } else {
                games.join(f[1])
            };
            Game {
                name: f[0].to_string(),
                disc,
                from: f[2].parse().expect("from"),
                frames: f[3].parse().expect("frames"),
                input: f[4].to_string(),
                ds_shift: f.get(5).map_or(0, |v| v.parse().expect("ds_shift")),
            }
        })
        .collect()
}

fn fnv(h: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(h, |h, &b| {
        (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
    })
}

/// A cheap fingerprint of a disc: the cue text plus, for every bin it names,
/// the size and the first and last mebibyte.
fn disc_id(cue: &Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let text = std::fs::read_to_string(cue).unwrap_or_default();
    let mut h = fnv(0xcbf2_9ce4_8422_2325, text.as_bytes());
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("FILE") else {
            continue;
        };
        let name = rest.split('"').nth(1).unwrap_or("");
        let Ok(mut f) = std::fs::File::open(cue.parent().unwrap_or(Path::new(".")).join(name)) else {
            continue;
        };
        let len = f.metadata().map_or(0, |m| m.len());
        h = fnv(h, &len.to_le_bytes());
        let mut buf = vec![0u8; 1 << 20];
        for at in [0, len.saturating_sub(1 << 20)] {
            let _ = f.seek(SeekFrom::Start(at));
            let n = f.read(&mut buf).unwrap_or(0);
            h = fnv(h, &buf[..n]);
        }
    }
    format!("{h:016x}")
}

#[derive(Clone, Debug, Default)]
struct Run {
    fps_cpu: f64,
    fps_gcycle: f64,
    /// Host instructions per emulated frame, in thousands.
    kinsn_frame: f64,
    state_hash: String,
    display_hash: String,
    cycles: String,
    instructions: String,
}

fn kv(line: &str) -> BTreeMap<String, String> {
    line.split_whitespace()
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn run_psoxide(jit_run: &Path, g: &Game, mode: &str, tapes: &Path) -> Result<Run, String> {
    let mut cmd = Command::new(jit_run);
    cmd.arg("bench")
        .arg(&g.disc)
        .arg(g.frames.to_string())
        .arg(mode)
        .arg("--from")
        .arg(g.from.to_string());
    match g.input.split_once(':') {
        Some(("tape", t)) => {
            cmd.arg("--tape").arg(tapes.join(t));
        }
        Some(("pulses", "common")) => {
            cmd.arg("--pulses").arg(common_pulses());
        }
        Some(("pulses", "none")) => {}
        Some(("pulses", spec)) => {
            cmd.arg("--pulses").arg(spec);
        }
        _ => return Err(format!("bad input `{}`", g.input)),
    }
    let out = cmd
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!("exit {}", out.status));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text
        .lines()
        .find(|l| l.starts_with("mode="))
        .ok_or("no result line")?;
    let m = kv(line);
    let num = |k: &str| m.get(k).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    let timed = (num("frames") - num("timed_from")).max(1.0);
    Ok(Run {
        kinsn_frame: num("host_ginsn") * 1e6 / timed,
        fps_cpu: num("fps_per_cpu_s"),
        fps_gcycle: num("fps_per_gcycle"),
        state_hash: m.get("state_hash").cloned().unwrap_or_default(),
        display_hash: m.get("display_hash").cloned().unwrap_or_default(),
        cycles: m.get("cycles").cloned().unwrap_or_default(),
        instructions: m.get("instructions").cloned().unwrap_or_default(),
    })
}

fn run_duckstation(bin: &Path, g: &Game, cpu: &str) -> Result<Run, String> {
    let (pulses, ok) = match g.input.split_once(':') {
        Some(("pulses", "common")) => (common_pulses(), true),
        Some(("pulses", "none")) => (String::new(), true),
        Some(("pulses", spec)) => (spec.to_string(), true),
        _ => (String::new(), false),
    };
    if !ok {
        return Err("tape input".into());
    }
    let shift = g.ds_shift;
    let shifted = pulses
        .split(',')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (mask, rest) = p.split_once('@').unwrap();
            let (start, len) = rest.split_once('+').unwrap();
            format!("{mask}@{}+{len}", start.parse::<u64>().unwrap() + shift)
        })
        .collect::<Vec<_>>()
        .join(",");
    let mut child = Command::new(bin)
        .args(["-frames", &(g.frames + shift).to_string()])
        .args(["-renderer", "Software", "-cpu", cpu, "--"])
        .arg(&g.disc)
        .env("BENCH_PULSES", shifted)
        .env("BENCH_MARK", (g.from + shift).to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;
    let pid = child.id();
    let mut mark = None;
    let mut end = None;
    for line in BufReader::new(child.stdout.take().unwrap()).lines().map_while(Result::ok) {
        let tag = line.split_whitespace().nth(1).unwrap_or("");
        if line.starts_with("BENCH ") && (tag == "mark" || tag == "end") {
            let m = kv(&line);
            let frame: f64 = m["frame"].parse().unwrap_or(0.0);
            let cpu_s: f64 = m["cpu"].parse().unwrap_or(0.0);
            let rec = (frame, cpu_s, host_counters(pid));
            if tag == "mark" {
                mark = Some(rec);
            } else {
                end = Some(rec);
            }
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("exit {status}"));
    }
    let (m, e) = (mark.ok_or("no mark")?, end.ok_or("no end")?);
    let frames = e.0 - m.0;
    let gcycles = (e.2 .1 - m.2 .1) as f64 * 1e-9;
    Ok(Run {
        kinsn_frame: (e.2 .0 - m.2 .0) as f64 * 1e-3 / frames,
        fps_cpu: frames / (e.1 - m.1),
        fps_gcycle: frames / gcycles.max(1e-9),
        ..Run::default()
    })
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if v.is_empty() {
        return f64::NAN;
    }
    if v.len() % 2 == 1 {
        v[v.len() / 2]
    } else {
        (v[v.len() / 2 - 1] + v[v.len() / 2]) / 2.0
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut take = |flag: &str| -> Option<String> {
        let i = args.iter().position(|a| a == flag)?;
        let v = args.get(i + 1).cloned();
        args.drain(i..(i + 2).min(args.len()));
        v
    };
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let manifest = take("--manifest").map_or(crate_dir.join("bench/suite.txt"), PathBuf::from);
    let reference_path =
        take("--reference").map_or(crate_dir.join("bench/reference.txt"), PathBuf::from);
    let games_dir = take("--games").map_or(home().join("Downloads/ps1 games"), PathBuf::from);
    let tapes_dir = take("--tapes").map_or(
        home().join("Library/Application Support/PSoXide-perf/tapes-manny"),
        PathBuf::from,
    );
    let reps: usize = take("--reps").map_or(3, |v| v.parse().expect("--reps"));
    let modes: Vec<String> = take("--modes")
        .unwrap_or_else(|| "run,tier".into())
        .split(',')
        .map(String::from)
        .collect();
    let only: Option<Vec<String>> =
        take("--only").map(|v| v.split(',').map(String::from).collect());
    let md_out = take("--md");
    let duckstation = take("--duckstation").map(PathBuf::from);
    let ds_modes: Vec<String> = take("--ds-modes")
        .unwrap_or_else(|| "Interpreter,CachedInterpreter,Recompiler".into())
        .split(',')
        .map(String::from)
        .collect();
    let bless = args.iter().any(|a| a == "--bless");

    let jit_run = std::env::current_exe()
        .expect("current_exe")
        .with_file_name("jit_run");
    assert!(jit_run.exists(), "build jit_run first (cargo build --release -p psoxide-jit --examples)");

    let mut reference: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if let Ok(text) = std::fs::read_to_string(&reference_path) {
        for l in text.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
            let f: Vec<String> = l.split_whitespace().map(String::from).collect();
            reference.insert(f[0].clone(), f[1..].to_vec());
        }
    }

    let games: Vec<Game> = parse_manifest(&manifest, &games_dir)
        .into_iter()
        .filter(|g| only.as_ref().is_none_or(|o| o.contains(&g.name)))
        .collect();

    struct Cell {
        cpu: f64,
        gcycle: f64,
        kinsn: f64,
    }
    let mut cells: BTreeMap<(String, String), Cell> = BTreeMap::new();
    let mut identity: BTreeMap<String, String> = BTreeMap::new();
    let mut new_reference: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut failed = false;

    for g in &games {
        if !g.disc.exists() {
            println!("{}: disc missing ({}), skipped", g.name, g.disc.display());
            identity.insert(g.name.clone(), "skipped (no disc)".into());
            continue;
        }
        let id = disc_id(&g.disc);
        let mut first: Option<Run> = None;
        let mut verdicts: Vec<String> = Vec::new();
        for mode in &modes {
            let (mut cpu, mut gc, mut ki) = (Vec::new(), Vec::new(), Vec::new());
            for rep in 0..reps {
                match run_psoxide(&jit_run, g, mode, &tapes_dir) {
                    Ok(r) => {
                        println!(
                            "{} {mode} rep {rep}: {:.1} fps/cpu-s, {:.2} fps/Gcycle, state {}",
                            g.name, r.fps_cpu, r.fps_gcycle, r.state_hash
                        );
                        cpu.push(r.fps_cpu);
                        gc.push(r.fps_gcycle);
                        ki.push(r.kinsn_frame);
                        match &first {
                            None => first = Some(r.clone()),
                            Some(f) => {
                                if (&f.state_hash, &f.display_hash, &f.cycles, &f.instructions)
                                    != (&r.state_hash, &r.display_hash, &r.cycles, &r.instructions)
                                {
                                    verdicts.push(format!("MISMATCH {mode} rep {rep} vs first run"));
                                    failed = true;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        println!("{} {mode} rep {rep}: FAILED {e}", g.name);
                        verdicts.push(format!("FAILED {mode}: {e}"));
                        failed = true;
                    }
                }
            }
            cells.insert(
                (g.name.clone(), format!("PSoXide {mode}")),
                Cell {
                    cpu: median(&mut cpu),
                    gcycle: median(&mut gc),
                    kinsn: median(&mut ki),
                },
            );
        }
        if let Some(f) = &first {
            let state = vec![
                id.clone(),
                f.state_hash.clone(),
                f.display_hash.clone(),
                f.cycles.clone(),
                f.instructions.clone(),
            ];
            match reference.get(&g.name) {
                Some(r) if r[0] != id => verdicts.push("disc changed (not compared)".into()),
                Some(r) if *r == state => {}
                Some(_) => {
                    verdicts.push("DIFFERS from reference".into());
                    failed = true;
                }
                None => verdicts.push("no reference".into()),
            }
            new_reference.insert(g.name.clone(), state);
        }
        identity.insert(
            g.name.clone(),
            if verdicts.is_empty() {
                "identical".into()
            } else {
                verdicts.join("; ")
            },
        );
        if let Some(ds) = &duckstation {
            for cpu_mode in &ds_modes {
                let (mut cpu, mut gc, mut ki) = (Vec::new(), Vec::new(), Vec::new());
                for _ in 0..reps {
                    match run_duckstation(ds, g, cpu_mode) {
                        Ok(r) => {
                            println!(
                                "{} DuckStation {cpu_mode}: {:.1} fps/cpu-s, {:.2} fps/Gcycle",
                                g.name, r.fps_cpu, r.fps_gcycle
                            );
                            cpu.push(r.fps_cpu);
                            gc.push(r.fps_gcycle);
                            ki.push(r.kinsn_frame);
                        }
                        Err(e) => {
                            println!("{} DuckStation {cpu_mode}: {e}", g.name);
                            break;
                        }
                    }
                }
                if !cpu.is_empty() {
                    cells.insert(
                        (g.name.clone(), format!("DuckStation {cpu_mode}")),
                        Cell {
                            cpu: median(&mut cpu),
                            gcycle: median(&mut gc),
                            kinsn: median(&mut ki),
                        },
                    );
                }
            }
        }
    }

    // Tables.
    let mut cols: Vec<String> = modes.iter().map(|m| format!("PSoXide {m}")).collect();
    if duckstation.is_some() {
        cols.extend(ds_modes.iter().map(|m| format!("DuckStation {m}")));
    }
    let mut md = String::new();
    for (title, pick) in [
        ("Frames per host CPU-second (median)", 0),
        ("Frames per billion host cycles (median)", 1),
        ("Host instructions per emulated frame, thousands (median; lower is better)", 2),
    ] {
        md.push_str(&format!("\n### {title}\n\n| game |"));
        for c in &cols {
            md.push_str(&format!(" {c} |"));
        }
        md.push_str(" identity |\n|---|");
        for _ in &cols {
            md.push_str("---:|");
        }
        md.push_str("---|\n");
        let mut geo: BTreeMap<&str, (f64, u32)> = BTreeMap::new();
        for g in &games {
            md.push_str(&format!("| {} |", g.name));
            for c in &cols {
                match cells.get(&(g.name.clone(), c.clone())) {
                    Some(cell) => {
                        let v = match pick {
                            0 => cell.cpu,
                            1 => cell.gcycle,
                            _ => cell.kinsn,
                        };
                        md.push_str(&format!(" {v:.1} |"));
                        let e = geo.entry(c.as_str()).or_insert((0.0, 0));
                        e.0 += v.ln();
                        e.1 += 1;
                    }
                    None => md.push_str(" |"),
                }
            }
            md.push_str(&format!(" {} |\n", identity.get(&g.name).map_or("", String::as_str)));
        }
        md.push_str("| geomean (own rows) |");
        for c in &cols {
            match geo.get(c.as_str()) {
                Some((s, n)) => md.push_str(&format!(" {:.1} |", (s / *n as f64).exp())),
                None => md.push_str(" |"),
            }
        }
        md.push_str(" |\n");
    }
    println!("{md}");
    if let Some(p) = md_out {
        std::fs::write(&p, &md).expect("write --md");
    }
    if bless {
        let mut out = String::from(
            "# name disc_id state_hash display_hash guest_cycles instructions (written by bench_suite --bless)\n",
        );
        for (k, v) in reference.iter().filter(|(k, _)| !new_reference.contains_key(*k)) {
            out.push_str(&format!("{k} {}\n", v.join(" ")));
        }
        for (k, v) in &new_reference {
            out.push_str(&format!("{k} {}\n", v.join(" ")));
        }
        std::fs::write(&reference_path, out).expect("write reference");
        println!("reference written: {}", reference_path.display());
    }
    if failed && !bless {
        println!("EXACTNESS FAILURE");
        std::process::exit(1);
    }
}
