//! The home screen: a console-style shell that replaces the old cross media
//! bar. One jewel case on a quiet floor, its title, one line, one button.
//!
//! Home is a view over [`MenuState`]: the same library, settings and game
//! rows, the same [`MenuInput`] edges from keyboard, pad and touch, the same
//! [`MenuAction`]s going out. Only the drawing and the focus rules are new.

mod controls;
mod covers;
mod grid;
mod hints;
mod launch;
mod motion;
mod pages;
mod scene;
mod slots;
pub(crate) mod tokens;

use egui::{Align2, Color32, FontId, Pos2, Rect, Shape, Stroke, Vec2};

use super::{LibraryItem, MenuAction, MenuInput, MenuState};
use crate::icons;
use crate::ui::momentum::Momentum;
use crate::ui_sound::UiSound;
use hints::{Btn, HintStyle};
use scene::{fade, CoverKind, CYAN};

pub(crate) use controls::Command as DeviceCommand;
pub(crate) use covers::CoverState;

/// Seconds of countdown before the demo disc starts on a first visit.
const COUNTDOWN_SECS: f32 = 3.0;

/// The countdown length: three seconds, or `?countdown=N` in the browser.
fn countdown_secs() -> f32 {
    #[cfg(target_arch = "wasm32")]
    if let Some(secs) = crate::web_bench::countdown_param() {
        return secs;
    }
    COUNTDOWN_SECS
}
/// Where the project's site lives (the quiet link under the home screen).
pub(super) const SITE_URL: &str = "https://ebonura.github.io/PSoXide/";
/// Where the free builds, and the optional donation, live.
const ITCH_URL: &str = "https://bonnie-studios.itch.io/psoxide";
/// Where the Demo Disc download lives (shown when no copy is on this computer).
const DEMO_DISC_URL: &str = "https://bonnie-studios.itch.io/psoxide-demo-disc";
pub(super) const SOURCE_URL: &str = "https://github.com/EBonura/PSoXide";

/// The pages the shell can show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Page {
    /// The cases and animations: the library of discs, and the game you left.
    Home,
    /// What you can do to the running game.
    Game,
    /// Controllers, keyboard and touch, inside the shell.
    Controls,
    Settings,
    /// The save-state slots: save into one, or load from one. Reached from the Game page, the
    /// toolbar and the touch buttons; the Game tab stays lit while it is up.
    Slots,
    Welcome,
}

impl Page {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Game => "Game",
            Self::Controls => "Controls",
            Self::Settings => "Settings",
            Self::Slots => "Slots",
            Self::Welcome => "Welcome",
        }
    }

    /// The page whose tab is lit while this one is up.
    pub(crate) fn nav_page(self) -> Self {
        match self {
            Self::Slots => Self::Game,
            other => other,
        }
    }
}

/// Which row of the screen has the controller's focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Zone {
    Nav,
    Body,
}

/// The nav entries, in order. `Game` shows only while a game is started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NavItem {
    Home,
    Game,
    Controls,
    Settings,
}

impl NavItem {
    fn label(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Game => "Game",
            Self::Controls => "Controls",
            Self::Settings => "Settings",
        }
    }

    fn page(self) -> Page {
        match self {
            Self::Home => Page::Home,
            Self::Game => Page::Game,
            Self::Controls => Page::Controls,
            Self::Settings => Page::Settings,
        }
    }
}

fn nav_items(in_game: bool) -> Vec<NavItem> {
    let mut items = vec![NavItem::Home];
    if in_game {
        items.push(NavItem::Game);
    }
    items.extend([NavItem::Settings, NavItem::Controls]);
    items
}

/// How far along the demo disc is.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub(crate) enum DemoStatus {
    /// This build has no demo disc (desktop).
    #[default]
    Absent,
    /// Streaming; the fraction is 0..1, or negative when unknown.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    Loading(f32),
    /// Booted and waiting behind the home screen.
    Ready,
}

/// What the shell tells the home screen each frame.
#[derive(Clone, Debug, Default)]
pub(crate) struct HomeFeed {
    pub demo: DemoStatus,
    pub keyboard: bool,
    pub gamepads: usize,
    pub touch: bool,
    /// Left edge (points) of the touch Menu and Fullscreen buttons, when they
    /// sit in the top band.
    pub touch_ui_left: Option<f32>,
    /// The player sits in another page's frame (itch.io), which puts its own fullscreen button
    /// over the bottom right corner.
    pub framed: bool,
    /// The game that is loaded: its launch id and title.
    pub current: Option<(String, String)>,
    /// Safe-area insets in points: top, bottom, left, right.
    pub safe: [f32; 4],
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Countdown {
    /// Not a first visit, or no demo disc.
    Off,
    /// First visit; waiting for the disc to be ready to start.
    Armed,
    Running(f32),
    Cancelled,
    Done,
}

/// What a click or tap on a drawn element does.
#[derive(Clone, Debug)]
enum Click {
    Nav(NavItem),
    Logo,
    Case(usize),
    Primary,
    Restart,
    Burn,
    SizeToggle,
    /// A shoulder glyph beside the tabs: step to the previous (-1) or next (1) section.
    TabStep(i32),
    Rail,
    /// The shell's close button on a phone: back to the game that is paused behind it.
    Close,
    Act(MenuAction),
    Link(&'static str),
    Welcome,
    WelcomeStep(i32),
    Row(usize),
    RowBurn(usize),
    Slider(usize, Rect),
    /// A control of the Controls page.
    Ctl(controls::Item),
    Dismiss,
}

pub(crate) struct HomeState {
    pub(super) page: Page,
    zone: Zone,
    nav: usize,
    case_focus: usize,
    case_pos: f32,
    time: f32,
    countdown: Countdown,
    welcome: usize,
    /// The "What is this?" pages are showing because this is the first run: the first-visit
    /// countdown waits behind them and the pages' own taps and keys never cancel it.
    explainer: bool,
    /// The explainer was dismissed; the app takes this once, to remember it in the saved settings.
    explainer_seen: bool,
    /// Sounds asked for since the app last took them (see `MenuState::take_sounds`).
    pub(super) sounds: Vec<UiSound>,
    /// What the last frame looked like, to hear what changed.
    watch: Option<SoundWatch>,
    /// A confirm or a back was pressed or tapped this frame.
    snd_confirm: bool,
    snd_back: bool,
    started: bool,
    feed: HomeFeed,
    dither: Option<egui::TextureHandle>,
    /// The small pictures of the demo disc's games, loaded when the home screen first shows them.
    disc_tiles: Option<Vec<Option<egui::TextureHandle>>>,
    /// The demo disc's cover, for its case on Home (the disc design with a wedge for each game).
    demo_cover: Option<egui::TextureHandle>,
    /// Where the contents chips were drawn this frame, `[x0, y0, x1, y1, chips, style]` (journeys).
    contents: Option<[f32; 6]>,
    list_scroll: f32,
    /// The motion of the list or controls page under a finger: coasting, stretch, tap-to-stop.
    kin: Momentum,
    /// The page the motion belongs to: a page change ends it.
    kin_page: Page,
    /// The list page's scroll, its furthest scroll and the top and bottom of its view as of the
    /// last frame, for the journeys.
    list_view: [f32; 4],
    controls: controls::ControlsState,
    press: Option<Press>,
    hits: Vec<(Rect, Click)>,
    pub(super) covers: CoverState,
    /// Where the page opened from, for Back.
    first_visit: bool,
    last_row: usize,
    /// The shell was closed since it last opened: the next opening starts at the top.
    closed: bool,
    /// Where Back goes from the slot sheet: the page it was opened from, or `None` to close the
    /// shell over the game.
    slots_return: Option<Page>,
    /// An input already cancelled the countdown this frame (a pad button or a
    /// stick, seen before the menu's own edges): that press only cancels.
    cancelled_this_frame: bool,
    launch: Option<launch::Launch>,
    /// The launch has been handed to the app (it starts with the sequence, so the boot
    /// runs alongside the animation).
    launch_sent: bool,
    /// The sequence has played out; it holds on its last moment until the game is up.
    launch_done: bool,
    pending_launch: Option<MenuAction>,
    /// The egui clock at the last drawn frame: the sequence runs on elapsed time, so a slow
    /// or throttled page skips frames and still takes the same time.
    clock: Option<f64>,
    label_tex: Option<egui::TextureHandle>,
    /// Where the slot sheet is drawn this frame (by the shell around it).
    section: Option<Rect>,
    /// The compact grid instead of the big cases.
    compact: bool,
    /// The letter shown big for a moment after a jump, and how long it has left.
    badge: Option<(char, f32)>,
    grid_scroll: f32,
    grid_cols: usize,
    /// The letter rail of the grid on a phone, when it is up.
    rail: Option<(Rect, Vec<char>)>,
    /// Where the primary button ended last frame.
    btn_bottom: f32,
    /// The animation state of each case that has been drawn: focus spring,
    /// picture fade and loading shimmer, by cover key (or title).
    anim: std::collections::HashMap<String, CaseAnim>,
    /// Seconds of the last frame (elapsed time, not the clamped step).
    frame_dt: f32,
    /// How lit each hint glyph is from a press (L1, R1, Triangle, Square): 1 at the press,
    /// easing to 0.
    pad_press: [f32; 4],
    /// How many first letters the shelf has (the L2/R2 hint shows when there is a jump to make).
    letters: usize,
    /// The reduce-motion setting.
    calm_setting: bool,
    /// The browser asked for reduced motion.
    calm_browser: bool,
    /// Desktop: the Demo Disc is a disc in the games folder (or beside the program), found by
    /// name, not one the page streams in.
    local_demo: bool,
    /// The page whose parts are being revealed, and seconds since it opened: its parts arrive one
    /// after another (title, then rows). Reset when the page changes or the shell reopens.
    shown_page: Page,
    page_age: f32,
    /// Which way the page change went along the nav (-1 back, 1 forward), for the slide-in.
    page_dir: f32,
    /// Seconds since the Home text last changed (a new page or another case in focus).
    home_age: f32,
    last_case: usize,
    /// The tab highlight, in tabs from the first: it slides between them with a small spring.
    nav_hl: motion::Spring,
    /// The keyboard or pad focus on the tabs, the same way.
    nav_ring: motion::Spring,
    /// How much of the highlight shows (it fades when no tab is the page, as on Welcome), and
    /// how far the focus is on the tabs.
    nav_vis: f32,
    ring_vis: f32,
    /// The focus frame of a list page: where it is (in px from the list's top), gliding.
    row_cage: motion::Spring,
    /// Where the welcome pages sit, in pages from the first: a finger drags it, and it glides to
    /// the page `welcome` once let go (or when Next, Back or a key turns the page).
    pager: motion::Spring,
    /// A finger or the mouse is dragging the welcome pages.
    pager_drag: Option<PagerDrag>,
    /// How far apart the welcome pages are laid out, in points (set when they are drawn).
    pager_span: f32,
    /// The speed of the last drag when it let go (points per second), for the journeys.
    pager_last_vel: f32,
    /// For the journeys: where the welcome buttons' row starts, how far down the words of the
    /// pages on show reach once cut off above it, and how far they would reach uncut (points).
    pager_probe: [f32; 3],
    /// A control being pressed: where it is and how far the press feedback has risen.
    press_rect: Option<Rect>,
    press_t: f32,
}

/// The animation of one case: how focused it is (a spring, so it settles with
/// a hint of overshoot), how far its picture has faded in, and the loading
/// shimmer.
#[derive(Clone, Copy, Debug, Default)]
struct CaseAnim {
    focus: f32,
    vel: f32,
    art: f32,
    load: f32,
    /// Seconds the picture has been loading (the shimmer waits a moment so a
    /// cached picture never flashes it).
    loading_for: f32,
}

/// Seconds a picture must be loading before the shimmer shows.
const SHIMMER_DELAY: f32 = 0.15;
/// Seconds the picture takes to fade in.
const ART_FADE_SECS: f32 = 0.35;

/// A drag across the welcome pages.
#[derive(Clone, Debug)]
struct PagerDrag {
    /// Where the pages were when the drag began, and the pointer's x then (points).
    base: f32,
    origin_x: f32,
    /// How far the pointer has gone from `origin_x` (points).
    dx: f32,
    /// The pointer's recent x positions with the time of each (seconds), to tell a flick.
    samples: Vec<(f64, f32)>,
}

/// A flick is the pointer's speed over this many seconds before it lets go.
const PAGER_FLICK_WINDOW: f64 = 0.1;

impl PagerDrag {
    fn new(base: f32, origin_x: f32, now: f64, lag: f32) -> Self {
        Self {
            base,
            origin_x,
            dx: 0.0,
            samples: vec![(now - f64::from(lag), origin_x)],
        }
    }

    /// The pointer is at `x` at time `now`.
    fn note(&mut self, now: f64, x: f32) {
        self.dx = x - self.origin_x;
        self.samples.push((now, x));
        self.samples
            .retain(|(t, _)| now - t <= 2.0 * PAGER_FLICK_WINDOW);
    }

    /// Points per second to the right over the last `PAGER_FLICK_WINDOW` seconds up to `now`; a
    /// pointer that stopped before then has no speed.
    fn velocity(&self, now: f64) -> f32 {
        let recent: Vec<&(f64, f32)> = self
            .samples
            .iter()
            .filter(|(t, _)| now - t <= PAGER_FLICK_WINDOW)
            .collect();
        let (Some(first), Some(last)) = (recent.first(), recent.last()) else {
            return 0.0;
        };
        let span = (last.0 - first.0).max(1.0 / 60.0);
        ((last.1 - first.1) as f64 / span) as f32
    }
}

/// A drag this far (points), or this fraction of the span, turns the page.
const PAGER_TURN_MIN: f32 = 70.0;
const PAGER_TURN_MAX: f32 = 120.0;
const PAGER_TURN_FRACTION: f32 = 0.22;
/// A flick this fast (points per second) turns the page after a short drag.
const PAGER_FLICK: f32 = 400.0;
/// ...but only after the page has moved at least this far.
const PAGER_FLICK_MIN: f32 = 30.0;
/// How much of a drag past the first or last page shows (the pages resist).
const PAGER_RESIST: f32 = 0.3;

/// Where the pages sit (in pages from the first) after the pointer moved `dx` points from where
/// the drag began at `base`: one for one with the pointer, resisting past the ends.
fn pager_pos(base: f32, dx: f32, span: f32, count: usize) -> f32 {
    let raw = base - dx / span.max(1.0);
    let last = count.saturating_sub(1) as f32;
    if raw < 0.0 {
        raw * PAGER_RESIST
    } else if raw > last {
        last + (raw - last) * PAGER_RESIST
    } else {
        raw
    }
}

/// The page a drag let go at `pos` (pages from the first, moving at `vel` points per second to
/// the right) settles on, starting from page `at`. A short or slow drag snaps back; a drag past
/// the turn distance, or a quick flick, turns one page, and never past the first or last.
fn pager_target(pos: f32, vel: f32, at: usize, span: f32, count: usize) -> usize {
    let turn = (PAGER_TURN_FRACTION * span).clamp(PAGER_TURN_MIN, PAGER_TURN_MAX);
    let moved = (pos - at as f32) * span;
    let toward_next = -vel;
    let next = moved >= turn || (moved >= PAGER_FLICK_MIN && toward_next >= PAGER_FLICK);
    let prev = -moved >= turn || (-moved >= PAGER_FLICK_MIN && -toward_next >= PAGER_FLICK);
    if next && at + 1 < count {
        at + 1
    } else if prev && at > 0 {
        at - 1
    } else {
        at
    }
}

/// A pointer press being tracked, to tell a tap from a scroll.
#[derive(Clone, Copy, Debug)]
struct Press {
    start: Pos2,
    travel: f32,
    scrolled: bool,
    on_slider: Option<(usize, Rect)>,
}

impl HomeState {
    pub(super) fn new(first_visit: bool, explainer: bool) -> Self {
        Self {
            page: if explainer { Page::Welcome } else { Page::Home },
            explainer,
            explainer_seen: false,
            sounds: Vec::new(),
            watch: None,
            snd_confirm: false,
            snd_back: false,
            zone: Zone::Body,
            nav: 0,
            case_focus: 0,
            case_pos: 0.0,
            time: 0.0,
            countdown: Countdown::Off,
            welcome: 0,
            started: false,
            feed: HomeFeed::default(),
            dither: None,
            disc_tiles: None,
            demo_cover: None,
            contents: None,
            list_scroll: 0.0,
            kin: Momentum::default(),
            kin_page: Page::Home,
            list_view: [0.0; 4],
            controls: controls::ControlsState::default(),
            press: None,
            hits: Vec::new(),
            covers: CoverState::default(),
            first_visit,
            last_row: usize::MAX,
            closed: false,
            slots_return: None,
            cancelled_this_frame: false,
            launch: None,
            launch_sent: false,
            launch_done: false,
            pending_launch: None,
            clock: None,
            label_tex: None,
            section: None,
            compact: false,
            badge: None,
            grid_scroll: 0.0,
            grid_cols: 3,
            rail: None,
            btn_bottom: 0.0,
            anim: std::collections::HashMap::new(),
            frame_dt: 0.0,
            pad_press: [0.0; 4],
            letters: 0,
            calm_setting: false,
            calm_browser: prefers_reduced_motion(),
            local_demo: false,
            shown_page: Page::Home,
            page_age: 0.0,
            page_dir: 0.0,
            home_age: 0.0,
            last_case: 0,
            nav_hl: motion::Spring::default(),
            nav_ring: motion::Spring::default(),
            nav_vis: 0.0,
            ring_vis: 0.0,
            row_cage: motion::Spring::default(),
            pager: motion::Spring::default(),
            pager_drag: None,
            pager_span: 360.0,
            pager_last_vel: 0.0,
            pager_probe: [0.0; 3],
            press_rect: None,
            press_t: 0.0,
        }
    }

    pub(super) fn set_reduce_motion(&mut self, on: bool) {
        self.calm_setting = on;
    }

    /// Whether motion is calmed (the setting, or the browser's preference).
    pub(super) fn calm(&self) -> bool {
        self.calm_setting || self.calm_browser
    }

    /// The Square hint just left of a second action's words.
    fn square_glyph(&self, p: &egui::Painter, lay: &Layout, words: Rect, a: f32) {
        let Some(style) = self.hint_style(lay) else {
            return;
        };
        let u = lay.u;
        let w = hints::btn_width(p, u, style, Btn::Square);
        let c = Pos2::new(words.left() - 12.0 * u - w / 2.0, words.center().y);
        hints::draw_btn(p, c, u, a, style, Btn::Square, self.pad_press[3]);
    }

    /// What kind of button hints to draw: glyphs when a controller is connected, key caps
    /// beside a keyboard on a desktop, none on a touch screen.
    fn hint_style(&self, lay: &Layout) -> Option<HintStyle> {
        if self.feed.gamepads > 0 {
            Some(HintStyle::Pad)
        } else if self.feed.touch || lay.kind != LayoutKind::Desktop {
            None
        } else {
            Some(HintStyle::Keys)
        }
    }

    /// Room a row of hints at the bottom of a page takes (desktop only).
    fn hint_reserve(&self, lay: &Layout) -> f32 {
        if lay.kind == LayoutKind::Desktop && self.hint_style(lay).is_some() {
            38.0 * lay.u
        } else {
            0.0
        }
    }

    /// The hint row at the bottom of a page of the shell.
    fn draw_page_hints(&self, pc: &PageCtx<'_>, items: &[(Btn, &str)]) {
        let lay = pc.lay;
        let Some(style) = self.hint_style(lay) else {
            return;
        };
        if lay.kind != LayoutKind::Desktop {
            return;
        }
        let u = lay.u;
        let y = lay.safe.bottom() - 22.0 * u;
        hints::hint_row(
            pc.painter,
            lay.safe.left() + 48.0 * u,
            y,
            u,
            pc.appear,
            style,
            items,
            13.0 * u,
        );
    }

    /// Step one case's animation and return its picture and effects.
    fn case_fx(
        &mut self,
        ctx: &egui::Context,
        item: &CaseItem,
        focused: bool,
    ) -> (Option<egui::TextureId>, scene::CaseFx) {
        let art = item
            .cover_key
            .as_deref()
            .and_then(|k| self.covers.texture(ctx, k));
        // The demo disc has its own cover, drawn by the project: every size shows it.
        let art = art.or_else(|| {
            if !matches!(item.kind, CoverKind::Demo) {
                return None;
            }
            if self.demo_cover.is_none() {
                self.demo_cover = load_demo_cover(ctx);
            }
            self.demo_cover.as_ref().map(egui::TextureHandle::id)
        });
        let loading = item
            .cover_key
            .as_deref()
            .is_some_and(|k| self.covers.status(k) == covers::CoverStatus::Loading);
        let key = item.cover_key.clone().unwrap_or_else(|| item.title.clone());
        let dt = self.frame_dt;
        let a = self.anim.entry(key).or_default();
        // A spring toward 0 or 1: about 0.4 s to settle, a little overshoot.
        let target = if focused { 1.0 } else { 0.0 };
        let steps = ((dt / 0.01).ceil() as usize).clamp(1, 12);
        let h = dt.min(0.12) / steps as f32;
        for _ in 0..steps {
            let force = 190.0 * (target - a.focus) - 17.0 * a.vel;
            a.vel += force * h;
            a.focus += a.vel * h;
        }
        a.focus = a.focus.clamp(0.0, 1.12);
        if loading {
            a.loading_for += dt;
        } else {
            a.loading_for = 0.0;
        }
        let want_load = if a.loading_for > SHIMMER_DELAY {
            1.0
        } else {
            0.0
        };
        a.load += (want_load - a.load) * (dt * 8.0).min(1.0);
        a.art = if art.is_some() {
            (a.art + dt / ART_FADE_SECS).min(1.0)
        } else {
            0.0
        };
        let fx = scene::CaseFx {
            loading: a.load,
            art_fade: tokens::ease_out_expo(a.art),
            focus: a.focus,
            time: self.time,
            calm: self.calm(),
        };
        (art, fx)
    }

    /// Reopening the shell starts at its top: the pause list of a started
    /// game, or the home page, never a page left open last time.
    fn note_open_state(&mut self, ms: &mut MenuState) {
        if !ms.open {
            self.closed = true;
            self.started = true;
            // Once the game is going, the first-visit countdown is over.
            if matches!(self.countdown, Countdown::Armed | Countdown::Running(_)) {
                self.countdown = Countdown::Cancelled;
            }
        } else if std::mem::take(&mut self.closed) {
            self.page = Page::Home;
            self.zone = Zone::Body;
            self.list_scroll = 0.0;
            self.last_row = usize::MAX;
            self.launch = None;
            self.launch_sent = false;
            self.launch_done = false;
            self.page_age = 0.0;
            self.home_age = 0.0;
            self.page_dir = 0.0;
            if self.pausing(ms) {
                ms.select_category("Game");
                // The game that is running is the first case: the shell opens on it.
                self.case_focus = 0;
                self.case_pos = 0.0;
            }
        }
    }

    /// Advance the reveal clocks and notice a page change (which way it went along the nav).
    fn tick_motion(&mut self, ms: &MenuState, dt: f32) {
        if self.page != self.shown_page {
            let items = nav_items(self.pausing(ms));
            let at = |page: Page| items.iter().position(|i| i.page() == page.nav_page());
            self.page_dir = match (at(self.shown_page), at(self.page)) {
                (Some(from), Some(to)) if to > from => 1.0,
                (Some(from), Some(to)) if to < from => -1.0,
                _ => 0.0,
            };
            self.shown_page = self.page;
            self.page_age = 0.0;
            self.home_age = 0.0;
        } else {
            self.page_age += dt;
            self.home_age += dt;
        }
        self.tick_pager(dt);
        if self.case_focus != self.last_case {
            self.last_case = self.case_focus;
            // Another case: its words come in again, but from half way, so they never blank.
            self.home_age = 0.05;
        }
    }

    /// Move the welcome pages: with the finger while it drags, else gliding to the page in place.
    fn tick_pager(&mut self, dt: f32) {
        let count = pages::WELCOME.len();
        if self.page != Page::Welcome {
            self.pager_drag = None;
            self.pager = motion::Spring::at(self.welcome as f32);
            return;
        }
        if let Some(d) = &self.pager_drag {
            self.pager = motion::Spring::at(pager_pos(d.base, d.dx, self.pager_span, count));
        } else {
            self.pager
                .step_damped(self.welcome as f32, dt, self.calm(), 1.0);
        }
    }

    /// Show the welcome pages from the first, with nothing sliding.
    fn welcome_from_start(&mut self) {
        self.welcome = 0;
        self.pager = motion::Spring::at(0.0);
        self.pager_drag = None;
    }

    /// How far part `index` of the page is revealed, 0 to 1 (always 1 with reduced motion).
    fn reveal(&self, index: usize) -> f32 {
        if self.calm() {
            1.0
        } else {
            motion::stage(self.page_age, index)
        }
    }

    /// The same for the Home page's text, which also re-arrives when another case takes focus.
    fn home_reveal(&self, index: usize) -> f32 {
        if self.calm() {
            1.0
        } else {
            motion::stage(self.home_age, index)
        }
    }

    /// Where a part that is `s` revealed sits relative to its place: a short slide along the way
    /// the nav went (or up from below when the page did not come from a neighbour).
    fn slide(&self, s: f32, u: f32) -> Vec2 {
        let k = 1.0 - s;
        if self.page_dir == 0.0 {
            Vec2::new(0.0, k * 12.0 * u)
        } else {
            Vec2::new(self.page_dir * k * 24.0 * u, 0.0)
        }
    }

    /// Whether a started game is waiting behind the shell.
    fn pausing(&self, ms: &MenuState) -> bool {
        self.started && ms.game_loaded
    }
}

impl MenuState {
    /// Turn the home screen on, replacing the cross media bar. `first_visit`
    /// arms the demo-disc countdown; `explainer` opens on the "What is this?" pages first, and the
    /// countdown starts when they are dismissed.
    pub fn enable_home(&mut self, first_visit: bool, explainer: bool) {
        self.home = Some(Box::new(HomeState::new(first_visit, explainer)));
    }

    /// Whether the first-run explainer was dismissed since the last call: the app remembers it
    /// (native: the saved settings; the web remembers it in local storage itself).
    pub fn take_explainer_seen(&mut self) -> bool {
        self.home
            .as_mut()
            .is_some_and(|h| std::mem::take(&mut h.explainer_seen))
    }

    /// Whether the home screen is the shell.
    pub fn home_enabled(&self) -> bool {
        self.home.is_some()
    }

    /// Whether a started game waits behind the shell (the pause shell), as opposed to the home
    /// screen with nothing running. The on-screen controller belongs to a game, so it shows in the
    /// first case only.
    pub fn home_over_game(&self) -> bool {
        self.home
            .as_ref()
            .is_some_and(|h| h.pausing(self) && h.launch.is_none())
    }

    /// How much of the window the shell covers, 0 (closed) to 1 (fully up). The toolbar steps
    /// aside as this rises; always 0 without the home screen.
    pub fn shell_cover(&self) -> f32 {
        if self.home.is_some() {
            self.appear
        } else {
            0.0
        }
    }

    /// Desktop builds: the shelf always has the Demo Disc as its first case, taken from the
    /// discs the library lists (see `HomeState::local_demo_cases`).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn set_local_demo(&mut self, on: bool) {
        if let Some(h) = self.home.as_mut() {
            h.local_demo = on;
        }
    }

    /// Feed the home screen what the app knows this frame.
    pub fn set_home_feed(&mut self, feed: HomeFeed) {
        if let Some(h) = self.home.as_mut() {
            if h.countdown == Countdown::Off
                && h.first_visit
                && !h.started
                && feed.demo != DemoStatus::Absent
            {
                h.countdown = Countdown::Armed;
            }
            h.feed = feed;
        }
    }

    /// Any input outside the menu's own edges (a pad button, a stick) cancels
    /// the first-visit countdown.
    pub fn note_input(&mut self) {
        if let Some(h) = self.home.as_mut() {
            h.cancel_countdown();
        }
    }

    /// Whether a first-visit countdown is on screen (for the page probe).
    #[allow(dead_code)]
    pub fn home_probe(&self) -> (&'static str, &'static str) {
        match self.home.as_ref() {
            Some(h) => (
                h.page.name(),
                match h.countdown {
                    Countdown::Off => "off",
                    Countdown::Armed => "armed",
                    Countdown::Running(_) => "running",
                    Countdown::Cancelled => "cancelled",
                    Countdown::Done => "done",
                },
            ),
            None => ("", ""),
        }
    }

    /// Cover requests the home screen wants fetched, and results to hand back.
    pub fn covers(&mut self) -> Option<&mut CoverState> {
        self.home.as_mut().map(|h| &mut h.covers)
    }

    /// The home screen's state as JSON, for the browser journeys: the page,
    /// focus, countdown and every drawn hit area with a name.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub fn home_probe_json(&self) -> String {
        let Some(h) = self.home.as_ref() else {
            return "null".into();
        };
        let escape = |t: &str| t.replace('\\', "\\\\").replace('"', "\\\"");
        let rows = pages::visible_rows(self, h.page);
        let hits: Vec<String> = h
            .hits
            .iter()
            .map(|(r, click)| {
                let name = match click {
                    Click::Nav(item) => format!("nav:{}", item.label()),
                    Click::Logo => "logo".into(),
                    Click::Case(i) => format!("case:{i}"),
                    Click::Primary => "primary".into(),
                    Click::Act(action) => format!("act:{action:?}"),
                    Click::Restart => "restart".into(),
                    Click::SizeToggle => "size".into(),
                    Click::TabStep(n) => format!("tab-step:{n}"),
                    Click::Rail => "rail".into(),
                    Click::Close => "close".into(),
                    Click::Burn => "burn".into(),
                    Click::Link(url) => format!("link:{url}"),
                    Click::Welcome => "welcome".into(),
                    Click::WelcomeStep(n) => format!("welcome-step:{n}"),
                    Click::Row(i) => {
                        format!("row:{}", rows.get(*i).map_or("", |r| r.label.as_str()))
                    }
                    Click::RowBurn(i) => {
                        format!("burn:{}", rows.get(*i).map_or("", |r| r.label.as_str()))
                    }
                    Click::Slider(i, _) => {
                        format!("slider:{}", rows.get(*i).map_or("", |r| r.label.as_str()))
                    }
                    Click::Ctl(item) => item.probe_name(),
                    Click::Dismiss => "dismiss".into(),
                };
                format!(
                    "[\"{}\",{:.0},{:.0},{:.0},{:.0}]",
                    escape(&name),
                    r.left(),
                    r.top(),
                    r.right(),
                    r.bottom()
                )
            })
            .collect();
        let cases: Vec<String> = h
            .cases(self)
            .iter()
            .take(16)
            .map(|c| {
                let key = c.cover_key.clone().unwrap_or_else(|| c.title.clone());
                let anim = h.anim.get(&key).copied().unwrap_or_default();
                let cover = match c.cover_key.as_deref().map(|k| h.covers.status(k)) {
                    Some(covers::CoverStatus::Loading) => "loading",
                    Some(covers::CoverStatus::Ready) => "ready",
                    Some(covers::CoverStatus::Missing) => "missing",
                    Some(covers::CoverStatus::Idle) => "idle",
                    None => "none",
                };
                format!(
                    "[\"{}\",\"{}\",\"{}\",{:.2},{:.2},{:.2}]",
                    escape(&c.title),
                    escape(&c.line),
                    cover,
                    anim.focus,
                    anim.load,
                    anim.art
                )
            })
            .collect();
        let left = match h.countdown {
            Countdown::Running(l) => l,
            _ => -1.0,
        };
        format!(
            "{{\"page\":\"{}\",\"zone\":\"{}\",\"countdown\":\"{}\",\"left\":{:.2},\"case\":{},\"nav\":{},\"started\":{},\"in_game\":{},\"launch\":{:.2},\"cover\":{:.2},\"nav_items\":{},\"first_visit\":{},\"covers\":{},\"calm\":{},\"compact\":{},\"nav_hl\":{:.3},\"nav_vis\":{:.2},\"page_age\":{:.2},\"keyboard\":{},\"framed\":{},\"press\":{:.2},\"controls\":{},\"list\":[{:.1},{:.1},{:.1},{:.1},{}],\"contents\":{},\"explainer\":{},\"welcome\":{},\"welcome_pos\":{:.3},\"welcome_drag\":{},\"welcome_vel\":{:.0},\"welcome_rows\":[{:.1},{:.1},{:.1}],\"cases\":[{}],\"hits\":[{}]}}",
            h.page.name(),
            if h.zone == Zone::Nav { "nav" } else { "body" },
            self.home_probe().1,
            left,
            h.case_focus,
            h.nav,
            h.started,
            h.pausing(self),
            h.launch.as_ref().map_or(-1.0, |l| l.t),
            self.appear,
            nav_items(h.pausing(self)).len(),
            h.first_visit,
            h.covers.loaded(),
            h.calm(),
            h.compact,
            h.nav_hl.pos,
            h.nav_vis,
            h.page_age.min(99.0),
            h.feed.keyboard,
            h.feed.framed,
            h.press_t,
            h.controls.probe_json(self),
            h.list_view[0],
            h.list_view[1],
            h.list_view[2],
            h.list_view[3],
            i32::from(h.kin.is_active()),
            h.contents.map_or("null".to_string(), |c| format!(
                "[{:.1},{:.1},{:.1},{:.1},{},{}]",
                c[0], c[1], c[2], c[3], c[4] as i32, c[5] as i32
            )),
            h.explainer,
            h.welcome,
            h.pager.pos,
            h.pager_drag.is_some(),
            h.pager_last_vel,
            h.pager_probe[0],
            h.pager_probe[1],
            h.pager_probe[2],
            cases.join(","),
            hits.join(",")
        )
    }

    /// Put the home screen in a given state, for headless previews.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn home_preview(
        &mut self,
        page: &str,
        case: usize,
        demo: bool,
        countdown: bool,
        launch: Option<f32>,
    ) {
        let Some(h) = self.home.as_mut() else {
            return;
        };
        h.started = launch.is_none() && (page == "game" || page == "resume");
        if h.started {
            h.feed.current = Some((
                crate::app::DEMO_DISC_ID.to_string(),
                "PSoXide Demo Disc".to_string(),
            ));
        }
        h.page = match page {
            "game" => Page::Game,
            "controls" | "controls-keyboard" | "controls-touch" => Page::Controls,
            "slots" => Page::Slots,
            "settings" => Page::Settings,
            "welcome" => Page::Welcome,
            _ => Page::Home,
        };
        h.controls.show_tab(match page {
            "controls-keyboard" => 1,
            "controls-touch" => 2,
            _ => 0,
        });
        h.case_focus = case;
        h.case_pos = case as f32;
        h.feed.keyboard = true;
        if demo {
            h.feed.demo = DemoStatus::Ready;
        }
        if countdown {
            h.first_visit = true;
            h.countdown = Countdown::Running(6.0);
        }
        if let Some(t) = launch {
            h.launch = Some(launch::Launch {
                t,
                skipping: false,
                action: MenuAction::StartDemoDisc,
                kind: CoverKind::Demo,
                title: "PSoXide Demo Disc".into(),
                cover_key: None,
                frozen: true,
            });
        }
    }

    /// Show a section of the shell by name; false when it does not exist.
    pub fn home_goto(&mut self, page: &str) -> bool {
        if self.home.is_none() {
            return false;
        }
        let item = match page {
            "Home" => NavItem::Home,
            "Game" => NavItem::Game,
            "Controls" => NavItem::Controls,
            "Settings" => NavItem::Settings,
            _ => return false,
        };
        let mut shell = self.home.take().expect("home");
        // Asked for by name: this is where the shell opens, not the top.
        shell.closed = false;
        shell.open_nav(self, item);
        self.home = Some(shell);
        true
    }

    /// Preview with a controller connected (the hints show glyphs).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn home_preview_pad(&mut self, on: bool) {
        if let Some(h) = self.home.as_mut() {
            h.feed.gamepads = usize::from(on);
        }
    }

    /// Preview the compact grid.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn home_preview_compact(&mut self, on: bool) {
        if let Some(h) = self.home.as_mut() {
            h.compact = on;
        }
    }

    /// Where the shell wants the slot sheet drawn this frame.
    pub fn home_section(&self) -> Option<(&'static str, Rect)> {
        let h = self.home.as_ref()?;
        let rect = h.section?;
        match h.page {
            Page::Slots if self.open => Some((h.page.name(), rect)),
            _ => None,
        }
    }

    /// How far the section's content has arrived, 0 to 1: the page's parts come in one after
    /// another, and the egui content of the slot sheet fades in with the rest.
    pub fn home_section_reveal(&self) -> f32 {
        self.home.as_ref().map_or(1.0, |h| h.reveal(1))
    }

    /// Whether Escape, the pad's Select+Start and the touch Menu button mean
    /// "back" right now: on the home screen before anything started (they never
    /// start the disc) and on any section other than Home (they never leave the
    /// shell from there).
    pub fn home_blocks_ps_button(&self) -> bool {
        self.open
            && self
                .home
                .as_ref()
                .is_some_and(|h| !h.started || h.page != Page::Home)
    }

    /// Whether the Controls page is showing.
    pub(super) fn home_page_is_controls(&self) -> bool {
        self.open && self.home.as_ref().is_some_and(|h| h.page == Page::Controls)
    }

    /// Whether the Controls page wants this frame's devices from the app.
    pub fn home_wants_devices(&self) -> bool {
        self.home.as_ref().is_some_and(|h| h.wants_devices())
    }

    /// Hand the Controls page the connected devices.
    pub(crate) fn home_set_devices(&mut self, devices: Vec<crate::input::InputDeviceInfo>) {
        if let Some(h) = self.home.as_mut() {
            h.set_controls_devices(devices);
        }
    }

    /// What the Controls page asked to change on a device.
    pub(crate) fn home_take_device_commands(&mut self) -> Vec<DeviceCommand> {
        self.home
            .as_mut()
            .map(|h| h.take_controls_commands())
            .unwrap_or_default()
    }

    /// Whether the welcome pages are showing.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(super) fn home_page_is_welcome(&self) -> bool {
        self.home.as_ref().is_some_and(|h| h.page == Page::Welcome)
    }

    /// Open the welcome pages (the Settings "About" row).
    pub(super) fn home_show_welcome(&mut self) {
        if let Some(h) = self.home.as_mut() {
            h.page = Page::Welcome;
            h.welcome_from_start();
        }
    }

    pub(super) fn home_update(&mut self, input: &MenuInput) -> Option<MenuAction> {
        let mut h = self.home.take()?;
        let out = h.update(self, input);
        self.home = Some(h);
        out
    }

    pub(super) fn home_draw(&mut self, ctx: &egui::Context, dt: f32, warning: Option<&str>) {
        let Some(mut h) = self.home.take() else {
            return;
        };
        h.draw(self, ctx, dt, warning);
        self.home = Some(h);
    }
}

impl HomeState {
    /// The first-run explainer is on screen.
    fn explainer_up(&self) -> bool {
        self.explainer && self.page == Page::Welcome
    }

    fn cancel_countdown(&mut self) {
        // Reading the pages is not a choice: the countdown waits behind them.
        if self.explainer_up() {
            return;
        }
        match self.countdown {
            Countdown::Armed | Countdown::Running(_) => {
                self.countdown = Countdown::Cancelled;
                self.cancelled_this_frame = true;
                remember_visit();
            }
            _ => {}
        }
    }

    /// Desktop: the Demo Disc case and the other builds of it, from the discs the library lists.
    ///
    /// The newest public release is the Demo Disc (the hero case, whatever else is around). Every
    /// other build in the folder (an older release, the private builds, the diagnostic ones) is a
    /// case of its own right after the shelf's first cases, newest first. Only discs that exist
    /// reach this list, so a cue sheet whose images are gone never shows up. With no public
    /// release at all the hero stays and points at the download.
    fn local_demo_cases(&self, ms: &MenuState) -> LocalDemo {
        use psoxide_settings::demo_disc::DemoDisc;
        let mut found: Vec<(DemoDisc, &LibraryItem)> = ms
            .games
            .iter()
            .filter_map(|g| DemoDisc::parse(&g.title).map(|d| (d, g)))
            .collect();
        found.sort_by(|a, b| a.0.newest_first(&b.0));
        let claimed: Vec<String> = found.iter().map(|(_, g)| g.id.clone()).collect();
        let hero_at = found.iter().position(|(d, _)| d.is_public());
        let hero = match hero_at.map(|i| found.remove(i)) {
            Some((disc, item)) => CaseItem {
                kind: CoverKind::Demo,
                title: "PSoXide Demo Disc".into(),
                line: match disc.version {
                    Some((major, minor)) => format!("Seven games and demos · v{major}.{minor}"),
                    None => "Seven games and demos".into(),
                },
                action: MenuAction::LaunchGame(item.id.clone()),
                button: "Start disc",
                cover_key: Some(demo_cover_key("hero")),
                restart: None,
                burn: None,
                launches: true,
                shelf: false,
                badge: None,
            },
            None => CaseItem {
                kind: CoverKind::Demo,
                title: "PSoXide Demo Disc".into(),
                line: "Not on this computer yet. Put it in your games folder".into(),
                action: MenuAction::OpenUrl(DEMO_DISC_URL.into()),
                button: "Get the disc",
                cover_key: Some(demo_cover_key("hero")),
                restart: None,
                burn: None,
                launches: false,
                shelf: false,
                badge: None,
            },
        };
        let others = found
            .iter()
            .map(|(disc, item)| CaseItem {
                kind: CoverKind::Plain,
                title: disc.short_title().unwrap_or_else(|| item.title.clone()),
                line: disc.describe(),
                action: MenuAction::LaunchGame(item.id.clone()),
                button: "Play",
                cover_key: Some(demo_cover_key(&item.id)),
                restart: None,
                burn: None,
                launches: true,
                shelf: false,
                badge: None,
            })
            .collect();
        LocalDemo {
            hero,
            others,
            claimed,
        }
    }

    /// The shelf: the game you left, the demo disc, your own discs.
    fn cases(&self, ms: &MenuState) -> Vec<CaseItem> {
        let mut out = Vec::new();
        let current = self.feed.current.as_ref().filter(|_| self.pausing(ms));
        let demo_id = crate::app::DEMO_DISC_ID;
        let local = self.local_demo.then(|| self.local_demo_cases(ms));
        let has_demo = local.is_none()
            && (ms.homebrew.iter().any(|e| e.id == demo_id)
                || self.feed.demo != DemoStatus::Absent);
        if let Some((id, title)) = current {
            let is_demo = id == demo_id;
            out.push(CaseItem {
                kind: if is_demo {
                    CoverKind::Demo
                } else {
                    CoverKind::Plain
                },
                title: if is_demo {
                    "PSoXide Demo Disc".into()
                } else {
                    title.clone()
                },
                line: "Pick up where you left off".into(),
                action: MenuAction::ResumeGame,
                button: "Resume",
                cover_key: if local.is_some()
                    && psoxide_settings::demo_disc::DemoDisc::parse(title).is_some()
                {
                    Some(demo_cover_key("current"))
                } else {
                    (!is_demo).then(|| id.clone())
                },
                restart: Some(MenuAction::LaunchGame(id.clone())),
                burn: None,
                launches: false,
                shelf: false,
                badge: Some("Running"),
            });
        }
        if let Some(local) = local.as_ref() {
            out.push(local.hero.clone());
        }
        if has_demo && current.is_none_or(|(id, _)| id != demo_id) {
            out.push(CaseItem {
                kind: CoverKind::Demo,
                title: "PSoXide Demo Disc".into(),
                line: "Seven games and demos".into(),
                action: MenuAction::StartDemoDisc,
                button: "Start disc",
                cover_key: None,
                restart: None,
                burn: None,
                launches: true,
                shelf: false,
                badge: None,
            });
        }
        out.push(CaseItem {
            kind: CoverKind::Ghost,
            title: "Open a games folder".into(),
            line: "Choose a folder of disc images or programs".into(),
            action: MenuAction::ChooseGamesPath,
            button: "Choose folder",
            cover_key: None,
            restart: None,
            burn: None,
            launches: false,
            shelf: false,
            badge: None,
        });
        // A folder connected on an earlier visit is one click from being open again.
        #[cfg(target_arch = "wasm32")]
        if ms.saved_folder {
            out.push(CaseItem {
                kind: CoverKind::Ghost,
                title: "Reopen your games folder".into(),
                line: "The folder you opened last time".into(),
                action: MenuAction::Reconnect,
                button: "Reopen folder",
                cover_key: None,
                restart: None,
                burn: None,
                launches: false,
                shelf: false,
                badge: None,
            });
        }
        let mut shown: std::collections::HashSet<&str> = std::collections::HashSet::new();
        if let Some((id, _)) = current {
            shown.insert(id.as_str());
        }
        if let Some(local) = local.as_ref() {
            // The Demo Disc builds have their own cases: never again as a plain game or a recent.
            shown.extend(local.claimed.iter().map(String::as_str));
        }
        // Real recents first: discs this browser or this machine really played.
        for item in ms.recent_discs.iter().take(5) {
            if shown.insert(item.id.as_str()) {
                out.push(CaseItem {
                    kind: CoverKind::Plain,
                    title: item.title.clone(),
                    line: "Played recently".into(),
                    action: MenuAction::LaunchGame(item.id.clone()),
                    button: "Play",
                    cover_key: Some(item.id.clone()),
                    restart: None,
                    burn: None,
                    launches: true,
                    shelf: false,
                    badge: None,
                });
            }
        }
        if let Some(local) = local.as_ref() {
            out.extend(local.others.iter().cloned());
        }
        // Then the rest of the shelf: your games folder and the SDK examples.
        let mut rest: Vec<&LibraryItem> = ms.games.iter().collect();
        rest.sort_by_key(|g| g.title.to_lowercase());
        for item in rest {
            if shown.insert(item.id.as_str()) {
                out.push(CaseItem {
                    kind: CoverKind::Plain,
                    title: item.title.clone(),
                    line: if item.subtitle.is_empty() {
                        "From your games folder".into()
                    } else {
                        item.subtitle.clone()
                    },
                    action: MenuAction::LaunchGame(item.id.clone()),
                    button: "Play",
                    cover_key: Some(item.id.clone()),
                    restart: None,
                    burn: (item.launchable && item.burnable)
                        .then(|| MenuAction::OpenBurnMenu(item.id.clone())),
                    launches: true,
                    shelf: true,
                    badge: None,
                });
            }
        }
        for item in &ms.homebrew {
            if item.id != demo_id && shown.insert(item.id.as_str()) {
                out.push(CaseItem {
                    kind: CoverKind::Plain,
                    title: item.title.clone(),
                    line: if item.launchable {
                        item.subtitle.clone()
                    } else {
                        "Not built yet".into()
                    },
                    action: if item.launchable {
                        MenuAction::LaunchGame(item.id.clone())
                    } else {
                        MenuAction::BuildExamples
                    },
                    button: if item.launchable {
                        "Play"
                    } else {
                        "Build examples"
                    },
                    // The SDK's own examples have no box art to look up.
                    cover_key: None,
                    restart: None,
                    burn: (item.launchable && item.burnable)
                        .then(|| MenuAction::OpenBurnMenu(item.id.clone())),
                    launches: item.launchable,
                    shelf: false,
                    badge: None,
                });
            }
        }
        out
    }

    /// Run a case's action: a disc plays the launch sequence first.
    fn activate(
        &mut self,
        case: &CaseItem,
        action: &MenuAction,
        launches: bool,
    ) -> Option<MenuAction> {
        if launches && self.launch.is_none() {
            self.launch = Some(launch::Launch {
                t: 0.0,
                skipping: false,
                action: action.clone(),
                kind: case.kind,
                title: case.title.clone(),
                cover_key: case.cover_key.clone(),
                frozen: false,
            });
            self.launch_sent = false;
            self.launch_done = false;
            // A disc that has to load starts loading with the sequence. The demo disc waiting
            // behind the shell is already booted: it resumes when the sequence ends.
            if !(matches!(action, MenuAction::StartDemoDisc) && self.feed.demo == DemoStatus::Ready)
            {
                self.launch_sent = true;
                self.pending_launch = Some(action.clone());
            }
            return None;
        }
        Some(action.clone())
    }

    fn update(&mut self, ms: &mut MenuState, input: &MenuInput) -> Option<MenuAction> {
        if let Some(action) = ms.pending_pointer_action.take() {
            return Some(action);
        }
        let playing = self.launch.is_some() && !self.launch_done;
        // While the sequence plays, Escape and the other menu toggles skip it: the shell is not
        // reopened under a game that is already booting.
        if input.toggle_open && !playing {
            ms.open = !ms.open;
        }
        self.note_open_state(ms);
        if self.take_countdown_start() {
            if let Some(case) = self
                .cases(ms)
                .into_iter()
                .find(|c| c.action == MenuAction::StartDemoDisc)
            {
                let action = case.action.clone();
                return self.activate(&case, &action, true);
            }
        }
        let any = input.up
            || input.down
            || input.left
            || input.right
            || input.confirm
            || input.back
            || input.tab_prev
            || input.tab_next
            || input.options
            || input.secondary
            || input.letter_prev
            || input.letter_next;
        self.snd_confirm |= input.confirm;
        self.snd_back |= input.back;
        for (i, pressed) in [
            input.tab_prev,
            input.tab_next,
            input.options,
            input.secondary,
        ]
        .into_iter()
        .enumerate()
        {
            if pressed {
                self.pad_press[i] = 1.0;
            }
        }
        if playing {
            // Any input skips the sequence to the boot (not the press that began it).
            if any || input.toggle_open {
                self.skip_launch();
            }
            return None;
        }
        if !ms.open {
            return None;
        }
        if any {
            // The press that cancels the countdown only cancels it: it does not
            // also press Start disc.
            let counting = !self.explainer_up()
                && (matches!(self.countdown, Countdown::Armed | Countdown::Running(_))
                    || self.cancelled_this_frame);
            self.cancel_countdown();
            self.cancelled_this_frame = false;
            if counting {
                return None;
            }
        }
        self.cancelled_this_frame = false;
        if ms.about_open {
            ms.about_open = false;
        }
        // After the sequence the shell holds on the spinning disc while the game loads:
        // any input gives up and goes back to Home.
        if self.launch.is_some() {
            if any {
                self.launch = None;
                self.launch_sent = false;
                self.launch_done = false;
            }
            return None;
        }
        if (input.tab_prev || input.tab_next) && self.page != Page::Welcome {
            self.cycle_tab(ms, if input.tab_next { 1 } else { -1 });
            return None;
        }
        match self.page {
            Page::Welcome => self.update_welcome(input),
            Page::Home => self.update_home(ms, input),
            Page::Game | Page::Settings => self.update_list(ms, input),
            Page::Controls => self.update_controls(ms, input),
            Page::Slots => self.update_slots(ms, input),
        }
    }

    /// L1/R1 and friends: the next section of the nav, wrapping.
    fn cycle_tab(&mut self, ms: &mut MenuState, step: i32) {
        let items = nav_items(self.pausing(ms));
        let at = items
            .iter()
            .position(|i| i.page() == self.page.nav_page())
            .unwrap_or(0) as i32;
        let next = (at + step).rem_euclid(items.len() as i32) as usize;
        self.nav = next;
        self.open_nav(ms, items[next]);
    }

    fn update_welcome(&mut self, input: &MenuInput) -> Option<MenuAction> {
        let last = self.welcome + 1 >= pages::WELCOME.len();
        if input.confirm && last {
            self.page = Page::Home;
        } else if input.right || input.confirm {
            // Right on the last page goes nowhere: only Done (or Skip, or Back) closes the pages.
            if !last {
                self.welcome += 1;
            }
        }
        if input.left && self.welcome > 0 {
            self.welcome -= 1;
        }
        if input.back {
            self.page = Page::Home;
        }
        None
    }

    /// Left, right and confirm on the nav row.
    fn update_nav(&mut self, ms: &mut MenuState, input: &MenuInput) {
        let items = nav_items(self.pausing(ms));
        self.nav = self.nav.min(items.len() - 1);
        if input.left && self.nav > 0 {
            self.nav -= 1;
        }
        if input.right && self.nav + 1 < items.len() {
            self.nav += 1;
        }
        if input.down || input.back {
            self.zone = Zone::Body;
        }
        if input.confirm {
            self.open_nav(ms, items[self.nav]);
        }
    }

    fn update_home(&mut self, ms: &mut MenuState, input: &MenuInput) -> Option<MenuAction> {
        if self.zone == Zone::Nav {
            self.update_nav(ms, input);
            return None;
        }
        let cases = self.cases(ms);
        let last = cases.len().saturating_sub(1);
        if input.options {
            self.compact = !self.compact;
            return None;
        }
        if input.secondary {
            // The second action of the focused case, whichever it has.
            let click = cases.get(self.case_focus).and_then(|c| {
                if c.restart.is_some() {
                    Some(Click::Restart)
                } else if c.burn.is_some() {
                    Some(Click::Burn)
                } else {
                    None
                }
            });
            return click.and_then(|click| self.apply_click(ms, click));
        }
        if input.letter_prev || input.letter_next {
            self.jump_letter(&cases, if input.letter_next { 1 } else { -1 });
            return None;
        }
        let cols = if self.compact {
            self.grid_cols.max(1)
        } else {
            1
        };
        if input.left && self.case_focus > 0 {
            self.case_focus -= 1;
        }
        if input.right && self.case_focus < last {
            self.case_focus += 1;
        }
        if self.compact {
            if input.down && self.case_focus + cols <= last {
                self.case_focus += cols;
            }
            if input.up && self.case_focus >= cols {
                self.case_focus -= cols;
                return None;
            }
        }
        if input.up {
            self.zone = Zone::Nav;
            self.nav = nav_items(self.pausing(ms))
                .iter()
                .position(|i| i.page() == Page::Home)
                .unwrap_or(0);
        }
        if input.confirm {
            if let Some(case) = cases.get(self.case_focus) {
                let action = case.action.clone();
                return self.activate(case, &action, case.launches);
            }
        }
        if input.back {
            if self.pausing(ms) {
                // Back from a game's shell resumes it.
                ms.open = false;
            }
        }
        None
    }

    /// Show a section of the shell.
    fn open_nav(&mut self, ms: &mut MenuState, item: NavItem) {
        if item == NavItem::Controls && self.page != Page::Controls {
            self.controls.reset();
        }
        if item != NavItem::Controls {
            ms.controls_capture = None;
        }
        self.page = item.page();
        match item {
            NavItem::Game => ms.select_category("Game"),
            NavItem::Settings => ms.select_category("Settings"),
            _ => {}
        }
        self.zone = Zone::Body;
        self.list_scroll = 0.0;
        self.last_row = usize::MAX;
    }

    /// Game and Settings: rows of the matching category.
    fn update_list(&mut self, ms: &mut MenuState, input: &MenuInput) -> Option<MenuAction> {
        let name = if self.page == Page::Settings {
            "Settings"
        } else {
            "Game"
        };
        if ms.categories[ms.category_index].name != name {
            ms.select_category(name);
        }
        if self.zone == Zone::Nav {
            self.update_nav(ms, input);
            return None;
        }
        let rows = pages::visible_rows(ms, self.page);
        let count = rows.len();
        if count > 0 {
            if input.up {
                if ms.item_index == 0 {
                    self.zone = Zone::Nav;
                    self.nav = nav_items(self.pausing(ms))
                        .iter()
                        .position(|i| i.page() == self.page.nav_page())
                        .unwrap_or(0);
                } else {
                    ms.item_index -= 1;
                }
            }
            if input.down && ms.item_index + 1 < count {
                ms.item_index += 1;
            }
        }
        let current = rows.get(ms.item_index).map(|r| r.action.clone());
        if let Some(action) = current {
            if self.page == Page::Settings {
                if let Some(step) = pages::step_action(ms, &action, input) {
                    return Some(step);
                }
            } else if input.confirm {
                return Some(action);
            }
        }
        if input.back {
            self.page = Page::Home;
        }
        None
    }

    fn draw(&mut self, ms: &mut MenuState, ctx: &egui::Context, dt: f32, warning: Option<&str>) {
        ms.about_open = false;
        // Elapsed time, not the frame step: the shell clamps that to 0.1 s for the emulator, which
        // would stretch the sequence on a page that gets a few frames a second.
        let now = ctx.input(|i| i.time);
        let dt = self
            .clock
            .map_or(dt, |before| (now - before) as f32)
            .clamp(0.0, 1.5);
        self.clock = Some(now);
        self.frame_dt = dt;
        for v in &mut self.pad_press {
            *v = (*v - dt * 4.5).max(0.0);
        }
        // The sequence keeps the shell up while the game boots behind it.
        let playing = self.launch.is_some() && !self.launch_done;
        let target = if ms.open || playing { 1.0 } else { 0.0 };
        let k = (super::FADE_SPEED * dt).min(0.5);
        ms.appear += (target - ms.appear) * k;
        if (ms.appear - target).abs() < 0.01 {
            ms.appear = target;
        }
        self.note_open_state(ms);
        if self.kin_page != self.page || !ms.open {
            self.kin.stop();
            self.kin_page = self.page;
        }
        if ms.appear <= 0.0 {
            self.hits.clear();
            return;
        }
        ctx.request_repaint();
        self.time += dt;
        self.tick_motion(ms, dt);
        self.tick_countdown(dt);
        // Any key, click or touch before the user chose anything cancels.
        let activity = ctx.input(|i| {
            i.pointer.any_pressed()
                || i.events.iter().any(|e| {
                    matches!(
                        e,
                        egui::Event::Key { pressed: true, .. } | egui::Event::Touch { .. }
                    )
                })
        });
        if activity {
            self.cancel_countdown();
            // Any input skips the launch to the boot (not the press that began it).
            self.skip_launch();
        }
        let appear = ms.appear;
        let full = ctx.screen_rect();
        let layer = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Middle,
            egui::Id::new("menu"),
        ));
        self.hits.clear();
        let lay = Layout::new(full, self.feed.safe);
        let in_game = self.pausing(ms) && self.launch.is_none();
        self.dither.get_or_insert_with(|| {
            ctx.load_texture(
                "home-dither",
                scene::dither_image(),
                egui::TextureOptions {
                    magnification: egui::TextureFilter::Nearest,
                    minification: egui::TextureFilter::Nearest,
                    wrap_mode: egui::TextureWrapMode::Repeat,
                    mipmap_mode: None,
                },
            )
        });
        let dither = self.dither.as_ref().map(egui::TextureHandle::id);
        // The same shell before and during a game: over the void, or over the
        // game itself dimmed down.
        if in_game {
            // The game is still there, but dark enough that the shell reads against it: its
            // picture must never compete with the words (the old 70% let a game's text show
            // through the title).
            let scrim = f32::from(ms.backdrop_pct.max(88)) * 2.55 * 0.95;
            layer.rect_filled(full, 0.0, Color32::from_black_alpha((appear * scrim) as u8));
        } else {
            layer.rect_filled(full, 0.0, fade(scene::VOID, appear));
        }
        let page_dim = if self.page == Page::Home { 1.0 } else { 0.45 };
        scene::draw_backdrop(
            &layer,
            dither,
            scene::Backdrop {
                rect: full,
                horizon: lay.horizon,
                alpha: appear * page_dim * if in_game { 0.7 } else { 1.0 },
                time: self.time,
            },
        );
        let mut pc = PageCtx {
            painter: &layer,
            lay: &lay,
            appear,
        };
        self.section = None;
        if self.label_tex.is_none() {
            self.label_tex = decode_label(ctx);
        }
        self.tick_launch(dt);
        if self.launch.is_some() {
            self.draw_launch(ms, ctx, &mut pc);
        } else {
            match self.page {
                Page::Home => self.draw_home(ms, ctx, &mut pc),
                Page::Welcome => self.draw_welcome(ms, &mut pc),
                Page::Controls => controls::draw(self, ms, ctx, &mut pc),
                Page::Slots => self.draw_section_frame(ms, &mut pc),
                Page::Game | Page::Settings => self.draw_list_page(ms, ctx, &mut pc),
            }
        }
        if self.launch.is_none() {
            self.draw_chrome(ms, &mut pc, warning);
            self.draw_badge(&pc, dt);
        }
        self.handle_pointer(ms, ctx);
        self.draw_press_feedback(ctx, dt);
        self.emit_sounds(ms);
    }

    /// Ask for the sound of whatever changed this frame: the shell or a sheet opening or closing,
    /// a page, the cursor, a value, a confirm or a back. One sound per frame at most.
    fn emit_sounds(&mut self, ms: &MenuState) {
        let list_page = matches!(self.page, Page::Game | Page::Settings);
        let (row_value, row_plain) = if list_page {
            pages::visible_rows(ms, self.page)
                .get(ms.item_index)
                .map_or((String::new(), true), |r| {
                    (r.value.clone(), pages::is_plain(r))
                })
        } else {
            (String::new(), true)
        };
        let now = SoundWatch {
            open: ms.open,
            page: self.page,
            case: self.case_focus,
            nav: (self.zone == Zone::Nav).then_some(self.nav),
            row: ms.item_index,
            row_value,
            welcome: self.welcome,
            slot: ms.slots.sel,
        };
        let confirmed = std::mem::take(&mut self.snd_confirm);
        let backed = std::mem::take(&mut self.snd_back);
        let Some(before) = self.watch.replace(now.clone()) else {
            return;
        };
        let sound = if !before.open && now.open {
            Some(UiSound::Open)
        } else if before.open && !now.open {
            Some(UiSound::Close)
        } else if !now.open {
            None
        } else if backed {
            Some(UiSound::Back)
        } else if before.page != now.page {
            Some(match (before.page, now.page) {
                (_, Page::Slots | Page::Welcome) => UiSound::Open,
                (Page::Slots | Page::Welcome, _) => UiSound::Close,
                _ => UiSound::Tab,
            })
        } else if list_page && before.row == now.row && before.row_value != now.row_value {
            // A value changed under the cursor: a switch, a step or just a new word.
            Some(match now.row_value.as_str() {
                "On" => UiSound::On,
                "Off" => UiSound::Off,
                _ if row_plain => UiSound::Confirm,
                _ => UiSound::Step,
            })
        } else if confirmed && !(list_page && !row_plain) {
            Some(UiSound::Confirm)
        } else if before.case != now.case
            || before.nav != now.nav
            || before.row != now.row
            || before.welcome != now.welcome
            || before.slot != now.slot
        {
            Some(UiSound::Move)
        } else {
            None
        };
        if let Some(sound) = sound {
            self.sounds.push(sound);
        }
    }

    /// The control under a finger or the mouse button answers at once: a soft glow rises under it
    /// while it is held and drains when it is let go. One rule for every control; things that
    /// would look wrong lit (the cases, the sliders, a scroll) are left out.
    fn draw_press_feedback(&mut self, ctx: &egui::Context, dt: f32) {
        let held = self
            .press
            .filter(|p| !p.scrolled && p.on_slider.is_none() && p.travel < 10.0);
        let full = ctx.screen_rect();
        let target = held.and_then(|press| {
            self.hits
                .iter()
                .rev()
                .find(|(r, _)| r.contains(press.start))
                .filter(|(r, click)| {
                    r.width() * r.height() < full.width() * full.height() * 0.25
                        && !matches!(
                            click,
                            Click::Case(_) | Click::Slider(..) | Click::Rail | Click::Dismiss
                        )
                })
                .map(|(r, _)| *r)
        });
        let pressed = ctx.input(|i| i.pointer.primary_down());
        match target {
            Some(rect) if pressed => {
                if self.press_rect != Some(rect) {
                    self.press_t = 0.0;
                }
                self.press_rect = Some(rect);
                self.press_t = (self.press_t + dt / 0.07).min(1.0);
            }
            _ => self.press_t = (self.press_t - dt / 0.20).max(0.0),
        }
        if self.calm() && target.is_none() {
            self.press_t = 0.0;
        }
        let Some(rect) = self.press_rect.filter(|_| self.press_t > 0.0) else {
            return;
        };
        let k = self.press_t;
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Middle,
            egui::Id::new("menu"),
        ));
        let round = (rect.height() / 2.0).min(14.0);
        painter.rect_filled(rect, round, fade(tokens::FOCUS, 0.14 * k));
        painter.rect_stroke(
            rect,
            round,
            Stroke::new(1.0, fade(tokens::FOCUS, 0.38 * k)),
            egui::StrokeKind::Inside,
        );
        ctx.request_repaint();
    }

    fn tick_countdown(&mut self, dt: f32) {
        if self.explainer && self.page != Page::Welcome {
            // Dismissed, by any way out of the pages: never shown by itself again.
            self.explainer = false;
            self.explainer_seen = true;
            remember_explainer();
        }
        self.case_pos += (self.case_focus as f32 - self.case_pos) * (1.0 - (-9.0 * dt).exp());
        if (self.case_pos - self.case_focus as f32).abs() < 0.002 {
            self.case_pos = self.case_focus as f32;
        }
        match self.countdown {
            Countdown::Armed if self.feed.demo == DemoStatus::Ready && !self.explainer_up() => {
                self.countdown = Countdown::Running(countdown_secs());
            }
            Countdown::Running(left) => {
                let left = left - dt;
                if left <= 0.0 {
                    self.countdown = Countdown::Done;
                    remember_visit();
                } else {
                    self.countdown = Countdown::Running(left);
                }
            }
            _ => {}
        }
    }

    /// The countdown ran out: the shell asks the app to start the disc.
    pub(super) fn take_countdown_start(&mut self) -> bool {
        if self.countdown == Countdown::Done && !self.started {
            // One shot: after this the visit is over.
            self.countdown = Countdown::Cancelled;
            true
        } else {
            false
        }
    }
}

/// The Demo Disc and its sibling builds on the desktop shelf.
struct LocalDemo {
    /// The first case: the newest public release (or the way to get it).
    hero: CaseItem,
    /// Every other build, newest first.
    others: Vec<CaseItem>,
    /// Launch ids of all the demo discs found, for keeping them off the alphabetical shelf.
    claimed: Vec<String>,
}

/// The cover key of a demo disc case: each case has its own, so each keeps its own motion, while
/// all of them draw the same shipped picture.
fn demo_cover_key(case: &str) -> String {
    format!("{}:{case}", covers::DEMO_COVER_PREFIX)
}

#[derive(Clone)]
struct CaseItem {
    kind: CoverKind,
    title: String,
    line: String,
    action: MenuAction,
    button: &'static str,
    cover_key: Option<String>,
    /// A second, quiet action: start the game over.
    restart: Option<MenuAction>,
    burn: Option<MenuAction>,
    /// Whether choosing it plays the launch sequence.
    launches: bool,
    /// Part of the sorted shelf (the letter jump walks these).
    shelf: bool,
    /// A state shown as a small chip under the title (the game that is running).
    badge: Option<&'static str>,
}

/// Screen geometry for the current size and orientation.
#[derive(Clone, Copy, Debug)]
pub(super) struct Layout {
    pub full: Rect,
    /// The area inside the safe-area insets.
    pub safe: Rect,
    pub kind: LayoutKind,
    /// Design-unit scale.
    pub u: f32,
    pub horizon: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LayoutKind {
    Desktop,
    Portrait,
    Landscape,
}

impl Layout {
    /// Room the phone's bottom bar takes (zero on a desktop, where the nav is on top).
    pub(super) fn bar_reserve(&self) -> f32 {
        match self.kind {
            LayoutKind::Desktop => 0.0,
            LayoutKind::Portrait => 76.0 * self.u,
            LayoutKind::Landscape => 60.0 * self.u,
        }
    }

    fn new(full: Rect, insets: [f32; 4]) -> Self {
        let safe = Rect::from_min_max(
            Pos2::new(full.left() + insets[2], full.top() + insets[0]),
            Pos2::new(full.right() - insets[3], full.bottom() - insets[1]),
        );
        let (w, h) = (full.width(), full.height());
        let (kind, u, horizon) = if w < 700.0 && h > w {
            (LayoutKind::Portrait, (w / 390.0).clamp(0.8, 1.5), 0.60)
        } else if h < 520.0 {
            (LayoutKind::Landscape, (h / 390.0).clamp(0.8, 1.4), 0.66)
        } else {
            (
                LayoutKind::Desktop,
                (w / 1280.0).min(h / 720.0).clamp(0.62, 1.9),
                0.63,
            )
        };
        Self {
            full,
            safe,
            kind,
            u,
            horizon,
        }
    }
}

pub(super) struct PageCtx<'a> {
    pub painter: &'a egui::Painter,
    pub lay: &'a Layout,
    pub appear: f32,
}

/// Whether the browser asks for reduced motion (never on the desktop).
fn prefers_reduced_motion() -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        return web_sys::window()
            .and_then(|w| {
                w.match_media("(prefers-reduced-motion: reduce)")
                    .ok()
                    .flatten()
            })
            .is_some_and(|q| q.matches());
    }
    #[cfg(not(target_arch = "wasm32"))]
    false
}

/// Remember in this browser that the first-run explainer was seen (local storage, never fatal).
fn remember_explainer() {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
            let _ = storage.set_item(EXPLAINER_KEY, "1");
        }
    }
}

/// Whether the web player should open on the explainer: a first visit that has not seen it.
/// (Native keeps this in the saved settings instead, see `EmulatorSettings::welcome_seen`.)
#[allow(dead_code)]
pub(crate) fn explainer_unseen() -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        let seen = web_sys::window()
            .and_then(|w| w.local_storage().ok().flatten())
            .and_then(|s| s.get_item(EXPLAINER_KEY).ok().flatten())
            .is_some();
        return !seen && is_first_visit();
    }
    #[cfg(not(target_arch = "wasm32"))]
    false
}

fn remember_visit() {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
            let _ = storage.set_item(VISIT_KEY, "1");
        }
    }
}

/// Whether this browser has opened the home screen before.
#[allow(dead_code)]
pub(crate) fn is_first_visit() -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        return !web_sys::window()
            .and_then(|w| w.local_storage().ok().flatten())
            .and_then(|s| s.get_item(VISIT_KEY).ok().flatten())
            .is_some();
    }
    #[cfg(not(target_arch = "wasm32"))]
    false
}

#[cfg(target_arch = "wasm32")]
const VISIT_KEY: &str = "psoxide.home.visited";
#[cfg(target_arch = "wasm32")]
const EXPLAINER_KEY: &str = "psoxide.home.explainer";

/// The pictures of `pages::DISC_GAMES`, in order (`None` where a game has no picture).
fn load_disc_tiles(ctx: &egui::Context) -> Vec<Option<egui::TextureHandle>> {
    pages::DISC_GAMES
        .iter()
        .map(|(name, png)| {
            let rgba = image::load_from_memory((*png)?).ok()?.to_rgba8();
            let (w, h) = rgba.dimensions();
            let img =
                egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw());
            Some(ctx.load_texture(format!("disc-{name}"), img, egui::TextureOptions::LINEAR))
        })
        .collect()
}

/// The demo disc's cover as a texture.
fn load_demo_cover(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    const COVER_PNG: &[u8] = include_bytes!("../../../../../../../assets/home/demo-cover.png");
    let rgba = image::load_from_memory(COVER_PNG).ok()?.to_rgba8();
    let (w, h) = rgba.dimensions();
    let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw());
    Some(ctx.load_texture("demo-disc-cover", img, egui::TextureOptions::LINEAR))
}

fn decode_label(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    const LABEL_PNG: &[u8] =
        include_bytes!("../../../../../../../assets/branding/demo-disc-label.png");
    let rgba = image::load_from_memory(LABEL_PNG).ok()?.to_rgba8();
    let (w, h) = rgba.dimensions();
    let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw());
    Some(ctx.load_texture("demo-disc-label", img, egui::TextureOptions::LINEAR))
}

impl HomeState {
    /// Skip the sequence: it plays out fast on its own curves, so the hand-off
    /// stays continuous. The press that started it does not count.
    fn skip_launch(&mut self) {
        if let Some(l) = self.launch.as_mut() {
            if !self.launch_done && !l.frozen && l.t > launch::SKIP_AFTER {
                l.skipping = true;
            }
        }
    }

    /// Advance the launch sequence by elapsed time. At its end the game gets its action if it
    /// has not had it yet; the disc goes on spinning until the game's boot closes the shell.
    fn tick_launch(&mut self, dt: f32) {
        let Some(l) = self.launch.as_mut() else {
            return;
        };
        if l.frozen {
            return;
        }
        l.t += dt * if l.skipping { launch::SKIP_SPEED } else { 1.0 };
        if !self.launch_done && l.t >= launch::DURATION {
            self.launch_done = true;
            if !self.launch_sent {
                self.launch_sent = true;
                self.pending_launch = Some(l.action.clone());
            }
        }
    }

    fn draw_launch(&mut self, ms: &mut MenuState, ctx: &egui::Context, pc: &mut PageCtx<'_>) {
        let Some(l) = self.launch.as_ref() else {
            return;
        };
        let art = l
            .cover_key
            .as_deref()
            .and_then(|k| self.covers.texture(ctx, k));
        let label = self.label_tex.as_ref().map(egui::TextureHandle::id);
        launch::draw(pc.painter, pc.lay, l, art, label, pc.appear);
        let (t, skipping) = (l.t, l.skipping);
        if !skipping && !self.launch_done {
            self.draw_skip_hint(pc, t);
        }
        if let Some(action) = self.pending_launch.take() {
            ms.pending_pointer_action = Some(action);
        }
    }

    /// "Any button skips", quietly under the launch's title once it is under way.
    fn draw_skip_hint(&self, pc: &PageCtx<'_>, t: f32) {
        let lay = pc.lay;
        let u = lay.u;
        let a = pc.appear * (((t - 0.5) / 0.3).clamp(0.0, 1.0)) * 0.9;
        if a <= 0.01 {
            return;
        }
        let p = pc.painter;
        let y = lay.safe.bottom() - 16.0 * u;
        let x = lay.full.center().x;
        let size = 12.0 * u.max(0.9);
        match self.hint_style(lay) {
            Some(HintStyle::Pad) => {
                let words = "Any button skips";
                let g = p.layout_no_wrap(words.into(), FontId::proportional(size), tokens::DIM);
                let r = hints::face_radius(u) * 0.9;
                let total = r * 2.0 + 8.0 * u + g.size().x;
                let left = x - total / 2.0;
                hints::draw_face(p, Pos2::new(left + r, y), r, Btn::Cross, a, 0.0);
                p.text(
                    Pos2::new(left + r * 2.0 + 8.0 * u, y),
                    Align2::LEFT_CENTER,
                    words,
                    FontId::proportional(size),
                    fade(tokens::DIM, a),
                );
            }
            Some(HintStyle::Keys) => {
                p.text(
                    Pos2::new(x, y),
                    Align2::CENTER_CENTER,
                    "Any key skips",
                    FontId::proportional(size),
                    fade(tokens::DIM, a),
                );
            }
            None => {
                p.text(
                    Pos2::new(x, y),
                    Align2::CENTER_CENTER,
                    "Tap to skip",
                    FontId::proportional(size),
                    fade(tokens::DIM, a),
                );
            }
        }
    }

    /// The frame of the slot sheet; the content comes from the
    /// app around the shell (it needs the emulator state).
    fn draw_section_frame(&mut self, ms: &MenuState, pc: &mut PageCtx<'_>) {
        let lay = pc.lay;
        let u = lay.u;
        let top = lay.safe.top()
            + match lay.kind {
                LayoutKind::Desktop => 88.0 * u,
                LayoutKind::Portrait => 96.0 * u,
                LayoutKind::Landscape => 62.0 * u,
            };
        let side = if lay.kind == LayoutKind::Desktop {
            48.0 * u
        } else {
            12.0 * u
        };
        let rect = Rect::from_min_max(
            Pos2::new(lay.safe.left() + side, top),
            Pos2::new(
                lay.safe.right() - side,
                lay.safe.bottom()
                    - if lay.kind == LayoutKind::Desktop {
                        12.0 * u + self.hint_reserve(lay)
                    } else {
                        lay.bar_reserve() + 4.0 * u
                    },
            ),
        );
        let switch_to = match ms.slots.mode {
            super::SlotsMode::Save => "Load view",
            super::SlotsMode::Load => "Save view",
        };
        self.draw_page_hints(
            pc,
            &[
                (Btn::Cross, "Choose"),
                (Btn::Square, switch_to),
                (Btn::Triangle, "Delete"),
                (Btn::Circle, "Back"),
            ],
        );
        let outline = chamfer(rect.expand(6.0), tokens::CHAMFER * u);
        let head = self.reveal(0);
        pc.painter.add(Shape::convex_polygon(
            outline.clone(),
            fade(
                Color32::from_rgba_unmultiplied(8, 16, 22, 184),
                pc.appear * head,
            ),
            Stroke::new(1.0, fade(tokens::hairline(), pc.appear * head)),
        ));
        self.section = Some(rect);
    }

    fn draw_home(&mut self, ms: &mut MenuState, ctx: &egui::Context, pc: &mut PageCtx<'_>) {
        let lay = *pc.lay;
        let p = pc.painter;
        let a = pc.appear;
        let u = lay.u;
        let cases = self.cases(ms);
        self.contents = None;
        self.case_focus = self.case_focus.min(cases.len().saturating_sub(1));
        self.letters = grid::letter_groups(&cases).len();
        let f = self.case_focus;
        if self.compact {
            self.draw_compact(ms, ctx, pc, &cases);
            return;
        }
        let (w, h) = (lay.full.width(), lay.full.height());
        // Case row placement per layout.
        let (cx, cy, step, base) = match lay.kind {
            LayoutKind::Desktop => (w * 0.5, h * 0.435, 300.0 * u, 1.0 * u),
            LayoutKind::Portrait => (w * 0.5, h * 0.375, 180.0 * u, 0.78 * u),
            LayoutKind::Landscape => (w * 0.27, h * 0.5, 150.0 * u, 0.62 * u),
        };
        let cam = scene::Camera {
            origin: Pos2::new(w * 0.5, h * 0.30),
            focal: 1000.0 * u,
        };
        // Farthest cases first, so the focused one is on top.
        let mut order: Vec<usize> = (0..cases.len()).collect();
        order.sort_by(|&x, &y| {
            let dx = (x as f32 - self.case_pos).abs();
            let dy = (y as f32 - self.case_pos).abs();
            dy.total_cmp(&dx)
        });
        let time = self.time;
        for i in order {
            let item = &cases[i];
            let d = i as f32 - self.case_pos;
            if d.abs() > 4.6 {
                continue;
            }
            let near = (1.0 - d.abs()).clamp(0.0, 1.0);
            let hero = matches!(item.kind, CoverKind::Demo);
            let size = if hero {
                scene::V3::new(236.0, 210.0, 17.0)
            } else {
                scene::V3::new(150.0, 134.0, 12.0)
            };
            let (art, fx) = self.case_fx(ctx, item, i == f);
            let lit = fx.focus;
            let kind = if art.is_some() {
                CoverKind::Art
            } else {
                item.kind
            };
            // The focused case comes alive: it lifts toward you, turns slowly
            // to show its depth and stands in its own light. Reduced motion
            // keeps only a slightly larger case and the glow.
            let moving = if fx.calm { 0.0 } else { lit };
            let scale =
                base * (1.0 + 0.14 * near + if fx.calm { 0.05 } else { 0.03 } * lit.min(1.0));
            let center = scene::V3::new(
                cx + d * step,
                cy - near * 10.0 * u - 18.0 * u * moving,
                -d.abs() * 90.0 * u + 90.0 * u * moving,
            );
            let turn = moving * (0.2 * (time * 0.55).sin() + 0.1 * (time * 0.31 + 1.3).sin());
            let draw = scene::CaseDraw {
                kind,
                title: &item.title,
                art,
                size,
                center,
                yaw: turn - d.clamp(-1.5, 1.5) * 0.30,
                pitch: -0.07 + 0.025 * moving * (time * 0.43).sin(),
                lid: 0.0,
                scale,
                opacity: a * (0.38 + 0.62 * near),
                seed: hash_str(&item.title),
                fx,
            };
            scene::draw_case(p, &cam, &draw);
            // Hit area for a click on the case.
            let half = Vec2::new(size.x, size.y) * scale * 0.5 + Vec2::splat(6.0);
            let at = cam.project(center);
            let hit = Rect::from_center_size(at, half * 2.0);
            self.hits.push((hit, Click::Case(i)));
            if lit > 0.02 {
                case_brackets(p, hit.expand(5.0 * u), lit, a, u, fx.calm);
            }
        }
        let Some(item) = cases.get(f) else {
            return;
        };
        // The page's own words arrive in turn: title and line, then the button, then the
        // quiet actions, each sliding up a little as it fades in.
        let (rt, rb, rq) = (
            self.home_reveal(0),
            self.home_reveal(2),
            self.home_reveal(3),
        );
        let (ta, ba, qa) = (a * rt, a * rb, a * rq);
        let m0 = motion::mark(ctx, p);
        let text_col = tokens::TEXT;
        let sub_col = tokens::MUTED;
        let (tx, ty, align) = match lay.kind {
            LayoutKind::Landscape => (w * 0.52, h * 0.30, Align2::LEFT_TOP),
            LayoutKind::Portrait => (w * 0.5, h * 0.565, Align2::CENTER_TOP),
            LayoutKind::Desktop => (w * 0.5, h * 0.675, Align2::CENTER_TOP),
        };
        let title_px = match lay.kind {
            LayoutKind::Desktop => tokens::TITLE_DESKTOP * u,
            _ => tokens::TITLE_PHONE * u,
        };
        // Michroma is wide: shrink a long title until it fits the screen.
        let max_title_w = if align == Align2::LEFT_TOP {
            w - tx - 24.0 * u
        } else {
            w - 40.0 * u
        };
        let mut title_font = crate::theme::title_font(title_px);
        let probe = p.layout_no_wrap(item.title.clone(), title_font.clone(), tokens::TEXT);
        if probe.size().x > max_title_w {
            title_font.size *= max_title_w / probe.size().x;
        }
        let r = p.text(
            Pos2::new(tx, ty),
            align,
            &item.title,
            title_font,
            fade(text_col, ta),
        );
        let line_px = (if lay.kind == LayoutKind::Desktop {
            16.0
        } else {
            14.0
        }) * u;
        let mut sub_y = r.bottom() + 8.0 * u;
        if let Some(text) = item.badge {
            // The state of the case, as a small chip: a lit dot and a word.
            let font = FontId::proportional(tokens::CAPTION * u.max(0.95));
            let galley = p.layout_no_wrap(text.to_uppercase(), font, fade(tokens::TEXT_2, ta));
            let (cw, ch) = (galley.size().x + 34.0 * u, 24.0 * u);
            let left = match align {
                Align2::LEFT_TOP => tx,
                _ => tx - cw / 2.0,
            };
            let chip = Rect::from_min_size(Pos2::new(left, sub_y), Vec2::new(cw, ch));
            p.rect_filled(
                chip,
                ch / 2.0,
                fade(Color32::from_rgb(10, 34, 40), ta * 0.9),
            );
            p.rect_stroke(
                chip,
                ch / 2.0,
                Stroke::new(1.0, fade(tokens::ACCENT, ta * 0.55)),
                egui::StrokeKind::Inside,
            );
            // The dot breathes while the game waits (still under reduced motion).
            let pulse = if self.calm() {
                0.8
            } else {
                0.65 + 0.35 * (self.time * 2.6).sin().abs()
            };
            let dot = Pos2::new(chip.left() + 13.0 * u, chip.center().y);
            p.circle_filled(dot, 3.2 * u, fade(tokens::OK, ta * pulse));
            p.galley(
                Pos2::new(
                    chip.left() + 24.0 * u,
                    chip.center().y - galley.size().y / 2.0,
                ),
                galley,
                tokens::TEXT_2,
            );
            sub_y = chip.bottom() + 8.0 * u;
        }
        // A case with a state chip lets the chip say it (the line stays in the probe), which
        // keeps a short phone's button clear of the tab bar.
        let sub = if item.badge.is_some() {
            Rect::from_min_size(Pos2::new(tx, sub_y - 8.0 * u), Vec2::ZERO)
        } else {
            p.text(
                Pos2::new(tx, sub_y),
                align,
                &item.line,
                FontId::proportional(line_px),
                fade(sub_col, ta),
            )
        };
        // What is on the demo disc, always in view: one chip per game (a small picture and the name),
        // wrapped to the width. Not while the disc runs.
        let mut below = sub.bottom();
        if matches!(item.kind, CoverKind::Demo) && item.badge.is_none() {
            // What the rest of the page needs under it: the countdown line, the button, the quiet
            // links and the bar; the chips take what is left.
            let counting = self.progress_line().is_some();
            // The button, the countdown line above it and, on a phone held upright, the links
            // under it are placed from the chips down: the chips may not push them over the bar.
            let after_chips = 16.0 + if counting { 22.0 } else { 0.0 };
            let floor = match lay.kind {
                LayoutKind::Desktop => h * 0.855 - after_chips * u,
                LayoutKind::Portrait => {
                    lay.safe.bottom()
                        - lay.bar_reserve()
                        - (4.0 + 48.0 + 44.0 + 6.0 + after_chips) * u
                }
                LayoutKind::Landscape => {
                    lay.safe.bottom() - lay.bar_reserve() - (4.0 + 44.0 + 6.0 + after_chips) * u
                }
            };
            let max_w = match align {
                Align2::LEFT_TOP => (w - tx - 24.0 * u).max(120.0 * u),
                _ => (w - 40.0 * u).min(720.0 * u),
            };
            let top = below + 12.0 * u;
            below =
                self.draw_disc_chips(ctx, p, Pos2::new(tx, top), align, max_w, floor - top, u, ta);
        }
        // The countdown line, or the loading line, or nothing.
        let mut cd_y = below + 16.0 * u;
        if matches!(item.kind, CoverKind::Demo) && !self.started {
            if let Some(line) = self.progress_line() {
                self.draw_progress_line(p, tx, cd_y, align, u, ta, &line);
                cd_y += 22.0 * u;
            }
        }
        motion::shift_since(ctx, p, m0, self.slide(rt, u));
        let m1 = motion::mark(ctx, p);
        // The one button.
        let label = item.button.to_uppercase();
        let pad_glyph = self.hint_style(&lay) == Some(HintStyle::Pad);
        let bw =
            (label.chars().count() as f32 * 9.0 + 64.0 + if pad_glyph { 30.0 } else { 0.0 }) * u;
        let bh = 44.0 * u;
        let btn_y = cd_y.max(match lay.kind {
            LayoutKind::Desktop => h * 0.855,
            _ => cd_y,
        }) + 6.0 * u;
        let btn_pos = match align {
            Align2::LEFT_TOP => Pos2::new(tx, btn_y),
            _ => Pos2::new(tx - bw / 2.0, btn_y),
        };
        let btn = Rect::from_min_size(btn_pos, Vec2::new(bw, bh));
        let focused = self.zone == Zone::Body;
        p.rect_filled(btn, bh / 2.0, fade(CYAN, ba));
        if focused {
            p.rect_stroke(
                btn.expand(3.0 * u),
                bh / 2.0 + 3.0 * u,
                Stroke::new(1.5, fade(tokens::FOCUS, ba * 0.8)),
                egui::StrokeKind::Outside,
            );
        }
        let label_at = if pad_glyph {
            hints::draw_face_on_accent(
                p,
                Pos2::new(btn.left() + 24.0 * u, btn.center().y),
                hints::face_radius(u) * 0.9,
                Btn::Cross,
                ba,
            );
            btn.center() + Vec2::new(12.0 * u, 0.0)
        } else {
            btn.center()
        };
        p.text(
            label_at,
            Align2::CENTER_CENTER,
            label,
            FontId::proportional(13.0 * u),
            fade(tokens::ON_ACCENT, ba),
        );
        self.hits.push((btn, Click::Primary));
        self.btn_bottom = btn.bottom();
        motion::shift_since(ctx, p, m1, self.slide(rb, u));
        let m2 = motion::mark(ctx, p);
        // Quiet second actions under the button.
        let mut qy = btn.bottom() + 14.0 * u;
        let quiet = |p: &egui::Painter, text: &str, y: f32| {
            p.text(
                Pos2::new(
                    match align {
                        Align2::LEFT_TOP => btn.left(),
                        _ => btn.center().x,
                    },
                    y,
                ),
                match align {
                    Align2::LEFT_TOP => Align2::LEFT_TOP,
                    _ => Align2::CENTER_TOP,
                },
                text,
                FontId::proportional(13.0 * u),
                fade(tokens::ACCENT, qa),
            )
        };
        if item.restart.is_some() {
            let r = quiet(p, "Start over", qy);
            self.hits
                .push((r.expand2(Vec2::new(12.0, 8.0)), Click::Restart));
            self.square_glyph(p, &lay, r, qa);
            qy = r.bottom() + 6.0 * u;
            // The footer makes room under the button and what hangs from it.
            self.btn_bottom = r.bottom() + 6.0 * u;
        }
        if item.burn.is_some() {
            let r = quiet(p, "Burn to CD", qy);
            self.hits
                .push((r.expand2(Vec2::new(12.0, 8.0)), Click::Burn));
            self.btn_bottom = r.bottom() + 6.0 * u;
            // Square is the first of Start over and Burn that the case has.
            if item.restart.is_none() && !matches!(item.kind, CoverKind::Demo) {
                self.square_glyph(p, &lay, r, qa);
            }
        }
        motion::shift_since(ctx, p, m2, self.slide(rq, u));
        // Quiet links at the bottom.
        self.draw_footer(pc);
    }

    /// The games on the demo disc as chips under the title: a small picture and the name, wrapped
    /// to `max_w` and centred or left on `at` as `align` says. When the room (`budget`, points)
    /// is short the pictures go, then the chips: the names never go. Returns the bottom.
    #[allow(clippy::too_many_arguments)]
    fn draw_disc_chips(
        &mut self,
        ctx: &egui::Context,
        p: &egui::Painter,
        at: Pos2,
        align: Align2,
        max_w: f32,
        budget: f32,
        u: f32,
        a: f32,
    ) -> f32 {
        let tiles = self.disc_tiles.get_or_insert_with(|| load_disc_tiles(ctx));
        let font_for =
            |style: usize| FontId::proportional(if style == 0 { 12.0 } else { 11.5 } * u.max(0.95));
        let gap = 6.0 * u;
        let row_gap = |style: usize| if style == 0 { 6.0 * u } else { 4.0 * u };
        let names = pages::DISC_GAMES;
        // Style 0 has pictures, 1 only the names in chips, 2 one wrapped line of names.
        let measure = |style: usize| -> (Vec<(f32, f32)>, f32) {
            let (ch, pic) = match style {
                0 => (26.0 * u, 28.0 * u),
                _ => (19.0 * u, 0.0),
            };
            let font = font_for(style);
            let mut rows: Vec<(f32, f32)> = Vec::new(); // (x offset of the row's start, row width)
            let mut widths: Vec<f32> = Vec::new();
            let mut cur = 0.0_f32;
            let mut count = 0;
            for (name, png) in names {
                let tw = p
                    .layout_no_wrap((*name).to_string(), font.clone(), tokens::TEXT)
                    .size()
                    .x;
                let cw = tw
                    + if pic > 0.0 && png.is_some() {
                        18.0 * u + pic + 6.0 * u
                    } else {
                        14.0 * u
                    };
                if count > 0 && cur + gap + cw > max_w {
                    widths.push(cur);
                    cur = 0.0;
                    count = 0;
                }
                cur += if count > 0 { gap } else { 0.0 } + cw;
                count += 1;
            }
            widths.push(cur);
            for w in widths {
                rows.push((0.0, w));
            }
            let rg = row_gap(style);
            let height = rows.len() as f32 * (ch + rg) - rg;
            (rows, height)
        };
        let mut style = 0;
        while style < 1 && measure(style).1 > budget {
            style += 1;
        }
        let top = at.y;
        if measure(style).1 > budget {
            // Still too tall: the names as plain wrapped text.
            let text = names
                .iter()
                .map(|(n, _)| *n)
                .collect::<Vec<_>>()
                .join("  \u{b7}  ");
            let galley = p.layout(text, font_for(2), fade(tokens::TEXT_2, a), max_w);
            let x = match align {
                Align2::LEFT_TOP => at.x,
                _ => at.x - galley.size().x / 2.0,
            };
            let bottom = top + galley.size().y;
            let width = galley.size().x;
            p.galley(Pos2::new(x, top), galley, tokens::TEXT_2);
            self.contents = Some([x, top, x + width, bottom, names.len() as f32, 2.0]);
            return bottom;
        }
        let (ch, pic) = if style == 0 {
            (26.0 * u, 28.0 * u)
        } else {
            (19.0 * u, 0.0)
        };
        let (rows, height) = measure(style);
        let _ = rows;
        let font = font_for(style);
        // Lay the chips out again, now drawing.
        let mut y = top;
        let mut line: Vec<(&str, usize, f32)> = Vec::new();
        let mut line_w = 0.0_f32;
        let flush = |line: &mut Vec<(&str, usize, f32)>, line_w: f32, y: f32| {
            let mut x = match align {
                Align2::LEFT_TOP => at.x,
                _ => at.x - line_w / 2.0,
            };
            for (name, i, cw) in line.drain(..) {
                let chip = Rect::from_min_size(Pos2::new(x, y), Vec2::new(cw, ch));
                p.rect_filled(chip, ch / 2.0, fade(Color32::from_rgb(10, 24, 32), a * 0.9));
                p.rect_stroke(
                    chip,
                    ch / 2.0,
                    Stroke::new(1.0, fade(tokens::hairline(), a)),
                    egui::StrokeKind::Inside,
                );
                let mut tx = chip.left() + 7.0 * u;
                if let (true, Some(Some(tex))) = (pic > 0.0, tiles.get(i)) {
                    {
                        let r = Rect::from_min_size(
                            Pos2::new(chip.left() + 3.0 * u, chip.center().y - pic * 0.375),
                            Vec2::new(pic, pic * 0.75),
                        );
                        p.image(
                            tex.id(),
                            r,
                            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                            fade(Color32::WHITE, a),
                        );
                    }
                    tx = chip.left() + 3.0 * u + pic + 6.0 * u;
                }
                p.text(
                    Pos2::new(tx, chip.center().y),
                    Align2::LEFT_CENTER,
                    name,
                    font.clone(),
                    fade(tokens::TEXT_2, a),
                );
                x += cw + gap;
            }
        };
        for (i, (name, png)) in names.iter().enumerate() {
            let tw = p
                .layout_no_wrap((*name).to_string(), font.clone(), tokens::TEXT)
                .size()
                .x;
            let cw = tw
                + if pic > 0.0 && png.is_some() {
                    18.0 * u + pic + 6.0 * u
                } else {
                    14.0 * u
                };
            if !line.is_empty() && line_w + gap + cw > max_w {
                flush(&mut line, line_w, y);
                y += ch + row_gap(style);
                line_w = 0.0;
            }
            line_w += if line.is_empty() { 0.0 } else { gap } + cw;
            line.push((name, i, cw));
        }
        flush(&mut line, line_w, y);
        let left = match align {
            Align2::LEFT_TOP => at.x,
            _ => at.x - max_w.min(widest(&measure(style).0)) / 2.0,
        };
        self.contents = Some([
            left,
            top,
            left + widest(&measure(style).0),
            top + height,
            names.len() as f32,
            style as f32,
        ]);
        top + height
    }

    /// The compact grid with the focused case's title and button under it.
    fn draw_compact(
        &mut self,
        ms: &mut MenuState,
        ctx: &egui::Context,
        pc: &mut PageCtx<'_>,
        cases: &[CaseItem],
    ) {
        let lay = *pc.lay;
        let a = pc.appear;
        let u = lay.u;
        let info = self.draw_grid(ms, ctx, pc, cases);
        let p = pc.painter;
        let Some(item) = cases.get(self.case_focus) else {
            return;
        };
        let desktop = lay.kind == LayoutKind::Desktop;
        let wide = lay.kind != LayoutKind::Portrait;
        let label = item.button.to_uppercase();
        let pad_glyph = self.hint_style(&lay) == Some(HintStyle::Pad);
        let bw =
            (label.chars().count() as f32 * 8.5 + 56.0 + if pad_glyph { 28.0 } else { 0.0 }) * u;
        let bh = 40.0 * u;
        let (tx, ty) = if wide {
            (info.left() + 48.0 * u, info.center().y - 22.0 * u)
        } else {
            (info.center().x, info.top() + 8.0 * u)
        };
        let mut font = crate::theme::title_font(if desktop { 17.0 * u } else { 14.0 * u });
        let max_w = if wide {
            info.width() - bw - 140.0 * u
        } else {
            info.width() - 40.0 * u
        };
        let probe = p.layout_no_wrap(item.title.clone(), font.clone(), tokens::TEXT);
        if probe.size().x > max_w {
            font.size *= max_w / probe.size().x;
        }
        let t = p.text(
            Pos2::new(tx, ty),
            if wide {
                Align2::LEFT_TOP
            } else {
                Align2::CENTER_TOP
            },
            &item.title,
            font,
            fade(tokens::TEXT, a),
        );
        p.text(
            Pos2::new(tx, t.bottom() + 6.0 * u),
            if wide {
                Align2::LEFT_TOP
            } else {
                Align2::CENTER_TOP
            },
            &item.line,
            FontId::proportional(13.0 * u),
            fade(tokens::MUTED, a),
        );
        let btn = if wide {
            Rect::from_min_size(
                Pos2::new(info.right() - 48.0 * u - bw, info.center().y - bh / 2.0),
                Vec2::new(bw, bh),
            )
        } else {
            Rect::from_center_size(
                Pos2::new(info.center().x, info.bottom() - bh / 2.0 - 6.0 * u),
                Vec2::new(bw, bh),
            )
        };
        p.rect_filled(btn, bh / 2.0, fade(CYAN, a));
        if self.zone == Zone::Body {
            p.rect_stroke(
                btn.expand(3.0 * u),
                bh / 2.0 + 3.0 * u,
                Stroke::new(1.5, fade(tokens::FOCUS, a * 0.8)),
                egui::StrokeKind::Outside,
            );
        }
        let label_at = if pad_glyph {
            hints::draw_face_on_accent(
                p,
                Pos2::new(btn.left() + 22.0 * u, btn.center().y),
                hints::face_radius(u) * 0.85,
                Btn::Cross,
                a,
            );
            btn.center() + Vec2::new(11.0 * u, 0.0)
        } else {
            btn.center()
        };
        p.text(
            label_at,
            Align2::CENTER_CENTER,
            label,
            FontId::proportional(12.5 * u),
            fade(tokens::ON_ACCENT, a),
        );
        self.hits.push((btn, Click::Primary));
    }

    fn progress_line(&self) -> Option<ProgressLine> {
        match (self.countdown, self.feed.demo) {
            (Countdown::Running(left), _) => Some(ProgressLine::Count(
                left.ceil().max(1.0) as u32,
                1.0 - left / countdown_secs(),
            )),
            (Countdown::Armed, DemoStatus::Loading(f)) => Some(ProgressLine::Loading(f)),
            (Countdown::Cancelled, _) if self.first_visit => Some(ProgressLine::Cancelled),
            _ => None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_progress_line(
        &self,
        p: &egui::Painter,
        x: f32,
        y: f32,
        align: Align2,
        u: f32,
        a: f32,
        line: &ProgressLine,
    ) {
        let col = fade(tokens::TEXT_2, a);
        let dim = fade(tokens::ICON_REST, a);
        let text = match line {
            ProgressLine::Count(n, _) => format!("Starting in {n}"),
            ProgressLine::Loading(f) if *f >= 0.0 => {
                format!("Loading the disc  {}%", (f * 100.0).round() as u32)
            }
            ProgressLine::Loading(_) => "Loading the disc".to_string(),
            ProgressLine::Cancelled => "Autostart cancelled".to_string(),
        };
        let hint = match line {
            ProgressLine::Count(..) => "   Any input cancels",
            _ => "",
        };
        let icon = 18.0 * u;
        let gal = p.layout_no_wrap(text, FontId::proportional(15.0 * u), col);
        let hal = p.layout_no_wrap(hint.to_string(), FontId::proportional(12.0 * u), dim);
        let total = icon + 8.0 * u + gal.size().x + hal.size().x;
        let left = match align {
            Align2::LEFT_TOP => x,
            _ => x - total / 2.0,
        };
        let c = Pos2::new(left + icon / 2.0, y + gal.size().y / 2.0);
        match line {
            ProgressLine::Count(_, frac) => {
                p.circle_stroke(
                    c,
                    icon / 2.0 - 1.0,
                    Stroke::new(1.5, fade(Color32::from_rgb(40, 56, 68), a)),
                );
                let steps = 36;
                let upto = (steps as f32 * frac.clamp(0.0, 1.0)) as usize;
                let pts: Vec<Pos2> = (0..=upto)
                    .map(|i| {
                        let t = -std::f32::consts::FRAC_PI_2
                            + i as f32 / steps as f32 * std::f32::consts::TAU;
                        c + Vec2::new(t.cos(), t.sin()) * (icon / 2.0 - 1.0)
                    })
                    .collect();
                if pts.len() > 1 {
                    p.add(Shape::line(pts, Stroke::new(2.0, fade(CYAN, a))));
                }
            }
            ProgressLine::Loading(_) => {
                let t = self.time * 3.0;
                let pts: Vec<Pos2> = (0..=14)
                    .map(|i| {
                        let ang = t + i as f32 / 14.0 * 4.0;
                        c + Vec2::new(ang.cos(), ang.sin()) * (icon / 2.0 - 1.0)
                    })
                    .collect();
                p.add(Shape::line(pts, Stroke::new(2.0, fade(CYAN, a * 0.8))));
            }
            ProgressLine::Cancelled => {
                let d = 4.0 * u;
                let s = Stroke::new(1.5, dim);
                p.line_segment([c + Vec2::new(-d, -d), c + Vec2::new(d, d)], s);
                p.line_segment([c + Vec2::new(-d, d), c + Vec2::new(d, -d)], s);
            }
        }
        let tx = left + icon + 8.0 * u;
        p.add(Shape::galley(Pos2::new(tx, y), gal.clone(), col));
        p.add(Shape::galley(
            Pos2::new(tx + gal.size().x, y + 2.0 * u),
            hal,
            dim,
        ));
    }

    fn draw_footer(&mut self, pc: &mut PageCtx<'_>) {
        let lay = pc.lay;
        let p = pc.painter;
        let a = pc.appear;
        let u = lay.u;
        let s = lay.safe;
        let dim = fade(tokens::DIM, a);
        let link = fade(tokens::ACCENT, a);
        let mut y = s.bottom() - 26.0 * u - lay.bar_reserve();
        // On a short phone the button comes down to the footer: the links go under it.
        let mut room_for_site_link = true;
        if lay.kind != LayoutKind::Desktop && self.btn_bottom + 34.0 * u > y {
            y = self.btn_bottom + 30.0 * u;
            // Under the button there may be no room before the bar: the site link
            // then lives on the welcome pages only.
            let bar_top = lay.safe.bottom() - lay.bar_reserve() - 4.0 * u;
            if y + 14.0 * u > bar_top {
                room_for_site_link = false;
                y = (lay.safe.bottom() - 26.0 * u - lay.bar_reserve()).min(y);
            }
        }
        if room_for_site_link {
            let more = p.text(
                Pos2::new(
                    s.right()
                        - 48.0 * u * (lay.kind == LayoutKind::Desktop) as u8 as f32
                        - if lay.kind == LayoutKind::Desktop {
                            0.0
                        } else {
                            20.0 * u
                        },
                    y,
                ),
                Align2::RIGHT_CENTER,
                "More PSoXide games you build yourself",
                FontId::proportional(13.0 * u),
                link,
            );
            self.hits
                .push((more.expand2(Vec2::new(8.0, 10.0)), Click::Link(SITE_URL)));
        }
        let what = p.text(
            Pos2::new(
                s.left()
                    + if lay.kind == LayoutKind::Desktop {
                        48.0 * u
                    } else {
                        20.0 * u
                    },
                y,
            ),
            Align2::LEFT_CENTER,
            "What is this?",
            FontId::proportional(13.0 * u),
            dim,
        );
        self.hits
            .push((what.expand2(Vec2::new(8.0, 10.0)), Click::Welcome));
        // Button hints on the desktop shell: glyphs for a controller, key caps for a keyboard;
        // touch and phones go without.
        if lay.kind == LayoutKind::Desktop {
            let mut x = what.right() + 40.0 * u;
            if let Some(style) = self.hint_style(lay) {
                let back = if self.started { "Resume" } else { "Back" };
                for (btn, label) in [
                    (Btn::Cross, "Select"),
                    (Btn::Circle, back),
                    (Btn::Triangle, "View"),
                ] {
                    let press = if btn == Btn::Triangle {
                        self.pad_press[2]
                    } else {
                        0.0
                    };
                    let w = hints::btn_width(p, u, style, btn);
                    let g = hints::draw_btn(p, Pos2::new(x + w / 2.0, y), u, a, style, btn, press);
                    let r = p.text(
                        Pos2::new(g.right() + 8.0 * u, y),
                        Align2::LEFT_CENTER,
                        label,
                        FontId::proportional(13.0 * u),
                        dim,
                    );
                    if btn == Btn::Triangle {
                        self.hits.push((g.union(r).expand(6.0), Click::SizeToggle));
                    }
                    x = r.right() + 22.0 * u;
                }
                // L2 and R2 jump by letter, when the shelf has letters to jump between.
                if self.letters > 1 {
                    for btn in [Btn::L2, Btn::R2] {
                        let w = hints::btn_width(p, u, style, btn);
                        hints::draw_btn(p, Pos2::new(x + w / 2.0, y), u, a, style, btn, 0.0);
                        x += w + 6.0 * u;
                    }
                    let r = p.text(
                        Pos2::new(x + 2.0 * u, y),
                        Align2::LEFT_CENTER,
                        "A to Z",
                        FontId::proportional(13.0 * u),
                        dim,
                    );
                    // The links after the hints are desktop-only.
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        x = r.right() + 22.0 * u;
                    }
                    #[cfg(target_arch = "wasm32")]
                    let _ = r;
                }
            }
            // Desktop only: a disc that is not in the games folder, and a rescan.
            #[cfg(not(target_arch = "wasm32"))]
            {
                let mut qx = x + 40.0 * u;
                for (label, action) in [
                    ("Open disc...", MenuAction::OpenDisc),
                    ("Refresh library", MenuAction::RescanLibrary),
                ] {
                    let r = p.text(
                        Pos2::new(qx, y),
                        Align2::LEFT_CENTER,
                        label,
                        FontId::proportional(13.0 * u),
                        link,
                    );
                    self.hits
                        .push((r.expand2(Vec2::new(8.0, 10.0)), Click::Act(action)));
                    qx = r.right() + 24.0 * u;
                }
            }
        }
    }

    /// Move the tab highlight and the focus ring toward their tabs, once per frame.
    fn tick_nav(&mut self, items: &[NavItem], shell_up: f32) {
        let dt = self.frame_dt;
        // While the shell is fading in the springs jump, so the highlight never travels
        // from where the last visit left it.
        let snap = self.calm() || shell_up < 0.6;
        let active = items.iter().position(|i| i.page() == self.page.nav_page());
        if let Some(at) = active {
            self.nav_hl.step(at as f32, dt, snap || self.nav_vis < 0.05);
        }
        let ease = |v: f32, target: f32| v + (target - v) * (1.0 - (-14.0 * dt).exp());
        self.nav_vis = if snap {
            f32::from(active.is_some())
        } else {
            ease(self.nav_vis, f32::from(active.is_some()))
        };
        let focused = self.zone == Zone::Nav;
        let ring_at = if focused {
            self.nav.min(items.len().saturating_sub(1)) as f32
        } else {
            self.nav_hl.pos
        };
        self.nav_ring
            .step(ring_at, dt, snap || self.ring_vis < 0.05);
        self.ring_vis = if snap {
            f32::from(focused)
        } else {
            ease(self.ring_vis, f32::from(focused))
        };
    }

    /// How near tab `i` is to the tab highlight, 1 on it and 0 a tab away: its colour follows.
    fn tab_near(&self, i: usize) -> f32 {
        (1.0 - (self.nav_hl.pos - i as f32).abs()).clamp(0.0, 1.0) * self.nav_vis
    }

    /// The desktop nav: plain text along the top, and a marker that slides to the active tab.
    fn draw_top_nav(
        &mut self,
        p: &egui::Painter,
        lay: &Layout,
        items: &[NavItem],
        logo: Rect,
        u: f32,
        a: f32,
    ) {
        let mut x = logo.right() + 34.0 * u;
        let ny = logo.center().y;
        let style = self.hint_style(lay);
        // L1 at the left end of the tabs.
        if let Some(style) = style {
            let w = hints::btn_width(p, u, style, Btn::L1);
            let c = Pos2::new(x + w / 2.0, ny);
            hints::draw_btn(p, c, u, a, style, Btn::L1, self.pad_press[0]);
            self.hits.push((
                Rect::from_center_size(c, Vec2::new(w + 10.0, 30.0 * u)),
                Click::TabStep(-1),
            ));
            x += w + 18.0 * u;
        }
        let mut centres: Vec<(f32, f32)> = Vec::with_capacity(items.len());
        let mut bottom = ny;
        for (i, item) in items.iter().enumerate() {
            let near = self
                .tab_near(i)
                .max(self.ring_vis * (1.0 - (self.nav_ring.pos - i as f32).abs()).clamp(0.0, 1.0));
            let col = scene::mix(tokens::DIM, tokens::TEXT, near);
            let r = p.text(
                Pos2::new(x, ny),
                Align2::LEFT_CENTER,
                item.label(),
                FontId::proportional(15.5 * u),
                fade(col, a),
            );
            centres.push((r.center().x, r.bottom()));
            bottom = r.bottom();
            self.hits
                .push((r.expand2(Vec2::new(10.0, 12.0)), Click::Nav(*item)));
            x = r.right() + 26.0 * u;
        }
        // The marker: a small diamond under the tab. It rests on the active tab (cyan) and
        // follows the focus while the pad or keys are on the tabs (the focus colour).
        let pos = self.nav_hl.pos + (self.nav_ring.pos - self.nav_hl.pos) * self.ring_vis;
        if let (Some(first), Some(last)) = (centres.first(), centres.last()) {
            let at = |t: f32| -> f32 {
                let t = t.clamp(0.0, (centres.len() - 1) as f32);
                let i = t.floor() as usize;
                let j = (i + 1).min(centres.len() - 1);
                centres[i].0 + (centres[j].0 - centres[i].0) * (t - i as f32)
            };
            let vis = (self.nav_vis + self.ring_vis).min(1.0);
            let c = Pos2::new(
                at(pos).clamp(first.0 - 6.0 * u, last.0 + 6.0 * u),
                bottom + 5.0 * u,
            );
            let d = 3.0 * u;
            let col = scene::mix(CYAN, tokens::FOCUS, self.ring_vis);
            p.add(Shape::convex_polygon(
                vec![
                    c + Vec2::new(0.0, -d),
                    c + Vec2::new(d, 0.0),
                    c + Vec2::new(0.0, d),
                    c + Vec2::new(-d, 0.0),
                ],
                fade(col, a * vis),
                Stroke::NONE,
            ));
        }
        // R1 at the right end.
        if let Some(style) = style {
            let w = hints::btn_width(p, u, style, Btn::R1);
            let c = Pos2::new(x - 8.0 * u + w / 2.0, ny);
            hints::draw_btn(p, c, u, a, style, Btn::R1, self.pad_press[1]);
            self.hits.push((
                Rect::from_center_size(c, Vec2::new(w + 10.0, 30.0 * u)),
                Click::TabStep(1),
            ));
        }
    }

    /// The phone nav: a floating pill at the bottom, clear of the touch
    /// overlay's own buttons at the top.
    fn draw_bottom_bar(
        &mut self,
        p: &egui::Painter,
        lay: &Layout,
        items: &[NavItem],
        u: f32,
        a: f32,
    ) {
        let h = (if lay.kind == LayoutKind::Landscape {
            44.0
        } else {
            56.0
        }) * u;
        let style = self.hint_style(lay);
        let side = style.map_or(0.0, |st| hints::btn_width(p, u, st, Btn::L1) + 10.0 * u);
        // Always centred in the viewport, with the same margin either side. A host page's own
        // fullscreen button (itch) may sit beside the bar in a frame; the bar does not move for it.
        let w = (items.len() as f32 * 76.0 * u).min(lay.safe.width() - 24.0 * u - 2.0 * side);
        let bar = Rect::from_center_size(
            Pos2::new(lay.safe.center().x, lay.safe.bottom() - h / 2.0 - 10.0 * u),
            Vec2::new(w, h),
        );
        p.rect_filled(
            bar,
            h / 2.0,
            fade(Color32::from_rgba_unmultiplied(12, 20, 28, 235), a),
        );
        p.rect_stroke(
            bar,
            h / 2.0,
            Stroke::new(1.0, fade(Color32::from_rgb(38, 58, 72), a)),
            egui::StrokeKind::Inside,
        );
        // L1 and R1 at the two ends of the bar.
        if let Some(st) = style {
            for (btn, step, x) in [
                (Btn::L1, -1, bar.left() - 6.0 * u - side / 2.0 - 2.0 * u),
                (Btn::R1, 1, bar.right() + 6.0 * u + side / 2.0 + 2.0 * u),
            ] {
                let c = Pos2::new(x, bar.center().y);
                let press = self.pad_press[if step < 0 { 0 } else { 1 }];
                hints::draw_btn(p, c, u, a, st, btn, press);
                self.hits.push((
                    Rect::from_center_size(c, Vec2::new(side, 40.0 * u)),
                    Click::TabStep(step),
                ));
            }
        }
        let cell = w / items.len() as f32;
        let pill_r = (h - 10.0 * u) / 2.0;
        let inset = Vec2::new(4.0 * u, 5.0 * u);
        let last = (items.len() - 1) as f32;
        // The highlight: one pill that slides between the tabs (a spring, so it settles with a
        // small bounce) instead of switching on in the new cell.
        if self.nav_vis > 0.01 {
            let x = bar.left() + cell * self.nav_hl.pos.clamp(0.0, last);
            let pill =
                Rect::from_min_size(Pos2::new(x, bar.top()), Vec2::new(cell, h)).shrink2(inset);
            p.rect_filled(
                pill,
                pill_r,
                fade(Color32::from_rgb(16, 52, 62), a * self.nav_vis),
            );
        }
        if self.ring_vis > 0.01 {
            let x = bar.left() + cell * self.nav_ring.pos.clamp(0.0, last);
            let ring =
                Rect::from_min_size(Pos2::new(x, bar.top()), Vec2::new(cell, h)).shrink2(inset);
            p.rect_stroke(
                ring,
                pill_r,
                Stroke::new(2.0, fade(tokens::FOCUS, a * self.ring_vis)),
                egui::StrokeKind::Inside,
            );
        }
        for (i, item) in items.iter().enumerate() {
            let c = Rect::from_min_size(
                Pos2::new(bar.left() + cell * i as f32, bar.top()),
                Vec2::new(cell, h),
            );
            let col = fade(scene::mix(tokens::ICON_REST, CYAN, self.tab_near(i)), a);
            let tall = lay.kind != LayoutKind::Landscape;
            let icon_c = if tall {
                Pos2::new(c.center().x, c.center().y - 8.0 * u)
            } else {
                Pos2::new(c.left() + 22.0 * u, c.center().y)
            };
            draw_nav_icon(p, *item, icon_c, 9.0 * u, col);
            let label_pos = if tall {
                Pos2::new(c.center().x, c.center().y + 14.0 * u)
            } else {
                Pos2::new(c.left() + 36.0 * u, c.center().y)
            };
            p.text(
                label_pos,
                if tall {
                    Align2::CENTER_CENTER
                } else {
                    Align2::LEFT_CENTER
                },
                item.label(),
                FontId::proportional(tokens::CAPTION * u.max(1.0)),
                col,
            );
            self.hits.push((c, Click::Nav(*item)));
        }
    }

    /// Nav, controller indicators, the setup warning and the stream bar.
    fn draw_chrome(&mut self, ms: &mut MenuState, pc: &mut PageCtx<'_>, warning: Option<&str>) {
        let lay = pc.lay;
        let p = pc.painter;
        let a = pc.appear;
        let u = lay.u;
        let s = lay.safe;
        let desktop = lay.kind == LayoutKind::Desktop;
        let left = s.left() + if desktop { 48.0 * u } else { 20.0 * u };
        let top = s.top() + if desktop { 30.0 * u } else { 16.0 * u };
        // Logo: PSo X ide, with the cyan X.
        let font = FontId::proportional(if desktop { 17.0 * u } else { 16.0 * u });
        let mut job = egui::text::LayoutJob::default();
        for (t, c) in [
            ("PSo", tokens::TEXT),
            ("X", crate::theme::MENU_ACCENT),
            ("ide", tokens::TEXT),
        ] {
            job.append(
                t,
                0.0,
                egui::TextFormat {
                    font_id: font.clone(),
                    color: fade(c, a),
                    ..Default::default()
                },
            );
        }
        let galley = p.layout_job(job);
        let logo_rect = Rect::from_min_size(Pos2::new(left, top), galley.size());
        p.add(Shape::galley(logo_rect.min, galley, tokens::TEXT));
        self.hits.push((logo_rect.expand(6.0), Click::Logo));
        if self.page == Page::Home {
            let side = 34.0 * u;
            let at = if desktop {
                Pos2::new(s.right() - 48.0 * u - side / 2.0, top + 54.0 * u)
            } else {
                Pos2::new(
                    logo_rect.right() + 22.0 * u + side / 2.0,
                    logo_rect.center().y,
                )
            };
            let b = Rect::from_center_size(at, Vec2::splat(side));
            p.rect_stroke(
                b,
                6.0,
                Stroke::new(1.0, fade(tokens::RING, a)),
                egui::StrokeKind::Inside,
            );
            let col = fade(tokens::MUTED, a);
            if self.compact {
                p.rect_stroke(
                    b.shrink(9.0 * u),
                    2.0,
                    Stroke::new(1.4, col),
                    egui::StrokeKind::Inside,
                );
            } else {
                let q = 5.0 * u;
                for (dx, dy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                    p.rect_stroke(
                        Rect::from_center_size(
                            at + Vec2::new(dx * 5.5 * u, dy * 5.5 * u),
                            Vec2::splat(q),
                        ),
                        1.0,
                        Stroke::new(1.3, col),
                        egui::StrokeKind::Inside,
                    );
                }
            }
            self.hits.push((b.expand(4.0), Click::SizeToggle));
            // The button that does the same: Triangle, or Tab on a keyboard.
            if let Some(style) = self.hint_style(lay) {
                let w = hints::btn_width(p, u, style, Btn::Triangle);
                let gc = if desktop {
                    Pos2::new(b.left() - 10.0 * u - w / 2.0, b.center().y)
                } else {
                    Pos2::new(b.right() + 10.0 * u + w / 2.0, b.center().y)
                };
                hints::draw_btn(p, gc, u, a, style, Btn::Triangle, self.pad_press[2]);
                self.hits.push((
                    Rect::from_center_size(gc, Vec2::new(w + 8.0, 30.0 * u)),
                    Click::SizeToggle,
                ));
            }
        }
        let items = nav_items(self.pausing(ms));
        self.tick_nav(&items, ms.appear);
        if desktop {
            self.draw_top_nav(p, lay, &items, logo_rect, u, a);
        } else {
            self.draw_bottom_bar(p, lay, &items, u, a);
        }
        // Controller indicators, top right: small, grey, cyan when in use.
        let mut right_edge = s.right() - if desktop { 48.0 * u } else { 20.0 * u };
        let iy = top + 9.0 * u;
        // On a phone the touch controller's own buttons step aside while the shell is up, so the
        // way back to the game lives in the header: one tap on the X.
        if self.pausing(ms) && !desktop {
            let side = 34.0 * u;
            let b =
                Rect::from_center_size(Pos2::new(right_edge - side / 2.0, iy), Vec2::splat(side));
            p.circle_filled(
                b.center(),
                side / 2.0,
                fade(Color32::from_rgba_unmultiplied(8, 16, 22, 200), a),
            );
            p.circle_stroke(
                b.center(),
                side / 2.0,
                Stroke::new(1.0, fade(tokens::RING, a)),
            );
            let k = side * 0.17;
            let ink = Stroke::new(1.6, fade(tokens::TEXT_2, a));
            let c = b.center();
            p.line_segment([c + Vec2::new(-k, -k), c + Vec2::new(k, k)], ink);
            p.line_segment([c + Vec2::new(-k, k), c + Vec2::new(k, -k)], ink);
            self.hits.push((b.expand(7.0 * u), Click::Close));
            right_edge -= side + 14.0 * u;
        }
        let mut ix = self.feed.touch_ui_left.map_or(right_edge, |x| x - 10.0 * u);
        let touch_on = self.feed.touch;
        let states = [
            (self.feed.touch || ms.touch_mode, 2),
            (self.feed.gamepads > 0, 1),
            (self.feed.keyboard && !touch_on, 0),
        ];
        // On a phone the touch buttons take the top right: the device icons give way.
        let room = self.feed.touch_ui_left.is_none() || desktop;
        for (on, which) in states.into_iter().filter(|_| room) {
            let col = if on { CYAN } else { tokens::RING };
            let c = Pos2::new(ix - 12.0 * u, iy);
            draw_device_icon(p, c, 11.0 * u, which, fade(col, a));
            ix -= 32.0 * u;
        }
        if let Some(text) = warning {
            let (anchor, align) = if desktop {
                (
                    Pos2::new(lay.full.center().x, s.top() + 8.0),
                    Align2::CENTER_TOP,
                )
            } else {
                (
                    Pos2::new(s.left() + 20.0 * u, s.bottom() - 56.0 * u),
                    Align2::LEFT_TOP,
                )
            };
            p.text(
                anchor,
                align,
                text,
                FontId::proportional(12.5 * u),
                fade(tokens::ACCENT, a * 0.8),
            );
        }
        // The disc stream, a hairline at the very bottom.
        if let DemoStatus::Loading(f) = self.feed.demo {
            if f >= 0.0 {
                let r = Rect::from_min_max(
                    Pos2::new(lay.full.left(), lay.full.bottom() - 2.0),
                    Pos2::new(
                        lay.full.left() + lay.full.width() * f.clamp(0.0, 1.0),
                        lay.full.bottom(),
                    ),
                );
                p.rect_filled(r, 0.0, fade(CYAN, a * 0.6));
            }
        }
    }

    fn draw_welcome(&mut self, ms: &MenuState, pc: &mut PageCtx<'_>) {
        pages::draw_welcome(self, ms, pc);
    }

    fn draw_list_page(&mut self, ms: &mut MenuState, ctx: &egui::Context, pc: &mut PageCtx<'_>) {
        pages::draw_list(self, ms, ctx, pc);
    }

    fn handle_pointer(&mut self, ms: &mut MenuState, ctx: &egui::Context) {
        let (pressed, down, released, pos, delta) = ctx.input(|i| {
            (
                i.pointer.any_pressed(),
                i.pointer.primary_down(),
                i.pointer.any_released(),
                i.pointer.interact_pos(),
                i.pointer.delta(),
            )
        });
        // Where the button went down and up, from the events themselves: on a page that gets a
        // few frames a second the pointer has often moved on by the time the frame runs, and a
        // click must land where it was made, not where the mouse is now.
        let (press_at, release_at) = ctx.input(|i| {
            let (mut down_at, mut up_at) = (None, None);
            for event in &i.events {
                if let egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    ..
                } = event
                {
                    if *pressed {
                        down_at = Some(*pos);
                    } else {
                        up_at = Some(*pos);
                    }
                }
            }
            (down_at, up_at)
        });
        if pressed {
            if let Some(pos) = press_at.or(pos) {
                let slider = self.hits.iter().rev().find_map(|(r, c)| match c {
                    Click::Slider(i, track) if r.contains(pos) => Some((*i, *track)),
                    _ => None,
                });
                // A finger put down on a coasting list stops it and presses nothing.
                let caught = self.scrolls_by_finger() && self.kin.grab();
                self.press = Some(Press {
                    start: pos,
                    travel: 0.0,
                    scrolled: caught,
                    on_slider: slider,
                });
            }
        }
        // The letter rail: press or drag along it to jump by letter.
        if down {
            if let (Some((rail, letters)), Some(pos)) = (self.rail.as_ref(), pos) {
                if rail.expand2(Vec2::new(10.0, 0.0)).contains(pos) {
                    let k =
                        (((pos.y - rail.top()) / rail.height()) * letters.len() as f32) as usize;
                    let letter = letters[k.min(letters.len() - 1)];
                    let cases = self.cases(ms);
                    self.cancel_countdown();
                    self.jump_to(&cases, letter);
                    return;
                }
            }
        }
        if let (Some(press), true) = (self.press.as_mut(), down) {
            // On the press frame the pointer jumps to the touch point: not travel.
            if !pressed {
                press.travel += delta.length();
            }
            if let (Some((row, track)), Some(pos)) = (press.on_slider, pos) {
                let t = ((pos.x - track.left()) / track.width()).clamp(0.0, 1.0);
                if let Some(action) = pages::slider_set(ms, row, t) {
                    ms.pending_pointer_action = Some(action);
                }
            } else if press.travel > 10.0 && self.page == Page::Welcome {
                // The pages follow the pointer one for one; letting go decides where they settle.
                press.scrolled = true;
                let at = pos.unwrap_or(press.start);
                let settled = self.pager_drag.is_none()
                    && (self.pager.pos - self.welcome as f32).abs() < 0.01;
                let now = ctx.input(|i| i.time);
                let (base, frame) = (self.pager.pos, self.frame_dt);
                let origin = if settled { press.start.x } else { at.x };
                self.pager_drag
                    .get_or_insert_with(|| PagerDrag::new(base, origin, now, frame))
                    .note(now, at.x);
            } else if press.travel > 10.0
                && matches!(
                    self.page,
                    Page::Game | Page::Settings | Page::Home | Page::Controls
                )
            {
                press.scrolled = true;
                let now = ctx.input(|i| i.time);
                match self.page {
                    Page::Controls => self.controls.drag(&mut self.kin, -delta.y, now),
                    Page::Game | Page::Settings => {
                        let [_, max, top, bottom] = self.list_view;
                        self.kin
                            .drag(&mut self.list_scroll, -delta.y, now, max, bottom - top);
                    }
                    _ => {}
                }
            }
        }
        if !down {
            let now = ctx.input(|i| i.time);
            let mut drag = self.pager_drag.take();
            // A whole swipe inside one frame (a slow page, a quick flick): the press and the release
            // came in the same frame, so there was no drag to follow, only where it began and ended.
            if let (None, true, Page::Welcome, Some(pr), Some(end)) =
                (&drag, released, self.page, self.press, release_at.or(pos))
            {
                if pr.on_slider.is_none() && (end - pr.start).length() > 10.0 {
                    drag = Some(PagerDrag::new(
                        self.pager.pos,
                        pr.start.x,
                        now,
                        self.frame_dt,
                    ));
                }
            }
            if let Some(mut d) = drag {
                self.press = None;
                // Where it let go, from the event: the last move often comes in the same frame.
                if let Some(end) = release_at.or(pos) {
                    d.note(now, end.x);
                }
                self.release_pager(&d, now);
                return;
            }
        }
        if released {
            if self.kin.is_dragging() {
                let now = ctx.input(|i| i.time);
                match self.page {
                    Page::Controls => self.controls.release(&mut self.kin, now),
                    _ => self.kin.release(self.list_scroll, now, self.list_view[1]),
                }
            }
            let press = self.press.take();
            let Some(pos) = release_at.or(pos) else {
                return;
            };
            // A sideways swipe: the next case on Home, the next section elsewhere.
            if let Some(pr) = press {
                let (dx, dy) = (pos.x - pr.start.x, pos.y - pr.start.y);
                if pr.on_slider.is_none() && dx.abs() > 80.0 && dy.abs() < dx.abs() * 0.6 {
                    self.kin.stop();
                    self.cancel_countdown();
                    let step = if dx < 0.0 { 1 } else { -1 };
                    if self.page == Page::Home {
                        let last = self.cases(ms).len().saturating_sub(1) as i32;
                        self.case_focus = (self.case_focus as i32 + step).clamp(0, last) as usize;
                    } else if self.page != Page::Welcome {
                        self.cycle_tab(ms, step);
                    }
                    return;
                }
            }
            if press.is_some_and(|p| p.scrolled || p.on_slider.is_some()) {
                return;
            }
            let hit = self
                .hits
                .iter()
                .rev()
                .find(|(r, _)| r.contains(pos))
                .cloned();
            if let Some((_, click)) = hit {
                self.cancel_countdown();
                if let Some(action) = self.apply_click(ms, click) {
                    ms.pending_pointer_action = Some(action);
                }
            }
        }
    }

    /// The pointer let go of the welcome pages: they glide on to the page they settle on, which
    /// makes the same sound as Next or Back when it is another one.
    fn release_pager(&mut self, d: &PagerDrag, now: f64) {
        let count = pages::WELCOME.len();
        let span = self.pager_span;
        let pos = pager_pos(d.base, d.dx, span, count);
        let vel = d.velocity(now);
        self.pager_last_vel = vel;
        let target = pager_target(pos, vel, self.welcome, span, count);
        if target > self.welcome {
            self.snd_confirm = true;
        } else if target < self.welcome {
            self.snd_back = true;
        }
        self.welcome = target;
        self.pager = motion::Spring::with_velocity(pos, (-vel / span.max(1.0)).clamp(-10.0, 10.0));
    }

    /// Whether the page under a finger scrolls with momentum.
    fn scrolls_by_finger(&self) -> bool {
        matches!(self.page, Page::Game | Page::Settings | Page::Controls)
    }

    fn apply_click(&mut self, ms: &mut MenuState, click: Click) -> Option<MenuAction> {
        match &click {
            Click::Dismiss | Click::Close | Click::WelcomeStep(-1) => self.snd_back = true,
            // A page change speaks for itself; a slider and a drag have their own sounds.
            Click::Nav(_) | Click::TabStep(_) | Click::Slider(..) | Click::Rail | Click::Logo => {}
            _ => self.snd_confirm = true,
        }
        match click {
            Click::Nav(item) => {
                self.open_nav(ms, item);
            }
            Click::Logo => {
                self.page = Page::Home;
                self.zone = Zone::Body;
            }
            Click::Case(i) => {
                let cases = self.cases(ms);
                if i == self.case_focus {
                    let case = cases.get(i)?;
                    let action = case.action.clone();
                    return self.activate(case, &action, case.launches);
                }
                self.case_focus = i.min(cases.len().saturating_sub(1));
            }
            Click::Primary => {
                let cases = self.cases(ms);
                let case = cases.get(self.case_focus)?;
                let action = case.action.clone();
                return self.activate(case, &action, case.launches);
            }
            Click::Restart => {
                let cases = self.cases(ms);
                let case = cases.get(self.case_focus)?;
                let action = case.restart.clone()?;
                return self.activate(case, &action, true);
            }
            Click::Act(action) => return Some(action),
            Click::SizeToggle => self.compact = !self.compact,
            Click::TabStep(step) => self.cycle_tab(ms, step),
            Click::Rail => {}
            Click::Close => return Some(MenuAction::ResumeGame),
            Click::Burn => {
                let cases = self.cases(ms);
                return cases.get(self.case_focus).and_then(|c| c.burn.clone());
            }
            Click::Link(url) => return Some(MenuAction::OpenUrl(url.to_string())),
            Click::Welcome => {
                self.page = Page::Welcome;
                self.welcome_from_start();
            }
            Click::WelcomeStep(step) => {
                let next = self.welcome as i32 + step;
                if next < 0 || next as usize >= pages::WELCOME.len() {
                    self.page = Page::Home;
                } else {
                    self.welcome = next as usize;
                }
            }
            Click::Row(i) => {
                let rows = pages::visible_rows(ms, self.page);
                ms.item_index = i.min(rows.len().saturating_sub(1));
                self.zone = Zone::Body;
                if let Some(row) = rows.get(i) {
                    return Some(row.action.clone());
                }
            }
            Click::RowBurn(i) => {
                let rows = pages::visible_rows(ms, self.page);
                return rows.get(i).and_then(|r| r.burn.clone());
            }
            Click::Dismiss => {
                self.page = Page::Home;
            }
            Click::Ctl(item) => return self.activate_item(ms, &item),
            Click::Slider(..) => {}
        }
        None
    }
}

/// The widest row of a chip flow, from `(start, width)` pairs.
fn widest(rows: &[(f32, f32)]) -> f32 {
    rows.iter().map(|r| r.1).fold(0.0, f32::max)
}

/// What `HomeState::emit_sounds` compares from one frame to the next.
#[derive(Clone, PartialEq)]
struct SoundWatch {
    open: bool,
    page: Page,
    case: usize,
    nav: Option<usize>,
    row: usize,
    row_value: String,
    welcome: usize,
    slot: usize,
}

enum ProgressLine {
    Count(u32, f32),
    Loading(f32),
    Cancelled,
}

fn hash_str(s: &str) -> u32 {
    s.bytes().fold(2166136261u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(16777619)
    })
}

fn draw_device_icon(p: &egui::Painter, c: Pos2, r: f32, which: u8, col: Color32) {
    let stroke = Stroke::new(1.3, col);
    match which {
        // Keyboard: a rounded slab with three rows of keys.
        0 => {
            let rect = Rect::from_center_size(c, Vec2::new(r * 2.0, r * 1.3));
            p.rect_stroke(rect, 2.0, stroke, egui::StrokeKind::Inside);
            for row in 0..3 {
                for k in 0..4 {
                    let x = rect.left() + 3.2 + k as f32 * (rect.width() - 6.4) / 3.0;
                    let y = rect.top() + 3.4 + row as f32 * (rect.height() - 6.8) / 2.0;
                    p.circle_filled(Pos2::new(x, y), 0.7, col);
                }
            }
        }
        // Gamepad: the phosphor glyph.
        1 => {
            p.text(
                c,
                Align2::CENTER_CENTER,
                icons::GAMEPAD_2,
                FontId::proportional(r * 2.3),
                col,
            );
        }
        // Touch: a phone.
        _ => {
            let rect = Rect::from_center_size(c, Vec2::new(r * 1.15, r * 2.0));
            p.rect_stroke(rect, 3.0, stroke, egui::StrokeKind::Inside);
            p.line_segment(
                [
                    rect.center_bottom() - Vec2::new(2.0, 2.6),
                    rect.center_bottom() - Vec2::new(-2.0, 2.6),
                ],
                stroke,
            );
        }
    }
}

/// The focus cage: a 2 px outline, 9 px brackets on the corners and a faint
/// tint, so focus is never colour alone.
/// The four corner brackets of the focused case: they start wider and dimmer
/// and ease in to the case as it takes focus (`e` is its focus, 0 to 1 and a
/// little over while it settles).
pub(super) fn case_brackets(p: &egui::Painter, r: Rect, e: f32, a: f32, u: f32, calm: bool) {
    let k = tokens::ease_out_expo(e.clamp(0.0, 1.0));
    let r = if calm {
        r
    } else {
        r.expand((1.0 - k) * 16.0 * u)
    };
    let alpha = a * k;
    let b = tokens::BRACKET * 1.5 * u;
    let s = Stroke::new(tokens::FOCUS_STROKE + 0.5, fade(tokens::FOCUS, alpha));
    for (corner, dx, dy) in [
        (r.left_top(), 1.0, 1.0),
        (r.right_top(), -1.0, 1.0),
        (r.left_bottom(), 1.0, -1.0),
        (r.right_bottom(), -1.0, -1.0),
    ] {
        p.line_segment([corner, corner + Vec2::new(dx * b, 0.0)], s);
        p.line_segment([corner, corner + Vec2::new(0.0, dy * b)], s);
    }
}

pub(super) fn focus_cage(p: &egui::Painter, r: Rect, a: f32, u: f32) {
    focus_cage_eased(p, r, a, u, 1.0, false);
}

/// [`focus_cage`] while it eases in: `e` runs 0 to 1, the frame and its fill
/// fade up and the corner brackets close in from a little outside.
pub(super) fn focus_cage_eased(p: &egui::Painter, r: Rect, a: f32, u: f32, e: f32, calm: bool) {
    let k = tokens::ease_out_expo(e.clamp(0.0, 1.0));
    let a = a * k;
    p.rect_filled(r, 0.0, fade(tokens::ACCENT, tokens::FOCUS_TINT * a));
    p.rect_stroke(
        r,
        0.0,
        Stroke::new(tokens::FOCUS_STROKE, fade(tokens::FOCUS, a)),
        egui::StrokeKind::Inside,
    );
    let b = tokens::BRACKET * u;
    let s = Stroke::new(tokens::FOCUS_STROKE + 0.5, fade(tokens::FOCUS, a));
    let o = 3.0 + if calm { 0.0 } else { (1.0 - k) * 10.0 * u };
    for (corner, dx, dy) in [
        (r.left_top(), 1.0, 1.0),
        (r.right_top(), -1.0, 1.0),
        (r.left_bottom(), 1.0, -1.0),
        (r.right_bottom(), -1.0, -1.0),
    ] {
        let c = corner + Vec2::new(-dx * o, -dy * o);
        p.line_segment([c, c + Vec2::new(dx * b, 0.0)], s);
        p.line_segment([c, c + Vec2::new(0.0, dy * b)], s);
    }
}

/// The outline of a panel with its one chamfered corner (top right).
pub(super) fn chamfer(r: Rect, cut: f32) -> Vec<Pos2> {
    vec![
        r.left_top(),
        Pos2::new(r.right() - cut, r.top()),
        Pos2::new(r.right(), r.top() + cut),
        r.right_bottom(),
        r.left_bottom(),
    ]
}

/// A small solid triangle: `down` true points down, false points right.
pub(super) fn triangle(p: &egui::Painter, c: Pos2, r: f32, down: bool, col: Color32) {
    let pts = if down {
        vec![
            c + Vec2::new(-r, -r * 0.6),
            c + Vec2::new(r, -r * 0.6),
            c + Vec2::new(0.0, r * 0.7),
        ]
    } else {
        vec![
            c + Vec2::new(-r * 0.6, -r),
            c + Vec2::new(r * 0.7, 0.0),
            c + Vec2::new(-r * 0.6, r),
        ]
    };
    p.add(Shape::convex_polygon(pts, col, Stroke::NONE));
}

/// A small triangle pointing up (the scroll hint at the top of a list).
pub(super) fn triangle_up(p: &egui::Painter, c: Pos2, r: f32, col: Color32) {
    let pts = vec![
        c + Vec2::new(-r, r * 0.6),
        c + Vec2::new(r, r * 0.6),
        c + Vec2::new(0.0, -r * 0.7),
    ];
    p.add(Shape::convex_polygon(pts, col, Stroke::NONE));
}

/// A small line icon for a nav entry.
fn draw_nav_icon(p: &egui::Painter, item: NavItem, c: Pos2, r: f32, col: Color32) {
    let stroke = Stroke::new(1.5, col);
    match item {
        NavItem::Home => {
            let roof = [
                c + Vec2::new(-r, 0.0),
                c + Vec2::new(0.0, -r),
                c + Vec2::new(r, 0.0),
            ];
            p.add(Shape::line(roof.to_vec(), stroke));
            let body = Rect::from_min_max(c + Vec2::new(-r * 0.7, 0.0), c + Vec2::new(r * 0.7, r));
            p.rect_stroke(body, 1.0, stroke, egui::StrokeKind::Inside);
        }
        NavItem::Game => {
            p.add(Shape::convex_polygon(
                vec![
                    c + Vec2::new(-r * 0.6, -r),
                    c + Vec2::new(r * 0.9, 0.0),
                    c + Vec2::new(-r * 0.6, r),
                ],
                Color32::TRANSPARENT,
                stroke,
            ));
        }
        NavItem::Settings => {
            for (i, k) in [0.35_f32, -0.2, 0.5].iter().enumerate() {
                let y = c.y + (i as f32 - 1.0) * r * 0.8;
                p.line_segment([Pos2::new(c.x - r, y), Pos2::new(c.x + r, y)], stroke);
                p.circle_filled(Pos2::new(c.x + k * r, y), r * 0.26, col);
            }
        }
        NavItem::Controls => {
            p.text(
                c,
                Align2::CENTER_CENTER,
                icons::GAMEPAD_2,
                FontId::proportional(r * 2.2),
                col,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(first_visit: bool, demo: DemoStatus) -> MenuState {
        let mut ms = MenuState::new();
        ms.enable_home(first_visit, false);
        ms.set_home_feed(HomeFeed {
            demo,
            keyboard: true,
            ..HomeFeed::default()
        });
        ms
    }

    fn press(ms: &mut MenuState, input: MenuInput) -> Option<MenuAction> {
        ms.update(&input)
    }

    fn tick(ms: &mut MenuState, secs: f32) {
        let mut h = ms.home.take().expect("home");
        h.tick_countdown(secs);
        ms.home = Some(h);
    }

    fn item(title: &str) -> LibraryItem {
        LibraryItem {
            folder: std::path::PathBuf::new(),
            id: format!("path:/games/{title}.cue"),
            title: title.into(),
            subtitle: String::new(),
            burnable: false,
            launchable: true,
        }
    }

    /// The desktop shelf for these game titles: (title, line, action, cover key) per case.
    fn desktop_shelf(titles: &[&str]) -> Vec<(String, String, MenuAction, Option<String>)> {
        let mut ms = MenuState::new();
        ms.enable_home(false, false);
        ms.set_local_demo(true);
        let games: Vec<LibraryItem> = titles.iter().map(|t| item(t)).collect();
        ms.set_library(&games, &[], &[]);
        let h = ms.home.take().expect("home");
        let cases = h.cases(&ms);
        ms.home = Some(h);
        cases
            .into_iter()
            .map(|c| (c.title, c.line, c.action, c.cover_key))
            .collect()
    }

    #[test]
    fn the_desktop_shelf_starts_with_the_newest_public_demo_disc() {
        let shelf = desktop_shelf(&[
            "Quake (quake-psx)",
            "PSoXide Demo Disc v0.48 Private DIAG",
            "PSoXide Demo Disc v0.47",
            "PSoXide Demo Disc v0.45",
            "PSoXide Demo Disc v0.47 Private DIAG",
            "WipEout (USA)",
        ]);
        let titles: Vec<&str> = shelf.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(
            titles,
            [
                "PSoXide Demo Disc",
                "Open a games folder",
                "Demo Disc v0.48 DIAG",
                "Demo Disc v0.47 DIAG",
                "Demo Disc v0.45",
                "Quake (quake-psx)",
                "WipEout (USA)",
            ],
            "the hero, then the other builds newest first, then the alphabetical shelf"
        );
        assert_eq!(
            shelf[0].2,
            MenuAction::LaunchGame("path:/games/PSoXide Demo Disc v0.47.cue".into()),
            "Start disc starts the newest public release"
        );
        assert!(shelf[0].1.contains("v0.47"));
        for demo in [&shelf[0], &shelf[2], &shelf[3], &shelf[4]] {
            let key = demo.3.as_deref().expect("a cover key");
            assert!(
                key.starts_with(covers::DEMO_COVER_PREFIX),
                "own cover: {key}"
            );
        }
        let keys: std::collections::HashSet<_> =
            [0, 2, 3, 4].iter().map(|i| &shelf[*i].3).collect();
        assert_eq!(keys.len(), 4, "each case moves on its own");
        assert!(
            shelf[5]
                .3
                .as_deref()
                .is_some_and(|k| !k.starts_with("embedded:")),
            "other games look their art up as before"
        );
    }

    #[test]
    fn with_only_private_builds_the_hero_points_at_the_download() {
        let shelf = desktop_shelf(&["PSoXide Demo Disc v0.48 Private DIAG"]);
        assert_eq!(shelf[0].0, "PSoXide Demo Disc");
        assert!(
            matches!(shelf[0].2, MenuAction::OpenUrl(_)),
            "{:?}",
            shelf[0].2
        );
        assert_eq!(shelf[2].0, "Demo Disc v0.48 DIAG");
        let nothing = desktop_shelf(&["Quake (quake-psx)"]);
        assert_eq!(nothing[0].0, "PSoXide Demo Disc");
        assert!(matches!(nothing[0].2, MenuAction::OpenUrl(_)));
        assert_eq!(nothing[1].0, "Open a games folder");
        assert_eq!(nothing[2].0, "Quake (quake-psx)");
    }

    #[test]
    fn the_web_shelf_is_untouched_by_the_desktop_rules() {
        let mut ms = home(false, DemoStatus::Ready);
        ms.set_library(
            &[item("PSoXide Demo Disc v0.47"), item("Quake (quake-psx)")],
            &[],
            &[],
        );
        let h = ms.home.take().expect("home");
        let titles: Vec<String> = h.cases(&ms).into_iter().map(|c| c.title).collect();
        ms.home = Some(h);
        assert_eq!(
            titles,
            [
                "PSoXide Demo Disc",
                "Open a games folder",
                "PSoXide Demo Disc v0.47",
                "Quake (quake-psx)"
            ]
        );
    }

    /// Run the clocks the way a drawn frame does, without drawing.
    fn step(ms: &mut MenuState, secs: f32) {
        let mut h = ms.home.take().expect("home");
        h.frame_dt = secs;
        h.tick_motion(ms, secs);
        let items = nav_items(h.pausing(ms));
        h.tick_nav(&items, 1.0);
        ms.home = Some(h);
    }

    #[test]
    fn the_tab_highlight_slides_to_the_page_and_settles_there() {
        let mut ms = home(false, DemoStatus::Absent);
        step(&mut ms, 0.016);
        assert!(ms.home_goto("Controls"));
        // The first frames after the change are on the way, not there.
        step(&mut ms, 0.016);
        let near = ms.home.as_ref().expect("home").nav_hl.pos;
        assert!(
            near < 1.0,
            "the highlight jumped to Controls at once: {near}"
        );
        let mut peak = near;
        for _ in 0..90 {
            step(&mut ms, 1.0 / 60.0);
            peak = peak.max(ms.home.as_ref().expect("home").nav_hl.pos);
        }
        let h = ms.home.as_ref().expect("home");
        // Home, Settings, Controls: Controls is the third tab.
        assert!(
            (h.nav_hl.pos - 2.0).abs() < 0.01,
            "settled at {}",
            h.nav_hl.pos
        );
        assert!(peak < 2.4, "a small overshoot only: {peak}");
    }

    #[test]
    fn reduced_motion_puts_the_highlight_and_every_part_in_place_at_once() {
        let mut ms = home(false, DemoStatus::Absent);
        ms.set_reduce_motion(true);
        step(&mut ms, 0.016);
        ms.home_goto("Controls");
        step(&mut ms, 0.016);
        let h = ms.home.as_ref().expect("home");
        assert_eq!(h.nav_hl.pos, 2.0);
        assert_eq!(h.reveal(5), 1.0);
        assert_eq!(h.home_reveal(0), 1.0);
    }

    #[test]
    fn a_page_change_restarts_the_reveal_and_remembers_the_way_it_went() {
        let mut ms = home(false, DemoStatus::Absent);
        step(&mut ms, 2.0);
        assert_eq!(ms.home.as_ref().expect("home").reveal(0), 1.0);
        ms.home_goto("Settings");
        step(&mut ms, 0.016);
        step(&mut ms, 0.12);
        let h = ms.home.as_ref().expect("home");
        assert!(h.reveal(0) < 0.99, "the first part is still arriving");
        assert!(h.reveal(4) < h.reveal(0), "parts arrive in turn");
        assert_eq!(h.page_dir, 1.0, "Home to Settings goes forward");
        ms.home_goto("Home");
        step(&mut ms, 0.016);
        assert_eq!(ms.home.as_ref().expect("home").page_dir, -1.0);
    }

    #[test]
    fn countdown_waits_for_the_disc_then_starts_it() {
        let mut ms = home(true, DemoStatus::Loading(-1.0));
        tick(&mut ms, 5.0);
        assert_eq!(
            ms.home_probe().1,
            "armed",
            "no countdown while the disc streams"
        );
        ms.set_home_feed(HomeFeed {
            demo: DemoStatus::Ready,
            ..HomeFeed::default()
        });
        tick(&mut ms, 0.1);
        assert_eq!(ms.home_probe().1, "running");
        tick(&mut ms, 2.5);
        assert_eq!(ms.home_probe().1, "running");
        tick(&mut ms, 1.0);
        assert_eq!(ms.home_probe().1, "done");
        assert_eq!(press(&mut ms, MenuInput::default()), None);
        assert!(
            ms.home.as_ref().expect("home").launch.is_some(),
            "the launch sequence starts"
        );
    }

    #[test]
    fn the_first_run_opens_on_the_explainer_and_the_countdown_waits_behind_it() {
        let mut ms = MenuState::new();
        ms.enable_home(true, true);
        ms.set_home_feed(HomeFeed {
            demo: DemoStatus::Ready,
            keyboard: true,
            ..HomeFeed::default()
        });
        tick(&mut ms, 5.0);
        assert_eq!(
            ms.home_probe(),
            ("Welcome", "armed"),
            "no countdown while it is read"
        );
        ms.note_input();
        assert_eq!(ms.home_probe().1, "armed", "reading is not a choice");
        for _ in 0..2 {
            assert_eq!(
                press(
                    &mut ms,
                    MenuInput {
                        confirm: true,
                        ..Default::default()
                    }
                ),
                None
            );
            assert_eq!(
                ms.home_probe().1,
                "armed",
                "turning a page does not cancel it"
            );
        }
        assert_eq!(ms.home_probe().0, "Welcome");
        assert!(!ms.take_explainer_seen(), "not dismissed yet");
        press(
            &mut ms,
            MenuInput {
                confirm: true,
                ..Default::default()
            },
        );
        assert_eq!(ms.home_probe().0, "Home", "the last page closes it");
        tick(&mut ms, 0.1);
        assert_eq!(ms.home_probe().1, "running", "then the countdown starts");
        assert!(ms.take_explainer_seen(), "dismissed, once");
        assert!(!ms.take_explainer_seen());
    }

    #[test]
    fn skipping_the_explainer_also_starts_the_countdown_and_marks_it_seen() {
        let mut ms = MenuState::new();
        ms.enable_home(true, true);
        ms.set_home_feed(HomeFeed {
            demo: DemoStatus::Ready,
            keyboard: true,
            ..HomeFeed::default()
        });
        tick(&mut ms, 0.1);
        press(
            &mut ms,
            MenuInput {
                back: true,
                ..Default::default()
            },
        );
        assert_eq!(ms.home_probe().0, "Home");
        tick(&mut ms, 0.1);
        assert_eq!(ms.home_probe().1, "running");
        assert!(ms.take_explainer_seen());
    }

    #[test]
    fn a_return_visit_has_no_countdown() {
        let mut ms = home(false, DemoStatus::Ready);
        tick(&mut ms, 10.0);
        assert_eq!(ms.home_probe().1, "off");
        assert_eq!(press(&mut ms, MenuInput::default()), None);
    }

    #[test]
    fn any_input_cancels_the_countdown_and_only_cancels() {
        for input in [
            MenuInput {
                up: true,
                ..Default::default()
            },
            MenuInput {
                right: true,
                ..Default::default()
            },
            MenuInput {
                back: true,
                ..Default::default()
            },
            MenuInput {
                confirm: true,
                ..Default::default()
            },
        ] {
            let mut ms = home(true, DemoStatus::Ready);
            tick(&mut ms, 0.1);
            assert_eq!(ms.home_probe().1, "running");
            assert_eq!(
                press(&mut ms, input),
                None,
                "{input:?} must not also start the disc"
            );
            assert_eq!(ms.home_probe().1, "cancelled");
            tick(&mut ms, 10.0);
            assert_eq!(ms.home_probe().1, "cancelled", "stays home");
            assert!(ms.open);
        }
        // A pad button or stick seen outside the menu's own edges cancels too.
        let mut ms = home(true, DemoStatus::Ready);
        tick(&mut ms, 0.1);
        ms.note_input();
        assert_eq!(
            press(
                &mut ms,
                MenuInput {
                    confirm: true,
                    ..Default::default()
                }
            ),
            None
        );
        assert_eq!(
            press(
                &mut ms,
                MenuInput {
                    confirm: true,
                    ..Default::default()
                }
            ),
            None,
            "the next press is a real one"
        );
        assert!(ms.home.as_ref().expect("home").launch.is_some());
    }

    #[test]
    fn confirm_on_the_hero_starts_the_demo_and_the_next_case_opens_a_folder() {
        let mut ms = home(false, DemoStatus::Ready);
        let confirm = MenuInput {
            confirm: true,
            ..Default::default()
        };
        assert_eq!(
            press(&mut ms, confirm),
            None,
            "the disc plays its launch sequence first"
        );
        let launch = ms
            .home
            .as_mut()
            .expect("home")
            .launch
            .take()
            .expect("launching");
        assert_eq!(launch.action, MenuAction::StartDemoDisc);
        press(
            &mut ms,
            MenuInput {
                right: true,
                ..Default::default()
            },
        );
        assert_eq!(press(&mut ms, confirm), Some(MenuAction::ChooseGamesPath));
    }

    #[test]
    fn up_reaches_the_nav_and_enter_opens_a_page_back_returns() {
        let mut ms = home(false, DemoStatus::Ready);
        let key = |f: fn(&mut MenuInput)| {
            let mut i = MenuInput::default();
            f(&mut i);
            i
        };
        press(&mut ms, key(|i| i.up = true));
        press(&mut ms, key(|i| i.right = true));
        press(&mut ms, key(|i| i.confirm = true));
        assert_eq!(ms.home_probe().0, "Settings");
        assert_eq!(ms.selection_probe().0, "Settings");
        press(&mut ms, key(|i| i.back = true));
        assert_eq!(ms.home_probe().0, "Home");
        // Controls is a page of the shell like the others, and Back leaves it.
        press(&mut ms, key(|i| i.up = true));
        for _ in 0..2 {
            press(&mut ms, key(|i| i.right = true));
        }
        press(&mut ms, key(|i| i.confirm = true));
        assert_eq!(ms.home_probe().0, "Controls");
        press(&mut ms, key(|i| i.back = true));
        assert_eq!(ms.home_probe().0, "Home");
    }

    #[test]
    fn l1_and_r1_cycle_the_sections_and_wrap() {
        let mut ms = home(false, DemoStatus::Ready);
        let next = MenuInput {
            tab_next: true,
            ..Default::default()
        };
        let prev = MenuInput {
            tab_prev: true,
            ..Default::default()
        };
        for want in ["Settings", "Controls", "Home", "Settings", "Controls"] {
            press(&mut ms, next);
            assert_eq!(ms.home_probe().0, want);
        }
        press(&mut ms, prev);
        press(&mut ms, prev);
        press(&mut ms, prev);
        assert_eq!(ms.home_probe().0, "Controls", "backwards wraps from Home");
        // Back from any section is Home.
        press(
            &mut ms,
            MenuInput {
                back: true,
                ..Default::default()
            },
        );
        assert_eq!(ms.home_probe().0, "Home");
    }

    #[test]
    fn a_started_game_adds_the_game_section_and_a_resume_case() {
        let mut ms = home(false, DemoStatus::Ready);
        ms.set_game_loaded(true);
        let mut h = ms.home.take().expect("home");
        h.started = true;
        h.feed.current = Some(("game".into(), "A Game".into()));
        ms.home = Some(h);
        let confirm = MenuInput {
            confirm: true,
            ..Default::default()
        };
        assert_eq!(
            press(&mut ms, confirm),
            Some(MenuAction::ResumeGame),
            "the first case is the paused game"
        );
        let next = MenuInput {
            tab_next: true,
            ..Default::default()
        };
        press(&mut ms, next);
        assert_eq!(ms.home_probe().0, "Game");
        assert_eq!(ms.selection_probe().0, "Game");
    }

    #[test]
    fn the_welcome_pages_step_forward_and_back_and_close() {
        let mut ms = home(false, DemoStatus::Absent);
        ms.show_about();
        assert_eq!(ms.panel_probe(), "about");
        press(
            &mut ms,
            MenuInput {
                right: true,
                ..Default::default()
            },
        );
        assert_eq!(ms.home.as_ref().expect("home").welcome, 1);
        press(
            &mut ms,
            MenuInput {
                left: true,
                ..Default::default()
            },
        );
        assert_eq!(ms.home.as_ref().expect("home").welcome, 0);
        press(
            &mut ms,
            MenuInput {
                back: true,
                ..Default::default()
            },
        );
        assert_eq!(ms.panel_probe(), "none");
    }

    #[test]
    fn right_on_the_last_welcome_page_stays_and_confirm_closes_it() {
        let mut ms = home(false, DemoStatus::Absent);
        ms.show_about();
        let key = |f: fn(&mut MenuInput)| {
            let mut i = MenuInput::default();
            f(&mut i);
            i
        };
        for _ in 0..2 {
            press(&mut ms, key(|i| i.right = true));
        }
        assert_eq!(ms.home.as_ref().expect("home").welcome, 2);
        press(&mut ms, key(|i| i.right = true));
        assert_eq!(
            ms.panel_probe(),
            "about",
            "right does not close the last page"
        );
        assert_eq!(ms.home.as_ref().expect("home").welcome, 2);
        press(&mut ms, key(|i| i.confirm = true));
        assert_eq!(ms.panel_probe(), "none", "confirm (Done) closes it");
    }

    #[test]
    fn a_welcome_drag_follows_the_pointer_and_resists_past_the_ends() {
        let (span, count) = (400.0, 3);
        // Dragging left by 100 points moves the pages 100 points toward the next one.
        assert!((pager_pos(0.0, -100.0, span, count) - 0.25).abs() < 1e-6);
        // Dragging right on the first page, or left on the last, moves less than the pointer.
        let first = pager_pos(0.0, 100.0, span, count);
        assert!(first < 0.0 && first > -0.25 * 0.5, "{first}");
        let last = pager_pos(2.0, -100.0, span, count);
        assert!(last > 2.0 && last < 2.125, "{last}");
    }

    #[test]
    fn a_short_or_slow_welcome_swipe_snaps_back_and_a_long_or_quick_one_turns_the_page() {
        let (span, count) = (400.0, 3);
        let at = |moved: f32, vel: f32, page: usize| {
            pager_target(page as f32 + moved / span, vel, page, span, count)
        };
        // Short and slow: back where it was.
        assert_eq!(at(40.0, -80.0, 1), 1);
        assert_eq!(at(-40.0, 80.0, 1), 1);
        // Far enough, even when slow.
        assert_eq!(at(110.0, -50.0, 1), 2);
        assert_eq!(at(-110.0, 50.0, 1), 0);
        // Short but quick.
        assert_eq!(at(45.0, -900.0, 1), 2);
        assert_eq!(at(-45.0, 900.0, 1), 0);
        // A quick flick that has hardly moved is a twitch, not a swipe.
        assert_eq!(at(10.0, -2000.0, 1), 1);
        // Never past the ends: the last page stays, and the first stays.
        assert_eq!(at(300.0, -2000.0, 2), 2);
        assert_eq!(at(-300.0, 2000.0, 0), 0);
        // A flick against the drag does not turn it.
        assert_eq!(at(45.0, 900.0, 1), 1);
    }

    #[test]
    fn a_welcome_flick_is_the_speed_just_before_letting_go() {
        // 60 points in 60 ms is 1000 points per second.
        let mut d = PagerDrag::new(0.0, 100.0, 10.0, 0.0);
        d.note(10.03, 130.0);
        d.note(10.06, 160.0);
        assert!(
            (d.velocity(10.06) - 1000.0).abs() < 1.0,
            "{}",
            d.velocity(10.06)
        );
        assert_eq!(d.dx, 60.0);
        // Held still for longer than the window before letting go: no speed left.
        assert_eq!(d.velocity(10.4), 0.0);
        // A long slow drag has a low speed at the end.
        let mut slow = PagerDrag::new(0.0, 0.0, 0.0, 0.0);
        for i in 1..=30 {
            slow.note(f64::from(i) * 0.05, i as f32 * 4.0);
        }
        assert!(
            (slow.velocity(1.5) - 80.0).abs() < 1.0,
            "{}",
            slow.velocity(1.5)
        );
    }

    #[test]
    fn settings_arrows_step_a_slider_and_flip_a_toggle() {
        let mut ms = home(false, DemoStatus::Absent);
        ms.sync_video_audio(true, "None", 1.0, false);
        for _ in 0..1 {
            press(
                &mut ms,
                MenuInput {
                    tab_next: true,
                    ..Default::default()
                },
            );
        }
        for _ in 0..3 {
            press(
                &mut ms,
                MenuInput {
                    down: true,
                    ..Default::default()
                },
            );
        }
        assert_eq!(ms.selection_probe().1, "Volume");
        assert_eq!(
            press(
                &mut ms,
                MenuInput {
                    right: true,
                    ..Default::default()
                }
            ),
            Some(MenuAction::SetVolume(110))
        );
        assert_eq!(
            press(
                &mut ms,
                MenuInput {
                    left: true,
                    ..Default::default()
                }
            ),
            Some(MenuAction::SetVolume(90))
        );
        press(
            &mut ms,
            MenuInput {
                down: true,
                ..Default::default()
            },
        );
        assert_eq!(ms.selection_probe().1, "Mute");
        assert_eq!(
            press(
                &mut ms,
                MenuInput {
                    right: true,
                    ..Default::default()
                }
            ),
            Some(MenuAction::ToggleMute)
        );
    }

    #[test]
    fn the_ps_button_is_back_before_a_start_and_on_sections() {
        let mut ms = home(true, DemoStatus::Ready);
        assert!(ms.home_blocks_ps_button(), "Escape means Back here");
        ms.open = false;
        press(&mut ms, MenuInput::default());
        ms.open = true;
        press(&mut ms, MenuInput::default());
        assert!(
            !ms.home_blocks_ps_button(),
            "after a start, Escape on Home resumes"
        );
        press(
            &mut ms,
            MenuInput {
                tab_next: true,
                ..Default::default()
            },
        );
        assert!(
            ms.home_blocks_ps_button(),
            "but on a section it goes back to Home"
        );
    }

    #[test]
    fn reopening_the_shell_starts_at_home() {
        let mut ms = home(false, DemoStatus::Ready);
        ms.set_game_loaded(true);
        press(
            &mut ms,
            MenuInput {
                tab_next: true,
                ..Default::default()
            },
        );
        assert_eq!(ms.home_probe().0, "Settings");
        ms.open = false;
        press(&mut ms, MenuInput::default());
        ms.open = true;
        press(&mut ms, MenuInput::default());
        assert_eq!(
            ms.home_probe().0,
            "Home",
            "not the page left open last time"
        );
    }
}
