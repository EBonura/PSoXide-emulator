//! Menu overlay -- the launcher / pause shell drawn over the framebuffer.
//!
//! Horizontal animated category icons with a vertical item list beneath
//! the active category. Drawn via `egui::Painter` on a middle layer so
//! it overlays the framebuffer/central area but sits below the HUD.
//!
//! Navigation: arrows + Enter + Escape, or the pad (Cross confirms, Circle
//! backs out). Escape also toggles the overlay open/closed.
//!
//! Categories: Library (the games folder, then a Homebrew folder), Game
//! (only while a game is loaded) and Settings. Developer tools live in the
//! debug sidebar, which the toolbar toggles.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use egui::{Align2, FontId, Pos2, Rect, Vec2};

use crate::icons;
use crate::theme;

const CATEGORY_SPACING: f32 = 100.0;
const ICON_SIZE_ACTIVE: f32 = 32.0;
const ICON_SIZE_INACTIVE: f32 = 20.0;
const ITEM_HEIGHT: f32 = 40.0;
const FOLDER_INDENT: f32 = 18.0;
/// Hard ceiling on item-row width (huge libraries / long paths elide past it).
const ITEM_MAX_WIDTH: f32 = 820.0;
/// Floor so short categories (Game, Settings) don't shrink to a sliver.
const ITEM_MIN_WIDTH: f32 = 260.0;
/// How much a row's right-aligned value (path / region tag) may contribute to
/// the auto-sizing, and how wide it may draw before eliding.
const VALUE_WIDTH_CAP: f32 = 240.0;
const ITEM_GAP: f32 = 2.0;
const ROW_ACTION_WIDTH: f32 = 40.0;
const ANIM_SPEED: f32 = 10.0;
/// Open/close dissolve speed (exponential ease). Higher = snappier.
const FADE_SPEED: f32 = 16.0;

/// A menu action the Menu emits when the user confirms an item. The
/// app layer interprets these -- Menu stays stateless about the
/// emulator.
///
/// Note: dropped `Copy` in favour of `Clone` to carry the
/// game-ID payload on `LaunchGame`. The dispatch cost is one
/// `String::clone` per selection -- negligible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuAction {
    /// Toggle between continuous-run and paused.
    ToggleRun,
    /// Reseat the CPU at its reset vector.
    Reset,
    /// Start a new poll-exact port-1 recording, or stop and persist the
    /// active one. Native saves the tape under the current game's config
    /// tree; the web build reboots the game first so the tape counts from
    /// poll 0 of a cold boot, and stopping downloads it as a CSV.
    ToggleInputRecording,
    /// Load a recorded input tape (CSV / `.pxtape`) and replay it against a
    /// fresh boot of the current game. Native opens a file dialog; the web
    /// build opens the browser upload picker.
    LoadInputReplay,
    /// Save the running game. Native builds create a new history slot (see
    /// [`SaveStateRow`]/[`MenuState::sync_save_states`]); the browser replaces
    /// its one persistent per-game quick-save. Either becomes the F7 target.
    SaveState,
    /// Load the running game's state from the numbered save slot. The
    /// `bool` is "resume paused" -- leave the emulator frozen on the
    /// restored frame instead of immediately continuing.
    LoadState(u8, bool),
    /// Open the save-states panel (thumbnail list, pin-to-top,
    /// load-with-confirmation) -- driven by the toolbar icon as well as
    /// the Game category's "Load state" row.
    OpenSaveStates,
    /// Open the controls panel (controller ports and Digital/Analog mode,
    /// the clickable PS1 controller drawing, press-a-key rebinding, reset
    /// to defaults) -- driven by the toolbar icon as well as the Settings
    /// category's "Controls" row.
    OpenControls,
    /// Restore every port-1 binding to the built-in defaults.
    ResetControls,
    /// Pin the given slot as the save history's "top" -- the target
    /// [`MenuAction::LoadState`] via F7/quick-load resolves to --
    /// without touching slot numbering or any other save's position.
    PinAsTop(u8),
    /// Launch a game by its menu launch token. Retail games use the
    /// stable library ID; authored project builds use a path-qualified
    /// token so projects sharing the same PSX volume ID remain distinct.
    LaunchGame(String),
    /// Expand or collapse a directory in the Library list.
    ToggleLibraryFolder(PathBuf),
    /// Open the CD burn submenu for a launchable example/project disc.
    OpenBurnMenu(String),
    /// Re-walk the configured library root and refresh
    /// `library.ron`. The last row of the Library.
    RescanLibrary,
    /// Build all public SDK/engine examples, then rescan the
    /// library once the background make job completes. What an
    /// unbuilt example row does when confirmed.
    BuildExamples,
    /// Pick and persist the games library root.
    ChooseGamesPath,
    /// Switch between high-res and native-resolution rendering.
    CycleVideoScale,
    /// Cycle the sample-time texture filter.
    CycleTextureFilter,
    /// Step the output volume through a few presets.
    CycleVolume,
    /// Mute or unmute the audio.
    ToggleMute,
    /// Cycle the menu backdrop opacity through a few presets.
    CycleMenuOpacity,
    /// Cycle the DPI-aware host UI scale through compact and enlarged presets.
    CycleUiScale,
    /// Switch what a slow host gives up: game speed or smooth painting.
    ToggleSmoothSlowHost,
    /// Web: reconnect a previously-saved games folder.
    #[cfg(target_arch = "wasm32")]
    Reconnect,
    /// Open the About card (from the Settings menu).
    ShowAbout,
    /// Quit the application.
    Quit,
}

/// Every rebindable port-1 input the controls panel exposes -- the
/// full PS1 pad (d-pad, face, shoulders, Start/Select, stick clicks,
/// the DualShock Analog toggle) plus the keyboard-emulated analog
/// stick directions. The app layer maps each target onto its
/// `psoxide_settings` binding field; the Menu module stays decoupled
/// from the settings crate's types, same as [`LibraryItem`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PadBindTarget {
    Up,
    Down,
    Left,
    Right,
    Cross,
    Circle,
    Square,
    Triangle,
    L1,
    L2,
    R1,
    R2,
    Start,
    Select,
    L3,
    R3,
    Analog,
    LStickUp,
    LStickDown,
    LStickLeft,
    LStickRight,
    RStickUp,
    RStickDown,
    RStickLeft,
    RStickRight,
}

impl PadBindTarget {
    /// Every target, in the order the panel's fallback list renders.
    pub const ALL: [PadBindTarget; 25] = [
        PadBindTarget::Up,
        PadBindTarget::Down,
        PadBindTarget::Left,
        PadBindTarget::Right,
        PadBindTarget::Cross,
        PadBindTarget::Circle,
        PadBindTarget::Square,
        PadBindTarget::Triangle,
        PadBindTarget::L1,
        PadBindTarget::L2,
        PadBindTarget::R1,
        PadBindTarget::R2,
        PadBindTarget::Start,
        PadBindTarget::Select,
        PadBindTarget::L3,
        PadBindTarget::R3,
        PadBindTarget::Analog,
        PadBindTarget::LStickUp,
        PadBindTarget::LStickDown,
        PadBindTarget::LStickLeft,
        PadBindTarget::LStickRight,
        PadBindTarget::RStickUp,
        PadBindTarget::RStickDown,
        PadBindTarget::RStickLeft,
        PadBindTarget::RStickRight,
    ];

    /// Short display name drawn on/next to the hotspot.
    pub fn label(self) -> &'static str {
        match self {
            PadBindTarget::Up => "D-Pad Up",
            PadBindTarget::Down => "D-Pad Down",
            PadBindTarget::Left => "D-Pad Left",
            PadBindTarget::Right => "D-Pad Right",
            PadBindTarget::Cross => "Cross",
            PadBindTarget::Circle => "Circle",
            PadBindTarget::Square => "Square",
            PadBindTarget::Triangle => "Triangle",
            PadBindTarget::L1 => "L1",
            PadBindTarget::L2 => "L2",
            PadBindTarget::R1 => "R1",
            PadBindTarget::R2 => "R2",
            PadBindTarget::Start => "Start",
            PadBindTarget::Select => "Select",
            PadBindTarget::L3 => "L3",
            PadBindTarget::R3 => "R3",
            PadBindTarget::Analog => "Analog",
            PadBindTarget::LStickUp => "L-Stick Up",
            PadBindTarget::LStickDown => "L-Stick Down",
            PadBindTarget::LStickLeft => "L-Stick Left",
            PadBindTarget::LStickRight => "L-Stick Right",
            PadBindTarget::RStickUp => "R-Stick Up",
            PadBindTarget::RStickDown => "R-Stick Down",
            PadBindTarget::RStickLeft => "R-Stick Left",
            PadBindTarget::RStickRight => "R-Stick Right",
        }
    }
}

/// Per-frame input snapshot the shell assembles from keyboard events.
#[derive(Default, Debug, Clone, Copy)]
pub struct MenuInput {
    pub up: bool,
    pub down: bool,
    pub left: bool,
    pub right: bool,
    pub confirm: bool,
    pub back: bool,
    pub toggle_open: bool,
}

/// One row inside a category. Labels + values are `String` so we
/// can populate them from library entries at runtime (titles,
/// region tags, sizes). Static strings like "Run" / "Pause" also
/// fit the same shape at a small allocation cost -- the whole
/// category tree is rebuilt at most a few times a session.
struct MenuItem {
    depth: usize,
    label: String,
    action: MenuAction,
    burn_action: Option<MenuAction>,
    /// Optional right-aligned subtitle -- used for region tags
    /// ("NTSC-U"), file sizes, and keyboard shortcut hints.
    value: Option<String>,
}

/// One Menu column. `name` identifies a category in code and tests
/// without comparing Unicode codepoints.
struct Category {
    name: &'static str,
    icon: char,
    items: Vec<MenuItem>,
}

/// Expansion key of the Library's Homebrew folder. Games-folder keys are
/// paths relative to the games root, so an absolute one never collides with
/// a real folder of the same name.
pub(crate) const HOMEBREW_FOLDER: &str = "/homebrew";

pub struct MenuState {
    games: Vec<LibraryItem>,
    /// SDK examples, project builds and (web) the streamed demo disc,
    /// listed inside the Library's Homebrew folder.
    homebrew: Vec<LibraryItem>,
    /// Right-hand value of the Library's games-folder row (native: the path).
    games_path: String,
    expanded_folders: HashSet<PathBuf>,
    /// Whether the Game column is showing (a game is loaded).
    game_loaded: bool,
    /// Drive the Game column's Pause/Resume and recording row labels.
    running: bool,
    recording: bool,
    pub open: bool,
    category_index: usize,
    item_index: usize,
    anim_x: f32,
    /// Open/close dissolve factor: 0 (hidden) .. 1 (fully shown).
    appear: f32,
    /// Menu backdrop opacity (percent), synced from settings; drives the
    /// `draw` backdrop alpha.
    backdrop_pct: u8,
    /// Per-frame animated scroll position for the item list, in
    /// "rows of (ITEM_HEIGHT + ITEM_GAP)". A value of `N` means
    /// item `N` is drawn at the top of the visible strip.
    /// Eased toward the integer target computed from `item_index`
    /// each frame by the same `ANIM_SPEED` knob that drives the
    /// category slide, so navigating a long list produces a smooth
    /// scroll rather than a snap.
    scroll_y: f32,
    /// Seconds the current selection has been highlighted -- drives the
    /// marquee scroll of an overflowing selected label.
    marquee_t: f32,
    /// The (category, item) the marquee is tracking; resets `marquee_t`
    /// when the selection moves.
    marquee_key: (usize, usize),
    /// Whether the top-right About card is showing. Mouse-driven; cleared
    /// whenever the menu itself closes.
    about_open: bool,
    /// Whether the save-states panel (thumbnail list, pin-to-top,
    /// load-with-confirmation) is showing. Unlike `about_open`, this
    /// is reachable from the always-visible toolbar icon independent
    /// of whether the main Menu overlay (`open`) is up, so it is not
    /// tied to `open` the way the About card is.
    save_states_open: bool,
    /// Live snapshot of this game's saves, newest first, set by
    /// [`MenuState::sync_save_states`], for the save-states panel.
    save_rows: Vec<SaveStateRow>,
    /// A "Load" click in the save-states panel doesn't load
    /// immediately -- it stages the target slot here so the panel can
    /// show a confirm-with-"resume paused" dialog first.
    pending_load_confirm: Option<PendingLoadConfirm>,
    /// Lazily-loaded, path-keyed cache of save-thumbnail textures.
    /// Never invalidated: a slot's `.png` never changes after it's
    /// written (saves are a history, not overwritten slots), so a
    /// path is a stable cache key for the process's lifetime.
    save_thumb_cache: HashMap<PathBuf, egui::TextureHandle>,
    /// Whether the controls panel (controller ports, PS1 controller
    /// drawing + rebinds) is showing. Like `save_states_open`, reachable
    /// from the toolbar icon independent of the Menu overlay.
    controls_open: bool,
    /// The target currently waiting for a key press, if the user
    /// clicked a hotspot. The shell's keyboard handler consumes the
    /// next physical key into this instead of routing it anywhere
    /// else (Escape cancels).
    controls_capture: Option<PadBindTarget>,
    /// Current binding label per target, synced from the app layer via
    /// [`MenuState::sync_controls`] whenever a binding changes.
    controls_labels: HashMap<PadBindTarget, String>,
    /// Targets whose key is physically held right now, synced by the
    /// shell each frame while the panel is open. Drives the panel's
    /// green held-highlights (a live rollover/ghosting tester).
    controls_live_held: Vec<PadBindTarget>,
    pending_pointer_action: Option<MenuAction>,
    categories: Vec<Category>,
}

/// A save staged for loading, pending the user confirming (and
/// optionally toggling) "resume paused" in the save-states panel.
#[derive(Debug, Clone)]
struct PendingLoadConfirm {
    slot: u8,
    label: String,
    thumbnail_path: Option<PathBuf>,
    resume_paused: bool,
}

impl Default for MenuState {
    fn default() -> Self {
        Self::new()
    }
}

/// An entry passed into the Menu from the library layer -- minimal
/// subset of [`psoxide_settings::LibraryEntry`] the Menu needs to
/// render an item (title + id for dispatch + region/size as the
/// right-aligned value). Kept separate so the Menu module stays
/// decoupled from the settings crate's types (and from the GUI
/// from the tests' perspective).
#[derive(Debug, Clone)]
pub struct LibraryItem {
    /// Directory relative to the games root; empty for items at the root.
    pub folder: PathBuf,
    /// Launch token passed to [`MenuAction::LaunchGame`].
    pub id: String,
    /// Main label -- typically the PVD volume identifier or the
    /// file stem.
    pub title: String,
    /// Right-aligned subtitle, e.g. "NTSC-U · 602 MiB".
    pub subtitle: String,
    /// Whether the launcher should show the CD burn affordance.
    pub burnable: bool,
    /// Whether confirming the row should launch a built artifact.
    pub launchable: bool,
}

/// One existing save state, as far as the Menu needs to know to draw
/// a "Load state" row -- the fully-formatted display label (e.g. "2m
/// ago -- tick 45,000,667") plus the slot number to dispatch on
/// selection. Built by `AppState` from `psoxide_settings::savestate`
/// data; kept as a plain string here for the same reason as
/// [`LibraryItem`] -- the Menu module doesn't depend on the settings
/// crate's types.
#[derive(Debug, Clone)]
pub struct SaveStateRow {
    /// Slot number to pass to [`MenuAction::LoadState`].
    pub slot: u8,
    /// Pre-formatted row label.
    pub label: String,
    /// Path to this slot's screenshot thumbnail, if a readable one
    /// exists on disk (older saves, or ones whose capture failed,
    /// have none -- the panel draws a placeholder for those).
    pub thumbnail_path: Option<PathBuf>,
    /// Whether this is the save history's pinned "top" -- the one
    /// F7/quick-load currently resolves to. Exactly one row is `true`
    /// whenever `save_rows` is non-empty.
    pub is_top: bool,
}

impl MenuState {
    pub fn new() -> Self {
        Self::with_running(false)
    }

    pub fn with_running(running: bool) -> Self {
        // The Library starts empty and is filled by `set_library` once
        // AppState loads the cached entries. The Game column appears with
        // the first loaded game (`set_game_loaded`).
        let categories = vec![
            build_library_category(&[], &[], &HashSet::new(), ""),
            build_settings_category(),
        ];

        Self {
            games: Vec::new(),
            homebrew: Vec::new(),
            games_path: String::new(),
            expanded_folders: HashSet::new(),
            game_loaded: false,
            running,
            recording: false,
            open: true,
            category_index: 0,
            item_index: 0,
            anim_x: 0.0,
            appear: 0.0,
            backdrop_pct: 90,
            scroll_y: 0.0,
            marquee_t: 0.0,
            marquee_key: (0, 0),
            about_open: false,
            save_states_open: false,
            save_rows: Vec::new(),
            pending_load_confirm: None,
            save_thumb_cache: HashMap::new(),
            controls_open: false,
            controls_capture: None,
            controls_labels: HashMap::new(),
            controls_live_held: Vec::new(),
            pending_pointer_action: None,
            categories,
        }
    }

    /// Rebuild the Library from a library snapshot. Examples and project
    /// builds both land in the Homebrew folder. Call after load, after a
    /// rescan, and whenever the library changes. The selected row is kept
    /// when it still exists and clamped otherwise.
    pub fn set_library(
        &mut self,
        games: &[LibraryItem],
        examples: &[LibraryItem],
        projects: &[LibraryItem],
    ) {
        self.games = games.to_vec();
        self.homebrew = examples.iter().chain(projects).cloned().collect();
        self.expanded_folders.retain(|folder| {
            folder == Path::new(HOMEBREW_FOLDER)
                || games.iter().any(|game| game.folder.starts_with(folder))
        });
        self.rebuild_library();
    }

    /// Rebuild the Library column in place. A selected game or folder stays
    /// selected (or falls back to the top when it is gone); a selected
    /// action row keeps its place, which is still an action row.
    fn rebuild_library(&mut self) {
        let selected = self.categories[0]
            .items
            .get(self.item_index)
            .map(|item| item.action.clone());
        self.categories[0] = build_library_category(
            &self.games,
            &self.homebrew,
            &self.expanded_folders,
            &self.games_path,
        );
        if self.category_index != 0 {
            return;
        }
        let items = &self.categories[0].items;
        self.item_index = match selected {
            Some(action @ (MenuAction::LaunchGame(_) | MenuAction::ToggleLibraryFolder(_))) => {
                items
                    .iter()
                    .position(|item| item.action == action)
                    .unwrap_or(0)
            }
            _ => self.item_index.min(items.len().saturating_sub(1)),
        };
    }

    /// Folder expansion lasts for this session; newly found folders start closed.
    pub fn toggle_library_folder(&mut self, folder: &Path) {
        let action = MenuAction::ToggleLibraryFolder(folder.to_path_buf());
        if !self.categories[0]
            .items
            .iter()
            .any(|item| item.action == action)
        {
            return;
        }
        if !self.expanded_folders.remove(folder) {
            self.expanded_folders.insert(folder.to_path_buf());
        }
        self.rebuild_library();
        if self.category_index == 0 {
            self.item_index = self.categories[0]
                .items
                .iter()
                .position(|item| item.action == action)
                .unwrap_or(0);
        }
        self.marquee_t = 0.0;
    }

    /// Show the Game column while a game is loaded and drop it otherwise.
    /// Cheap to call every frame; the selected column is kept by name.
    pub fn set_game_loaded(&mut self, loaded: bool) {
        if loaded == self.game_loaded {
            return;
        }
        self.game_loaded = loaded;
        let current = self.categories[self.category_index].name;
        if loaded {
            self.categories
                .insert(1, build_game_category(self.running, self.recording));
        } else {
            self.categories.retain(|category| category.name != "Game");
        }
        match self.categories.iter().position(|c| c.name == current) {
            Some(index) => self.category_index = index,
            None => {
                self.category_index = 0;
                self.item_index = 0;
                self.scroll_y = 0.0;
            }
        }
        self.anim_x = self.category_index as f32;
    }

    /// Flip the Game column's Pause/Resume label. Called when
    /// `AppState.running` flips.
    pub fn sync_run_label(&mut self, running: bool) {
        self.running = running;
        self.set_label(
            &MenuAction::ToggleRun,
            if running { "Pause" } else { "Resume" },
        );
    }

    /// Keep the Game column's recording row in sync with the F8 latch.
    pub fn sync_input_recording_label(&mut self, recording: bool) {
        self.recording = recording;
        self.set_label(
            &MenuAction::ToggleInputRecording,
            recording_label(recording),
        );
    }

    fn set_label(&mut self, action: &MenuAction, label: &str) {
        for item in self.categories.iter_mut().flat_map(|c| c.items.iter_mut()) {
            if item.action == *action && item.label != label {
                item.label = label.to_string();
            }
        }
    }

    fn set_value(&mut self, action: &MenuAction, value: String) {
        for item in self.categories.iter_mut().flat_map(|c| c.items.iter_mut()) {
            if item.action == *action && item.value.as_deref() != Some(value.as_str()) {
                item.value = Some(value.clone());
            }
        }
    }

    /// Replace the save-states panel's rows with a live save listing.
    /// Call after a save/load completes and whenever the running game
    /// changes (a different game has different saves).
    pub fn sync_save_states(&mut self, rows: &[SaveStateRow]) {
        self.save_rows = rows.to_vec();
    }

    /// Open the save-states panel. Driven by the toolbar icon and the
    /// Game category's "Load state" row.
    pub fn open_save_states(&mut self) {
        self.save_states_open = true;
    }

    /// Open the controls panel. Driven by the toolbar icon and the
    /// Settings category's "Controls" row.
    pub fn open_controls(&mut self) {
        self.controls_open = true;
    }

    /// Replace the per-target binding labels the controls panel shows.
    /// The app layer calls this at startup and after every rebind /
    /// reset, so the panel always reflects what the settings actually
    /// persist.
    pub fn sync_controls(&mut self, labels: impl IntoIterator<Item = (PadBindTarget, String)>) {
        self.controls_labels = labels.into_iter().collect();
    }

    /// The target currently waiting for a key press, if any. The
    /// shell's keyboard handler checks this every key event: while a
    /// capture is armed, keys feed the rebind instead of the game.
    pub fn controls_capture(&self) -> Option<PadBindTarget> {
        self.controls_capture
    }

    /// Disarm the pending capture (key consumed or Escape pressed).
    pub fn clear_controls_capture(&mut self) {
        self.controls_capture = None;
    }

    /// Whether the controls panel is currently showing -- the shell
    /// checks this to route Escape to "close panel" instead of the
    /// menu toggle, and to skip the live-held sync when it's not up.
    pub fn controls_panel_open(&self) -> bool {
        self.controls_open
    }

    /// Close the controls panel, disarming any pending capture.
    pub fn close_controls(&mut self) {
        self.controls_open = false;
        self.controls_capture = None;
    }

    /// Replace the live held-target set the panel highlights. Synced
    /// by the shell each frame while the panel is open.
    pub fn set_controls_live_held(&mut self, held: Vec<PadBindTarget>) {
        self.controls_live_held = held;
    }

    /// Store the menu-backdrop opacity (percent) and reflect it in the
    /// Settings item's displayed value. Driven by `video.menu_opacity_pct`.
    pub fn set_menu_opacity(&mut self, pct: u8) {
        self.backdrop_pct = pct.min(100);
        self.set_value(
            &MenuAction::CycleMenuOpacity,
            format!("{}%", self.backdrop_pct),
        );
    }

    /// Reflect the persisted host UI scale in the Settings row.
    pub fn set_ui_scale(&mut self, pct: u8) {
        self.set_value(
            &MenuAction::CycleUiScale,
            format!("{}%", pct.clamp(50, 150)),
        );
    }

    /// Reflect the slow-host choice in its Settings row.
    pub fn set_smooth_slow_host(&mut self, smooth: bool) {
        self.set_value(
            &MenuAction::ToggleSmoothSlowHost,
            slow_host_label(smooth).into(),
        );
    }

    /// Reflect the video and audio settings in their Settings rows. The
    /// toolbar changes volume and mute directly, so the shell calls this
    /// every frame; rows are only rewritten when a value changed.
    pub fn sync_video_audio(&mut self, high_res: bool, filter: &str, volume: f32, muted: bool) {
        self.set_value(
            &MenuAction::CycleVideoScale,
            if high_res { "High-res" } else { "Native" }.into(),
        );
        self.set_value(&MenuAction::CycleTextureFilter, filter.into());
        self.set_value(&MenuAction::CycleVolume, format!("{:.0}%", volume * 100.0));
        self.set_value(
            &MenuAction::ToggleMute,
            if muted { "On" } else { "Off" }.into(),
        );
    }

    /// Show the games folder on the Library's folder row.
    pub fn set_games_path_label(&mut self, games: impl Into<String>) {
        self.games_path = games.into();
        self.rebuild_library();
    }

    /// Move selection to the category named `name`, if it exists.
    #[cfg_attr(not(any(test, target_arch = "wasm32")), allow(dead_code))]
    pub fn select_category(&mut self, name: &str) {
        if let Some(idx) = self.categories.iter().position(|c| c.name == name) {
            self.category_index = idx;
            self.item_index = 0;
            self.scroll_y = 0.0;
        }
    }

    /// Open the About card. Driven by the Settings "About" row.
    pub fn show_about(&mut self) {
        self.about_open = true;
    }

    /// Feed one frame of input. Returns `Some(action)` when a confirm
    /// selects an item.
    pub fn update(&mut self, input: &MenuInput) -> Option<MenuAction> {
        if let Some(action) = self.pending_pointer_action.take() {
            return Some(action);
        }
        if input.toggle_open {
            self.open = !self.open;
        }
        if !self.open {
            return None;
        }

        // The About card is modal: swallow menu navigation while it is up, and
        // let confirm or back dismiss it (mouse users get Close / click-outside).
        if self.about_open {
            if input.confirm || input.back {
                self.about_open = false;
            }
            return None;
        }

        let num_cats = self.categories.len();
        if num_cats > 0 {
            if input.left {
                // Wrap to the last category from the first, mirroring the
                // up/down item wrap below.
                self.category_index = if self.category_index == 0 {
                    num_cats - 1
                } else {
                    self.category_index - 1
                };
                self.item_index = 0;
                // Snap the scroll so the new category's list shows from the
                // top -- avoids an awkward animation from mid-list.
                self.scroll_y = 0.0;
            }
            if input.right {
                self.category_index = (self.category_index + 1) % num_cats;
                self.item_index = 0;
                self.scroll_y = 0.0;
            }
        }

        let num_items = self.categories[self.category_index].items.len();
        if num_items > 0 {
            if input.up {
                self.item_index = if self.item_index == 0 {
                    num_items - 1
                } else {
                    self.item_index - 1
                };
            }
            if input.down {
                self.item_index = (self.item_index + 1) % num_items;
            }
        }

        if input.confirm && num_items > 0 {
            return Some(
                self.categories[self.category_index].items[self.item_index]
                    .action
                    .clone(),
            );
        }

        if input.back {
            self.open = false;
        }
        None
    }

    /// Take a click action queued while painting this frame.
    ///
    /// The shell drains this immediately after [`Self::draw`] so browser file
    /// pickers still run as part of the click-triggered redraw. Leaving it for
    /// the next animation frame can lose the browser's transient user gesture.
    pub fn take_pending_pointer_action(&mut self) -> Option<MenuAction> {
        self.pending_pointer_action.take()
    }

    /// Public reader for the currently-selected item's action --
    /// tests use it to assert the menu is populated correctly
    /// without driving input events.
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn selected_action(&self) -> Option<&MenuAction> {
        self.categories
            .get(self.category_index)
            .and_then(|c| c.items.get(self.item_index))
            .map(|i| &i.action)
    }

    /// Current category name -- also exposed for test assertions.
    #[cfg(test)]
    pub fn current_category(&self) -> Option<&'static str> {
        self.categories.get(self.category_index).map(|c| c.name)
    }

    /// Draw the controls panel when it is open. Like the save-states panel
    /// it is toolbar-reachable and independent of the Menu overlay, so
    /// rebinding works mid-game. `controllers` draws the controller-port
    /// section, which needs the shell's input router. Closing the panel
    /// disarms any pending key capture with it.
    pub fn draw_controls(&mut self, ctx: &egui::Context, controllers: impl FnOnce(&mut egui::Ui)) {
        if !self.controls_open {
            return;
        }
        controls_panel(
            ctx,
            &mut self.controls_open,
            &self.controls_labels,
            &self.controls_live_held,
            &mut self.controls_capture,
            &mut self.pending_pointer_action,
            controllers,
        );
        if !self.controls_open {
            self.controls_capture = None;
        }
    }

    /// Draw the Menu overlay on a middle-layer painter. `dt` drives the
    /// slide animation.
    pub fn draw(&mut self, ctx: &egui::Context, dt: f32, warning: Option<&str>) {
        // The save-states panel is reachable straight from the
        // always-visible toolbar icon, independent of whether the
        // full Menu overlay is open -- so it's drawn (and its own
        // egui::Window handles its layering) before the `appear`-gated
        // early-return below, unlike the About card which only makes
        // sense as a child of the open Menu.
        if self.save_states_open {
            save_states_panel(
                ctx,
                &mut self.save_states_open,
                &self.save_rows,
                &mut self.pending_load_confirm,
                &mut self.save_thumb_cache,
                &mut self.pending_pointer_action,
            );
        }
        // The About card belongs to the open menu; drop it the moment the menu
        // is dismissed so it can't linger through the close dissolve.
        if !self.open {
            self.about_open = false;
        }
        // Quick dissolve: ease `appear` toward 1 when open / 0 when closed, and
        // keep drawing (faded) until it reaches 0. Every colour below is run
        // through `fade`, so the whole overlay cross-fades in and out.
        let target = if self.open { 1.0 } else { 0.0 };
        // Cap the per-frame step so a single long frame (e.g. the hitch when a
        // game/example/demo boots) can't collapse the fade into a hard cut --
        // it stays a dissolve across the following frames.
        let k = (FADE_SPEED * dt).min(0.5);
        self.appear += (target - self.appear) * k;
        if (self.appear - target).abs() < 0.01 {
            self.appear = target;
        }
        if self.appear <= 0.0 {
            return;
        }
        if self.appear != target {
            ctx.request_repaint();
        }
        let appear = self.appear;
        let fade = |c: egui::Color32| c.gamma_multiply(appear);

        let screen = ctx.screen_rect();
        let sw = screen.width();
        let sh = screen.height();

        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Middle,
            egui::Id::new("menu"),
        ));
        let pointer_release = ctx.input(|input| {
            input
                .pointer
                .any_released()
                .then(|| input.pointer.latest_pos())
                .flatten()
        });
        let pointer_hover = ctx.input(|input| input.pointer.hover_pos());
        let interactive_release = if self.about_open {
            None
        } else {
            pointer_release
        };

        let backdrop_alpha = (self.backdrop_pct as u16 * 255 / 100) as u8;
        painter.rect_filled(
            screen,
            0.0,
            fade(egui::Color32::from_rgba_premultiplied(
                0,
                0,
                0,
                backdrop_alpha,
            )),
        );
        if let Some(warning) = warning {
            let banner_h = 34.0;
            let rect = Rect::from_min_size(screen.min, Vec2::new(sw, banner_h));
            // A calm info bar, not an alarm: this is setup guidance, not an error.
            painter.rect_filled(rect, 0.0, fade(egui::Color32::from_rgb(28, 42, 50)));
            painter.text(
                Pos2::new(sw / 2.0, banner_h / 2.0),
                Align2::CENTER_CENTER,
                warning,
                FontId::proportional(14.0),
                fade(theme::MENU_ACCENT),
            );
        }

        // Animate horizontal slide.
        let target_x = self.category_index as f32;
        self.anim_x += (target_x - self.anim_x) * ANIM_SPEED * dt;
        if (self.anim_x - target_x).abs() < 0.001 {
            self.anim_x = target_x;
        }

        let center_x = sw / 2.0;
        let center_y = sh * 0.38;

        // Category row. These used to be keyboard/gamepad only even though
        // they look like clickable tabs, which made Settings unreachable by
        // pointer in the browser build.
        let mut clicked_category = None;
        for (i, cat) in self.categories.iter().enumerate() {
            let offset = i as f32 - self.anim_x;
            let x = center_x + offset * CATEGORY_SPACING;
            let is_active = i == self.category_index;
            let size = if is_active {
                ICON_SIZE_ACTIVE
            } else {
                ICON_SIZE_INACTIVE
            };
            if x < -50.0 || x > sw + 50.0 {
                continue;
            }
            let hit_rect =
                Rect::from_center_size(Pos2::new(x, center_y + 8.0), Vec2::new(64.0, 72.0));
            let hovered = !self.about_open
                && pointer_hover.is_some_and(|position| hit_rect.contains(position));
            if hovered {
                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            let color = fade(if is_active || hovered {
                theme::MENU_ACCENT
            } else {
                theme::MENU_TEXT_DIM
            });
            painter.text(
                Pos2::new(x, center_y),
                Align2::CENTER_CENTER,
                cat.icon.to_string(),
                icons::font(size),
                color,
            );
            if is_active {
                painter.text(
                    Pos2::new(x, center_y + size / 2.0 + 20.0),
                    Align2::CENTER_TOP,
                    cat.name,
                    FontId::proportional(16.0),
                    fade(theme::MENU_TEXT_BRIGHT),
                );
            }
            if interactive_release.is_some_and(|position| hit_rect.contains(position)) {
                clicked_category = Some(i);
            }
        }
        if let Some(index) = clicked_category {
            self.category_index = index;
            self.item_index = 0;
            self.scroll_y = 0.0;
        }

        // Item list.
        // Reset the selected-row marquee when the selection changes, then
        // advance it (it only actually scrolls labels that overflow).
        let sel_key = (self.category_index, self.item_index);
        if sel_key != self.marquee_key {
            self.marquee_key = sel_key;
            self.marquee_t = 0.0;
        }
        self.marquee_t += dt;
        let marquee_t = self.marquee_t;

        let cat = &self.categories[self.category_index];
        let items_start_y = center_y + ICON_SIZE_ACTIVE + 44.0;
        let label_font = FontId::proportional(15.0);
        let value_font = FontId::proportional(13.0);

        // Auto-size the row box to THIS category's content: fit the widest
        // label fully, plus its action icons, plus a capped value column -- so
        // each category is only as wide as it needs (no global stretch).
        // Values past the cap elide; the window width is the hard ceiling.
        let max_avail = (sw - 2.0 * 40.0).min(ITEM_MAX_WIDTH);
        let measure = |text: &str, font: FontId| {
            line_galley(ctx, text, font, theme::MENU_TEXT_DIM, None)
                .size()
                .x
        };
        let needed = cat.items.iter().fold(0.0_f32, |acc, item| {
            let label_w = measure(&item.label, label_font.clone())
                + item.depth as f32 * FOLDER_INDENT
                + if cat.name == "Library"
                    && matches!(
                        item.action,
                        MenuAction::ToggleLibraryFolder(_) | MenuAction::LaunchGame(_)
                    )
                {
                    18.0
                } else {
                    0.0
                };
            let value_w = item
                .value
                .as_deref()
                .filter(|v| !v.is_empty())
                .map_or(0.0, |v| measure(v, value_font.clone()).min(VALUE_WIDTH_CAP));
            let has_play =
                matches!(item.action, MenuAction::LaunchGame(_)) && item.burn_action.is_some();
            let action_w = (usize::from(item.burn_action.is_some()) + usize::from(has_play)) as f32
                * ROW_ACTION_WIDTH;
            let value_gap = if value_w > 0.0 { 16.0 } else { 0.0 };
            // 14px left pad + 12px right pad = 26.
            acc.max(26.0 + label_w + value_gap + value_w + action_w)
        });
        let item_width = needed.clamp(ITEM_MIN_WIDTH, max_avail);
        let items_x = center_x - item_width / 2.0;
        let row_stride = ITEM_HEIGHT + ITEM_GAP;

        // How many full rows fit between `items_start_y` and the
        // bottom edge of the screen (with a small bottom margin so
        // the list doesn't butt against the edge).
        //
        // `max(1)` so a degenerate window height (tiny resize during
        // launch) still produces at least one visible row and avoids
        // a divide-by-zero in the visible-count math below.
        // Stop above the notice and hint lines painted at the bottom.
        let bottom_margin = 56.0;
        let available_h = (sh - items_start_y - bottom_margin).max(row_stride);
        let visible_rows = (available_h / row_stride).floor().max(1.0) as usize;

        // Compute a TARGET scroll position that keeps the selected
        // item visible with a lead-in margin: once you hit row
        // `edge_margin` from the top or bottom, further navigation
        // scrolls the whole list instead of just moving the cursor.
        //
        // For very short lists (num_items ≤ visible_rows) the target
        // is 0 -- nothing to scroll.
        let num_items = cat.items.len();
        let edge_margin: usize = if visible_rows >= 5 { 2 } else { 1 };
        let target_scroll = if num_items <= visible_rows {
            0.0_f32
        } else {
            let max_scroll = (num_items - visible_rows) as f32;
            let sel = self.item_index as f32;
            let top_lead = edge_margin as f32;
            let bottom_lead = (visible_rows - 1 - edge_margin) as f32;
            // Ideal scroll keeps the selected row between
            // [scroll + top_lead, scroll + bottom_lead] inclusive.
            let t = if sel < self.scroll_y + top_lead {
                sel - top_lead
            } else if sel > self.scroll_y + bottom_lead {
                sel - bottom_lead
            } else {
                self.scroll_y
            };
            t.clamp(0.0, max_scroll)
        };

        // Ease `scroll_y` toward the target using the same
        // `ANIM_SPEED * dt` blend that drives the horizontal
        // category slide -- so navigation feels uniform between
        // axes. Snap when we're within a pixel of the target.
        self.scroll_y += (target_scroll - self.scroll_y) * ANIM_SPEED * dt;
        if (self.scroll_y - target_scroll).abs() * row_stride < 0.5 {
            self.scroll_y = target_scroll;
        }

        // Rows scrolling past either end are clipped to the list, so they
        // never cover the column name or the notice lines.
        let list_rect = Rect::from_min_max(
            Pos2::new(0.0, items_start_y),
            Pos2::new(sw, sh - bottom_margin),
        );
        let list_painter = painter.with_clip_rect(list_rect);
        let pointer_hover = pointer_hover.filter(|p| list_rect.contains(*p));
        let interactive_release = interactive_release.filter(|p| list_rect.contains(*p));
        for (i, item) in cat.items.iter().enumerate() {
            let painter = &list_painter;
            let y = items_start_y + (i as f32 - self.scroll_y) * row_stride;
            let row_bottom = y + ITEM_HEIGHT;
            // Cull items entirely above the list region or below the
            // bottom margin. One row of overhang on each side so the
            // scroll animation doesn't "pop" items in/out at the
            // moment they fully arrive.
            if row_bottom < items_start_y - row_stride || y > sh - bottom_margin {
                continue;
            }
            let rect =
                Rect::from_min_size(Pos2::new(items_x, y), Vec2::new(item_width, ITEM_HEIGHT));
            let action_count = usize::from(item.burn_action.is_some())
                + usize::from(
                    matches!(item.action, MenuAction::LaunchGame(_)) && item.burn_action.is_some(),
                );
            let main_rect = row_main_rect(rect, action_count);
            let row_hovered = !self.about_open
                && pointer_hover.is_some_and(|position| main_rect.contains(position));
            let is_selected = i == self.item_index;
            let highlighted = is_selected || row_hovered;
            if row_hovered {
                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if interactive_release.is_some_and(|position| main_rect.contains(position)) {
                self.item_index = i;
                self.pending_pointer_action = Some(item.action.clone());
            }

            let bg = fade(if highlighted {
                theme::MENU_ITEM_SEL
            } else {
                theme::MENU_ITEM_BG
            });
            painter.rect_filled(rect, 0.0, bg);

            if is_selected {
                painter.rect_filled(
                    Rect::from_min_size(Pos2::new(items_x, y), Vec2::new(3.0, ITEM_HEIGHT)),
                    0.0,
                    fade(theme::MENU_ACCENT),
                );
            }

            // Text lives between `content_left` and the action icons. The value
            // (right) is capped + elided; the label (left) takes the rest --
            // elided when idle, marquee-scrolled when selected and overflowing.
            let launch_action = matches!(item.action, MenuAction::LaunchGame(_))
                .then_some(&item.action)
                .filter(|_| item.burn_action.is_some());
            let mut content_left = items_x + 14.0 + item.depth as f32 * FOLDER_INDENT;
            if let MenuAction::ToggleLibraryFolder(folder) = &item.action {
                let center = Pos2::new(content_left + 4.0, y + ITEM_HEIGHT / 2.0);
                let points = if self.expanded_folders.contains(folder) {
                    vec![
                        center + Vec2::new(-4.0, -2.0),
                        center + Vec2::new(4.0, -2.0),
                        center + Vec2::new(0.0, 3.0),
                    ]
                } else {
                    vec![
                        center + Vec2::new(-2.0, -4.0),
                        center + Vec2::new(-2.0, 4.0),
                        center + Vec2::new(3.0, 0.0),
                    ]
                };
                painter.add(egui::Shape::convex_polygon(
                    points,
                    fade(theme::MENU_TEXT_DIM),
                    egui::Stroke::NONE,
                ));
                content_left += 18.0;
            } else if cat.name == "Library" && matches!(item.action, MenuAction::LaunchGame(_)) {
                content_left += 18.0;
            }
            let avail_right = items_x + item_width - 12.0 - action_count as f32 * ROW_ACTION_WIDTH;

            let value_galley = item.value.as_deref().filter(|v| !v.is_empty()).map(|val| {
                let vcolor = fade(if highlighted {
                    theme::MENU_TEXT_VALUE
                } else {
                    theme::MENU_TEXT_DIM
                });
                let vmax = VALUE_WIDTH_CAP.min((avail_right - content_left).max(0.0));
                line_galley(ctx, val, value_font.clone(), vcolor, Some(vmax))
            });
            let value_w = value_galley.as_ref().map_or(0.0, |g| g.size().x);
            let value_left = avail_right - value_w;

            let label_color = fade(if highlighted {
                theme::MENU_TEXT_BRIGHT
            } else {
                theme::MENU_TEXT_DIM
            });
            let label_gap = if value_w > 0.0 { 16.0 } else { 0.0 };
            let label_budget = (value_left - label_gap - content_left).max(8.0);
            let full = line_galley(ctx, &item.label, label_font.clone(), label_color, None);
            if is_selected && full.size().x > label_budget {
                let overflow = full.size().x - label_budget;
                let off = marquee_offset(marquee_t, overflow);
                let clip = Rect::from_min_size(
                    Pos2::new(content_left, y),
                    Vec2::new(label_budget, ITEM_HEIGHT),
                );
                let ty = y + (ITEM_HEIGHT - full.size().y) / 2.0;
                painter.with_clip_rect(clip).galley(
                    Pos2::new(content_left - off, ty),
                    full,
                    label_color,
                );
            } else {
                let g = line_galley(
                    ctx,
                    &item.label,
                    label_font.clone(),
                    label_color,
                    Some(label_budget),
                );
                let ty = y + (ITEM_HEIGHT - g.size().y) / 2.0;
                painter.galley(Pos2::new(content_left, ty), g, label_color);
            }

            if let Some(g) = value_galley {
                let ty = y + (ITEM_HEIGHT - g.size().y) / 2.0;
                painter.galley(Pos2::new(value_left, ty), g, fade(theme::MENU_TEXT_DIM));
            }

            let mut action_index = 0;
            if let Some(action) = item.burn_action.as_ref() {
                draw_row_icon_action(
                    ctx,
                    painter,
                    row_action_rect(items_x, item_width, y, action_index),
                    pointer_hover,
                    interactive_release,
                    icons::DISC,
                    "Burn disc",
                    is_selected,
                    action,
                    &mut self.pending_pointer_action,
                    value_font.clone(),
                    appear,
                );
                action_index += 1;
            }
            if let Some(action) = launch_action {
                draw_row_icon_action(
                    ctx,
                    painter,
                    row_action_rect(items_x, item_width, y, action_index),
                    pointer_hover,
                    interactive_release,
                    icons::PLAY,
                    "Play",
                    is_selected,
                    action,
                    &mut self.pending_pointer_action,
                    value_font.clone(),
                    appear,
                );
            }
        }

        // Scroll indicators: small triangles at the top/bottom edges
        // of the item strip when there's content outside the visible
        // window. Gives the user an affordance that "there's more
        // here" without waiting for them to hit the edge.
        let indicator_color = fade(theme::MENU_TEXT_DIM);
        let has_above = self.scroll_y > 0.1;
        let has_below = (self.scroll_y + visible_rows as f32) < num_items as f32 - 0.1;
        // Painted, not text: the menu font has no arrow glyphs.
        let arrow = |tip: Pos2, dy: f32| {
            painter.add(egui::Shape::convex_polygon(
                vec![tip, tip + Vec2::new(-5.0, -dy), tip + Vec2::new(5.0, -dy)],
                indicator_color,
                egui::Stroke::NONE,
            ));
        };
        if has_above {
            arrow(Pos2::new(center_x, items_start_y - 11.0), -5.0);
        }
        if has_below {
            arrow(Pos2::new(center_x, sh - bottom_margin + 8.0), 5.0);
        }

        // Project framing, shown on every menu screen in both builds.
        painter.text(
            Pos2::new(sw / 2.0, sh - 46.0),
            Align2::CENTER_TOP,
            "PSoXide is an independent, open-source PS1 developer environment. Load your homebrew games directly. No firmware image is required.",
            FontId::proportional(11.0),
            fade(theme::MENU_TEXT_DIM),
        );

        // Bottom hint bar.
        painter.text(
            Pos2::new(sw / 2.0, sh - 30.0),
            Align2::CENTER_TOP,
            "Click/Enter: Select   Esc: Close   Arrows: Navigate",
            FontId::proportional(12.0),
            fade(theme::MENU_HINT),
        );

        // About card overlay, painted on top when opened from Settings.
        if self.about_open {
            about_panel(ctx, &mut self.about_open);
        }
    }
}

fn row_action_rect(items_x: f32, item_width: f32, y: f32, index_from_right: usize) -> Rect {
    let right = items_x + item_width - index_from_right as f32 * ROW_ACTION_WIDTH;
    Rect::from_min_size(
        Pos2::new(right - ROW_ACTION_WIDTH, y),
        Vec2::new(ROW_ACTION_WIDTH, ITEM_HEIGHT),
    )
}

/// Clickable body of a row, excluding any dedicated right-side action icons.
fn row_main_rect(rect: Rect, action_count: usize) -> Rect {
    Rect::from_min_max(
        rect.min,
        Pos2::new(
            (rect.right() - action_count as f32 * ROW_ACTION_WIDTH).max(rect.left()),
            rect.bottom(),
        ),
    )
}

/// Lay out one line of text. `max_width = Some(w)` elides to `w` with an
/// ellipsis; `None` lays the full line out (for marquee scrolling).
fn line_galley(
    ctx: &egui::Context,
    text: &str,
    font_id: FontId,
    color: egui::Color32,
    max_width: Option<f32>,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(
        text.to_owned(),
        egui::text::TextFormat {
            font_id,
            color,
            ..Default::default()
        },
    );
    job.wrap = egui::text::TextWrapping {
        max_width: max_width.unwrap_or(f32::INFINITY),
        max_rows: 1,
        overflow_character: max_width.map(|_| '…'),
        ..Default::default()
    };
    ctx.fonts(|f| f.layout_job(job))
}

/// Ping-pong marquee offset (px) for an overflowing label: pause, scroll to
/// the end, pause, scroll back. `t` is seconds since the row was selected.
fn marquee_offset(t: f32, overflow: f32) -> f32 {
    const SPEED: f32 = 45.0; // px/s
    const PAUSE: f32 = 1.3; // s held at each end
    let travel = (overflow / SPEED).max(0.001);
    let period = 2.0 * (PAUSE + travel);
    let mut p = t % period;
    if p < PAUSE {
        0.0
    } else if p < PAUSE + travel {
        (p - PAUSE) / travel * overflow
    } else if p < 2.0 * PAUSE + travel {
        overflow
    } else {
        p -= 2.0 * PAUSE + travel;
        overflow - (p / travel) * overflow
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_row_icon_action(
    ctx: &egui::Context,
    painter: &egui::Painter,
    rect: Rect,
    pointer_hover: Option<Pos2>,
    pointer_release: Option<Pos2>,
    icon: char,
    tooltip: &str,
    selected: bool,
    action: &MenuAction,
    pending_pointer_action: &mut Option<MenuAction>,
    tooltip_font: FontId,
    alpha: f32,
) {
    let hovered = pointer_hover.is_some_and(|pos| rect.contains(pos));
    if hovered {
        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        let hover_rect = rect.shrink2(Vec2::new(5.0, 5.0));
        painter.rect_filled(
            hover_rect,
            4.0,
            egui::Color32::from_rgba_premultiplied(0, 191, 230, 42).gamma_multiply(alpha),
        );
        painter.rect_stroke(
            hover_rect,
            4.0,
            egui::Stroke::new(1.0, theme::MENU_ACCENT.gamma_multiply(alpha)),
            egui::StrokeKind::Inside,
        );
    }

    let icon_color = (if hovered || selected {
        theme::MENU_ACCENT
    } else {
        theme::MENU_TEXT_DIM
    })
    .gamma_multiply(alpha);
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        icon.to_string(),
        icons::font(15.0),
        icon_color,
    );

    if hovered {
        let width = (tooltip.len() as f32 * 7.0 + 18.0).max(44.0);
        let tooltip_rect = Rect::from_min_size(
            Pos2::new(rect.right() - width - 6.0, rect.top() - 26.0),
            Vec2::new(width, 22.0),
        );
        painter.rect_filled(tooltip_rect, 3.0, theme::MENU_ITEM_BG.gamma_multiply(alpha));
        painter.text(
            tooltip_rect.center(),
            Align2::CENTER_CENTER,
            tooltip,
            tooltip_font,
            theme::MENU_TEXT_BRIGHT.gamma_multiply(alpha),
        );
    }

    if pointer_release.is_some_and(|pos| rect.contains(pos)) {
        *pending_pointer_action = Some(action.clone());
    }
}

/// The save-states panel: a "Save state" action, the pinned "top"
/// entry (what F7/quick-load targets) shown separately above a
/// divider, and a scrollable, newest-first list of every save for the
/// running game. Built from plain egui widgets (`egui::Window` +
/// `ScrollArea`), same reasoning as [`about_panel`] -- this needs
/// real image widgets and a checkbox, which the hand-painted category
/// list beneath it has no machinery for.
///
/// Button clicks feed `pending_pointer_action` -- the same channel
/// `draw_row_icon_action` uses for pointer-driven category rows --
/// rather than returning a value directly, so this integrates with
/// [`MenuState::update`]'s existing "take the pending action next
/// frame" dispatch without a second code path.
fn save_states_panel(
    ctx: &egui::Context,
    open: &mut bool,
    rows: &[SaveStateRow],
    pending_load_confirm: &mut Option<PendingLoadConfirm>,
    thumb_cache: &mut HashMap<PathBuf, egui::TextureHandle>,
    pending_pointer_action: &mut Option<MenuAction>,
) {
    const THUMB_SIZE: Vec2 = Vec2::new(84.0, 63.0);

    let mut still_open = *open;
    egui::Window::new("Save States")
        .open(&mut still_open)
        .collapsible(false)
        .resizable(false)
        .default_width(380.0)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .show(ctx, |ui| {
            let save_label = if cfg!(target_arch = "wasm32") {
                "Save browser quick-state"
            } else {
                "Save state"
            };
            let save_help = if cfg!(target_arch = "wasm32") {
                "Replace this game's persistent browser quick-save (F5)"
            } else {
                "Push a new save (F5) and pin it as the quick-load target"
            };
            if ui
                .add(egui::Button::new(
                    egui::RichText::new(format!("{}  {save_label}", icons::SAVE)).size(14.0),
                ))
                .on_hover_text(save_help)
                .clicked()
            {
                *pending_pointer_action = Some(MenuAction::SaveState);
            }
            ui.add_space(8.0);

            if rows.is_empty() {
                ui.label(
                    egui::RichText::new("No saves yet")
                        .color(theme::MENU_TEXT_DIM)
                        .italics(),
                );
                return;
            }

            if let Some(top) = rows.iter().find(|r| r.is_top) {
                egui::Frame::group(ui.style())
                    .stroke(egui::Stroke::new(1.0, theme::MENU_ACCENT))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            draw_thumb(
                                ui,
                                ctx,
                                thumb_cache,
                                top.thumbnail_path.as_deref(),
                                THUMB_SIZE,
                            );
                            ui.vertical(|ui| {
                                ui.label(
                                    egui::RichText::new("Top -- loads on F7")
                                        .strong()
                                        .color(theme::MENU_ACCENT)
                                        .size(12.0),
                                );
                                ui.label(egui::RichText::new(&top.label).size(13.0));
                                if ui.small_button("Load...").clicked() {
                                    *pending_load_confirm = Some(PendingLoadConfirm {
                                        slot: top.slot,
                                        label: top.label.clone(),
                                        thumbnail_path: top.thumbnail_path.clone(),
                                        resume_paused: true,
                                    });
                                }
                            });
                        });
                    });
            }

            ui.add_space(6.0);
            ui.separator();
            ui.label(
                egui::RichText::new("All saves")
                    .color(theme::MENU_TEXT_DIM)
                    .size(12.0),
            );
            ui.add_space(4.0);

            egui::ScrollArea::vertical()
                .max_height(340.0)
                .show(ui, |ui| {
                    for row in rows {
                        ui.horizontal(|ui| {
                            draw_thumb(
                                ui,
                                ctx,
                                thumb_cache,
                                row.thumbnail_path.as_deref(),
                                THUMB_SIZE,
                            );
                            ui.vertical(|ui| {
                                ui.label(egui::RichText::new(&row.label).size(13.0));
                                ui.horizontal(|ui| {
                                    if ui.small_button("Load...").clicked() {
                                        *pending_load_confirm = Some(PendingLoadConfirm {
                                            slot: row.slot,
                                            label: row.label.clone(),
                                            thumbnail_path: row.thumbnail_path.clone(),
                                            resume_paused: true,
                                        });
                                    }
                                    if row.is_top {
                                        ui.label(
                                            egui::RichText::new("pinned")
                                                .color(theme::MENU_TEXT_DIM)
                                                .size(11.0),
                                        );
                                    } else if ui
                                        .small_button("Pin as top")
                                        .on_hover_text(
                                            "Make this the save F7/quick-load targets, \
                                             without moving it in this list",
                                        )
                                        .clicked()
                                    {
                                        *pending_pointer_action =
                                            Some(MenuAction::PinAsTop(row.slot));
                                    }
                                });
                            });
                        });
                        ui.add_space(4.0);
                    }
                });
        });
    *open = still_open;

    // Load-confirmation sub-modal: staged by a "Load..." click above,
    // not dispatched until the user confirms (and has had a chance to
    // flip "resume paused").
    if let Some(confirm) = pending_load_confirm.clone() {
        let mut keep_confirming = true;
        let mut resume_paused = confirm.resume_paused;
        egui::Window::new("Load this save?")
            .collapsible(false)
            .resizable(false)
            .order(egui::Order::Foreground)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    draw_thumb(
                        ui,
                        ctx,
                        thumb_cache,
                        confirm.thumbnail_path.as_deref(),
                        THUMB_SIZE,
                    );
                    ui.label(&confirm.label);
                });
                ui.add_space(6.0);
                ui.checkbox(&mut resume_paused, "Resume paused");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Load").clicked() {
                        *pending_pointer_action =
                            Some(MenuAction::LoadState(confirm.slot, resume_paused));
                        keep_confirming = false;
                    }
                    if ui.button("Cancel").clicked() {
                        keep_confirming = false;
                    }
                });
            });
        if keep_confirming {
            if resume_paused != confirm.resume_paused {
                *pending_load_confirm = Some(PendingLoadConfirm {
                    resume_paused,
                    ..confirm
                });
            }
        } else {
            *pending_load_confirm = None;
        }
    }
}

/// The controls panel: the controller-port section (`controllers`, drawn
/// by the shell: which host device drives which PS1 port, Digital or
/// Analog), then a painter-drawn PS1 controller whose parts are
/// clickable keyboard rebind hotspots -- each showing its current key
/// right on the drawing -- plus a grouped clickable list of every target
/// below it, a live capture banner, and a reset-to-defaults button.
///
/// Controls light up green while their key is physically held, which
/// doubles as an in-app rollover/ghosting tester: hold three keys and
/// see exactly which ones the keyboard actually delivered.
///
/// The silhouette is drawn from the PS1 pad's real construction: two
/// circular pods carrying the d-pad and face cluster, a narrower
/// bridge between them, and capsule grips flaring down-outward.
/// Everything routes through one scale constant so the whole drawing
/// (and its text) can be resized in one place.
///
/// Rebinds don't go through [`MenuAction`]: clicking a hotspot arms
/// `capture`, and the shell's keyboard handler consumes the next
/// physical key into the binding (Escape cancels). Reset does dispatch
/// via `pending_pointer_action`, like every other pointer-driven
/// action.
fn controls_panel(
    ctx: &egui::Context,
    open: &mut bool,
    labels: &HashMap<PadBindTarget, String>,
    held: &[PadBindTarget],
    capture: &mut Option<PadBindTarget>,
    pending_pointer_action: &mut Option<MenuAction>,
    controllers: impl FnOnce(&mut egui::Ui),
) {
    use egui::{Color32, CornerRadius, Sense, Stroke};

    /// One knob for the whole drawing: positions, sizes, and font
    /// sizes below are in a 470x300 design space multiplied by this.
    const S: f32 = 1.35;
    const CANVAS: Vec2 = Vec2::new(470.0 * S, 300.0 * S);

    let bind_of =
        |t: PadBindTarget| -> String { labels.get(&t).cloned().unwrap_or_else(|| "-".to_string()) };
    // Space on the drawing is tight: well-known names compact to
    // glyphs/abbreviations, anything still too long elides. The list
    // and hover text always carry the full name.
    let short_bind = |t: PadBindTarget| -> String {
        let full = bind_of(t);
        // ASCII stand-ins for the arrows: the menu font has no
        // U+2190..2193 glyphs (they render as .notdef boxes).
        let compact = match full.as_str() {
            "ArrowUp" => "^".to_string(),
            "ArrowDown" => "v".to_string(),
            "ArrowLeft" => "<".to_string(),
            "ArrowRight" => ">".to_string(),
            "Backspace" => "Bksp".to_string(),
            "Space" => "Spc".to_string(),
            other => other.replace("Numpad", "Num"),
        };
        if compact.chars().count() > 7 {
            let head: String = compact.chars().take(6).collect();
            format!("{head}\u{2026}")
        } else {
            compact
        }
    };

    // Keep frames coming while the panel is up: the capture pulse
    // animates, and the held-key highlights must track key releases
    // even when the paused game isn't producing new frames itself.
    if capture.is_some() {
        ctx.request_repaint_after(std::time::Duration::from_millis(50));
    } else {
        ctx.request_repaint_after(std::time::Duration::from_millis(120));
    }

    let mut still_open = *open;
    egui::Window::new("Controls")
        .open(&mut still_open)
        .collapsible(false)
        .resizable(false)
        // Foreground: the Menu overlay paints on a mid-layer painter
        // above ordinary windows, so without this the category list
        // draws straight over the panel when both are up.
        .order(egui::Order::Foreground)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        // The whole panel scrolls when the window is shorter than it.
        .vscroll(true)
        .default_height(ctx.screen_rect().height() - 120.0)
        .show(ctx, |ui| {
            let heading = |ui: &mut egui::Ui, text: &str| {
                ui.label(
                    egui::RichText::new(text)
                        .color(theme::MENU_ACCENT)
                        .size(14.0)
                        .strong(),
                );
            };
            heading(ui, "Controllers");
            controllers(ui);
            ui.add_space(6.0);
            ui.separator();
            heading(ui, "Keyboard");
            // Capture banner / hint line above the drawing.
            match *capture {
                Some(target) => {
                    ui.label(
                        egui::RichText::new(format!(
                            "{}  Press a key for {} ... (Esc cancels)",
                            icons::GAMEPAD_2,
                            target.label()
                        ))
                        .color(theme::MENU_ACCENT)
                        .strong()
                        .size(14.0),
                    );
                }
                None => {
                    ui.label(
                        egui::RichText::new(
                            "Click a control, then press the key to bind it. Changes apply \
                             immediately and are saved. Held keys light up green.",
                        )
                        .color(theme::MENU_TEXT_DIM)
                        .size(13.0),
                    );
                }
            }
            ui.add_space(4.0);

            let (resp, painter) = ui.allocate_painter(CANVAS, Sense::hover());
            let origin = resp.rect.min;
            let at = |x: f32, y: f32| Pos2::new(origin.x + x * S, origin.y + y * S);
            let sz = |w: f32, h: f32| Vec2::new(w * S, h * S);
            let sc = |v: f32| v * S;
            // Pulse for the armed hotspot: 2 Hz sine on the ui clock.
            let pulse = ((ui.input(|i| i.time) * std::f64::consts::TAU).sin() * 0.5 + 0.5) as f32;

            let body_fill = Color32::from_rgb(58, 62, 71);
            let body_shade = Color32::from_rgb(48, 52, 60);
            let btn_fill = Color32::from_rgb(30, 33, 39);
            let held_fill = Color32::from_rgba_unmultiplied(70, 190, 110, 70);
            let held_ring = Stroke::new(2.0, Color32::from_rgb(80, 210, 120));
            let bind_text = Color32::from_rgb(150, 200, 235);

            // One clickable region. Paints a held/hover/armed ring,
            // shows the current bind as hover text, and arms the
            // capture on click.
            let mut hotspot = |ui: &mut egui::Ui, rect: Rect, target: PadBindTarget| {
                let id = resp.id.with(target.label());
                let r = ui.interact(rect, id, Sense::click());
                let armed = *capture == Some(target);
                if held.contains(&target) {
                    ui.painter().rect_filled(rect, 4.0, held_fill);
                    ui.painter().rect_stroke(
                        rect.expand(1.0),
                        4.0,
                        held_ring,
                        egui::StrokeKind::Outside,
                    );
                }
                if armed {
                    let ring = Color32::from_rgb(
                        60 + (170.0 * pulse) as u8,
                        180,
                        230 - (80.0 * pulse) as u8,
                    );
                    ui.painter().rect_stroke(
                        rect.expand(2.0),
                        4.0,
                        Stroke::new(2.0, ring),
                        egui::StrokeKind::Outside,
                    );
                } else if r.hovered() {
                    ui.painter().rect_stroke(
                        rect.expand(2.0),
                        4.0,
                        Stroke::new(1.5, theme::MENU_ACCENT),
                        egui::StrokeKind::Outside,
                    );
                }
                let r = r.on_hover_text(format!(
                    "{} - current: {} (click to rebind)",
                    target.label(),
                    bind_of(target)
                ));
                if r.clicked() {
                    *capture = Some(target);
                }
            };

            // Capsule polygon spanning `a`..`b` with radius `r` --
            // used for the grips. Convex by construction.
            let capsule = |a: Pos2, b: Pos2, r: f32| -> Vec<Pos2> {
                let theta = (b - a).angle();
                let mut pts = Vec::with_capacity(26);
                for i in 0..=12 {
                    let phi = theta - std::f32::consts::FRAC_PI_2
                        + std::f32::consts::PI * (i as f32 / 12.0);
                    pts.push(b + r * Vec2::angled(phi));
                }
                for i in 0..=12 {
                    let phi = theta
                        + std::f32::consts::FRAC_PI_2
                        + std::f32::consts::PI * (i as f32 / 12.0);
                    pts.push(a + r * Vec2::angled(phi));
                }
                pts
            };

            // --- Body: grips first (they sit behind), then the two
            // circular pods, then the narrower bridge that joins them.
            painter.add(egui::Shape::convex_polygon(
                capsule(at(96.0, 160.0), at(62.0, 272.0), sc(31.0)),
                body_shade,
                Stroke::NONE,
            ));
            painter.add(egui::Shape::convex_polygon(
                capsule(at(374.0, 160.0), at(408.0, 272.0), sc(31.0)),
                body_shade,
                Stroke::NONE,
            ));
            painter.circle_filled(at(105.0, 128.0), sc(66.0), body_fill);
            painter.circle_filled(at(365.0, 128.0), sc(66.0), body_fill);
            painter.rect_filled(
                Rect::from_min_max(at(105.0, 84.0), at(365.0, 172.0)),
                CornerRadius::same(10),
                body_fill,
            );

            // Shoulder buttons floating above the pods' top edge;
            // bind keys drawn right in the button.
            let shoulder = |painter: &egui::Painter, rect: Rect, name: &str, key: &str| {
                painter.rect_filled(rect, CornerRadius::same(6), btn_fill);
                painter.text(
                    Pos2::new(rect.center().x, rect.center().y - sc(5.0)),
                    Align2::CENTER_CENTER,
                    name,
                    FontId::proportional(sc(9.0)),
                    Color32::from_rgb(200, 205, 215),
                );
                painter.text(
                    Pos2::new(rect.center().x, rect.center().y + sc(6.0)),
                    Align2::CENTER_CENTER,
                    key,
                    FontId::proportional(sc(7.5)),
                    bind_text,
                );
            };
            let l2 = Rect::from_min_max(at(56.0, 8.0), at(120.0, 32.0));
            let l1 = Rect::from_min_max(at(56.0, 38.0), at(120.0, 62.0));
            let r2 = Rect::from_min_max(at(350.0, 8.0), at(414.0, 32.0));
            let r1 = Rect::from_min_max(at(350.0, 38.0), at(414.0, 62.0));
            shoulder(&painter, l2, "L2", &short_bind(PadBindTarget::L2));
            shoulder(&painter, l1, "L1", &short_bind(PadBindTarget::L1));
            shoulder(&painter, r2, "R2", &short_bind(PadBindTarget::R2));
            shoulder(&painter, r1, "R1", &short_bind(PadBindTarget::R1));
            hotspot(ui, l2, PadBindTarget::L2);
            hotspot(ui, l1, PadBindTarget::L1);
            hotspot(ui, r2, PadBindTarget::R2);
            hotspot(ui, r1, PadBindTarget::R1);

            // D-pad on the left pod: cross of two bars, each arm
            // carrying its bind key.
            let dpad_c = at(105.0, 128.0);
            painter.rect_filled(
                Rect::from_center_size(dpad_c, sz(30.0, 92.0)),
                CornerRadius::same(5),
                btn_fill,
            );
            painter.rect_filled(
                Rect::from_center_size(dpad_c, sz(92.0, 30.0)),
                CornerRadius::same(5),
                btn_fill,
            );
            let dpad_arm = |off: Vec2, target: PadBindTarget| {
                let rect = Rect::from_center_size(dpad_c + off * S, sz(30.0, 30.0));
                painter.text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    short_bind(target),
                    FontId::proportional(sc(7.0)),
                    bind_text,
                );
                rect
            };
            let up_r = dpad_arm(Vec2::new(0.0, -31.0), PadBindTarget::Up);
            let down_r = dpad_arm(Vec2::new(0.0, 31.0), PadBindTarget::Down);
            let left_r = dpad_arm(Vec2::new(-31.0, 0.0), PadBindTarget::Left);
            let right_r = dpad_arm(Vec2::new(31.0, 0.0), PadBindTarget::Right);
            hotspot(ui, up_r, PadBindTarget::Up);
            hotspot(ui, down_r, PadBindTarget::Down);
            hotspot(ui, left_r, PadBindTarget::Left);
            hotspot(ui, right_r, PadBindTarget::Right);

            // Face buttons on the right pod: PS1 symbol colours, bind
            // key under each.
            let face_c = at(365.0, 128.0);
            let face = |painter: &egui::Painter, c: Pos2, sym: PadBindTarget| {
                painter.circle_filled(c, sc(16.0), btn_fill);
                let s = sc(6.5);
                match sym {
                    PadBindTarget::Triangle => {
                        let col = Color32::from_rgb(64, 190, 130);
                        let pts = [
                            Pos2::new(c.x, c.y - s),
                            Pos2::new(c.x - s, c.y + s * 0.8),
                            Pos2::new(c.x + s, c.y + s * 0.8),
                        ];
                        painter.line_segment([pts[0], pts[1]], Stroke::new(2.0, col));
                        painter.line_segment([pts[1], pts[2]], Stroke::new(2.0, col));
                        painter.line_segment([pts[2], pts[0]], Stroke::new(2.0, col));
                    }
                    PadBindTarget::Circle => {
                        painter.circle_stroke(
                            c,
                            s,
                            Stroke::new(2.0, Color32::from_rgb(235, 90, 90)),
                        );
                    }
                    PadBindTarget::Cross => {
                        let col = Color32::from_rgb(120, 150, 235);
                        painter.line_segment(
                            [Pos2::new(c.x - s, c.y - s), Pos2::new(c.x + s, c.y + s)],
                            Stroke::new(2.0, col),
                        );
                        painter.line_segment(
                            [Pos2::new(c.x - s, c.y + s), Pos2::new(c.x + s, c.y - s)],
                            Stroke::new(2.0, col),
                        );
                    }
                    PadBindTarget::Square => {
                        painter.rect_stroke(
                            Rect::from_center_size(c, Vec2::splat(s * 1.7)),
                            CornerRadius::ZERO,
                            Stroke::new(2.0, Color32::from_rgb(230, 130, 200)),
                            egui::StrokeKind::Middle,
                        );
                    }
                    _ => {}
                }
                painter.text(
                    c + Vec2::new(0.0, sc(25.0)),
                    Align2::CENTER_CENTER,
                    short_bind(sym),
                    FontId::proportional(sc(8.0)),
                    bind_text,
                );
            };
            let tri_c = face_c - Vec2::new(0.0, sc(36.0));
            let cross_c = face_c + Vec2::new(0.0, sc(36.0));
            let sq_c = face_c - Vec2::new(sc(36.0), 0.0);
            let cir_c = face_c + Vec2::new(sc(36.0), 0.0);
            face(&painter, tri_c, PadBindTarget::Triangle);
            face(&painter, cross_c, PadBindTarget::Cross);
            face(&painter, sq_c, PadBindTarget::Square);
            face(&painter, cir_c, PadBindTarget::Circle);
            let face_hit = Vec2::splat(sc(32.0));
            hotspot(
                ui,
                Rect::from_center_size(tri_c, face_hit),
                PadBindTarget::Triangle,
            );
            hotspot(
                ui,
                Rect::from_center_size(cross_c, face_hit),
                PadBindTarget::Cross,
            );
            hotspot(
                ui,
                Rect::from_center_size(sq_c, face_hit),
                PadBindTarget::Square,
            );
            hotspot(
                ui,
                Rect::from_center_size(cir_c, face_hit),
                PadBindTarget::Circle,
            );

            // Select / Start / Analog cluster on the bridge, each with
            // its bind key beneath.
            let mid_btn = |painter: &egui::Painter, rect: Rect, name: &str, key: &str| {
                painter.rect_filled(rect, CornerRadius::same(3), btn_fill);
                painter.text(
                    Pos2::new(rect.center().x, rect.top() - sc(6.0)),
                    Align2::CENTER_CENTER,
                    name,
                    FontId::proportional(sc(7.0)),
                    theme::MENU_TEXT_DIM,
                );
                painter.text(
                    Pos2::new(rect.center().x, rect.bottom() + sc(7.0)),
                    Align2::CENTER_CENTER,
                    key,
                    FontId::proportional(sc(7.5)),
                    bind_text,
                );
            };
            let select_r = Rect::from_min_max(at(194.0, 100.0), at(228.0, 113.0));
            let start_r = Rect::from_min_max(at(242.0, 100.0), at(276.0, 113.0));
            mid_btn(
                &painter,
                select_r,
                "SELECT",
                &short_bind(PadBindTarget::Select),
            );
            mid_btn(
                &painter,
                start_r,
                "START",
                &short_bind(PadBindTarget::Start),
            );
            hotspot(ui, select_r, PadBindTarget::Select);
            hotspot(ui, start_r, PadBindTarget::Start);

            let analog_r = Rect::from_min_max(at(217.0, 136.0), at(253.0, 149.0));
            painter.rect_filled(analog_r, CornerRadius::same(3), btn_fill);
            painter.text(
                analog_r.center(),
                Align2::CENTER_CENTER,
                "ANALOG",
                FontId::proportional(sc(6.0)),
                Color32::from_rgb(200, 80, 80),
            );
            painter.text(
                Pos2::new(analog_r.center().x, analog_r.bottom() + sc(7.0)),
                Align2::CENTER_CENTER,
                short_bind(PadBindTarget::Analog),
                FontId::proportional(sc(7.5)),
                bind_text,
            );
            hotspot(ui, analog_r, PadBindTarget::Analog);

            // Analog sticks between the grips, DualShock-style: the
            // circle is the stick click (L3/R3, bind key inside), the
            // four chips around it are the keyboard-emulated stick
            // directions.
            let stick = |ui: &mut egui::Ui,
                         painter: &egui::Painter,
                         hotspot: &mut dyn FnMut(&mut egui::Ui, Rect, PadBindTarget),
                         c: Pos2,
                         click: PadBindTarget,
                         dirs: [PadBindTarget; 4]| {
                painter.circle_filled(c, sc(22.0), btn_fill);
                painter.circle_filled(c, sc(14.0), body_shade);
                painter.text(
                    c - Vec2::new(0.0, sc(5.0)),
                    Align2::CENTER_CENTER,
                    match click {
                        PadBindTarget::L3 => "L3",
                        _ => "R3",
                    },
                    FontId::proportional(sc(8.0)),
                    Color32::from_rgb(200, 205, 215),
                );
                painter.text(
                    c + Vec2::new(0.0, sc(5.0)),
                    Align2::CENTER_CENTER,
                    short_bind(click),
                    FontId::proportional(sc(6.5)),
                    bind_text,
                );
                hotspot(ui, Rect::from_center_size(c, Vec2::splat(sc(30.0))), click);
                let chip = sc(15.0);
                let offs = sc(34.0);
                let dirs_off = [
                    Vec2::new(0.0, -offs),
                    Vec2::new(0.0, offs),
                    Vec2::new(-offs, 0.0),
                    Vec2::new(offs, 0.0),
                ];
                let glyphs = ["^", "v", "<", ">"];
                for ((target, off), glyph) in dirs.iter().zip(dirs_off).zip(glyphs) {
                    let r = Rect::from_center_size(c + off, Vec2::splat(chip));
                    painter.rect_filled(r, CornerRadius::same(3), btn_fill);
                    painter.text(
                        r.center(),
                        Align2::CENTER_CENTER,
                        glyph,
                        FontId::proportional(sc(9.0)),
                        theme::MENU_TEXT_DIM,
                    );
                    hotspot(ui, r, *target);
                }
            };
            stick(
                ui,
                &painter,
                &mut hotspot,
                at(184.0, 218.0),
                PadBindTarget::L3,
                [
                    PadBindTarget::LStickUp,
                    PadBindTarget::LStickDown,
                    PadBindTarget::LStickLeft,
                    PadBindTarget::LStickRight,
                ],
            );
            stick(
                ui,
                &painter,
                &mut hotspot,
                at(286.0, 218.0),
                PadBindTarget::R3,
                [
                    PadBindTarget::RStickUp,
                    PadBindTarget::RStickDown,
                    PadBindTarget::RStickLeft,
                    PadBindTarget::RStickRight,
                ],
            );

            ui.add_space(2.0);
            ui.separator();

            // Grouped clickable list -- same capture flow as the
            // drawing, guaranteed to cover every target, with full
            // (unelided) key names.
            const GROUPS: [(&str, &[PadBindTarget]); 6] = [
                (
                    "D-Pad",
                    &[
                        PadBindTarget::Up,
                        PadBindTarget::Down,
                        PadBindTarget::Left,
                        PadBindTarget::Right,
                    ],
                ),
                (
                    "Face buttons",
                    &[
                        PadBindTarget::Cross,
                        PadBindTarget::Circle,
                        PadBindTarget::Square,
                        PadBindTarget::Triangle,
                    ],
                ),
                (
                    "Shoulders",
                    &[
                        PadBindTarget::L1,
                        PadBindTarget::L2,
                        PadBindTarget::R1,
                        PadBindTarget::R2,
                    ],
                ),
                (
                    "Start / Select",
                    &[PadBindTarget::Start, PadBindTarget::Select],
                ),
                (
                    "Left stick",
                    &[
                        PadBindTarget::L3,
                        PadBindTarget::LStickUp,
                        PadBindTarget::LStickDown,
                        PadBindTarget::LStickLeft,
                        PadBindTarget::LStickRight,
                    ],
                ),
                (
                    "Right stick + DualShock",
                    &[
                        PadBindTarget::R3,
                        PadBindTarget::RStickUp,
                        PadBindTarget::RStickDown,
                        PadBindTarget::RStickLeft,
                        PadBindTarget::RStickRight,
                        PadBindTarget::Analog,
                    ],
                ),
            ];
            for (group, targets) in GROUPS {
                ui.add_space(3.0);
                ui.label(
                    egui::RichText::new(group)
                        .color(theme::MENU_ACCENT)
                        .size(12.0)
                        .strong(),
                );
                egui::Grid::new(group)
                    .num_columns(2)
                    .striped(true)
                    .min_col_width(190.0)
                    .show(ui, |ui| {
                        for &target in targets {
                            ui.label(egui::RichText::new(target.label()).size(13.0));
                            let armed = *capture == Some(target);
                            let text = if armed {
                                "press a key...".to_string()
                            } else {
                                bind_of(target)
                            };
                            let btn = egui::Button::new(
                                egui::RichText::new(text).size(13.0).color(if armed {
                                    theme::MENU_ACCENT
                                } else {
                                    Color32::from_rgb(220, 224, 232)
                                }),
                            )
                            .min_size(Vec2::new(150.0, 20.0));
                            if ui.add(btn).clicked() {
                                *capture = Some(target);
                            }
                            ui.end_row();
                        }
                    });
            }

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui
                    .button(format!("{}  Reset to defaults", icons::ROTATE_CCW))
                    .clicked()
                {
                    *pending_pointer_action = Some(MenuAction::ResetControls);
                    *capture = None;
                }
                ui.label(
                    egui::RichText::new("Binding a key already in use unbinds its old control.")
                        .color(theme::MENU_TEXT_DIM)
                        .size(11.0),
                );
            });
        });
    *open = still_open;
}
/// Paint a save's thumbnail at `size`, loading and caching the
/// texture on first use. Draws a plain placeholder box when `path` is
/// `None` (no capture for this save) or fails to decode.
fn draw_thumb(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    cache: &mut HashMap<PathBuf, egui::TextureHandle>,
    path: Option<&std::path::Path>,
    size: Vec2,
) {
    let placeholder = |ui: &mut egui::Ui| {
        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
        ui.painter()
            .rect_filled(rect, 4.0, egui::Color32::from_gray(28));
    };
    let Some(path) = path else {
        placeholder(ui);
        return;
    };
    if !cache.contains_key(path) {
        let loaded = std::fs::read(path)
            .ok()
            .and_then(|bytes| image::load_from_memory(&bytes).ok())
            .map(|img| img.to_rgba8());
        if let Some(rgba) = loaded {
            let (w, h) = rgba.dimensions();
            let color_image =
                egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw());
            let tex = ctx.load_texture(
                format!("savethumb-{}", path.display()),
                color_image,
                egui::TextureOptions::LINEAR,
            );
            cache.insert(path.to_path_buf(), tex);
        }
    }
    match cache.get(path) {
        Some(tex) => {
            ui.add(egui::Image::new((tex.id(), size)));
        }
        None => placeholder(ui),
    }
}

/// The About card: brand mark, build info, and a few real links. Built from
/// egui widgets (not the painter) so the links are first-class clickable
/// `ui.link`s -- much less code than hand-rolled hit-testing. Opened from the
/// Settings "About" row; closes on its Close button, confirm/back, or a click
/// outside it.
fn about_panel(ctx: &egui::Context, open: &mut bool) {
    let logo_tex = crate::ui::splash::logo_texture(ctx);
    let [tw, th] = logo_tex.size();
    let aspect = tw as f32 / th.max(1) as f32;
    let link = |ui: &mut egui::Ui, text: &str, url: &str| {
        if ui
            .link(
                egui::RichText::new(text)
                    .color(theme::MENU_ACCENT)
                    .size(14.0),
            )
            .clicked()
        {
            open_external_url(url);
        }
    };

    let area = egui::Area::new(egui::Id::new("about-card"))
        .order(egui::Order::Foreground)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .show(ctx, |ui| {
            egui::Frame::NONE
                .fill(egui::Color32::from_rgb(18, 20, 26))
                .stroke(egui::Stroke::new(1.0, theme::MENU_ACCENT))
                .corner_radius(egui::CornerRadius::same(8))
                .inner_margin(egui::Margin::symmetric(30, 26))
                .show(ui, |ui| {
                    ui.set_width(340.0);
                    ui.vertical_centered(|ui| {
                        let w = 210.0;
                        ui.image(egui::load::SizedTexture::new(
                            logo_tex.id(),
                            egui::vec2(w, w / aspect),
                        ));
                        ui.add_space(12.0);
                        ui.label(
                            egui::RichText::new(concat!("Version ", env!("CARGO_PKG_VERSION")))
                                .color(theme::MENU_TEXT_BRIGHT),
                        );
                        ui.label(
                            egui::RichText::new(
                                "Independent, open-source PS1 developer environment",
                            )
                            .color(theme::MENU_TEXT_DIM)
                            .size(13.0),
                        );
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new("License: GPL-2.0-or-later")
                                .color(theme::MENU_TEXT_DIM)
                                .size(13.0),
                        );
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new(
                                "Load your homebrew games directly. No firmware image is required.",
                            )
                            .color(theme::MENU_TEXT_DIM)
                            .size(12.0),
                        );
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(
                                "PlayStation and PS1 are trademarks of Sony Interactive \
                                 Entertainment. PSoXide is unaffiliated.",
                            )
                            .color(theme::MENU_HINT)
                            .size(11.0),
                        );
                        ui.add_space(4.0);
                        ui.add_space(16.0);
                        link(
                            ui,
                            "Source code on GitHub",
                            "https://github.com/EBonura/PSoXide",
                        );
                        ui.add_space(4.0);
                        link(
                            ui,
                            "Bonnie Studios on itch.io",
                            "https://bonnie-studios.itch.io/",
                        );
                        ui.add_space(18.0);
                        if ui.button("Close").clicked() {
                            *open = false;
                        }
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new("Cross / Enter to close")
                                .color(theme::MENU_HINT)
                                .size(11.0),
                        );
                    });
                });
        });

    // A click anywhere outside the card closes it.
    if area.response.clicked_elsewhere() {
        *open = false;
    }
}

/// Open a URL in the user's default browser (native) or a new tab (web).
#[cfg(not(target_arch = "wasm32"))]
fn open_external_url(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    if let Err(e) = std::process::Command::new(opener).arg(url).spawn() {
        eprintln!("[frontend] open url failed: {e}");
    }
}

#[cfg(target_arch = "wasm32")]
fn open_external_url(url: &str) {
    if let Some(w) = web_sys::window() {
        let _ = w.open_with_url_and_target(url, "_blank");
    }
}

/// A plain row: no indent, no burn action.
fn row(label: &str, action: MenuAction, value: Option<&str>) -> MenuItem {
    MenuItem {
        depth: 0,
        label: label.into(),
        action,
        burn_action: None,
        value: value.map(Into::into),
    }
}

/// Emulator settings. Values are filled in by the `set_*`/`sync_*` setters.
fn build_settings_category() -> Category {
    let mut items = vec![
        row("Controls", MenuAction::OpenControls, None),
        row("Video scale", MenuAction::CycleVideoScale, Some("High-res")),
        row(
            "Texture filter",
            MenuAction::CycleTextureFilter,
            Some("None"),
        ),
        row("Volume", MenuAction::CycleVolume, Some("100%")),
        row("Mute", MenuAction::ToggleMute, Some("Off")),
        row("Menu opacity", MenuAction::CycleMenuOpacity, Some("90%")),
        row("UI scale", MenuAction::CycleUiScale, Some("100%")),
        row(
            "When the computer is slow",
            MenuAction::ToggleSmoothSlowHost,
            Some(slow_host_label(false)),
        ),
        row("About", MenuAction::ShowAbout, None),
    ];
    // There is no "quit" in a browser tab.
    if cfg!(not(target_arch = "wasm32")) {
        items.push(row("Quit PSoXide", MenuAction::Quit, None));
    }
    Category {
        name: "Settings",
        icon: icons::HARD_DRIVE,
        items,
    }
}

#[derive(Default)]
struct LibraryFolder<'a> {
    children: BTreeMap<std::ffi::OsString, LibraryFolder<'a>>,
    games: Vec<&'a LibraryItem>,
    count: usize,
}

impl LibraryFolder<'_> {
    fn append_rows(
        &mut self,
        path: &Path,
        depth: usize,
        expanded: &HashSet<PathBuf>,
        items: &mut Vec<MenuItem>,
    ) {
        let mut children: Vec<_> = self.children.iter_mut().collect();
        children.sort_by_key(|(name, _)| name.to_string_lossy().to_lowercase());
        for (name, folder) in children {
            let path = path.join(name);
            items.push(MenuItem {
                depth,
                label: name.to_string_lossy().into_owned(),
                action: MenuAction::ToggleLibraryFolder(path.clone()),
                burn_action: None,
                value: Some(format!(
                    "{} {}",
                    folder.count,
                    if folder.count == 1 { "game" } else { "games" }
                )),
            });
            if expanded.contains(&path) {
                folder.append_rows(&path, depth + 1, expanded, items);
            }
        }
        self.games.sort_by_key(|game| game.title.to_lowercase());
        for game in &self.games {
            items.push(MenuItem {
                depth,
                label: game.title.clone(),
                action: MenuAction::LaunchGame(game.id.clone()),
                burn_action: None,
                value: (!game.subtitle.is_empty()).then(|| game.subtitle.clone()),
            });
        }
    }
}

/// Construct the Library: the games-folder tree, then the Homebrew folder
/// (SDK examples, project builds, and on the web the streamed demo disc),
/// then the folder and refresh rows, each exactly once.
fn build_library_category(
    games: &[LibraryItem],
    homebrew: &[LibraryItem],
    expanded: &HashSet<PathBuf>,
    games_path: &str,
) -> Category {
    let mut items = Vec::with_capacity(games.len() + homebrew.len() + 4);
    let mut root = LibraryFolder::default();
    for game in games {
        let mut folder = &mut root;
        folder.count += 1;
        for part in game.folder.components() {
            if let std::path::Component::Normal(name) = part {
                folder = folder.children.entry(name.to_owned()).or_default();
                folder.count += 1;
            }
        }
        folder.games.push(game);
    }
    root.append_rows(Path::new(""), 0, expanded, &mut items);

    if !homebrew.is_empty() {
        let key = PathBuf::from(HOMEBREW_FOLDER);
        let open = expanded.contains(&key);
        items.push(MenuItem {
            depth: 0,
            label: "Homebrew".into(),
            action: MenuAction::ToggleLibraryFolder(key),
            burn_action: None,
            value: Some(format!("{} entries", homebrew.len())),
        });
        if open {
            items.extend(homebrew.iter().map(|entry| {
                MenuItem {
                    depth: 1,
                    label: entry.title.clone(),
                    action: if entry.launchable {
                        MenuAction::LaunchGame(entry.id.clone())
                    } else {
                        MenuAction::BuildExamples
                    },
                    burn_action: (entry.launchable && entry.burnable)
                        .then(|| MenuAction::OpenBurnMenu(entry.id.clone())),
                    value: (!entry.subtitle.is_empty()).then(|| entry.subtitle.clone()),
                }
            }));
        }
    }

    if cfg!(target_arch = "wasm32") {
        items.push(row("Load games folder", MenuAction::ChooseGamesPath, None));
        // Reload the folder remembered from a previous visit (Chrome/Edge;
        // no-op where unsupported).
        #[cfg(target_arch = "wasm32")]
        items.push(row("Reconnect saved games", MenuAction::Reconnect, None));
    } else {
        items.push(row(
            "Choose games folder",
            MenuAction::ChooseGamesPath,
            Some(if games_path.is_empty() {
                "Missing"
            } else {
                games_path
            }),
        ));
        // The web library is whatever the folder picker returned, so a
        // rescan there has nothing to walk.
        items.push(row("Refresh library", MenuAction::RescanLibrary, Some("↻")));
    }
    Category {
        name: "Library",
        icon: icons::DISC,
        items,
    }
}

/// The Game column, shown only while a game is loaded: run control,
/// save states, reset and input tapes.
fn build_game_category(running: bool, recording: bool) -> Category {
    Category {
        name: "Game",
        icon: icons::GAMEPAD_2,
        items: vec![
            row(
                if running { "Pause" } else { "Resume" },
                MenuAction::ToggleRun,
                None,
            ),
            row("Save state", MenuAction::SaveState, Some("F5")),
            row("Load state", MenuAction::OpenSaveStates, Some("F7")),
            row("Reset", MenuAction::Reset, None),
            row(
                recording_label(recording),
                MenuAction::ToggleInputRecording,
                Some("F8"),
            ),
            row("Load input replay", MenuAction::LoadInputReplay, None),
        ],
    }
}

/// The recording row's label. Web recordings reboot the game first
/// (cold-boot tapes) and download as a CSV.
fn recording_label(recording: bool) -> &'static str {
    match (recording, cfg!(target_arch = "wasm32")) {
        (true, true) => "Stop recording (download CSV)",
        (true, false) => "Stop input recording",
        (false, true) => "Record input from boot",
        (false, false) => "Record input",
    }
}

/// Settings-row value for `video.smooth_slow_host`.
fn slow_host_label(smooth: bool) -> &'static str {
    if smooth {
        "Keep it smooth"
    } else {
        "Keep game speed"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_item(id: &str, title: &str, sub: &str) -> LibraryItem {
        LibraryItem {
            folder: PathBuf::new(),
            id: id.into(),
            title: title.into(),
            subtitle: sub.into(),
            burnable: false,
            launchable: true,
        }
    }

    fn draw_pointer_click(menu: &mut MenuState, position: Pos2) {
        let context = egui::Context::default();
        theme::apply(&context);
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(960.0, 720.0))),
            events: vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            ..Default::default()
        };
        let _ = context.run(input, |context| menu.draw(context, 1.0, None));
    }

    fn labels(menu: &MenuState, category: &str) -> Vec<String> {
        menu.categories
            .iter()
            .find(|c| c.name == category)
            .unwrap()
            .items
            .iter()
            .map(|item| item.label.clone())
            .collect()
    }

    fn names(menu: &MenuState) -> Vec<&'static str> {
        menu.categories.iter().map(|c| c.name).collect()
    }

    fn confirm(menu: &mut MenuState) -> Option<MenuAction> {
        menu.update(&MenuInput {
            confirm: true,
            ..Default::default()
        })
    }

    #[test]
    fn nested_library_folders_start_closed_and_launch_the_selected_game() {
        let mut game = dummy_item("hl", "Half-Life", "460 MiB");
        game.folder = PathBuf::from("Bonnie Studios/Ports");
        let root_game = dummy_item("root", "Root game", "");
        let games = [game, root_game];
        let mut menu = MenuState::new();
        menu.set_library(&games, &[], &[]);
        assert_eq!(
            labels(&menu, "Library"),
            [
                "Bonnie Studios",
                "Root game",
                "Choose games folder",
                "Refresh library"
            ]
        );
        assert_eq!(
            confirm(&mut menu),
            Some(MenuAction::ToggleLibraryFolder("Bonnie Studios".into()))
        );
        menu.toggle_library_folder(Path::new("Bonnie Studios"));
        assert_eq!(menu.categories[0].items[1].label, "Ports");
        assert_eq!(menu.categories[0].items[1].depth, 1);
        assert!(!labels(&menu, "Library").contains(&"Half-Life".to_string()));
        menu.toggle_library_folder(Path::new("Bonnie Studios/Ports"));
        assert_eq!(menu.categories[0].items[2].depth, 2);
        menu.item_index = 2;
        assert_eq!(
            confirm(&mut menu),
            Some(MenuAction::LaunchGame("hl".into()))
        );
        menu.set_library(&games, &[], &[]);
        assert_eq!(
            menu.selected_action(),
            Some(&MenuAction::LaunchGame("hl".into()))
        );
        menu.toggle_library_folder(Path::new("Bonnie Studios"));
        assert_eq!(menu.item_index, 0);
        assert_eq!(menu.categories[0].items.len(), 4);
        let mut fresh = MenuState::new();
        fresh.set_library(&games, &[], &[]);
        assert_eq!(fresh.categories[0].items.len(), 4);
    }

    #[test]
    fn same_named_subfolders_expand_independently_and_refresh_keeps_selection() {
        let mut a = dummy_item("a", "Game A", "");
        a.folder = "A/Tests".into();
        let mut b = dummy_item("b", "Game B", "");
        b.folder = "B/Tests".into();
        let mut menu = MenuState::new();
        menu.set_library(&[a.clone(), b.clone()], &[], &[]);
        menu.toggle_library_folder(Path::new("A"));
        menu.toggle_library_folder(Path::new("A/Tests"));
        menu.toggle_library_folder(Path::new("B"));
        assert!(labels(&menu, "Library").contains(&"Game A".to_string()));
        assert!(!labels(&menu, "Library").contains(&"Game B".to_string()));
        menu.item_index = 2;
        menu.set_library(&[a, b, dummy_item("new", "New root game", "")], &[], &[]);
        assert_eq!(
            menu.selected_action(),
            Some(&MenuAction::LaunchGame("a".into()))
        );
        menu.set_library(&[], &[], &[]);
        assert!(menu.expanded_folders.is_empty());
        assert_eq!(menu.item_index, 0);
    }

    #[test]
    fn clicking_a_folder_queues_expansion_instead_of_a_launch() {
        let mut game = dummy_item("hl", "Half-Life", "");
        game.folder = "Bonnie Studios".into();
        let mut menu = MenuState::new();
        menu.set_library(&[game], &[], &[]);
        draw_pointer_click(
            &mut menu,
            Pos2::new(480.0, 720.0 * 0.38 + ICON_SIZE_ACTIVE + 64.0),
        );
        assert_eq!(
            menu.take_pending_pointer_action(),
            Some(MenuAction::ToggleLibraryFolder("Bonnie Studios".into()))
        );
    }

    #[test]
    fn fresh_state_has_library_and_settings_only() {
        let s = MenuState::new();
        assert_eq!(names(&s), ["Library", "Settings"]);
    }

    #[test]
    fn game_column_appears_only_while_a_game_is_loaded() {
        let mut s = MenuState::new();
        s.select_category("Settings");
        s.set_game_loaded(true);
        assert_eq!(names(&s), ["Library", "Game", "Settings"]);
        // The selected column is kept across the insert.
        assert_eq!(s.current_category(), Some("Settings"));
        assert_eq!(
            labels(&s, "Game"),
            [
                "Resume",
                "Save state",
                "Load state",
                "Reset",
                "Record input",
                "Load input replay"
            ]
        );
        s.sync_run_label(true);
        s.sync_input_recording_label(true);
        assert_eq!(labels(&s, "Game")[0], "Pause");
        assert_eq!(labels(&s, "Game")[4], "Stop input recording");
        // Recording stays one row, one key.
        let game = s.categories.iter().find(|c| c.name == "Game").unwrap();
        let rec: Vec<_> = game
            .items
            .iter()
            .filter(|item| item.action == MenuAction::ToggleInputRecording)
            .collect();
        assert_eq!(rec.len(), 1);
        assert_eq!(rec[0].value.as_deref(), Some("F8"));

        s.select_category("Game");
        s.set_game_loaded(false);
        assert_eq!(names(&s), ["Library", "Settings"]);
        assert_eq!(s.current_category(), Some("Library"));
        // Labels survive the column being rebuilt.
        s.set_game_loaded(true);
        assert_eq!(labels(&s, "Game")[0], "Pause");
        assert_eq!(labels(&s, "Game")[4], "Stop input recording");
    }

    #[test]
    fn folder_and_refresh_rows_appear_exactly_once_at_the_bottom() {
        let mut s = MenuState::new();
        s.set_library(
            &[dummy_item("g1", "Crash", "NTSC-U")],
            &[dummy_item("e1", "hello-tri", "EXE")],
            &[dummy_item("p1", "Stone Room", "Project")],
        );
        s.toggle_library_folder(Path::new(HOMEBREW_FOLDER));
        let actions: Vec<_> = s.categories.iter().flat_map(|c| &c.items).collect();
        for wanted in [MenuAction::RescanLibrary, MenuAction::ChooseGamesPath] {
            assert_eq!(actions.iter().filter(|i| i.action == wanted).count(), 1);
        }
        let library = &s.categories[0].items;
        let n = library.len();
        assert_eq!(library[n - 2].action, MenuAction::ChooseGamesPath);
        assert_eq!(library[n - 1].action, MenuAction::RescanLibrary);
        s.set_games_path_label("discs");
        assert_eq!(s.categories[0].items[n - 2].value.as_deref(), Some("discs"));
        // An empty library has no placeholder rows, only the two actions.
        let empty = MenuState::new();
        assert_eq!(
            labels(&empty, "Library"),
            ["Choose games folder", "Refresh library"]
        );
        assert_eq!(
            empty.categories[0].items[0].value.as_deref(),
            Some("Missing")
        );
        assert!(!labels(&s, "Settings")
            .iter()
            .any(|label| label.to_lowercase().contains("folder")
                || label.to_lowercase().contains("bios")));
    }

    #[test]
    fn homebrew_folder_follows_the_games_tree() {
        let mut s = MenuState::new();
        let mut nested = dummy_item("g2", "Nested", "");
        nested.folder = "Ports".into();
        s.set_library(
            &[dummy_item("g1", "Crash", "NTSC-U · 600 MiB"), nested],
            &[dummy_item("e1", "hello-tri", "EXE")],
            &[dummy_item("p1", "Stone Room", "Project")],
        );
        assert_eq!(
            labels(&s, "Library"),
            [
                "Ports",
                "Crash",
                "Homebrew",
                "Choose games folder",
                "Refresh library"
            ]
        );
        let homebrew = &s.categories[0].items[2];
        assert_eq!(homebrew.value.as_deref(), Some("2 entries"));
        s.toggle_library_folder(Path::new(HOMEBREW_FOLDER));
        let items = &s.categories[0].items;
        assert_eq!(items[3].label, "hello-tri");
        assert_eq!(items[3].depth, 1);
        assert_eq!(items[4].label, "Stone Room");
        assert_eq!(items[4].action, MenuAction::LaunchGame("p1".into()));
        // A rescan keeps the Homebrew folder open.
        s.set_library(
            &[dummy_item("g1", "Crash", "")],
            &[dummy_item("e1", "hello-tri", "EXE")],
            &[],
        );
        assert_eq!(labels(&s, "Library")[2], "hello-tri");
        // No homebrew, no folder.
        s.set_library(&[dummy_item("g1", "Crash", "")], &[], &[]);
        assert!(!labels(&s, "Library").contains(&"Homebrew".to_string()));
    }

    #[test]
    fn burn_action_is_only_shown_for_burnable_homebrew() {
        let mut s = MenuState::new();
        let game = LibraryItem {
            burnable: true,
            ..dummy_item("g1", "Retail Disc", "NTSC-U")
        };
        let example = LibraryItem {
            burnable: true,
            ..dummy_item("e1", "hello-cdda", "CUE")
        };
        let source_example = LibraryItem {
            launchable: false,
            burnable: true,
            ..dummy_item("e2", "hello-tri", "not built")
        };
        let project = LibraryItem {
            burnable: true,
            ..dummy_item("p1", "Demo 10", "Project")
        };

        s.set_library(&[game], &[example, source_example], &[project]);
        s.toggle_library_folder(Path::new(HOMEBREW_FOLDER));
        let items = &s.categories[0].items;
        assert_eq!(items[0].burn_action, None);
        assert_eq!(
            items[2].burn_action,
            Some(MenuAction::OpenBurnMenu("e1".to_string()))
        );
        assert_eq!(items[3].burn_action, None);
        assert_eq!(items[3].action, MenuAction::BuildExamples);
        assert_eq!(
            items[4].burn_action,
            Some(MenuAction::OpenBurnMenu("p1".to_string()))
        );
    }

    #[test]
    fn set_library_preserves_category_across_rebuild() {
        let mut s = MenuState::new();
        s.select_category("Settings");
        s.set_library(&[], &[], &[]);
        assert_eq!(s.current_category(), Some("Settings"));
    }

    #[test]
    fn left_right_wraps_around_categories() {
        let mut s = MenuState::new();
        s.set_library(&[dummy_item("a", "A", "")], &[], &[]);
        s.set_game_loaded(true);
        let n = s.categories.len();
        assert_eq!(n, 3);
        assert_eq!(s.current_category(), Some("Library"));

        let left = MenuInput {
            left: true,
            ..Default::default()
        };
        let right = MenuInput {
            right: true,
            ..Default::default()
        };

        // Left from the first category wraps to the last.
        s.update(&left);
        assert_eq!(s.current_category(), Some("Settings"));
        // Right from the last wraps back to the first.
        s.update(&right);
        assert_eq!(s.current_category(), Some("Library"));
        // A full lap of rights returns to the start.
        for _ in 0..n {
            s.update(&right);
        }
        assert_eq!(s.current_category(), Some("Library"));
    }

    #[test]
    fn vertical_navigation_wraps_within_category() {
        let mut s = MenuState::new();
        s.set_library(
            &[
                dummy_item("a", "A", ""),
                dummy_item("b", "B", ""),
                dummy_item("c", "C", ""),
            ],
            &[],
            &[],
        );

        let up = MenuInput {
            up: true,
            ..Default::default()
        };
        s.update(&up);
        assert_eq!(s.selected_action(), Some(&MenuAction::RescanLibrary));

        let down = MenuInput {
            down: true,
            ..Default::default()
        };
        s.update(&down);
        assert_eq!(
            s.selected_action(),
            Some(&MenuAction::LaunchGame("a".to_string()))
        );
    }

    #[test]
    fn settings_holds_controls_video_audio_and_quit() {
        let mut s = MenuState::new();
        s.select_category("Settings");
        assert_eq!(s.selected_action(), Some(&MenuAction::OpenControls));
        assert_eq!(
            labels(&s, "Settings"),
            [
                "Controls",
                "Video scale",
                "Texture filter",
                "Volume",
                "Mute",
                "Menu opacity",
                "UI scale",
                "When the computer is slow",
                "About",
                "Quit PSoXide"
            ]
        );
        s.sync_video_audio(false, "xBR", 0.5, true);
        s.set_smooth_slow_host(true);
        let values: Vec<_> = s.categories[1]
            .items
            .iter()
            .map(|item| item.value.clone().unwrap_or_default())
            .collect();
        assert_eq!(&values[1..5], ["Native", "xBR", "50%", "On"]);
        assert_eq!(values[7], "Keep it smooth");
    }

    #[test]
    fn pointer_click_on_row_queues_its_action() {
        let mut s = MenuState::new();
        s.select_category("Settings");
        let first_row_center = Pos2::new(480.0, 720.0 * 0.38 + ICON_SIZE_ACTIVE + 64.0);

        draw_pointer_click(&mut s, first_row_center);

        assert_eq!(
            s.take_pending_pointer_action(),
            Some(MenuAction::OpenControls)
        );
    }

    #[test]
    fn pointer_click_on_category_selects_it() {
        let mut s = MenuState::new();
        let settings_index = s
            .categories
            .iter()
            .position(|category| category.name == "Settings")
            .unwrap();
        let settings_icon = Pos2::new(
            480.0 + settings_index as f32 * CATEGORY_SPACING,
            720.0 * 0.38,
        );

        draw_pointer_click(&mut s, settings_icon);

        assert_eq!(s.current_category(), Some("Settings"));
    }

    #[test]
    fn menu_settings_update_their_values() {
        let mut state = MenuState::new();
        state.set_ui_scale(75);
        state.set_menu_opacity(65);
        let settings = &state.categories[1];
        let value = |action: MenuAction| {
            settings
                .items
                .iter()
                .find(|item| item.action == action)
                .unwrap()
                .value
                .clone()
        };
        assert_eq!(value(MenuAction::CycleUiScale).as_deref(), Some("75%"));
        assert_eq!(value(MenuAction::CycleMenuOpacity).as_deref(), Some("65%"));
    }
}
