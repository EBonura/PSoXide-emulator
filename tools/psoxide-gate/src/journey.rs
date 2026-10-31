//! The per-game journey file (`tests/journey.toml` in a game repo).
//!
//! A journey is a list of steps that play the game the way a person does:
//! hold buttons, wait, take checkpoints. Every checkpoint captures a frame and
//! runs its asserts. Unknown keys are errors, so a typo cannot turn a check
//! into a silent no-op.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::img::{Rect, Tolerance};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Journey {
    /// Short id; names the golden folder and the report section.
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    /// Disc images to boot, relative to the repo root, first existing wins.
    #[serde(default)]
    pub disc: OneOrMany,
    /// ELF files, linker maps or `address name` lists, relative to the repo
    /// root, all loaded. Needed only by asserts that name a symbol.
    #[serde(default)]
    pub symbols: OneOrMany,
    /// Extra fixed addresses, for guests that have no symbol file.
    #[serde(default)]
    pub addr: BTreeMap<String, u32>,
    #[serde(default)]
    pub pad: PadMode,
    /// Controller in port 2. Default none, as the headless frontend has it;
    /// two-player games stop at "connect controller 2" without one.
    #[serde(default)]
    pub pad2: Pad2Mode,
    #[serde(default)]
    pub memcard: MemcardMode,
    /// Read-only raw card fixture, bound to its complete SHA256.
    #[serde(default)]
    pub card_fixture: Option<CardFixture>,
    /// Observe guest card transport and interrupt spans without injecting IRQs.
    #[serde(default)]
    pub card_observe: bool,
    /// Hard stop, in route ticks (about 60 per emulated second).
    #[serde(default = "default_max_ticks")]
    pub max_ticks: u64,
    #[serde(default)]
    pub render: RenderCfg,
    /// Named step lists a `block = "name"` step runs (once, or in a loop).
    #[serde(default, rename = "block")]
    pub blocks: Vec<Block>,
    #[serde(default, rename = "step")]
    pub steps: Vec<Step>,
}

/// A named, reusable list of steps (`[[block]]` with `[[block.step]]`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Block {
    pub name: String,
    #[serde(default, rename = "step")]
    pub steps: Vec<Step>,
}

/// Deepest nesting of loop and block steps a journey may have.
pub const MAX_NESTING: usize = 8;
/// Largest `max_iter` a loop step may ask for.
pub const MAX_ITER_LIMIT: u64 = 1_000_000;

fn default_max_ticks() -> u64 {
    60 * 60 * 20
}

#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PadMode {
    /// Analog DualShock forced on (poll id 0x73), like the headless frontend.
    #[default]
    Analog,
    /// Original digital pad (poll id 0x41).
    Digital,
    /// DualShock that starts digital and switches when the game asks.
    Dualshock,
}

#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Pad2Mode {
    #[default]
    None,
    Analog,
    Digital,
    Dualshock,
}

/// A second pad's input held during the same hold as the step's own.
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct Input {
    #[serde(default)]
    pub press: Vec<String>,
    #[serde(default)]
    pub lstick: Option<[u8; 2]>,
    #[serde(default)]
    pub rstick: Option<[u8; 2]>,
}

#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemcardMode {
    /// A freshly formatted card in slot 1 (the headless default).
    #[default]
    Fresh,
    /// No card in either slot.
    None,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardFixture {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardAction {
    pub action: String,
    /// Power cut after this many bytes have been accepted in the command.
    #[serde(default)]
    pub byte_index: Option<usize>,
    #[serde(default)]
    pub command: Option<u8>,
    /// Command occurrence counted from the start of this step, default 1.
    #[serde(default)]
    pub occurrence: Option<u32>,
    #[serde(default)]
    pub timeout: Option<u64>,
    /// Diagnostic IRQ event this many cycles before the scheduled ACK.
    #[serde(default)]
    pub ack_lead: Option<u64>,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(untagged)]
pub enum OneOrMany {
    #[default]
    Nothing,
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    pub fn list(&self) -> Vec<&str> {
        match self {
            OneOrMany::Nothing => Vec::new(),
            OneOrMany::One(s) => vec![s.as_str()],
            OneOrMany::Many(v) => v.iter().map(String::as_str).collect(),
        }
    }
}

/// Render matrix defaults for the whole journey.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct RenderCfg {
    /// Hardware-renderer internal scales to capture besides the CPU frame.
    #[serde(default = "default_scales")]
    pub scales: Vec<u32>,
}

fn default_scales() -> Vec<u32> {
    vec![1, 3]
}
impl Default for RenderCfg {
    fn default() -> Self {
        RenderCfg {
            scales: default_scales(),
        }
    }
}

/// One step. Exactly one kind of action per step: input (`press`, `lstick`,
/// `rstick`) and/or `wait`; `wait_until`; or `checkpoint` / bare `assert`s.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// Free-text label shown in the report.
    #[serde(default)]
    pub label: Option<String>,
    /// Poll-bound input tape, relative to the repo, played to exhaustion.
    #[serde(default)]
    pub tape: Option<String>,
    /// Start a poll-bound tape at the current port-1 poll instead of its recorded poll.
    #[serde(default)]
    pub tape_rebase: bool,
    /// Snapshot, persisted reboot, or bounded byte-triggered power_cut.
    #[serde(default)]
    pub card: Option<CardAction>,

    // ---- input ----
    /// Buttons held together: cross circle square triangle start select up
    /// down left right l1 r1 l2 r2 l3 r3.
    #[serde(default)]
    pub press: Vec<String>,
    /// Left stick, `[x, y]`, 0..=255, 128 centred (y 0 is up).
    #[serde(default)]
    pub lstick: Option<[u8; 2]>,
    #[serde(default)]
    pub rstick: Option<[u8; 2]>,
    /// Which pad the `press` / `lstick` / `rstick` above drive: 1 (default) or
    /// 2. Port 2 needs the journey's `pad2`.
    #[serde(default)]
    pub port: Option<u8>,
    /// Input for the other port, held at the same time (two-player play).
    #[serde(default)]
    pub other: Option<Input>,
    /// Ticks the input stays down (default 4, enough for one pad poll).
    #[serde(default)]
    pub hold: Option<u64>,
    /// Keep ordinary pad input down until a RAM or pixel condition succeeds.
    /// Requires a positive `timeout`; the pad is then released before `wait`.
    #[serde(default)]
    pub hold_until: Option<Assert>,
    /// Ticks of rest after the input is released, or the plain wait when the
    /// step has no input.
    #[serde(default)]
    pub wait: Option<u64>,
    /// Repeat the hold + rest this many times (default 1).
    #[serde(default)]
    pub repeat: Option<u64>,

    // ---- conditional wait ----
    /// Run until this RAM condition holds. Fails the journey after `timeout`
    /// ticks (default 3600).
    #[serde(default)]
    pub wait_until: Option<Assert>,
    #[serde(default)]
    pub timeout: Option<u64>,

    // ---- loops and blocks ----
    /// Run the `[[block]]` of this name (see `until` for a loop).
    #[serde(default)]
    pub block: Option<String>,
    /// Inline steps (`[[step.body]]`) repeated until `until` holds. Alternative to `block`.
    #[serde(default)]
    pub body: Vec<Step>,
    /// Guard: run the block or body only if this condition holds when the step is reached
    /// (any `wait_until` kind; otherwise the step costs no ticks). With `until` the guard
    /// decides whether the loop starts at all.
    #[serde(default)]
    pub when: Option<Assert>,
    /// Loop condition: any `wait_until` kind, checked before each iteration, so a loop whose
    /// condition already holds runs zero times. Needs `block` or `body`, `max_iter` and `timeout`.
    #[serde(default)]
    pub until: Option<Assert>,
    /// Most iterations the loop may run; the condition still false after them aborts the journey.
    #[serde(default)]
    pub max_iter: Option<u64>,

    // ---- checkpoint ----
    /// Capture a frame under this name and run the step's asserts.
    #[serde(default)]
    pub checkpoint: Option<String>,
    /// Compare the CPU frame with the committed golden (default true).
    #[serde(default)]
    pub golden: Option<bool>,
    #[serde(default)]
    pub tolerance: Option<Tolerance>,
    /// Regions ignored by every frame comparison (clocks, random sparks).
    #[serde(default)]
    pub mask: Vec<Rect>,
    /// 2D-only regions (text, menus) the hardware renderer must reproduce
    /// exactly at every scale. Native 1x pixel coordinates.
    #[serde(default)]
    pub strict: Vec<Rect>,
    /// Explicit final visible comparison classes; unknown pixels stay exact.
    #[serde(default)]
    pub comparison: crate::compare::Layout,
    /// Skip the CPU/HW render matrix for this checkpoint (default false).
    #[serde(default)]
    pub no_matrix: bool,
    #[serde(default, rename = "assert")]
    pub asserts: Vec<Assert>,
}

/// One check. `kind` selects which of the other fields apply:
///
/// - `ram`: `sym` or `addr`, `op`, `value`; optional `each` for arrays.
/// - `expr`: `expr`, an integer expression over RAM reads (see `crate::expr`): holds when
///   its value is not 0. The only fields it takes are `expr`, `expect_fail` and `note`.
/// - `presenting`: the display changed in at least `min_changes` of the last
///   `window` ticks.
/// - `not_flat`: no single colour covers more than `max_dominant` of the frame
///   (default 0.98): catches a black-screen hang.
/// - `flat`: one colour covers at least `min_dominant` of the frame (default
///   0.98): a cut to black between scenes, as a `wait_until` anchor.
/// - `dark`: at least `min_dominant` (default 0.95) of the frame is darker than
///   `max_level` (default 32, largest channel): a fade to black, as a
///   `wait_until` anchor.
/// - `not_dark`: the opposite of `dark`: a scene is on screen.
/// - `frame_diff`: this checkpoint's frame against an earlier checkpoint's
///   (`from`): the fraction of pixels (inside `region`, outside the step's
///   `mask`) that differ by more than `color_tol` (default 16) must be at least
///   `min_changed` and/or at most `max_changed`. "Nothing moved during the
///   countdown" and "the screen changed after the blast".
/// - `pc_not_stuck`: at least `min_distinct` distinct code lines were
///   executing over the last `window` ticks.
/// - `audio`: the SPU output reached `min_peak` within the last `window` ticks.
/// - `pixels`: `min_count` / `max_count` pixels of `color` (within
///   `color_tol`) inside `region`: a HUD heart, a menu cursor, a countdown digit.
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct Assert {
    pub kind: String,
    /// Known-failing today. The journey reports XFAIL and passes; once the
    /// feature lands the assert passes and is reported XPASS so the marker
    /// gets removed.
    #[serde(default)]
    pub expect_fail: bool,
    #[serde(default)]
    pub note: Option<String>,

    // window asserts
    #[serde(default)]
    pub window: Option<u64>,
    #[serde(default)]
    pub min_changes: Option<u64>,
    #[serde(default)]
    pub max_dominant: Option<f64>,
    #[serde(default)]
    pub min_dominant: Option<f64>,
    #[serde(default)]
    pub max_level: Option<u8>,
    #[serde(default)]
    pub min_distinct: Option<u64>,
    #[serde(default)]
    pub min_peak: Option<u32>,

    // ram
    #[serde(default)]
    pub sym: Option<String>,
    #[serde(default)]
    pub addr: Option<u32>,
    /// Byte offset added to the symbol or address.
    #[serde(default)]
    pub offset: Option<i64>,
    /// 1, 2 or 4 bytes (default 4).
    #[serde(default)]
    pub size: Option<u8>,
    #[serde(default)]
    pub signed: bool,
    /// eq ne lt le gt ge between zero nonzero
    #[serde(default)]
    pub op: Option<String>,
    #[serde(default)]
    pub value: Option<i64>,
    /// Upper bound for `between` (inclusive).
    #[serde(default)]
    pub value_hi: Option<i64>,
    /// Check `count` elements spaced `stride` bytes apart; all must hold.
    #[serde(default)]
    pub each: Option<Each>,

    // expr
    /// `expr` assert: an integer expression over guest RAM, for example
    /// `"i32(HK_FK_DX) < 0 && abs(i32(HK_FK_DX)) < 14 * 65536"`.
    #[serde(default)]
    pub expr: Option<String>,

    // pixels
    /// Native 1x frame rectangle to look in (default: the whole frame).
    #[serde(default)]
    pub region: Option<Rect>,
    /// `[r, g, b]` to look for.
    #[serde(default)]
    pub color: Option<[u8; 3]>,
    /// Per-channel distance that still counts as the colour (default 24).
    #[serde(default)]
    pub color_tol: Option<u8>,
    /// `frame_diff`: name of an earlier checkpoint.
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub min_changed: Option<f64>,
    #[serde(default)]
    pub max_changed: Option<f64>,
    #[serde(default)]
    pub min_count: Option<u64>,
    #[serde(default)]
    pub max_count: Option<u64>,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(deny_unknown_fields)]
pub struct Each {
    pub count: u32,
    pub stride: u32,
}

impl Journey {
    pub fn load(path: &Path) -> Result<Journey, String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let journey: Journey =
            toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        journey
            .validate()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(journey)
    }

    /// Directory the journey file lives in (`<repo>/tests`).
    pub fn repo_root(journey_path: &Path) -> PathBuf {
        let dir = journey_path.parent().unwrap_or(Path::new("."));
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        // tests/journey.toml -> repo root is the parent of tests/.
        if dir.file_name().is_some_and(|n| n == "tests") {
            dir.parent().unwrap_or(dir).to_path_buf()
        } else {
            dir.to_path_buf()
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if !safe_id(&self.name) {
            return Err(format!(
                "journey name `{}` must be a short id without spaces or slashes",
                self.name
            ));
        }
        if self.render.scales.iter().any(|s| *s == 0 || *s > 8) {
            return Err("render.scales must be within 1..=8".into());
        }
        if let Some(f) = &self.card_fixture {
            if self.memcard == MemcardMode::None
                || f.sha256.len() != 64
                || !f.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err("card_fixture needs an attached card and a 64-digit SHA256".into());
            }
        }
        let uses_port2 = self
            .all_steps()
            .iter()
            .any(|s| s.port == Some(2) || s.other.is_some());
        if uses_port2 && self.pad2 == Pad2Mode::None {
            return Err("a step drives port 2 but the journey has no `pad2`".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for (i, step) in self.steps.iter().enumerate() {
            let at = format!(
                "step {} ({})",
                i + 1,
                step.label
                    .as_deref()
                    .or(step.checkpoint.as_deref())
                    .unwrap_or("-")
            );
            step.validate(false).map_err(|e| format!("{at}: {e}"))?;
            if step
                .card
                .as_ref()
                .is_some_and(|c| c.action == "irq_during_ack")
                && !self.card_observe
            {
                return Err(format!("{at}: irq_during_ack requires card_observe"));
            }
            for a in &step.asserts {
                if let Some(from) = &a.from {
                    if !seen.contains(from) {
                        return Err(format!(
                            "{at}: frame_diff `from = \"{from}\"` is not an earlier checkpoint"
                        ));
                    }
                }
            }
            if let Some(cp) = &step.checkpoint {
                if !safe_id(cp) {
                    return Err(format!(
                        "{at}: checkpoint name must be a short id without spaces or slashes"
                    ));
                }
                if !seen.insert(cp.clone()) {
                    return Err(format!("{at}: duplicate checkpoint `{cp}`"));
                }
            }
        }
        self.validate_blocks()
    }

    /// Block names are unique ids, bodies hold only repeatable steps, every reference resolves,
    /// nothing calls itself, and nesting stays within `MAX_NESTING`.
    fn validate_blocks(&self) -> Result<(), String> {
        let mut names = std::collections::BTreeSet::new();
        for b in &self.blocks {
            if !safe_id(&b.name) {
                return Err(format!(
                    "block name `{}` must be a short id without spaces or slashes",
                    b.name
                ));
            }
            if !names.insert(b.name.as_str()) {
                return Err(format!("duplicate block `{}`", b.name));
            }
            if b.steps.is_empty() {
                return Err(format!("block `{}` has no steps", b.name));
            }
            for (i, step) in b.steps.iter().enumerate() {
                step.validate(true)
                    .map_err(|e| format!("block `{}` step {}: {e}", b.name, i + 1))?;
            }
        }
        let mut stack = Vec::new();
        self.check_nesting(&self.steps, &mut stack)?;
        for b in &self.blocks {
            stack.clear();
            stack.push(b.name.as_str());
            self.check_nesting(&b.steps, &mut stack)?;
        }
        Ok(())
    }

    /// Walk `steps`, following block calls; `stack` holds the blocks being expanded.
    fn check_nesting<'a>(
        &'a self,
        steps: &'a [Step],
        stack: &mut Vec<&'a str>,
    ) -> Result<(), String> {
        for step in steps {
            let inner: &[Step] = match &step.block {
                Some(name) => {
                    let Some(block) = self.block(name) else {
                        return Err(format!("unknown block `{name}`"));
                    };
                    if stack.contains(&name.as_str()) {
                        return Err(format!(
                            "block `{name}` calls itself ({} > {name})",
                            stack.join(" > ")
                        ));
                    }
                    stack.push(name);
                    &block.steps
                }
                None if !step.body.is_empty() => {
                    // An inline body is one level too.
                    stack.push("(body)");
                    &step.body
                }
                None => continue,
            };
            if stack.len() > MAX_NESTING {
                return Err(format!("blocks and loops nest deeper than {MAX_NESTING}"));
            }
            self.check_nesting(inner, stack)?;
            stack.pop();
        }
        Ok(())
    }

    pub fn block(&self, name: &str) -> Option<&Block> {
        self.blocks.iter().find(|b| b.name == name)
    }

    /// Every step, including those inside inline bodies and blocks.
    pub fn all_steps(&self) -> Vec<&Step> {
        fn walk<'a>(steps: &'a [Step], out: &mut Vec<&'a Step>) {
            for s in steps {
                out.push(s);
                walk(&s.body, out);
            }
        }
        let mut out = Vec::new();
        walk(&self.steps, &mut out);
        for b in &self.blocks {
            walk(&b.steps, &mut out);
        }
        out
    }

    pub fn checkpoints(&self) -> impl Iterator<Item = &Step> {
        self.steps.iter().filter(|s| s.checkpoint.is_some())
    }

    /// Whether any check reads the per-tick display history (`presenting`), the only
    /// consumer of `TickRec::changed`. Without one the machine skips hashing the display
    /// every tick, which was about 6% of a gate run.
    pub fn uses_display_history(&self) -> bool {
        self.all_steps().iter().any(|s| {
            s.asserts
                .iter()
                .chain(s.hold_until.iter())
                .chain(s.wait_until.iter())
                .any(|a| a.kind == "presenting")
        })
    }

    pub fn needs_hw(&self) -> bool {
        self.checkpoints().any(|s| !s.no_matrix) && !self.render.scales.is_empty()
    }
}

fn safe_id(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub const BUTTONS: &[&str] = &[
    "cross", "circle", "square", "triangle", "start", "select", "up", "down", "left", "right",
    "l1", "r1", "l2", "r2", "l3", "r3",
];

impl Step {
    pub fn has_input(&self) -> bool {
        !self.press.is_empty()
            || self.lstick.is_some()
            || self.rstick.is_some()
            || self.other.is_some()
    }

    /// A step that runs other steps: a block call or a loop.
    pub fn is_composite(&self) -> bool {
        self.block.is_some() || !self.body.is_empty()
    }

    /// `in_body`: the step repeats inside a loop body or a block, where a checkpoint (one golden
    /// per name), an assert, a card action and a tape cannot sit.
    fn validate(&self, in_body: bool) -> Result<(), String> {
        if in_body
            && (self.checkpoint.is_some()
                || !self.asserts.is_empty()
                || self.card.is_some()
                || self.tape.is_some())
        {
            return Err("checkpoint, assert, card and tape cannot sit in a loop body or block (check after the loop)".into());
        }
        self.validate_loop()?;
        let other_buttons = self.other.iter().flat_map(|o| o.press.iter());
        for b in self.press.iter().chain(other_buttons) {
            if !BUTTONS.contains(&b.to_ascii_lowercase().as_str()) {
                return Err(format!("unknown button `{b}`"));
            }
        }
        if !matches!(self.port.unwrap_or(1), 1 | 2) {
            return Err("`port` must be 1 or 2".into());
        }
        if self.port.is_some()
            && self.press.is_empty()
            && self.lstick.is_none()
            && self.rstick.is_none()
        {
            return Err("`port` needs press, lstick or rstick".into());
        }
        let kinds = [
            self.is_composite(),
            self.has_input() || self.wait.is_some(),
            self.wait_until.is_some(),
            self.checkpoint.is_some() || !self.asserts.is_empty(),
            self.card.is_some() || self.tape.is_some(),
        ];
        if kinds.iter().filter(|k| **k).count() == 0 {
            return Err("step does nothing (needs press/lstick/rstick, wait, wait_until, checkpoint, assert, block or body)".into());
        }
        if self.checkpoint.is_none()
            && (self.golden.is_some()
                || self.tolerance.is_some()
                || !self.mask.is_empty()
                || !self.strict.is_empty()
                || !self.comparison.two_d.is_empty()
                || !self.comparison.polygons.is_empty()
                || self.no_matrix)
        {
            return Err(
                "golden/tolerance/mask/strict/no_matrix only apply to a checkpoint step".into(),
            );
        }
        if self.hold.is_some() && !self.has_input() {
            return Err("`hold` needs press, lstick or rstick".into());
        }
        if self.timeout.is_some()
            && self.wait_until.is_none()
            && self.hold_until.is_none()
            && self.tape.is_none()
            && self.until.is_none()
        {
            return Err("`timeout` needs wait_until, hold_until, until or tape".into());
        }
        if self.tape_rebase && self.tape.is_none() {
            return Err("`tape_rebase` needs tape".into());
        }
        if self.tape.is_some() && self.timeout == Some(0) {
            return Err("tape timeout must be positive".into());
        }
        if let Some(cond) = &self.hold_until {
            if !self.has_input() {
                return Err("hold_until needs press, lstick or rstick".into());
            }
            if self.hold.is_some() || self.repeat.is_some() || self.wait_until.is_some() {
                return Err("hold_until cannot be combined with hold, repeat or wait_until".into());
            }
            if !matches!(self.timeout, Some(1..)) {
                return Err("hold_until needs a positive timeout".into());
            }
            if !["ram", "expr", "pixels"].contains(&cond.kind.as_str()) || cond.expect_fail {
                return Err("hold_until supports ordinary ram, expr or pixels conditions".into());
            }
            cond.validate()?;
        }
        if self.wait_until.is_some() && (self.has_input() || self.wait.is_some()) {
            return Err("wait_until cannot be combined with input or wait in one step".into());
        }
        if self.tape.is_some()
            && (self.has_input()
                || self.wait.is_some()
                || self.wait_until.is_some()
                || self.hold_until.is_some())
        {
            return Err(
                "tape cannot be combined with input, wait, wait_until or hold_until".into(),
            );
        }
        if let Some(c) = &self.card {
            if self.timeout.is_some() && c.timeout.is_some() {
                return Err("step timeout conflicts with card timeout".into());
            }
            if !["snapshot", "reboot", "power_cut", "irq_during_ack"].contains(&c.action.as_str()) {
                return Err(
                    "card action must be snapshot, reboot, power_cut or irq_during_ack".into(),
                );
            }
            if ["power_cut", "irq_during_ack"].contains(&c.action.as_str()) {
                if !matches!(c.command, Some(0x52 | 0x57))
                    || !matches!(c.byte_index, Some(2..=135))
                    || c.occurrence == Some(0)
                    || c.timeout == Some(0)
                {
                    return Err("card fault needs command 82/87, byte_index 2..=135 and positive occurrence/timeout".into());
                }
            } else if c.command.is_some()
                || c.byte_index.is_some()
                || c.occurrence.is_some()
                || c.timeout.is_some()
                || self.tape.is_some()
                || self.has_input()
            {
                return Err("only card faults accept a trigger, tape or input".into());
            }
            if c.action == "irq_during_ack" {
                if !matches!(c.ack_lead, Some(1..=10_000)) {
                    return Err("irq_during_ack needs ack_lead within 1..=10000 cycles".into());
                }
            } else if c.ack_lead.is_some() {
                return Err("ack_lead only applies to irq_during_ack".into());
            }
            if self.wait.is_some()
                || self.wait_until.is_some()
                || self.hold_until.is_some()
                || self.checkpoint.is_some()
                || !self.asserts.is_empty()
            {
                return Err("card action needs its own step".into());
            }
            if self.other.is_some() || self.hold.is_some() || self.repeat.is_some() {
                return Err("card action does not accept other, hold or repeat".into());
            }
        }
        if let Some(c) = &self.wait_until {
            if !WAIT_KINDS.contains(&c.kind.as_str()) {
                return Err(
                    "wait_until supports kind = ram, expr, pixels, flat, not_flat, dark or not_dark"
                        .into(),
                );
            }
            c.validate()?;
        }
        for a in &self.asserts {
            a.validate()?;
        }
        Ok(())
    }
}

impl Step {
    /// Rules of block calls and loops (`block`, `body`, `until`, `max_iter`).
    fn validate_loop(&self) -> Result<(), String> {
        if !self.is_composite() {
            if self.until.is_some() || self.max_iter.is_some() || self.when.is_some() {
                return Err("`when`, `until` and `max_iter` need `block` or `body`".into());
            }
            return Ok(());
        }
        if self.block.is_some() && !self.body.is_empty() {
            return Err("a step has `block` or `body`, not both".into());
        }
        if self.has_input()
            || self.hold.is_some()
            || self.wait.is_some()
            || self.repeat.is_some()
            || self.port.is_some()
            || self.wait_until.is_some()
            || self.hold_until.is_some()
            || self.checkpoint.is_some()
            || !self.asserts.is_empty()
            || self.card.is_some()
            || self.tape.is_some()
            || self.tape_rebase
        {
            return Err("a block or loop step takes only label, block/body, when, until, max_iter and timeout; put input and waits in its steps".into());
        }
        if let Some(guard) = &self.when {
            check_condition("when", guard)?;
        }
        for (i, s) in self.body.iter().enumerate() {
            s.validate(true)
                .map_err(|e| format!("body step {}: {e}", i + 1))?;
        }
        let Some(cond) = &self.until else {
            if self.max_iter.is_some() || self.timeout.is_some() {
                return Err(
                    "`max_iter` and `timeout` need `until` (without it the steps run once)".into(),
                );
            }
            if self.block.is_none() && self.when.is_none() {
                return Err("`body` needs `until` or `when`; use `block` to run steps once".into());
            }
            return Ok(());
        };
        if !matches!(self.max_iter, Some(1..=MAX_ITER_LIMIT)) {
            return Err(format!(
                "a loop needs `max_iter` within 1..={MAX_ITER_LIMIT}"
            ));
        }
        if !matches!(self.timeout, Some(1..)) {
            return Err("a loop needs a positive `timeout` (ticks for the whole loop)".into());
        }
        check_condition("until", cond)
    }
}

/// A `when` or `until` condition: one of the wait kinds, valid, not an expected failure.
fn check_condition(key: &str, cond: &Assert) -> Result<(), String> {
    if !WAIT_KINDS.contains(&cond.kind.as_str()) || cond.expect_fail {
        return Err(format!(
            "`{key}` supports kind = {} (no expect_fail)",
            WAIT_KINDS.join(", ")
        ));
    }
    cond.validate()
}

/// Condition kinds `wait_until` and `until` accept.
pub const WAIT_KINDS: &[&str] = &[
    "ram", "expr", "pixels", "flat", "not_flat", "dark", "not_dark",
];

pub const ASSERT_KINDS: &[&str] = &[
    "card_writes",
    "card_unchanged",
    "card_irq_ack",
    "ram",
    "expr",
    "presenting",
    "not_flat",
    "flat",
    "dark",
    "not_dark",
    "frame_diff",
    "pc_not_stuck",
    "audio",
    "pixels",
];

impl Assert {
    pub fn validate(&self) -> Result<(), String> {
        if !ASSERT_KINDS.contains(&self.kind.as_str()) {
            return Err(format!(
                "unknown assert kind `{}` (expected one of {})",
                self.kind,
                ASSERT_KINDS.join(", ")
            ));
        }
        if ["card_writes", "card_irq_ack"].contains(&self.kind.as_str())
            && self.min_count == Some(0)
        {
            return Err("card evidence min_count must be positive".into());
        }
        if self.kind == "ram" {
            if self.sym.is_some() == self.addr.is_some() {
                return Err("ram assert needs exactly one of `sym` or `addr`".into());
            }
            let op = self.op.as_deref().unwrap_or("eq");
            if ![
                "eq", "ne", "lt", "le", "gt", "ge", "between", "zero", "nonzero",
            ]
            .contains(&op)
            {
                return Err(format!("unknown op `{op}`"));
            }
            if !["zero", "nonzero"].contains(&op) && self.value.is_none() {
                return Err(format!("ram op `{op}` needs `value`"));
            }
            if op == "between" && self.value_hi.is_none() {
                return Err("ram op `between` needs `value_hi`".into());
            }
            if !matches!(self.size.unwrap_or(4), 1 | 2 | 4) {
                return Err("ram size must be 1, 2 or 4".into());
            }
        }
        if self.kind == "expr" {
            let Some(source) = &self.expr else {
                return Err("expr assert needs `expr`".into());
            };
            if self.sym.is_some()
                || self.addr.is_some()
                || self.offset.is_some()
                || self.size.is_some()
                || self.signed
                || self.op.is_some()
                || self.value.is_some()
                || self.value_hi.is_some()
                || self.each.is_some()
            {
                return Err(
                    "an expr assert takes only `expr`; put reads inside it, e.g. i32(SYM + 8)"
                        .into(),
                );
            }
            crate::expr::Expr::parse(source).map_err(|e| format!("expr `{source}`: {e}"))?;
        } else if self.expr.is_some() {
            return Err("`expr` only applies to kind = \"expr\"".into());
        }
        if self.kind == "frame_diff" {
            if self.from.is_none() {
                return Err("frame_diff needs `from` (an earlier checkpoint)".into());
            }
            if self.min_changed.is_none() && self.max_changed.is_none() {
                return Err("frame_diff needs `min_changed` and/or `max_changed`".into());
            }
        }
        if self.kind == "pixels" {
            if self.color.is_none() {
                return Err("pixels assert needs `color`".into());
            }
            if self.min_count.is_none() && self.max_count.is_none() {
                return Err("pixels assert needs `min_count` and/or `max_count`".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Journey, String> {
        let j: Journey = toml::from_str(text).map_err(|e| e.to_string())?;
        j.validate()?;
        Ok(j)
    }

    #[test]
    fn minimal_journey_parses() {
        let j = parse(
            r#"
name = "demo"
disc = "dist/demo.cue"
[[step]]
wait = 60
[[step]]
press = ["cross"]
hold = 6
wait = 30
repeat = 2
[[step]]
checkpoint = "title"
strict = [{ x = 0, y = 0, w = 320, h = 40 }]
[[step.assert]]
kind = "presenting"
window = 60
min_changes = 10
[[step.assert]]
kind = "ram"
sym = "STATE"
op = "eq"
value = 3
expect_fail = true
"#,
        )
        .unwrap();
        assert_eq!(j.steps.len(), 3);
        assert_eq!(j.steps[2].asserts.len(), 2);
        assert!(j.steps[2].asserts[1].expect_fail);
        assert_eq!(j.disc.list(), vec!["dist/demo.cue"]);
    }

    #[test]
    fn typos_and_empty_steps_are_errors() {
        assert!(parse("name = \"a\"\n[[step]]\nwaitt = 5\n").is_err());
        assert!(parse("name = \"a\"\n[[step]]\nlabel = \"x\"\n").is_err());
        assert!(parse("name = \"a\"\n[[step]]\npress = [\"jump\"]\n").is_err());
        assert!(parse("name = \"a\"\n[[step]]\ncheckpoint = \"c\"\n[[step.assert]]\nkind = \"ram\"\nsym = \"X\"\nop = \"eq\"\n").is_err());
        assert!(parse("name = \"a\"\n[[step]]\ngolden = false\nwait = 1\n").is_err());
    }

    #[test]
    fn duplicate_checkpoints_are_errors() {
        let text = "name = \"a\"\n[[step]]\ncheckpoint = \"c\"\n[[step]]\ncheckpoint = \"c\"\n";
        assert!(parse(text).unwrap_err().contains("duplicate"));
    }

    #[test]
    fn port_two_needs_a_pad_and_parses() {
        let ok = parse(
            "name = \"a\"\npad2 = \"analog\"\n[[step]]\nport = 2\npress = [\"start\"]\n[[step]]\npress = [\"up\"]\n[step.other]\npress = [\"down\"]\nlstick = [0, 128]\n",
        )
        .unwrap();
        assert_eq!(ok.steps[0].port, Some(2));
        assert!(ok.steps[1].other.is_some());
        assert!(
            parse("name = \"a\"\n[[step]]\nport = 2\npress = [\"start\"]\n")
                .unwrap_err()
                .contains("pad2")
        );
        assert!(parse(
            "name = \"a\"\npad2 = \"analog\"\n[[step]]\nport = 3\npress = [\"start\"]\n"
        )
        .is_err());
    }

    #[test]
    fn card_fault_steps_reject_missing_triggers_and_ignored_actions() {
        let base = "name = 'save'\n[[step]]\n";
        for bad in [
            "card = { action = 'power_cut' }",
            "card = { action = 'power_cut', command = 87, byte_index = 70, timeout = 0 }",
            "card = { action = 'power_cut', command = 87, byte_index = 70, occurrence = 0 }",
            "card = { action = 'power_cut', command = 87, byte_index = 137 }",
            "card = { action = 'reboot', byte_index = 70 }",
            "card = { action = 'reboot' }\nwait = 5",
            "tape = 'save.pxtape'\npress = ['cross']",
        ] {
            assert!(parse(&format!("{base}{bad}")).is_err(), "{bad}");
        }
        parse(&format!("{base}tape = 'save.pxtape'\ncard = {{ action = 'power_cut', command = 87, byte_index = 134 }}")).unwrap();
        assert!(parse(
            "name = 'save'\n[card_fixture]\npath = 'save.mcd'\nsha256 = 'bad'\n[[step]]\nwait = 1"
        )
        .is_err());
        assert!(parse(&format!(
            "{base}[[step.assert]]\nkind = 'card_irq_ack'\nmin_count = 0"
        ))
        .is_err());
    }

    #[test]
    fn tape_rebase_requires_a_standalone_tape_and_bounded_timeout() {
        let base = "name = 'route'\n[[step]]\n";
        assert!(parse(&format!("{base}tape_rebase = true\n")).is_err());
        assert!(parse(&format!(
            "{base}tape = 'route.pxtape'\ntape_rebase = true\ntimeout = 0\n"
        ))
        .is_err());
        for conflict in [
            "press = ['start']",
            "wait = 1",
            "wait_until = { kind = 'not_flat' }",
        ] {
            assert!(parse(&format!(
                "{base}tape = 'route.pxtape'\ntape_rebase = true\n{conflict}\n"
            ))
            .is_err());
        }
        let absolute = parse(&format!("{base}tape = 'route.pxtape'\n")).unwrap();
        assert!(!absolute.steps[0].tape_rebase);
        let relative = parse(&format!(
            "{base}tape = 'route.pxtape'\ntape_rebase = true\ntimeout = 4\n"
        ))
        .unwrap();
        assert!(relative.steps[0].tape_rebase);
        assert_eq!(relative.steps[0].timeout, Some(4));
        assert!(parse(&format!(
            "{base}tape = 'route.pxtape'\ntimeout = 4\ncard = {{ action = 'power_cut', command = 87, byte_index = 70, timeout = 5 }}\n"
        ))
        .unwrap_err()
        .contains("conflicts"));
    }

    #[test]
    fn bounded_hold_until_accepts_input_and_rejects_conflicting_modes() {
        let base = "name = 'held'\n[[step]]\nlstick = [128, 0]\n";
        let condition = "hold_until = { kind = 'ram', sym = 'PLAYER', offset = 8, signed = true, op = 'le', value = 690 }\n";
        let valid = parse(&format!("{base}{condition}timeout = 240\nwait = 3\n")).unwrap();
        assert_eq!(valid.steps[0].timeout, Some(240));
        assert_eq!(valid.steps[0].wait, Some(3));
        for suffix in [
            "hold = 4\n",
            "repeat = 2\n",
            "wait_until = { kind = 'ram', sym = 'PLAYER', op = 'eq', value = 1 }\n",
            "tape = 'input.pxtape'\n",
            "card = { action = 'snapshot' }\n",
        ] {
            assert!(
                parse(&format!("{base}{condition}timeout = 240\n{suffix}")).is_err(),
                "{suffix}"
            );
        }
        for bad in [
            format!("{base}{condition}"),
            format!("{base}{condition}timeout = 0\n"),
            format!("name = 'held'\n[[step]]\n{condition}timeout = 240\n"),
            format!("{base}hold_until = {{ kind = 'flat' }}\ntimeout = 240\n"),
            format!("{base}hold_until = {{ kind = 'ram', sym = 'PLAYER' }}\ntimeout = 240\n"),
            format!("{base}hold_until = {{ kind = 'pixels', color = [0, 0, 0] }}\ntimeout = 240\n"),
            format!("{base}hold_until = {{ kind = 'ram', sym = 'PLAYER', op = 'eq', value = 1, expect_fail = true }}\ntimeout = 240\n"),
        ] {
            assert!(parse(&bad).is_err(), "{bad}");
        }
        parse(&format!("{base}hold_until = {{ kind = 'pixels', color = [0, 0, 0], min_count = 100 }}\ntimeout = 240\n")).unwrap();
    }
}

#[cfg(test)]
mod expr_tests {
    use super::*;

    fn parse(text: &str) -> Result<Journey, String> {
        let j: Journey = toml::from_str(text).map_err(|e| e.to_string())?;
        j.validate()?;
        Ok(j)
    }

    #[test]
    fn expr_conditions_work_in_every_place_a_ram_condition_does() {
        let cond = "{ kind = 'expr', expr = \"i32(FOE_DX) < 0 && abs(i32(FOE_DX)) < 14 * 65536\" }";
        let j = parse(&format!(
            "name = 'e'\n[[step]]\nwait_until = {cond}\ntimeout = 60\n\
             [[step]]\nlstick = [0, 128]\nhold_until = {cond}\ntimeout = 60\n\
             [[step]]\ncheckpoint = 'c'\n[[step.assert]]\nkind = 'expr'\nexpr = 'u8(X) == 1'\nexpect_fail = true\nnote = 'later'\n"
        ))
        .unwrap();
        assert_eq!(j.steps.len(), 3);
        assert_eq!(j.steps[2].asserts[0].expr.as_deref(), Some("u8(X) == 1"));
    }

    #[test]
    fn the_documented_hold_until_example_loads() {
        let text = "name = 'e'\n[[step]]\nlstick = [0, 128]\ntimeout = 60\n[step.hold_until]\nkind = \"expr\"\nexpr = \"!(let dx = i32(HK_FK_DX), ph = u32(HK_FK_PHASE) in dx < 0 && dx > -14 * 65536 && ph >= 1 && ph <= 2)\"\n";
        parse(text).unwrap();
    }

    #[test]
    fn bad_expr_asserts_are_rejected_at_load() {
        let base = "name = 'e'\n[[step]]\ncheckpoint = 'c'\n[[step.assert]]\n";
        for bad in [
            "kind = 'expr'",
            "kind = 'expr'\nexpr = 'u8(X) +'",
            "kind = 'expr'\nexpr = 'X == 1'",
            "kind = 'expr'\nexpr = 'u8(X)'\nsym = 'X'",
            "kind = 'expr'\nexpr = 'u8(X)'\nop = 'eq'\nvalue = 1",
            "kind = 'ram'\nsym = 'X'\nop = 'eq'\nvalue = 1\nexpr = 'u8(X)'",
        ] {
            assert!(parse(&format!("{base}{bad}")).is_err(), "{bad}");
        }
        let err = parse(&format!("{base}kind = 'expr'\nexpr = 'u8(X) +'")).unwrap_err();
        assert!(err.contains("column"), "{err}");
        // hold_until still refuses expect_fail.
        assert!(parse(
            "name = 'e'\n[[step]]\nlstick = [0, 128]\ntimeout = 5\n\
             hold_until = { kind = 'expr', expr = 'u8(X)', expect_fail = true }"
        )
        .is_err());
    }

    #[test]
    fn loops_and_blocks_load_with_inline_and_named_bodies() {
        let j = parse(
            r#"
name = 'l'
[[block]]
name = 'kill_one'
[[block.step]]
lstick = [128, 128]
hold_until = { kind = 'expr', expr = 'u8(HP) == 0' }
timeout = 60

[[step]]
label = 'fight'
block = 'kill_one'
until = { kind = 'expr', expr = 'count(i in 0..8: u8(ALIVE[i]) != 0) == 0' }
max_iter = 40
timeout = 6000

[[step]]
label = 'retry'
until = { kind = 'ram', sym = 'DOOR', op = 'eq', value = 1 }
max_iter = 5
timeout = 600
[[step.body]]
press = ['cross']
wait = 4
[[step.body]]
until = { kind = 'expr', expr = 'u8(HP) == 0' }
max_iter = 3
timeout = 100
[[step.body.body]]
wait = 2

[[step]]
block = 'kill_one'
"#,
        )
        .unwrap();
        assert_eq!(j.blocks.len(), 1);
        assert_eq!(j.steps.len(), 3);
        assert_eq!(j.steps[1].body.len(), 2);
        assert_eq!(j.steps[1].body[1].body.len(), 1);
        assert_eq!(j.steps[0].max_iter, Some(40));
        // Steps inside bodies and blocks are visible to the whole-journey scans.
        assert_eq!(j.all_steps().len(), 3 + 2 + 1 + 1);
    }

    #[test]
    fn loop_steps_without_a_bound_or_a_body_are_rejected() {
        let blk = "[[block]]\nname = 'b'\n[[block.step]]\nwait = 1\n";
        let until = "until = { kind = 'expr', expr = 'u8(X) == 1' }\n";
        let cases: &[(&str, &str)] = &[
            ("no max_iter", "block = 'b'\n{until}timeout = 10\n"),
            ("no timeout", "block = 'b'\n{until}max_iter = 3\n"),
            ("zero max_iter", "block = 'b'\n{until}max_iter = 0\ntimeout = 10\n"),
            ("zero timeout", "block = 'b'\n{until}max_iter = 3\ntimeout = 0\n"),
            ("huge max_iter", "block = 'b'\n{until}max_iter = 1000001\ntimeout = 10\n"),
            ("unknown block", "block = 'nope'\n{until}max_iter = 3\ntimeout = 10\n"),
            ("both block and body", "block = 'b'\n{until}max_iter = 3\ntimeout = 10\n[[step.body]]\nwait = 1\n"),
            ("body without until", "[[step.body]]\nwait = 1\n"),
            ("max_iter without until", "block = 'b'\nmax_iter = 3\n"),
            ("timeout without until", "block = 'b'\ntimeout = 3\n"),
            ("until without a body", "{until}max_iter = 3\ntimeout = 10\nwait = 1\n"),
            ("loop with input", "block = 'b'\n{until}max_iter = 3\ntimeout = 10\npress = ['cross']\n"),
            ("loop with wait_until", "block = 'b'\n{until}max_iter = 3\ntimeout = 10\nwait_until = { kind = 'flat' }\n"),
            ("loop with a checkpoint", "block = 'b'\n{until}max_iter = 3\ntimeout = 10\ncheckpoint = 'c'\n"),
            ("until kind not allowed", "block = 'b'\nuntil = { kind = 'audio', min_peak = 1 }\nmax_iter = 3\ntimeout = 10\n"),
            ("until with expect_fail", "block = 'b'\nuntil = { kind = 'expr', expr = 'u8(X)', expect_fail = true }\nmax_iter = 3\ntimeout = 10\n"),
            ("bad until expression", "block = 'b'\nuntil = { kind = 'expr', expr = 'u8(X) +' }\nmax_iter = 3\ntimeout = 10\n"),
        ];
        for (name, step) in cases {
            let step = step.replace("{until}", until);
            let text = format!("name = 'l'\n{blk}[[step]]\n{step}");
            assert!(parse(&text).is_err(), "{name} should be rejected:\n{text}");
        }
        // The same loop with all its bounds is fine.
        parse(&format!(
            "name = 'l'\n{blk}[[step]]\nblock = 'b'\n{until}max_iter = 3\ntimeout = 10\n"
        ))
        .unwrap();
    }

    #[test]
    fn bodies_and_blocks_cannot_hold_checkpoints_asserts_cards_or_tapes() {
        let until = "until = { kind = 'expr', expr = 'u8(X) == 1' }\nmax_iter = 3\ntimeout = 10\n";
        for inner in [
            "checkpoint = 'c'",
            "wait = 1\n[[step.body.assert]]\nkind = 'presenting'",
            "tape = 'a.pxtape'",
            "card = { action = 'snapshot' }",
        ] {
            let inline = format!("name = 'l'\n[[step]]\n{until}[[step.body]]\n{inner}\n");
            assert!(parse(&inline).is_err(), "{inner}");
        }
        let block = "name = 'l'\n[[block]]\nname = 'b'\n[[block.step]]\ncheckpoint = 'c'\n";
        assert!(parse(block).is_err());
    }

    #[test]
    fn blocks_must_be_unique_resolvable_acyclic_and_shallow() {
        let b = |name: &str, call: &str| {
            format!("[[block]]\nname = '{name}'\n[[block.step]]\nblock = '{call}'\n")
        };
        let leaf = "[[block]]\nname = 'leaf'\n[[block.step]]\nwait = 1\n";
        // Duplicate, empty, bad name, self call, two block cycle, unknown call from a block.
        assert!(parse(&format!("name = 'l'\n{leaf}{leaf}")).is_err());
        assert!(parse("name = 'l'\n[[block]]\nname = 'e'\n").is_err());
        assert!(parse("name = 'l'\n[[block]]\nname = 'a b'\n[[block.step]]\nwait = 1\n").is_err());
        assert!(parse(&format!("name = 'l'\n{}", b("a", "a")))
            .unwrap_err()
            .contains("calls itself"));
        let cycle = format!("name = 'l'\n{}{}", b("a", "b"), b("b", "a"));
        assert!(parse(&cycle).unwrap_err().contains("calls itself"));
        assert!(parse(&format!("name = 'l'\n{}", b("a", "ghost")))
            .unwrap_err()
            .contains("unknown block"));
        // A chain of 7 calls is fine, a chain of 9 is too deep.
        let chain = |n: usize| {
            let mut t = String::from("name = 'l'\n[[step]]\nblock = 'c0'\n");
            for i in 0..n {
                t.push_str(&b(&format!("c{i}"), &format!("c{}", i + 1)));
            }
            t.push_str(&format!(
                "[[block]]\nname = 'c{n}'\n[[block.step]]\nwait = 1\n"
            ));
            t
        };
        parse(&chain(6)).unwrap();
        assert!(parse(&chain(9)).unwrap_err().contains("nest deeper"));
    }

    #[test]
    fn a_block_without_until_runs_once_and_takes_no_bounds() {
        let blk = "[[block]]\nname = 'b'\n[[block.step]]\nwait = 1\n";
        parse(&format!("name = 'l'\n{blk}[[step]]\nblock = 'b'\n")).unwrap();
        assert!(parse(&format!(
            "name = 'l'\n{blk}[[step]]\nblock = 'b'\nmax_iter = 2\n"
        ))
        .is_err());
    }

    #[test]
    fn port_two_inside_a_body_still_needs_pad2() {
        let text = "name = 'l'\n[[step]]\nuntil = { kind = 'expr', expr = 'u8(X)' }\nmax_iter = 2\ntimeout = 9\n[[step.body]]\nport = 2\npress = ['cross']\n";
        assert!(parse(text).unwrap_err().contains("pad2"));
        assert!(parse(&text.replace("name = 'l'", "name = 'l'\npad2 = 'analog'")).is_ok());
    }

    #[test]
    fn when_guards_a_block_or_an_inline_body() {
        let blk = "[[block]]\nname = 'b'\n[[block.step]]\nwait = 1\n";
        let when = "when = { kind = 'expr', expr = 'u8(X) == 1' }\n";
        for ok in [
            format!("[[step]]\nblock = 'b'\n{when}"),
            format!("[[step]]\n{when}[[step.body]]\nwait = 1\n"),
            format!(
                "[[step]]\nblock = 'b'\n{when}until = {{ kind = 'expr', expr = 'u8(X) == 0' }}\nmax_iter = 2\ntimeout = 9\n"
            ),
        ] {
            parse(&format!("name = 'l'\n{blk}{ok}")).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in [
            "[[step]]\nwait = 1\nwhen = { kind = 'expr', expr = 'u8(X)' }\n",
            "[[step]]\nblock = 'b'\nwhen = { kind = 'audio', min_peak = 1 }\n",
            "[[step]]\nblock = 'b'\nwhen = { kind = 'expr', expr = 'u8(X)', expect_fail = true }\n",
            "[[step]]\nblock = 'b'\nwhen = { kind = 'expr', expr = 'u8(X) +' }\n",
            "[[step]]\nblock = 'b'\nmax_iter = 2\nwhen = { kind = 'expr', expr = 'u8(X)' }\n",
        ] {
            assert!(parse(&format!("name = 'l'\n{blk}{bad}")).is_err(), "{bad}");
        }
    }
}
