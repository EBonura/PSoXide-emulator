//! The tick log, and the frames, scenes and loads read out of it.
//!
//! One tick is one vblank of emulated time. A frame is a tick in which the display start
//! flipped; its frame time is the bus cycles since the previous flip, a whole number of
//! vblanks. A load is a stretch the player cannot play through (see `config::Scene`); frames
//! that touch one are kept but marked `load` and left out of every fps figure.

use std::collections::HashMap;

use crate::config::Config;

/// Bus clock of the console.
pub(crate) const BUS_HZ: f64 = 33_868_800.0;

/// One row of the tick log.
pub(crate) struct Tick {
    pub tick: u64,
    pub step: u64,
    /// Bus cycles since the log started, at the end of this tick (reboots do not reset it).
    pub cycles: u64,
    pub cpu_delta: u64,
    pub flipped: bool,
    pub words: Vec<u32>,
}

/// A parsed tick log.
pub(crate) struct TickLog {
    /// Names of the RAM word columns, in file order.
    pub columns: Vec<String>,
    pub ticks: Vec<Tick>,
    /// Bus cycles in one vblank, the median tick length.
    pub period: u64,
}

impl TickLog {
    pub(crate) fn parse(text: &str) -> Result<TickLog, String> {
        let mut lines = text.lines();
        let head = lines.next().ok_or("empty tick log")?;
        let names: Vec<&str> = head.split(',').collect();
        let fixed = [
            "boot",
            "tick",
            "step",
            "bus_cycles",
            "cycle_delta",
            "cpu_delta",
            "flipped",
            "polls",
        ];
        if names.len() < fixed.len() || names[..fixed.len()] != fixed {
            return Err("not a psoxide-gate tick log (header columns differ)".into());
        }
        let mut ticks = Vec::new();
        let mut cycles = 0u64;
        let mut deltas = Vec::new();
        for (n, line) in lines.enumerate() {
            let f: Vec<&str> = line.split(',').collect();
            if f.len() != names.len() {
                // A half-written last row of a killed run.
                if line.is_empty() {
                    continue;
                }
                return Err(format!(
                    "row {}: {} fields, expected {}",
                    n + 2,
                    f.len(),
                    names.len()
                ));
            }
            let num = |i: usize| -> Result<u64, String> {
                f[i].parse::<u64>()
                    .map_err(|_| format!("row {}: bad number `{}`", n + 2, f[i]))
            };
            let delta = num(4)?;
            cycles += delta;
            deltas.push(delta);
            let mut words = Vec::with_capacity(names.len() - fixed.len());
            for i in fixed.len()..names.len() {
                words.push(num(i)? as u32);
            }
            ticks.push(Tick {
                tick: num(1)?,
                step: num(2)?,
                cycles,
                cpu_delta: num(5)?,
                flipped: f[6] == "1",
                words,
            });
        }
        if ticks.is_empty() {
            return Err("tick log has no rows".into());
        }
        deltas.sort_unstable();
        let period = deltas[deltas.len() / 2].max(1);
        Ok(TickLog {
            columns: names[fixed.len()..].iter().map(|s| s.to_string()).collect(),
            ticks,
            period,
        })
    }

    pub(crate) fn column(&self, name: &str) -> Result<usize, String> {
        self.columns.iter().position(|c| c == name).ok_or_else(|| {
            format!(
                "tick log has no column `{name}` (it has: {})",
                self.columns.join(", ")
            )
        })
    }

    /// Milliseconds in one vblank.
    pub(crate) fn vblank_ms(&self) -> f64 {
        self.period as f64 * 1000.0 / BUS_HZ
    }
}

/// One rendered frame.
pub(crate) struct Frame {
    pub tick: u64,
    pub step: u64,
    pub scene: String,
    pub vblanks: u64,
    pub cycles: u64,
    pub instrs: u64,
    pub load: bool,
    /// Seconds from the origin to the flip that ended the frame.
    pub t_s: f64,
    /// Bus cycles the guest spent in its update and render stages during the frame, when the
    /// game emits telemetry.
    pub stages: Option<[u64; 2]>,
    pub counters: Vec<u32>,
    pub pos: Vec<u32>,
}

impl Frame {
    pub(crate) fn ms(&self) -> f64 {
        self.cycles as f64 * 1000.0 / BUS_HZ
    }
}

/// A loading pause.
pub(crate) struct Load {
    pub from: String,
    pub to: String,
    pub start_tick: u64,
    pub vblanks: u64,
    pub start_s: f64,
    pub seconds: f64,
}

/// A route, measured.
pub(crate) struct Run {
    pub frames: Vec<Frame>,
    pub loads: Vec<Load>,
    /// Scenes in order of first appearance.
    pub scenes: Vec<String>,
    pub total_s: f64,
    pub vblank_ms: f64,
    /// Start of each journey step, in seconds from the origin (step number, time).
    pub step_starts: Vec<(u64, f64)>,
    pub counter_labels: Vec<String>,
    pub pos_labels: Vec<String>,
    /// Journey step labels from the gate's `-v` output.
    /// Whether the log carries guest telemetry (a profile build) for the update/render split.
    pub has_stages: bool,
    pub labels: HashMap<u64, String>,
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Pre,
    Play,
    Load,
}

/// Read the step names out of a `psoxide-gate run -v` log.
pub(crate) fn step_labels(gate_log: &str, offset: u64) -> HashMap<u64, String> {
    let mut labels = HashMap::new();
    for line in gate_log.lines() {
        // [gate] tick   7210 step 13 c0a0b first simulation tick
        let Some(rest) = line.strip_prefix("[gate] tick ") else {
            continue;
        };
        let mut it = rest.trim_start().splitn(4, ' ');
        let (_tick, _step) = (it.next(), it.next());
        if let Some(n) = it.next().and_then(|s| s.parse::<u64>().ok()) {
            labels.insert(n + offset, it.next().unwrap_or("").trim().to_string());
        }
    }
    labels
}

impl Run {
    /// Turn a tick log into frames and loads using the game's config.
    pub(crate) fn measure(
        log: &TickLog,
        cfg: &Config,
        labels: HashMap<u64, String>,
    ) -> Result<Run, String> {
        let scene_col = log.column(&cfg.scene.word)?;
        let ready_col = cfg
            .scene
            .ready
            .as_deref()
            .map(|r| log.column(r))
            .transpose()?;
        let counter_cols: Vec<usize> = cfg
            .counters
            .iter()
            .map(|c| log.column(&c.word))
            .collect::<Result<_, _>>()?;
        let pos_cols: Vec<usize> = cfg
            .pos
            .iter()
            .map(|c| log.column(c))
            .collect::<Result<_, _>>()?;
        let tel_cols: Option<[usize; 2]> =
            match (log.column("tel_update"), log.column("tel_render")) {
                (Ok(u), Ok(r)) => Some([u, r]),
                _ => None,
            };
        let has_stages = tel_cols.is_some_and(|c| {
            log.ticks
                .iter()
                .any(|t| t.words[c[0]] != 0 || t.words[c[1]] != 0)
        });
        let n = log.ticks.len();

        let mode_col = cfg
            .scene
            .mode
            .as_deref()
            .map(|m| log.column(m))
            .transpose()?;
        let (phase, scene_of, loads_raw) = match (mode_col, ready_col) {
            (Some(mc), _) => classify_mode(log, cfg, scene_col, mc),
            (None, Some(rc)) => classify_ready(log, cfg, scene_col, rc),
            (None, None) => classify_stall(log, cfg, scene_col),
        };
        let first_play = phase
            .iter()
            .position(|p| *p == Phase::Play)
            .ok_or("no gameplay found: the scene word never held a known scene")?;
        // Game time: every tick of play or loading counts, the boot, menus and the gaps between
        // journey segments do not.
        let mut clock = vec![0u64; n];
        let mut run = 0u64;
        for i in 0..n {
            if phase[i] != Phase::Pre {
                run += log.ticks[i].cycles - if i == 0 { 0 } else { log.ticks[i - 1].cycles };
            }
            clock[i] = run;
        }
        let c0 = clock[first_play];

        let mut frames = Vec::new();
        let mut last_flip: Option<usize> = None;
        for (i, t) in log.ticks.iter().enumerate() {
            if !t.flipped {
                continue;
            }
            let lf = match last_flip {
                Some(lf) if lf >= first_play => lf,
                _ => {
                    last_flip = Some(i);
                    continue;
                }
            };
            let span = lf + 1..=i;
            if span.clone().any(|k| phase[k] == Phase::Pre) {
                last_flip = Some(i);
                continue;
            }
            let touches_load = span.clone().any(|k| phase[k] == Phase::Load);
            let scene = if touches_load {
                span.clone()
                    .rev()
                    .find(|k| phase[*k] == Phase::Load)
                    .map(|k| scene_of[k].clone())
                    .unwrap_or_default()
            } else {
                scene_of[i].clone()
            };
            frames.push(Frame {
                tick: t.tick,
                step: t.step,
                scene,
                vblanks: t.tick - log.ticks[lf].tick,
                cycles: t.cycles - log.ticks[lf].cycles,
                instrs: log.ticks[lf + 1..=i].iter().map(|x| x.cpu_delta).sum(),
                load: touches_load,
                t_s: (clock[i] - c0) as f64 / BUS_HZ,
                counters: counter_cols
                    .iter()
                    .map(|c| {
                        log.ticks[lf + 1..=i]
                            .iter()
                            .map(|x| x.words[*c])
                            .max()
                            .unwrap_or(0)
                    })
                    .collect(),
                pos: pos_cols.iter().map(|c| t.words[*c]).collect(),
                stages: tel_cols.filter(|_| has_stages).map(|c| {
                    let sum = |k: usize| {
                        log.ticks[lf + 1..=i]
                            .iter()
                            .map(|x| u64::from(x.words[c[k]]))
                            .sum()
                    };
                    [sum(0), sum(1)]
                }),
            });
            last_flip = Some(i);
        }

        let vb_s = log.period as f64 / BUS_HZ;
        let loads: Vec<Load> = loads_raw
            .into_iter()
            .filter(|l| l.0 >= first_play)
            .map(|(start, from, to, vblanks)| Load {
                from,
                to,
                start_tick: log.ticks[start].tick,
                vblanks,
                start_s: (clock[start] - c0) as f64 / BUS_HZ,
                seconds: vblanks as f64 * vb_s,
            })
            .collect();
        let mut scenes: Vec<String> = Vec::new();
        for f in &frames {
            if !scenes.contains(&f.scene) {
                scenes.push(f.scene.clone());
            }
        }
        let mut step_starts: Vec<(u64, f64)> = Vec::new();
        for i in first_play..n {
            let t = &log.ticks[i];
            if step_starts.last().map(|s| s.0) != Some(t.step) {
                let begin = if i == 0 { 0 } else { clock[i - 1] };
                step_starts.push((t.step, begin.saturating_sub(c0) as f64 / BUS_HZ));
            }
        }
        let total_s = (clock[n - 1] - c0) as f64 / BUS_HZ;
        Ok(Run {
            frames,
            loads,
            scenes,
            total_s,
            vblank_ms: log.vblank_ms(),
            step_starts,
            counter_labels: cfg.counters.iter().map(|c| c.label.clone()).collect(),
            pos_labels: cfg.pos.clone(),
            labels,
            has_stages,
        })
    }

    pub(crate) fn play_frames(&self) -> impl Iterator<Item = &Frame> {
        self.frames.iter().filter(|f| !f.load)
    }
}

type Classified = (Vec<Phase>, Vec<String>, Vec<(usize, String, String, u64)>);

/// Loads that end at the first simulation tick of the new scene (`scene.ready` set).
fn classify_ready(log: &TickLog, cfg: &Config, sc: usize, rc: usize) -> Classified {
    let n = log.ticks.len();
    let mut phase = vec![Phase::Pre; n];
    let mut scene_of = vec![String::new(); n];
    let mut loads = Vec::new();
    let mut cur: Option<u32> = None;
    let mut cur_name = String::new();
    let mut cur_phase = Phase::Pre;
    let mut cand: Option<(u32, String, u32)> = None;
    let mut load_start: Option<usize> = None;
    let mut started = false;
    for (i, t) in log.ticks.iter().enumerate() {
        let id = t.words[sc];
        let sim = t.words[rc];
        let known = if t.step >= cfg.scene.from_step {
            cfg.scene_name(id)
        } else {
            None
        };
        match &known {
            Some(name) => {
                if cur != Some(id) && !matches!(&cand, Some((c, _, _)) if *c == id) {
                    cand = Some((id, name.clone(), sim));
                }
            }
            None => cand = None,
        }
        if let Some((cid, name, sim0)) = cand.clone() {
            if sim != sim0 && id == cid {
                if started {
                    if let Some(ls) = load_start {
                        loads.push((
                            ls,
                            cur_name.clone(),
                            name.clone(),
                            t.tick - log.ticks[ls].tick,
                        ));
                        for p in ls..i {
                            phase[p] = Phase::Load;
                            scene_of[p] = name.clone();
                        }
                    }
                }
                started = true;
                cur = Some(cid);
                cur_name = name;
                cur_phase = Phase::Play;
                load_start = None;
                cand = None;
            }
        }
        if started && cur_phase == Phase::Play && cur.is_some_and(|c| c != id) {
            cur_phase = Phase::Load;
            load_start = Some(i);
        }
        phase[i] = if started { cur_phase } else { Phase::Pre };
        scene_of[i] = cur_name.clone();
    }
    (phase, scene_of, loads)
}

/// Loads are frame intervals of `load_vblanks` or more (`scene.ready` not set).
fn classify_stall(log: &TickLog, cfg: &Config, sc: usize) -> Classified {
    let n = log.ticks.len();
    let mut phase = vec![Phase::Pre; n];
    let mut scene_of = vec![String::new(); n];
    let mut cur = String::new();
    let mut started = false;
    for (i, t) in log.ticks.iter().enumerate() {
        if t.step >= cfg.scene.from_step {
            if let Some(name) = cfg.scene_name(t.words[sc]) {
                cur = name;
                started = true;
            }
        }
        scene_of[i] = cur.clone();
        phase[i] = if started { Phase::Play } else { Phase::Pre };
    }
    let mut loads = Vec::new();
    let mut last_flip: Option<usize> = None;
    for (i, t) in log.ticks.iter().enumerate() {
        if !t.flipped {
            continue;
        }
        if let Some(lf) = last_flip {
            let vb = t.tick - log.ticks[lf].tick;
            if phase[lf] != Phase::Pre && vb >= cfg.scene.load_vblanks {
                loads.push((lf, scene_of[lf].clone(), scene_of[i].clone(), vb));
                for p in &mut phase[lf + 1..=i] {
                    *p = Phase::Load;
                }
            }
        }
        last_flip = Some(i);
    }
    (phase, scene_of, loads)
}

/// The game says what it is doing (`scene.mode`): `play` is play, `load` is loading, anything
/// else (title, menus, the boot of a journey segment) is not measured. Frame intervals of
/// `load_vblanks` or more inside play are loads too, for loads the game does not announce.
fn classify_mode(log: &TickLog, cfg: &Config, sc: usize, mc: usize) -> Classified {
    let n = log.ticks.len();
    let mut phase = vec![Phase::Pre; n];
    let mut scene_of = vec![String::new(); n];
    let mut cur = String::new();
    for (i, t) in log.ticks.iter().enumerate() {
        if t.step >= cfg.scene.from_step {
            if let Some(name) = cfg.scene_name(t.words[sc]) {
                cur = name;
            }
            let m = t.words[mc];
            phase[i] = if m == cfg.scene.play {
                Phase::Play
            } else if m == cfg.scene.load {
                Phase::Load
            } else {
                Phase::Pre
            };
        }
        scene_of[i] = cur.clone();
    }
    let mut loads = Vec::new();
    // Runs of announced loading. The scene after the run is where the load led.
    let mut i = 0;
    while i < n {
        if phase[i] != Phase::Load {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && phase[i] == Phase::Load {
            i += 1;
        }
        let to = scene_of
            .get(i)
            .cloned()
            .unwrap_or_else(|| scene_of[start].clone());
        let from = scene_of[start].clone();
        for s in &mut scene_of[start..i] {
            *s = to.clone();
        }
        loads.push((
            start,
            from,
            to,
            log.ticks[i - 1].tick - log.ticks[start].tick + 1,
        ));
    }
    // Unannounced stalls inside play.
    let mut last_flip: Option<usize> = None;
    for (i, t) in log.ticks.iter().enumerate() {
        if !t.flipped {
            continue;
        }
        if let Some(lf) = last_flip {
            let vb = t.tick - log.ticks[lf].tick;
            let span_play = phase[lf..=i].iter().all(|p| *p == Phase::Play);
            if span_play && vb >= cfg.scene.load_vblanks {
                loads.push((lf, scene_of[lf].clone(), scene_of[i].clone(), vb));
                for p in &mut phase[lf + 1..=i] {
                    *p = Phase::Load;
                }
            }
        }
        last_flip = Some(i);
    }
    loads.sort_by_key(|l| l.0);
    (phase, scene_of, loads)
}

impl TickLog {
    /// Join the logs of consecutive journey segments (each booted from the card the one before
    /// left) into one: ticks, steps and cycles continue where the previous segment stopped.
    /// Returns the step offset added to each segment.
    pub(crate) fn join(parts: Vec<TickLog>) -> Result<(TickLog, Vec<u64>), String> {
        let mut parts = parts.into_iter();
        let mut all = parts.next().ok_or("no tick log given")?;
        let mut offsets = vec![0u64];
        for p in parts {
            if p.columns != all.columns {
                return Err("tick logs of one route must log the same columns".into());
            }
            let tick_off = all.ticks.last().map_or(0, |t| t.tick);
            let step_off = all.ticks.iter().map(|t| t.step).max().unwrap_or(0);
            let cycle_off = all.ticks.last().map_or(0, |t| t.cycles);
            offsets.push(step_off);
            all.ticks.extend(p.ticks.into_iter().map(|mut t| {
                t.tick += tick_off;
                t.step += step_off;
                t.cycles += cycle_off;
                t
            }));
        }
        Ok((all, offsets))
    }
}
