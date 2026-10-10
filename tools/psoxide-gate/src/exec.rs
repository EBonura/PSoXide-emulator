//! Runs one journey: boots the disc, plays the steps, and judges every
//! checkpoint across the CPU/hardware render matrix.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::img::{Img, Rect, Tolerance};
use crate::journey::{Assert, Journey, Step};
use crate::machine::{button_mask, Boot, Machine};
use crate::render::HwSet;
use crate::symbols::Symbols;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    /// Failed, and marked `expect_fail`.
    XFail,
    /// Passed although marked `expect_fail`: remove the marker.
    XPass,
    /// Could not be evaluated for a stated, benign reason.
    Skip,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Fail => "FAIL",
            Status::XFail => "XFAIL",
            Status::XPass => "XPASS",
            Status::Skip => "SKIP",
        }
    }
    pub fn is_failure(self) -> bool {
        matches!(self, Status::Fail | Status::XPass)
    }
}

fn checked_assert(name: String, outcome: Result<(bool, String), String>, a: &Assert) -> Check {
    let (status, mut detail) = match outcome {
        Err(error) => (Status::Fail, error),
        Ok((ok, detail)) => (
            match (ok, a.expect_fail) {
                (true, false) => Status::Pass,
                (false, false) => Status::Fail,
                (false, true) => Status::XFail,
                (true, true) => Status::XPass,
            },
            detail,
        ),
    };
    if let Some(note) = &a.note {
        detail.push_str(&format!(" [{note}]"));
    }
    Check {
        name,
        status,
        detail,
    }
}

#[derive(Clone, Debug)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
}

/// Everything captured at one checkpoint.
pub struct Capture {
    pub cpu: Img,
    pub golden: Option<Img>,
    pub golden_error: Option<String>,
    /// `(scale, frame)` per hardware scale.
    pub hw: Vec<(u32, Img)>,
    pub hw_skip: Option<String>,
    /// Per-channel tolerances used for the hardware comparisons (for the heat maps).
    pub hw1_channel: u8,
    pub hwn_channel: u8,
}

pub struct Group {
    /// Checkpoint name, or `step N` for bare asserts.
    pub name: String,
    pub label: Option<String>,
    pub tick: u64,
    pub capture: Option<Capture>,
    pub checks: Vec<Check>,
    /// Whether this checkpoint wants its golden (re)written by `bless`.
    pub wants_golden: bool,
}

pub struct JourneyResult {
    pub name: String,
    pub title: String,
    pub disc: PathBuf,
    pub disc_id: String,
    pub groups: Vec<Group>,
    /// Set when the journey could not finish (guest fault, timeout, bad setup).
    pub abort: Option<String>,
    pub ticks: u64,
    pub emu_secs: f64,
    pub wall: Duration,
    pub hw_adapter: Option<String>,
    pub notes: Vec<String>,
}

impl JourneyResult {
    pub fn checks(&self) -> impl Iterator<Item = &Check> {
        self.groups.iter().flat_map(|g| g.checks.iter())
    }
    pub fn failures(&self) -> usize {
        self.checks().filter(|c| c.status.is_failure()).count() + usize::from(self.abort.is_some())
    }
    pub fn passed(&self) -> bool {
        self.failures() == 0
    }
    pub fn count(&self, s: Status) -> usize {
        self.checks().filter(|c| c.status == s).count()
    }
    /// One line for the fleet summary.
    pub fn summary(&self) -> String {
        let verdict = if self.passed() { "PASS" } else { "FAIL" };
        let mut s = format!(
            "{verdict} {:<14} {} checks: {} pass, {} fail",
            self.name,
            self.checks().count(),
            self.count(Status::Pass),
            self.count(Status::Fail),
        );
        for (st, word) in [
            (Status::XFail, "xfail"),
            (Status::XPass, "xpass"),
            (Status::Skip, "skip"),
        ] {
            let n = self.count(st);
            if n > 0 {
                s.push_str(&format!(", {n} {word}"));
            }
        }
        s.push_str(&format!(
            " | {} ticks ({:.0} s emulated) in {:.1} s | disc {}",
            self.ticks,
            self.emu_secs,
            self.wall.as_secs_f64(),
            self.disc_id
        ));
        if let Some(a) = &self.abort {
            s.push_str(&format!(" | ABORTED: {a}"));
        }
        s
    }
}

pub struct RunOptions {
    pub disc: PathBuf,
    pub repo_root: PathBuf,
    pub golden_dir: PathBuf,
    pub use_hw: bool,
    /// Override the journey's scales (e.g. `--scales 1`).
    pub scales: Option<Vec<u32>>,
    pub verbose: bool,
    pub strict: bool,
}

fn release_requirements(journey: &Journey, scales: &[u32], use_hw: bool) -> Result<(), String> {
    if !use_hw {
        return Err("release gate requires hardware rendering".into());
    }
    if !scales.contains(&1) || !scales.contains(&3) {
        return Err("release gate requires hardware scales 1 and 3".into());
    }
    if journey.checkpoints().next().is_none() {
        return Err("release gate requires at least one checkpoint".into());
    }
    if journey.checkpoints().any(|s| s.no_matrix) {
        return Err("release gate requires the render matrix at every checkpoint".into());
    }
    Ok(())
}

pub fn disc_id(cue_or_bin: &Path) -> String {
    let target = if cue_or_bin
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("cue"))
    {
        std::fs::read_to_string(cue_or_bin)
            .ok()
            .and_then(|t| {
                t.lines().find_map(|l| {
                    let l = l.trim();
                    let rest = l.strip_prefix("FILE")?.trim();
                    let name = if let Some(r) = rest.strip_prefix('"') {
                        r.split('"').next()?.to_string()
                    } else {
                        rest.split_whitespace().next()?.to_string()
                    };
                    Some(cue_or_bin.parent().unwrap_or(Path::new(".")).join(name))
                })
            })
            .unwrap_or_else(|| cue_or_bin.to_path_buf())
    } else {
        cue_or_bin.to_path_buf()
    };
    let Ok(mut file) = std::fs::File::open(&target) else {
        return "missing or unreadable".into();
    };
    let mut hash = Sha256::new();
    let mut block = [0u8; 1024 * 1024];
    loop {
        match file.read(&mut block) {
            Ok(0) => break,
            Ok(n) => hash.update(&block[..n]),
            Err(_) => return "unreadable".into(),
        }
    }
    hash.finalize()
        .iter()
        .take(6)
        .map(|b| format!("{b:02x}"))
        .collect()
}

struct Run<'a> {
    journey: &'a Journey,
    opts: &'a RunOptions,
    m: Machine,
    hw: Option<HwSet>,
    syms: Symbols,
    groups: Vec<Group>,
    /// CPU frame of every checkpoint so far, for `frame_diff`.
    shots: std::collections::BTreeMap<String, Img>,
}

pub fn run_journey(journey: &Journey, opts: &RunOptions) -> JourneyResult {
    let start = Instant::now();
    let mut result = JourneyResult {
        name: journey.name.clone(),
        title: journey
            .title
            .clone()
            .unwrap_or_else(|| journey.name.clone()),
        disc: opts.disc.clone(),
        disc_id: disc_id(&opts.disc),
        groups: Vec::new(),
        abort: None,
        ticks: 0,
        emu_secs: 0.0,
        wall: Duration::ZERO,
        hw_adapter: None,
        notes: Vec::new(),
    };
    let scales = opts
        .scales
        .clone()
        .unwrap_or_else(|| journey.render.scales.clone());
    let unique: std::collections::BTreeSet<u32> = scales.iter().copied().collect();
    if scales.iter().any(|s| *s == 0 || *s > 8) || unique.len() != scales.len() {
        result.abort = Some("hardware scales must be unique and within 1..=8".into());
        return result;
    }
    if opts.strict {
        if let Err(e) = release_requirements(journey, &scales, opts.use_hw) {
            result.abort = Some(e);
            return result;
        }
    }
    let want_hw = opts.use_hw && journey.needs_hw() && !scales.is_empty();
    let m = match Machine::boot(&Boot {
        disc: &opts.disc,
        pad: journey.pad,
        pad2: journey.pad2,
        memcard: journey.memcard,
        cmd_log: want_hw,
    }) {
        Ok(m) => m,
        Err(e) => {
            result.abort = Some(e);
            result.wall = start.elapsed();
            return result;
        }
    };
    let hw = if want_hw {
        match HwSet::new(&scales) {
            Ok(h) => {
                result.hw_adapter = Some(h.adapter.clone());
                Some(h)
            }
            Err(e) => {
                result.abort = Some(format!("hardware renderer: {e}"));
                result.wall = start.elapsed();
                return result;
            }
        }
    } else {
        if journey.needs_hw() && !opts.use_hw {
            result
                .notes
                .push("hardware renderer matrix skipped (--no-hw)".into());
        }
        None
    };

    let mut syms = Symbols::default();
    for (name, addr) in &journey.addr {
        syms.insert(name, *addr);
    }
    for rel in journey.symbols.list() {
        let path = opts.repo_root.join(rel);
        match syms.load(&path) {
            Ok(n) => result
                .notes
                .push(format!("symbols: {n} from {}", path.display())),
            Err(e) => result.notes.push(format!("symbols unavailable: {e}")),
        }
    }

    let mut run = Run {
        journey,
        opts,
        m,
        hw,
        syms,
        groups: Vec::new(),
        shots: std::collections::BTreeMap::new(),
    };
    run.m.release_pads();
    result.abort = run.play().err();
    result.ticks = run.m.ticks;
    result.emu_secs = run.m.ticks as f64 / 60.0;
    result.groups = run.groups;
    result.wall = start.elapsed();
    result
}

impl Run<'_> {
    fn tick(&mut self) -> Result<(), String> {
        if self.m.ticks >= self.journey.max_ticks {
            return Err(format!(
                "journey exceeded max_ticks ({})",
                self.journey.max_ticks
            ));
        }
        if let Some(hw) = self.hw.as_mut() {
            hw.before_tick(&self.m.bus);
        }
        self.m.tick()?;
        if let Some(hw) = self.hw.as_mut() {
            hw.after_tick(&mut self.m.bus);
        }
        Ok(())
    }

    fn ticks(&mut self, n: u64) -> Result<(), String> {
        for _ in 0..n {
            self.tick()?;
        }
        Ok(())
    }

    fn play(&mut self) -> Result<(), String> {
        let steps = &self.journey.steps;
        for (i, step) in steps.iter().enumerate() {
            if self.opts.verbose {
                eprintln!(
                    "[gate] tick {:>6} step {:>2} {}",
                    self.m.ticks,
                    i + 1,
                    step.label
                        .as_deref()
                        .or(step.checkpoint.as_deref())
                        .unwrap_or("")
                );
            }
            self.input_and_wait(step)?;
            if let Some(cond) = &step.wait_until {
                self.wait_until(i, cond, step.timeout.unwrap_or(3600))?;
            }
            if step.checkpoint.is_some() || !step.asserts.is_empty() {
                self.checkpoint(i, step);
            }
        }
        Ok(())
    }

    fn input_and_wait(&mut self, step: &Step) -> Result<(), String> {
        let repeat = step.repeat.unwrap_or(1).max(1);
        for _ in 0..repeat {
            if step.has_input() {
                let port = step.port.unwrap_or(1);
                let held = |press: &[String]| press.iter().fold(0u16, |m, b| m | button_mask(b));
                if !step.press.is_empty() || step.lstick.is_some() || step.rstick.is_some() {
                    self.m.set_pad(
                        port,
                        held(&step.press),
                        step.lstick.unwrap_or(Machine::centre()),
                        step.rstick.unwrap_or(Machine::centre()),
                    );
                }
                if let Some(o) = &step.other {
                    self.m.set_pad(
                        if port == 2 { 1 } else { 2 },
                        held(&o.press),
                        o.lstick.unwrap_or(Machine::centre()),
                        o.rstick.unwrap_or(Machine::centre()),
                    );
                }
                self.ticks(step.hold.unwrap_or(4))?;
                self.m.release_pads();
            }
            self.ticks(step.wait.unwrap_or(0))?;
        }
        Ok(())
    }

    fn wait_until(&mut self, index: usize, cond: &Assert, timeout: u64) -> Result<(), String> {
        for _ in 0..timeout {
            if self.eval_wait(cond).0 {
                return Ok(());
            }
            self.tick()?;
        }
        if self.eval_wait(cond).0 {
            return Ok(());
        }
        let (_, detail) = self.eval_wait(cond);
        let msg = format!(
            "step {}: wait_until timed out after {timeout} ticks ({detail})",
            index + 1
        );
        let cpu = self.m.display_image();
        let (hw, hw_skip) = match &self.hw {
            Some(renderer) => match renderer.capture(&self.m.bus, cpu.w, cpu.h) {
                Ok(frames) => (frames, None),
                Err(reason) => (Vec::new(), Some(reason.describe().to_string())),
            },
            None => (Vec::new(), None),
        };
        self.groups.push(Group {
            name: format!("step {}", index + 1),
            label: Some("wait_until".into()),
            tick: self.m.ticks,
            capture: Some(Capture {
                cpu,
                golden: None,
                golden_error: None,
                hw,
                hw_skip,
                hw1_channel: self.journey.render.hw1.channel,
                hwn_channel: self.journey.render.hwn.channel,
            }),
            checks: vec![Check {
                name: "wait_until".into(),
                status: Status::Fail,
                detail: msg.clone(),
            }],
            wants_golden: false,
        });
        Err(msg)
    }

    fn eval_wait(&self, cond: &Assert) -> (bool, String) {
        match cond.kind.as_str() {
            "pixels" => self.eval_pixels(cond, &self.m.display_image()),
            "flat" | "not_flat" | "dark" | "not_dark" => self.eval_flat(cond, None),
            _ => self.eval_ram(cond).unwrap_or_else(|e| (false, e)),
        }
    }

    /// `flat` / `not_flat`: how much of the frame one colour covers.
    fn eval_flat(&self, a: &Assert, frame: Option<&Img>) -> (bool, String) {
        let owned;
        let img = match frame {
            Some(i) => i,
            None => {
                owned = self.m.display_image();
                &owned
            }
        };
        if a.kind == "dark" || a.kind == "not_dark" {
            let level = a.max_level.unwrap_or(32);
            let f = img.dark_fraction(level);
            let dark = f >= a.min_dominant.unwrap_or(0.95);
            return (
                dark == (a.kind == "dark"),
                format!("{:.1}% of the frame is darker than {level}", f * 100.0),
            );
        }
        let f = img.dominant_fraction();
        let ok = if a.kind == "flat" {
            f >= a.min_dominant.unwrap_or(0.98)
        } else {
            f <= a.max_dominant.unwrap_or(0.98)
        };
        (
            ok,
            format!("most common colour covers {:.1}% of the frame", f * 100.0),
        )
    }

    fn eval_pixels(&self, a: &Assert, img: &Img) -> (bool, String) {
        let Some(col) = a.color else {
            return (false, "no colour".into());
        };
        let tol = a.color_tol.unwrap_or(24);
        let r = a.region.unwrap_or(Rect {
            x: 0,
            y: 0,
            w: img.w,
            h: img.h,
        });
        let mut n = 0u64;
        for y in r.y..(r.y + r.h).min(img.h) {
            for x in r.x..(r.x + r.w).min(img.w) {
                let p = img.px(x, y);
                if (0..3).all(|c| p[c].abs_diff(col[c]) <= tol) {
                    n += 1;
                }
            }
        }
        let ok = a.min_count.is_none_or(|m| n >= m) && a.max_count.is_none_or(|m| n <= m);
        (
            ok,
            format!(
                "{n} pixels of rgb({},{},{}) +-{tol} in {}x{} region at ({},{})",
                col[0], col[1], col[2], r.w, r.h, r.x, r.y
            ),
        )
    }

    fn checkpoint(&mut self, index: usize, step: &Step) {
        let name = step
            .checkpoint
            .clone()
            .unwrap_or_else(|| format!("step {}", index + 1));
        let mut checks = Vec::new();
        let mut capture = None;
        if step.checkpoint.is_some() {
            let cpu = self.m.display_image();
            let golden_path = self.opts.golden_dir.join(format!("{name}.png"));
            let (golden, golden_error) = if golden_path.exists() {
                match Img::load_png(&golden_path) {
                    Ok(img) => (Some(img), None),
                    Err(e) => (None, Some(e)),
                }
            } else {
                (
                    None,
                    Some(format!("no golden frame at {}", golden_path.display())),
                )
            };
            let (hw, hw_skip) = match (&self.hw, step.no_matrix) {
                (Some(hw), false) => match hw.capture(&self.m.bus, cpu.w, cpu.h) {
                    Ok(frames) => (frames, None),
                    Err(skip) => (Vec::new(), Some(skip.describe().to_string())),
                },
                _ => (Vec::new(), None),
            };
            let cfg = &self.journey.render;
            let cap = Capture {
                cpu,
                golden,
                golden_error,
                hw,
                hw_skip,
                hw1_channel: step.hw1.unwrap_or(cfg.hw1).channel,
                hwn_channel: step.hwn.unwrap_or(cfg.hwn).channel,
            };
            self.frame_checks(step, &cap, &mut checks);
            capture = Some(cap);
        }
        let cpu_ref = capture.as_ref().map(|c| &c.cpu);
        for a in &step.asserts {
            checks.push(self.eval_assert(a, cpu_ref, &step.mask));
        }
        if let Some(cap) = &capture {
            self.shots.insert(name.clone(), cap.cpu.clone());
        }
        self.groups.push(Group {
            name,
            label: step.label.clone(),
            tick: self.m.ticks,
            capture,
            checks,
            wants_golden: step.checkpoint.is_some() && step.golden != Some(false),
        });
    }

    fn frame_checks(&self, step: &Step, cap: &Capture, checks: &mut Vec<Check>) {
        let cfg = &self.journey.render;
        let exact = Tolerance::EXACT;
        if step.golden != Some(false) {
            let tol = step.tolerance.unwrap_or(exact);
            let check = match &cap.golden {
                None => Check {
                    name: "golden".into(),
                    status: Status::Fail,
                    detail: cap
                        .golden_error
                        .clone()
                        .unwrap_or_else(|| "no golden frame committed".into()),
                },
                Some(g) if (g.w, g.h) != (cap.cpu.w, cap.cpu.h) => Check {
                    name: "golden".into(),
                    status: Status::Fail,
                    detail: format!(
                        "size {}x{} but golden is {}x{}",
                        cap.cpu.w, cap.cpu.h, g.w, g.h
                    ),
                },
                Some(g) => {
                    let d = cap.cpu.diff(g, tol.channel, 1, &step.mask, None);
                    Check {
                        name: "golden".into(),
                        status: if d.within(tol) {
                            Status::Pass
                        } else {
                            Status::Fail
                        },
                        detail: d.describe(),
                    }
                }
            };
            checks.push(check);
        }
        if let Some(why) = &cap.hw_skip {
            checks.push(Check {
                name: "render matrix".into(),
                status: if self.opts.strict {
                    Status::Fail
                } else {
                    Status::Skip
                },
                detail: why.clone(),
            });
        }
        let hw1_tol = step.hw1.unwrap_or(cfg.hw1);
        let hwn_tol = step.hwn.unwrap_or(cfg.hwn);
        for (scale, frame) in &cap.hw {
            let base = if *scale == 1 {
                cap.cpu.clone()
            } else {
                cap.cpu.enlarge(*scale)
            };
            let label = format!(
                "hw {scale}x vs cpu{}",
                if *scale == 1 { "" } else { " enlarged" }
            );
            if (frame.w, frame.h) != (base.w, base.h) {
                checks.push(Check {
                    name: label,
                    status: Status::Fail,
                    detail: format!(
                        "hardware frame is {}x{}, expected {}x{}",
                        frame.w, frame.h, base.w, base.h
                    ),
                });
                continue;
            }
            let tol = if *scale == 1 { hw1_tol } else { hwn_tol };
            // At scale s the edges sit up to s pixels from the 1x frame's.
            let radius = if *scale == 1 { 0 } else { *scale };
            let d = frame.diff_near(&base, tol.channel, radius, *scale, &step.mask, None);
            checks.push(Check {
                name: label,
                status: if d.within(tol) {
                    Status::Pass
                } else {
                    Status::Fail
                },
                detail: d.describe(),
            });
            // Each strict rectangle is judged on its own, so a bleed along one line
            // of text cannot hide in the pixel count of a large region.
            for r in &step.strict {
                let d = frame.diff_near(
                    &base,
                    cfg.strict.channel,
                    1,
                    *scale,
                    &step.mask,
                    Some(std::slice::from_ref(r)),
                );
                checks.push(Check {
                    name: format!("strict ({},{} {}x{}) @{scale}x", r.x, r.y, r.w, r.h),
                    status: if d.within(cfg.strict) {
                        Status::Pass
                    } else {
                        Status::Fail
                    },
                    detail: d.describe(),
                });
            }
        }
    }

    fn eval_assert(&self, a: &Assert, frame: Option<&Img>, mask: &[Rect]) -> Check {
        if a.kind == "ram" {
            return checked_assert(ram_name(a), self.eval_ram(a), a);
        }
        let (name, ok, detail) = match a.kind.as_str() {
            "presenting" => {
                let window = a.window.unwrap_or(120);
                let need = a.min_changes.unwrap_or(10);
                let recent = self.m.recent(window);
                let n = recent.iter().filter(|t| t.flipped || t.changed).count() as u64;
                let flips = recent.iter().filter(|t| t.flipped).count();
                (
                    format!("presenting (>= {need} of last {window} ticks)"),
                    n >= need,
                    format!(
                        "{n} ticks showed a new frame ({flips} buffer flips) in the last {} ticks",
                        recent.len()
                    ),
                )
            }
            "pixels" => {
                let owned;
                let img = match frame {
                    Some(i) => i,
                    None => {
                        owned = self.m.display_image();
                        &owned
                    }
                };
                let (ok, detail) = self.eval_pixels(a, img);
                let col = a.color.unwrap_or([0; 3]);
                let want = match (a.min_count, a.max_count) {
                    (Some(lo), Some(hi)) => format!("{lo}..={hi}"),
                    (Some(lo), None) => format!(">= {lo}"),
                    (None, Some(hi)) => format!("<= {hi}"),
                    (None, None) => String::new(),
                };
                (
                    format!("pixels rgb({},{},{}) {want}", col[0], col[1], col[2]),
                    ok,
                    detail,
                )
            }
            "frame_diff" => {
                let owned;
                let now = match frame {
                    Some(i) => i,
                    None => {
                        owned = self.m.display_image();
                        &owned
                    }
                };
                let from = a.from.as_deref().unwrap_or("");
                let (ok, detail) = match self.shots.get(from) {
                    None => (false, format!("no frame captured for checkpoint `{from}`")),
                    Some(then) if (then.w, then.h) != (now.w, now.h) => {
                        (false, format!("frame size changed since `{from}`"))
                    }
                    Some(then) => {
                        let region = a.region.map(|r| vec![r]);
                        let d =
                            now.diff(then, a.color_tol.unwrap_or(16), 1, mask, region.as_deref());
                        let f = d.fraction();
                        let ok = a.min_changed.is_none_or(|m| f >= m)
                            && a.max_changed.is_none_or(|m| f <= m);
                        (
                            ok,
                            format!(
                                "{:.2}% of pixels differ from `{from}` ({} of {})",
                                f * 100.0,
                                d.differing,
                                d.compared
                            ),
                        )
                    }
                };
                let want = match (a.min_changed, a.max_changed) {
                    (Some(lo), Some(hi)) => format!("{:.1}%..={:.1}%", lo * 100.0, hi * 100.0),
                    (Some(lo), None) => format!(">= {:.1}%", lo * 100.0),
                    (None, Some(hi)) => format!("<= {:.1}%", hi * 100.0),
                    (None, None) => String::new(),
                };
                (format!("frame differs from `{from}` by {want}"), ok, detail)
            }
            "not_flat" => {
                let (ok, detail) = self.eval_flat(a, frame);
                let max = a.max_dominant.unwrap_or(0.98);
                (
                    format!("screen not one colour (<= {:.0}%)", max * 100.0),
                    ok,
                    detail,
                )
            }
            "dark" | "not_dark" => {
                let (ok, detail) = self.eval_flat(a, frame);
                let not = if a.kind == "dark" { "" } else { "not " };
                (
                    format!(
                        "screen {not}dark ({:.0}% under {})",
                        a.min_dominant.unwrap_or(0.95) * 100.0,
                        a.max_level.unwrap_or(32)
                    ),
                    ok,
                    detail,
                )
            }
            "flat" => {
                let (ok, detail) = self.eval_flat(a, frame);
                let min = a.min_dominant.unwrap_or(0.98);
                (
                    format!("screen one colour (>= {:.0}%)", min * 100.0),
                    ok,
                    detail,
                )
            }
            "pc_not_stuck" => {
                let window = a.window.unwrap_or(120);
                let need = a.min_distinct.unwrap_or(24);
                let n = self.m.distinct_pc_lines(window) as u64;
                (
                    format!("pc not stuck (>= {need} code lines over {window} ticks)"),
                    n >= need,
                    format!("{n} distinct 16-byte code lines sampled"),
                )
            }
            "audio" => {
                let window = a.window.unwrap_or(120);
                let need = a.min_peak.unwrap_or(64);
                let peak = self
                    .m
                    .recent(window)
                    .iter()
                    .map(|t| t.audio_peak)
                    .max()
                    .unwrap_or(0);
                (
                    format!("audio not silent (peak >= {need} over {window} ticks)"),
                    peak >= need,
                    format!("peak sample {peak}"),
                )
            }
            other => (other.to_string(), false, "unknown assert kind".into()),
        };
        checked_assert(name, Ok((ok, detail)), a)
    }

    /// Evaluate a `ram` assert. The detail names the actual values.
    fn eval_ram(&self, a: &Assert) -> Result<(bool, String), String> {
        let base = match (&a.sym, a.addr) {
            (Some(sym), _) => match self.syms.resolve(sym) {
                Ok(addr) => addr,
                Err(e) => {
                    let mut msg = e;
                    if self.syms.is_empty() {
                        msg.push_str(
                            " (no symbol file loaded; give the journey `symbols` or `addr`)",
                        );
                    }
                    return Err(msg);
                }
            },
            (None, Some(addr)) => addr,
            (None, None) => return Err("ram assert has neither sym nor addr".into()),
        };
        let size = usize::from(a.size.unwrap_or(4));
        let op = a.op.as_deref().unwrap_or("eq");
        let each = a.each.map(|e| (e.count.max(1), e.stride)).unwrap_or((1, 0));
        let mut values = Vec::new();
        for i in 0..each.0 {
            let addr =
                (i64::from(base) + a.offset.unwrap_or(0) + i64::from(i) * i64::from(each.1)) as u32;
            let Some(bytes) = self.m.ram_bytes(addr, size) else {
                return Err(format!("{addr:#010x} is not main RAM"));
            };
            let raw = match size {
                1 => {
                    i64::from(bytes[0])
                        - if a.signed && bytes[0] >= 0x80 {
                            0x100
                        } else {
                            0
                        }
                }
                2 => {
                    let v = u16::from_le_bytes([bytes[0], bytes[1]]);
                    i64::from(v) - if a.signed && v >= 0x8000 { 0x1_0000 } else { 0 }
                }
                _ => {
                    let v = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                    if a.signed {
                        i64::from(v as i32)
                    } else {
                        i64::from(v)
                    }
                }
            };
            values.push((addr, raw));
        }
        let want = a.value.unwrap_or(0);
        let hi = a.value_hi.unwrap_or(want);
        let holds = |v: i64| match op {
            "eq" => v == want,
            "ne" => v != want,
            "lt" => v < want,
            "le" => v <= want,
            "gt" => v > want,
            "ge" => v >= want,
            "between" => v >= want && v <= hi,
            "zero" => v == 0,
            "nonzero" => v != 0,
            _ => false,
        };
        let ok = values.iter().all(|(_, v)| holds(*v));
        let shown: Vec<String> = values
            .iter()
            .take(12)
            .map(|(addr, v)| {
                if values.len() == 1 {
                    format!("{v} (@{addr:#010x})")
                } else {
                    format!("{v}")
                }
            })
            .collect();
        let more = if values.len() > 12 { ", ..." } else { "" };
        Ok((
            ok,
            format!(
                "value{} = [{}{more}]",
                if values.len() > 1 { "s" } else { "" },
                shown.join(", ")
            ),
        ))
    }
}

fn ram_name(a: &Assert) -> String {
    let target = match (&a.sym, a.addr) {
        (Some(s), _) => s.clone(),
        (None, Some(addr)) => format!("{addr:#010x}"),
        _ => "?".into(),
    };
    let off = a.offset.map(|o| format!("+{o:#x}")).unwrap_or_default();
    let op = a.op.as_deref().unwrap_or("eq");
    let rhs = match op {
        "zero" | "nonzero" => String::new(),
        "between" => format!(" {}..={}", a.value.unwrap_or(0), a.value_hi.unwrap_or(0)),
        _ => format!(" {}", a.value.unwrap_or(0)),
    };
    let each = a
        .each
        .map(|e| format!(" for each of {} (stride {:#x})", e.count, e.stride))
        .unwrap_or_default();
    format!("ram {target}{off} {op}{rhs}{each}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journey(toml_text: &str) -> Journey {
        let journey: Journey = toml::from_str(toml_text).unwrap();
        journey.validate().unwrap();
        journey
    }

    #[test]
    fn release_requires_a_complete_render_matrix() {
        let j = journey("name = 'gate'\n[[step]]\ncheckpoint = 'menu'\n");
        assert!(release_requirements(&j, &[1, 3], true).is_ok());
        assert!(release_requirements(&j, &[1], true)
            .unwrap_err()
            .contains("scales 1 and 3"));
        assert!(release_requirements(&j, &[1, 3], false).is_err());

        let no_checkpoint = journey("name = 'gate'\n[[step]]\nwait = 1\n");
        assert!(release_requirements(&no_checkpoint, &[1, 3], true)
            .unwrap_err()
            .contains("checkpoint"));
        let skipped = journey("name = 'gate'\n[[step]]\ncheckpoint = 'menu'\nno_matrix = true\n");
        assert!(release_requirements(&skipped, &[1, 3], true)
            .unwrap_err()
            .contains("every checkpoint"));
    }

    #[test]
    fn expected_failure_cannot_hide_setup_errors_or_stale_markers() {
        let a = Assert {
            kind: "ram".into(),
            expect_fail: true,
            ..Default::default()
        };
        let unresolved_symbol = Symbols::default()
            .resolve("missing")
            .map(|_| (true, String::new()));
        assert_eq!(
            checked_assert("ram".into(), unresolved_symbol, &a).status,
            Status::Fail
        );
        assert_eq!(
            checked_assert("ram".into(), Err("invalid RAM address".into()), &a).status,
            Status::Fail
        );
        assert_eq!(
            checked_assert("ram".into(), Ok((false, "wrong value".into())), &a).status,
            Status::XFail
        );
        assert_eq!(
            checked_assert("ram".into(), Ok((true, "right value".into())), &a).status,
            Status::XPass
        );
        assert!(Status::XPass.is_failure());
    }

    #[test]
    fn disc_id_is_a_content_hash() {
        let dir = tempfile::tempdir().unwrap();
        let disc = dir.path().join("tiny.bin");
        std::fs::write(&disc, "abc").unwrap();
        assert_eq!(disc_id(&disc), "ba7816bf8f01");
    }
}
