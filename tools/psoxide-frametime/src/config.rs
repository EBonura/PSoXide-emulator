//! Per-game settings: which RAM words in the tick log identify the scene and which are
//! counters. Lives in the game repo next to its gate journeys (`tests/frametime.toml`).

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

/// A game's frame-time configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    /// Page title is "<title> Frame Time".
    pub title: String,
    /// One-sentence description of the route (the page's lede follows it).
    #[serde(default)]
    pub route: String,
    /// Which tick-log column names the scene, and how scenes are named.
    pub scene: Scene,
    /// Per-frame counters shown in the tables and used to judge the slow stretches. A frame takes
    /// the largest value its ticks logged, so a counter the game clears after drawing still reads.

    #[serde(default, rename = "counter")]
    pub counters: Vec<Counter>,
    /// Columns holding the player position (x, y, z), listed in the worst-frame tables.
    #[serde(default)]
    pub pos: Vec<String>,
    /// Hand-written stretches added after the measured ones.
    #[serde(default, rename = "note")]
    pub notes: Vec<Note>,
    /// Gate-side settings the tool prints with `gate-args`: the RAM words to log.
    #[serde(default)]
    pub gate: Gate,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Scene {
    /// Tick-log column holding the scene, map or room id.
    /// Empty when the scene comes from `steps`.
    #[serde(default)]
    pub word: String,
    /// Scenes by journey step, for a game with no scene word: each entry starts at step `from`
    /// (1-based, as the gate's -v output counts) and lasts until the next entry.
    #[serde(default)]
    pub steps: Vec<StepScene>,
    /// `id = "name"` pairs. Ids missing here are not scenes (loading junk) when the table is
    /// non-empty; with no table every value is a scene named `scene <id>`.
    #[serde(default)]
    pub names: BTreeMap<String, String>,
    /// A file of `name id` lines (the game's own table) read in addition to `names`,
    /// relative to the config file.
    #[serde(default)]
    pub names_file: Option<String>,
    /// Optional column that ticks while the simulation runs: a load then ends at its first
    /// change after the scene changes. Without it a load is a stall (`load_vblanks`).
    #[serde(default)]
    pub ready: Option<String>,
    /// A frame interval of at least this many vblanks is a load when `ready` is not set.
    #[serde(default = "default_load_vblanks")]
    pub load_vblanks: u64,
    /// Ids to treat as "no scene".
    #[serde(default)]
    pub ignore: Vec<u32>,
    /// Optional column the game sets to say what it is doing; with it, `play` is gameplay and
    /// `load` is loading, and every other value (title, menus, a segment's boot) is not measured.
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default = "default_play")]
    pub play: u32,
    #[serde(default = "default_load")]
    pub load: u32,
    /// Measure from this journey step (1-based) on; earlier ticks are the boot and menus.
    #[serde(default = "default_from_step")]
    pub from_step: u64,
}

fn default_load_vblanks() -> u64 {
    30
}

fn default_play() -> u32 {
    1
}

fn default_load() -> u32 {
    2
}

fn default_from_step() -> u64 {
    1
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StepScene {
    pub from: u64,
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Counter {
    /// Tick-log column.
    pub word: String,
    /// Name on the page.
    pub label: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Note {
    /// Scene the note belongs to, as named in the page.
    pub scene: String,
    /// Where in the route, as `m:ss` of game time.
    pub at: String,
    pub text: String,
}

/// RAM words the gate should log, one `NAME=SYMBOL+OFFSET` per entry (see
/// `psoxide-gate run --tick-log-ram`).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Gate {
    #[serde(default)]
    pub ram: Vec<String>,
}

impl Config {
    /// Read and check a config file; `names_file` is resolved against its folder.
    pub(crate) fn load(path: &Path) -> Result<Config, String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if let Some(rel) = cfg.scene.names_file.clone() {
            let file = path.parent().unwrap_or(Path::new(".")).join(rel);
            let table = std::fs::read_to_string(&file)
                .map_err(|e| format!("read {}: {e}", file.display()))?;
            for line in table.lines().filter(|l| !l.starts_with('#')) {
                let mut it = line.split_whitespace();
                if let (Some(name), Some(id)) = (it.next(), it.next()) {
                    if id.parse::<u32>().is_ok() {
                        cfg.scene.names.insert(id.to_string(), name.to_string());
                    }
                }
            }
        }
        for key in cfg.scene.names.keys() {
            if key.parse::<u32>().is_err() {
                return Err(format!("scene id `{key}` is not a number"));
            }
        }
        Ok(cfg)
    }

    /// Scene name for an id, or `None` when the id is not a scene.
    pub(crate) fn scene_name(&self, id: u32) -> Option<String> {
        if self.scene.ignore.contains(&id) {
            return None;
        }
        if self.scene.names.is_empty() {
            return Some(format!("scene {id}"));
        }
        self.scene.names.get(&id.to_string()).cloned()
    }
}
