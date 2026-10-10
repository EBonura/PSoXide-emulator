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
    /// Hard stop, in route ticks (about 60 per emulated second).
    #[serde(default = "default_max_ticks")]
    pub max_ticks: u64,
    #[serde(default)]
    pub render: RenderCfg,
    #[serde(default, rename = "step")]
    pub steps: Vec<Step>,
}

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
    /// HW 1x against the CPU frame.
    #[serde(default = "default_hw1")]
    pub hw1: Tolerance,
    /// HW 3x (and any larger scale) against the enlarged CPU frame.
    #[serde(default = "default_hwn")]
    pub hwn: Tolerance,
    /// Strict regions: HW against the enlarged CPU frame inside `strict`
    /// rectangles must match to this.
    #[serde(default = "default_strict")]
    pub strict: Tolerance,
}

fn default_scales() -> Vec<u32> {
    vec![1, 3]
}
// The CPU rasterizer keeps 15-bit colour and the hardware renderer shades in
// 8 bits, so a flat or Gouraud surface differs by up to 7 levels per channel
// with no fault. Measured on OoT's title, menu, cutscene and gameplay frames:
// at 1x under 0.5% of pixels differ by more than 16; at 3x the polygon edges
// (finer geometry than the 1x oracle) put up to 1.5% over 32.
fn default_hw1() -> Tolerance {
    Tolerance { channel: 24, fraction: 0.005 }
}
fn default_hwn() -> Tolerance {
    Tolerance { channel: 48, fraction: 0.04 }
}
fn default_strict() -> Tolerance {
    Tolerance { channel: 16, fraction: 0.002 }
}

impl Default for RenderCfg {
    fn default() -> Self {
        RenderCfg {
            scales: default_scales(),
            hw1: default_hw1(),
            hwn: default_hwn(),
            strict: default_strict(),
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
    /// Skip the CPU/HW render matrix for this checkpoint (default false).
    #[serde(default)]
    pub no_matrix: bool,
    #[serde(default)]
    pub hw1: Option<Tolerance>,
    #[serde(default)]
    pub hwn: Option<Tolerance>,
    #[serde(default, rename = "assert")]
    pub asserts: Vec<Assert>,
}

/// One check. `kind` selects which of the other fields apply:
///
/// - `ram`: `sym` or `addr`, `op`, `value`; optional `each` for arrays.
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
        let text = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let journey: Journey =
            toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        journey.validate().map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(journey)
    }

    /// Directory the journey file lives in (`<repo>/tests`).
    pub fn repo_root(journey_path: &Path) -> PathBuf {
        let dir = journey_path.parent().unwrap_or(Path::new("."));
        let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
        // tests/journey.toml -> repo root is the parent of tests/.
        if dir.file_name().is_some_and(|n| n == "tests") {
            dir.parent().unwrap_or(dir).to_path_buf()
        } else {
            dir.to_path_buf()
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.name.is_empty() || self.name.contains(['/', '\\', ' ']) {
            return Err(format!("journey name `{}` must be a short id without spaces or slashes", self.name));
        }
        if self.render.scales.iter().any(|s| *s == 0 || *s > 8) {
            return Err("render.scales must be within 1..=8".into());
        }
        let uses_port2 = self.steps.iter().any(|s| s.port == Some(2) || s.other.is_some());
        if uses_port2 && self.pad2 == Pad2Mode::None {
            return Err("a step drives port 2 but the journey has no `pad2`".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for (i, step) in self.steps.iter().enumerate() {
            let at = format!("step {} ({})", i + 1, step.label.as_deref().or(step.checkpoint.as_deref()).unwrap_or("-"));
            step.validate().map_err(|e| format!("{at}: {e}"))?;
            for a in &step.asserts {
                if let Some(from) = &a.from {
                    if !seen.contains(from) {
                        return Err(format!("{at}: frame_diff `from = \"{from}\"` is not an earlier checkpoint"));
                    }
                }
            }
            if let Some(cp) = &step.checkpoint {
                if cp.is_empty() || cp.contains(['/', '\\', ' ']) {
                    return Err(format!("{at}: checkpoint name must be a short id without spaces or slashes"));
                }
                if !seen.insert(cp.clone()) {
                    return Err(format!("{at}: duplicate checkpoint `{cp}`"));
                }
            }
        }
        Ok(())
    }

    pub fn checkpoints(&self) -> impl Iterator<Item = &Step> {
        self.steps.iter().filter(|s| s.checkpoint.is_some())
    }

    pub fn needs_hw(&self) -> bool {
        self.checkpoints().any(|s| !s.no_matrix) && !self.render.scales.is_empty()
    }
}

pub const BUTTONS: &[&str] = &[
    "cross", "circle", "square", "triangle", "start", "select", "up", "down", "left", "right",
    "l1", "r1", "l2", "r2", "l3", "r3",
];

impl Step {
    pub fn has_input(&self) -> bool {
        !self.press.is_empty() || self.lstick.is_some() || self.rstick.is_some() || self.other.is_some()
    }

    fn validate(&self) -> Result<(), String> {
        let other_buttons = self.other.iter().flat_map(|o| o.press.iter());
        for b in self.press.iter().chain(other_buttons) {
            if !BUTTONS.contains(&b.to_ascii_lowercase().as_str()) {
                return Err(format!("unknown button `{b}`"));
            }
        }
        if !matches!(self.port.unwrap_or(1), 1 | 2) {
            return Err("`port` must be 1 or 2".into());
        }
        if self.port.is_some() && self.press.is_empty() && self.lstick.is_none() && self.rstick.is_none() {
            return Err("`port` needs press, lstick or rstick".into());
        }
        let kinds = [
            self.has_input() || self.wait.is_some(),
            self.wait_until.is_some(),
            self.checkpoint.is_some() || !self.asserts.is_empty(),
        ];
        if kinds.iter().filter(|k| **k).count() == 0 {
            return Err("step does nothing (needs press/lstick/rstick, wait, wait_until, checkpoint or assert)".into());
        }
        if self.checkpoint.is_none() && (self.golden.is_some() || self.tolerance.is_some() || !self.mask.is_empty() || !self.strict.is_empty() || self.no_matrix || self.hw1.is_some() || self.hwn.is_some()) {
            return Err("golden/tolerance/mask/strict/no_matrix/hw1/hwn only apply to a checkpoint step".into());
        }
        if self.hold.is_some() && !self.has_input() {
            return Err("`hold` needs press, lstick or rstick".into());
        }
        if self.timeout.is_some() && self.wait_until.is_none() {
            return Err("`timeout` needs wait_until".into());
        }
        if self.wait_until.is_some() && (self.has_input() || self.wait.is_some()) {
            return Err("wait_until cannot be combined with input or wait in one step".into());
        }
        if let Some(c) = &self.wait_until {
            if !["ram", "pixels", "flat", "not_flat", "dark", "not_dark"].contains(&c.kind.as_str()) {
                return Err("wait_until supports kind = ram, pixels, flat, not_flat, dark or not_dark".into());
            }
            c.validate()?;
        }
        for a in &self.asserts {
            a.validate()?;
        }
        Ok(())
    }
}

pub const ASSERT_KINDS: &[&str] = &["ram", "presenting", "not_flat", "flat", "dark", "not_dark", "frame_diff", "pc_not_stuck", "audio", "pixels"];

impl Assert {
    pub fn validate(&self) -> Result<(), String> {
        if !ASSERT_KINDS.contains(&self.kind.as_str()) {
            return Err(format!("unknown assert kind `{}` (expected one of {})", self.kind, ASSERT_KINDS.join(", ")));
        }
        if self.kind == "ram" {
            if self.sym.is_some() == self.addr.is_some() {
                return Err("ram assert needs exactly one of `sym` or `addr`".into());
            }
            let op = self.op.as_deref().unwrap_or("eq");
            if !["eq", "ne", "lt", "le", "gt", "ge", "between", "zero", "nonzero"].contains(&op) {
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
        assert!(parse("name = \"a\"\n[[step]]\nport = 2\npress = [\"start\"]\n").unwrap_err().contains("pad2"));
        assert!(parse("name = \"a\"\npad2 = \"analog\"\n[[step]]\nport = 3\npress = [\"start\"]\n").is_err());
    }
}
