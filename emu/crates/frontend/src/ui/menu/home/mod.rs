//! The home screen: a console-style shell that replaces the old cross media
//! bar. One jewel case on a quiet floor, its title, one line, one button.
//!
//! Home is a view over [`MenuState`]: the same library, settings and game
//! rows, the same [`MenuInput`] edges from keyboard, pad and touch, the same
//! [`MenuAction`]s going out. Only the drawing and the focus rules are new.

mod covers;
mod launch;
mod pages;
mod scene;

use egui::{Align2, Color32, FontId, Pos2, Rect, Shape, Stroke, Vec2};

use super::{LibraryItem, MenuAction, MenuInput, MenuState};
use crate::icons;
use scene::{fade, CoverKind, CYAN};

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
const SITE_URL: &str = "https://ebonura.github.io/PSoXide/";
/// Where the free builds, and the optional donation, live.
const ITCH_URL: &str = "https://bonnie-studios.itch.io/psoxide";
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
    /// The debug section, a full page of the shell.
    Debug,
    Welcome,
}

impl Page {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Game => "Game",
            Self::Controls => "Controls",
            Self::Settings => "Settings",
            Self::Debug => "Debug",
            Self::Welcome => "Welcome",
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
    Debug,
}

impl NavItem {
    fn label(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Game => "Game",
            Self::Controls => "Controls",
            Self::Settings => "Settings",
            Self::Debug => "Debug",
        }
    }

    fn page(self) -> Page {
        match self {
            Self::Home => Page::Home,
            Self::Game => Page::Game,
            Self::Controls => Page::Controls,
            Self::Settings => Page::Settings,
            Self::Debug => Page::Debug,
        }
    }
}

fn nav_items(in_game: bool) -> Vec<NavItem> {
    let mut items = vec![NavItem::Home];
    if in_game {
        items.push(NavItem::Game);
    }
    items.extend([NavItem::Controls, NavItem::Settings, NavItem::Debug]);
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
    Peek,
    Restart,
    Burn,
    Act(MenuAction),
    Link(&'static str),
    Welcome,
    WelcomeStep(i32),
    Row(usize),
    RowBurn(usize),
    Slider(usize, Rect),
    Back,
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
    peek: bool,
    welcome: usize,
    started: bool,
    feed: HomeFeed,
    dither: Option<egui::TextureHandle>,
    list_scroll: f32,
    press: Option<Press>,
    hits: Vec<(Rect, Click)>,
    pub(super) covers: CoverState,
    /// Where the page opened from, for Back.
    first_visit: bool,
    last_row: usize,
    /// The shell was closed since it last opened: the next opening starts at the top.
    closed: bool,
    /// An input already cancelled the countdown this frame (a pad button or a
    /// stick, seen before the menu's own edges): that press only cancels.
    cancelled_this_frame: bool,
    launch: Option<launch::Launch>,
    /// The launch has been handed to the app; the sequence holds on its last moment.
    launch_sent: bool,
    pending_launch: Option<MenuAction>,
    label_tex: Option<egui::TextureHandle>,
    /// Where a Controls or Debug section is drawn this frame (by the shell around it).
    section: Option<Rect>,
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
    pub(super) fn new(first_visit: bool) -> Self {
        Self {
            page: Page::Home,
            zone: Zone::Body,
            nav: 0,
            case_focus: 0,
            case_pos: 0.0,
            time: 0.0,
            countdown: Countdown::Off,
            peek: false,
            welcome: 0,
            started: false,
            feed: HomeFeed::default(),
            dither: None,
            list_scroll: 0.0,
            press: None,
            hits: Vec::new(),
            covers: CoverState::default(),
            first_visit,
            last_row: usize::MAX,
            closed: false,
            cancelled_this_frame: false,
            launch: None,
            launch_sent: false,
            pending_launch: None,
            label_tex: None,
            section: None,
        }
    }

    /// Reopening the shell starts at its top: the pause list of a started
    /// game, or the home page, never a page left open last time.
    fn note_open_state(&mut self, ms: &mut MenuState) {
        if !ms.open {
            self.closed = true;
            self.started = true;
        } else if std::mem::take(&mut self.closed) {
            self.page = Page::Home;
            self.zone = Zone::Body;
            self.peek = false;
            self.list_scroll = 0.0;
            self.last_row = usize::MAX;
            self.launch = None;
            self.launch_sent = false;
            if self.pausing(ms) {
                ms.select_category("Game");
            }
        }
    }

    /// Whether a started game is waiting behind the shell.
    fn pausing(&self, ms: &MenuState) -> bool {
        self.started && ms.game_loaded
    }
}

impl MenuState {
    /// Turn the home screen on, replacing the cross media bar. `first_visit`
    /// arms the demo-disc countdown.
    pub fn enable_home(&mut self, first_visit: bool) {
        self.home = Some(Box::new(HomeState::new(first_visit)));
    }

    /// Whether the home screen is the shell.
    pub fn home_enabled(&self) -> bool {
        self.home.is_some()
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
                    Click::Peek => "peek".into(),
                    Click::Act(action) => format!("act:{action:?}"),
                    Click::Restart => "restart".into(),
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
                    Click::Back => "back".into(),
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
            .map(|c| format!("[\"{}\",\"{}\"]", escape(&c.title), escape(&c.line)))
            .collect();
        let left = match h.countdown {
            Countdown::Running(l) => l,
            _ => -1.0,
        };
        format!(
            "{{\"page\":\"{}\",\"zone\":\"{}\",\"countdown\":\"{}\",\"left\":{:.2},\"case\":{},\"nav\":{},\"peek\":{},\"started\":{},\"in_game\":{},\"launch\":{:.2},\"nav_items\":{},\"first_visit\":{},\"covers\":{},\"cases\":[{}],\"hits\":[{}]}}",
            h.page.name(),
            if h.zone == Zone::Nav { "nav" } else { "body" },
            self.home_probe().1,
            left,
            h.case_focus,
            h.nav,
            h.peek,
            h.started,
            h.pausing(self),
            h.launch.as_ref().map_or(-1.0, |l| l.t),
            nav_items(h.pausing(self)).len(),
            h.first_visit,
            h.covers.loaded(),
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
            "controls" => Page::Controls,
            "debug" => Page::Debug,
            "settings" => Page::Settings,
            "welcome" => Page::Welcome,
            _ => Page::Home,
        };
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
            "Debug" => NavItem::Debug,
            _ => return false,
        };
        let mut shell = self.home.take().expect("home");
        shell.open_nav(self, item);
        self.home = Some(shell);
        true
    }

    /// Where the shell wants a Controls or Debug section drawn this frame.
    pub fn home_section(&self) -> Option<(&'static str, Rect)> {
        let h = self.home.as_ref()?;
        let rect = h.section?;
        match h.page {
            Page::Controls | Page::Debug if self.open => Some((h.page.name(), rect)),
            _ => None,
        }
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

    /// Whether the welcome pages are showing.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(super) fn home_page_is_welcome(&self) -> bool {
        self.home.as_ref().is_some_and(|h| h.page == Page::Welcome)
    }

    /// Open the welcome pages (the Settings "About" row).
    pub(super) fn home_show_welcome(&mut self) {
        if let Some(h) = self.home.as_mut() {
            h.page = Page::Welcome;
            h.welcome = 0;
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
    fn cancel_countdown(&mut self) {
        match self.countdown {
            Countdown::Armed | Countdown::Running(_) => {
                self.countdown = Countdown::Cancelled;
                self.cancelled_this_frame = true;
                remember_visit();
            }
            _ => {}
        }
    }

    /// The shelf: the game you left, the demo disc, your own discs.
    fn cases(&self, ms: &MenuState) -> Vec<CaseItem> {
        let mut out = Vec::new();
        let current = self.feed.current.as_ref().filter(|_| self.pausing(ms));
        let demo_id = crate::app::DEMO_DISC_ID;
        let has_demo =
            ms.homebrew.iter().any(|e| e.id == demo_id) || self.feed.demo != DemoStatus::Absent;
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
                line: "Paused. Pick up where you left off".into(),
                action: MenuAction::ResumeGame,
                button: "Resume",
                cover_key: (!is_demo).then(|| id.clone()),
                restart: Some(MenuAction::LaunchGame(id.clone())),
                burn: None,
                launches: false,
            });
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
            });
        }
        let mut shown: std::collections::HashSet<&str> = std::collections::HashSet::new();
        if let Some((id, _)) = current {
            shown.insert(id.as_str());
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
                });
            }
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
                action: action.clone(),
                kind: case.kind,
                title: case.title.clone(),
                cover_key: case.cover_key.clone(),
                frozen: false,
            });
            self.launch_sent = false;
            self.peek = false;
            return None;
        }
        Some(action.clone())
    }

    fn update(&mut self, ms: &mut MenuState, input: &MenuInput) -> Option<MenuAction> {
        if let Some(action) = ms.pending_pointer_action.take() {
            return Some(action);
        }
        if input.toggle_open {
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
        if !ms.open {
            return None;
        }
        let any = input.up
            || input.down
            || input.left
            || input.right
            || input.confirm
            || input.back
            || input.tab_prev
            || input.tab_next;
        if any {
            // The press that cancels the countdown only cancels it: it does not
            // also press Start disc.
            let counting = matches!(self.countdown, Countdown::Armed | Countdown::Running(_))
                || self.cancelled_this_frame;
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
        // The launch sequence takes input only to skip it, or to give up when
        // the game is slow to come.
        if let Some(l) = self.launch.as_mut() {
            if any {
                if self.launch_sent {
                    self.launch = None;
                    self.launch_sent = false;
                } else if l.t > launch::SKIP_AFTER {
                    l.t = launch::DURATION;
                }
            }
            return None;
        }
        // A panel that floats over the shell takes Back first.
        if input.back && ms.save_states_open {
            ms.close_top_panel();
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
            Page::Controls | Page::Debug => self.update_section(ms, input),
        }
    }

    /// L1/R1 and friends: the next section of the nav, wrapping.
    fn cycle_tab(&mut self, ms: &mut MenuState, step: i32) {
        let items = nav_items(self.pausing(ms));
        let at = items
            .iter()
            .position(|i| i.page() == self.page)
            .unwrap_or(0) as i32;
        let next = (at + step).rem_euclid(items.len() as i32) as usize;
        self.nav = next;
        self.open_nav(ms, items[next]);
    }

    fn update_welcome(&mut self, input: &MenuInput) -> Option<MenuAction> {
        if input.right || input.confirm {
            if self.welcome + 1 >= pages::WELCOME.len() {
                self.page = Page::Home;
            } else {
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
        if input.left && self.case_focus > 0 {
            self.case_focus -= 1;
        }
        if input.right && self.case_focus < last {
            self.case_focus += 1;
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
            if self.peek {
                self.peek = false;
            } else if self.pausing(ms) {
                // Back from a game's shell resumes it.
                ms.open = false;
            }
        }
        None
    }

    /// Show a section of the shell.
    fn open_nav(&mut self, ms: &mut MenuState, item: NavItem) {
        self.page = item.page();
        match item {
            NavItem::Game => ms.select_category("Game"),
            NavItem::Settings => ms.select_category("Settings"),
            _ => {}
        }
        self.zone = Zone::Body;
        self.list_scroll = 0.0;
        self.last_row = usize::MAX;
        self.peek = false;
    }

    /// Controls and Debug: the section draws itself; the shell gives Back and the nav.
    fn update_section(&mut self, ms: &mut MenuState, input: &MenuInput) -> Option<MenuAction> {
        if self.zone == Zone::Nav {
            self.update_nav(ms, input);
            return None;
        }
        if input.up {
            self.zone = Zone::Nav;
            self.nav = nav_items(self.pausing(ms))
                .iter()
                .position(|i| i.page() == self.page)
                .unwrap_or(0);
        }
        if input.back {
            self.page = Page::Home;
        }
        None
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
                        .position(|i| i.page() == self.page)
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
        // The save-states panel floats over everything and owns its layer.
        if ms.save_states_open {
            super::save_states_panel(
                ctx,
                &mut ms.save_states_open,
                &ms.save_rows,
                &mut ms.pending_load_confirm,
                &mut ms.save_thumb_cache,
                &mut ms.pending_pointer_action,
            );
        }
        ms.about_open = false;
        let target = if ms.open { 1.0 } else { 0.0 };
        let k = (super::FADE_SPEED * dt).min(0.5);
        ms.appear += (target - ms.appear) * k;
        if (ms.appear - target).abs() < 0.01 {
            ms.appear = target;
        }
        self.note_open_state(ms);
        if ms.appear <= 0.0 {
            self.hits.clear();
            return;
        }
        ctx.request_repaint();
        self.time += dt;
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
        }
        let appear = ms.appear;
        let full = ctx.screen_rect();
        let layer = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Middle,
            egui::Id::new("menu"),
        ));
        self.hits.clear();
        let lay = Layout::new(full, self.feed.safe);
        let in_game = self.pausing(ms);
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
            let scrim = f32::from(ms.backdrop_pct.max(50)) * 2.55 * 0.8;
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
                Page::Controls | Page::Debug => self.draw_section_frame(&mut pc),
                Page::Game | Page::Settings => self.draw_list_page(ms, ctx, &mut pc),
            }
        }
        if self.launch.is_none() {
            self.draw_chrome(ms, &mut pc, warning);
        }
        self.handle_pointer(ms, ctx);
    }

    fn tick_countdown(&mut self, dt: f32) {
        self.case_pos += (self.case_focus as f32 - self.case_pos) * (1.0 - (-9.0 * dt).exp());
        if (self.case_pos - self.case_focus as f32).abs() < 0.002 {
            self.case_pos = self.case_focus as f32;
        }
        match self.countdown {
            Countdown::Armed if self.feed.demo == DemoStatus::Ready => {
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

fn decode_label(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    const LABEL_PNG: &[u8] =
        include_bytes!("../../../../../../../assets/branding/demo-disc-label.png");
    let rgba = image::load_from_memory(LABEL_PNG).ok()?.to_rgba8();
    let (w, h) = rgba.dimensions();
    let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw());
    Some(ctx.load_texture("demo-disc-label", img, egui::TextureOptions::LINEAR))
}

impl HomeState {
    /// Advance the launch sequence; at its end the action goes to the app and
    /// the last moment holds until the game's boot closes the shell.
    fn tick_launch(&mut self, dt: f32) {
        let Some(l) = self.launch.as_mut() else {
            return;
        };
        if l.frozen {
            return;
        }
        if self.launch_sent {
            // Holding on the spinning disc while the game loads.
            l.t = 4.6 + (l.t - 4.6 + dt) % 0.5;
            return;
        }
        l.t += dt;
        if l.t >= launch::DURATION {
            self.launch_sent = true;
            l.t = 4.6;
            self.pending_launch = Some(l.action.clone());
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
        if let Some(action) = self.pending_launch.take() {
            ms.pending_pointer_action = Some(action);
        }
    }

    /// The frame of a Controls or Debug section; the content comes from the
    /// app around the shell (it needs the emulator state).
    fn draw_section_frame(&mut self, pc: &mut PageCtx<'_>) {
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
                        12.0 * u
                    } else {
                        56.0 * u
                    },
            ),
        );
        pc.painter.rect_filled(
            rect.expand(6.0),
            10.0,
            fade(Color32::from_rgb(8, 14, 20), 0.94 * pc.appear),
        );
        pc.painter.rect_stroke(
            rect.expand(6.0),
            10.0,
            Stroke::new(1.0, fade(CYAN, 0.25 * pc.appear)),
            egui::StrokeKind::Outside,
        );
        self.section = Some(rect);
        if lay.kind != LayoutKind::Desktop {
            let back = pc.painter.text(
                Pos2::new(lay.safe.left() + 20.0 * u, lay.safe.bottom() - 28.0 * u),
                Align2::LEFT_CENTER,
                "\u{2039} Back",
                FontId::proportional(15.0 * u),
                fade(Color32::from_rgb(160, 180, 195), pc.appear),
            );
            self.hits
                .push((back.expand2(Vec2::new(14.0, 14.0)), Click::Back));
        }
    }

    fn draw_home(&mut self, ms: &mut MenuState, ctx: &egui::Context, pc: &mut PageCtx<'_>) {
        let lay = *pc.lay;
        let p = pc.painter;
        let a = pc.appear;
        let u = lay.u;
        let cases = self.cases(ms);
        self.case_focus = self.case_focus.min(cases.len().saturating_sub(1));
        let f = self.case_focus;
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
        let sway = (self.time * 0.57).sin() * 0.13;
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
            let scale = base * (1.0 + 0.14 * near);
            let art = item
                .cover_key
                .as_deref()
                .and_then(|k| self.covers.texture(ctx, k));
            let kind = if art.is_some() {
                CoverKind::Art
            } else {
                item.kind
            };
            let center = scene::V3::new(cx + d * step, cy - near * 10.0 * u, -d.abs() * 90.0 * u);
            let draw = scene::CaseDraw {
                kind,
                title: &item.title,
                art,
                size,
                center,
                yaw: sway * near - d.clamp(-1.5, 1.5) * 0.30,
                pitch: -0.07,
                lid: 0.0,
                scale,
                opacity: a * (0.38 + 0.62 * near),
                seed: hash_str(&item.title),
            };
            scene::draw_case(p, &cam, &draw);
            // Hit area for a click on the case.
            let half = Vec2::new(size.x, size.y) * scale * 0.5 + Vec2::splat(6.0);
            let at = cam.project(center);
            self.hits
                .push((Rect::from_center_size(at, half * 2.0), Click::Case(i)));
        }
        let Some(item) = cases.get(f) else {
            return;
        };
        let text_col = Color32::WHITE;
        let sub_col = Color32::from_rgb(159, 178, 193);
        let (tx, ty, align) = match lay.kind {
            LayoutKind::Landscape => (w * 0.52, h * 0.30, Align2::LEFT_TOP),
            LayoutKind::Portrait => (w * 0.5, h * 0.565, Align2::CENTER_TOP),
            LayoutKind::Desktop => (w * 0.5, h * 0.675, Align2::CENTER_TOP),
        };
        let title_px = match lay.kind {
            LayoutKind::Portrait => 21.0 * u,
            LayoutKind::Landscape => 22.0 * u,
            LayoutKind::Desktop => 28.0 * u,
        };
        let r = p.text(
            Pos2::new(tx, ty),
            align,
            &item.title,
            FontId::proportional(title_px),
            fade(text_col, a),
        );
        let line_px = (if lay.kind == LayoutKind::Desktop {
            16.0
        } else {
            14.0
        }) * u;
        let sub_y = r.bottom() + 8.0 * u;
        let sub = p.text(
            Pos2::new(tx, sub_y),
            align,
            &item.line,
            FontId::proportional(line_px),
            fade(sub_col, a),
        );
        // "What's on it", only on the demo disc and only on demand.
        let mut below = sub.bottom();
        if matches!(item.kind, CoverKind::Demo) {
            let wio_pos = match align {
                Align2::LEFT_TOP => Pos2::new(sub.left(), below + 8.0 * u),
                _ => Pos2::new(tx, below + 8.0 * u),
            };
            let wio = p.text(
                wio_pos,
                align,
                "What's on it",
                FontId::proportional(line_px * 0.92),
                fade(Color32::from_rgb(127, 207, 224), a),
            );
            p.line_segment(
                [wio.left_bottom(), wio.right_bottom()],
                Stroke::new(1.0, fade(Color32::from_rgb(127, 207, 224), a * 0.45)),
            );
            self.hits
                .push((wio.expand2(Vec2::new(10.0, 8.0)), Click::Peek));
            below = wio.bottom();
        }
        // The countdown line, or the loading line, or nothing.
        let mut cd_y = below + 16.0 * u;
        if matches!(item.kind, CoverKind::Demo) && !self.started {
            if let Some(line) = self.progress_line() {
                self.draw_progress_line(p, tx, cd_y, align, u, a, &line);
                cd_y += 22.0 * u;
            }
        }
        // The one button.
        let label = item.button.to_uppercase();
        let bw = (label.chars().count() as f32 * 9.0 + 64.0) * u;
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
        p.rect_filled(btn, bh / 2.0, fade(CYAN, a));
        if focused {
            p.rect_stroke(
                btn.expand(3.0 * u),
                bh / 2.0 + 3.0 * u,
                Stroke::new(1.5, fade(Color32::from_rgb(140, 236, 251), a * 0.8)),
                egui::StrokeKind::Outside,
            );
        }
        p.text(
            btn.center(),
            Align2::CENTER_CENTER,
            label,
            FontId::proportional(13.0 * u),
            fade(Color32::from_rgb(4, 34, 42), a),
        );
        self.hits.push((btn, Click::Primary));
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
                fade(Color32::from_rgb(111, 195, 214), a),
            )
        };
        if item.restart.is_some() {
            let r = quiet(p, "Start over", qy);
            self.hits
                .push((r.expand2(Vec2::new(12.0, 8.0)), Click::Restart));
            qy = r.bottom() + 6.0 * u;
        }
        if item.burn.is_some() {
            let r = quiet(p, "Burn to CD", qy);
            self.hits
                .push((r.expand2(Vec2::new(12.0, 8.0)), Click::Burn));
        }
        // Quiet links at the bottom.
        self.draw_footer(pc);
        if self.peek && matches!(item.kind, CoverKind::Demo) {
            self.draw_peek(pc);
        }
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
        let col = fade(Color32::from_rgb(207, 227, 236), a);
        let dim = fade(Color32::from_rgb(120, 140, 155), a);
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
        let dim = fade(Color32::from_rgb(127, 147, 162), a);
        let link = fade(Color32::from_rgb(111, 195, 214), a);
        let y = s.bottom() - 26.0 * u;
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
        // Controller hints on the desktop shell; touch and phones go without.
        if lay.kind == LayoutKind::Desktop && !self.feed.touch {
            let mut x = what.right() + 40.0 * u;
            let mut hint = |cross: bool, label: &str| {
                let c = Pos2::new(x + 9.0 * u, y);
                let ring = Stroke::new(1.2, fade(Color32::from_rgb(74, 89, 103), a));
                p.circle_stroke(c, 9.0 * u, ring);
                let g = Stroke::new(1.3, dim);
                if cross {
                    let d = 3.4 * u;
                    p.line_segment([c + Vec2::new(-d, -d), c + Vec2::new(d, d)], g);
                    p.line_segment([c + Vec2::new(-d, d), c + Vec2::new(d, -d)], g);
                } else {
                    p.circle_stroke(c, 3.6 * u, g);
                }
                let r = p.text(
                    Pos2::new(x + 24.0 * u, y),
                    Align2::LEFT_CENTER,
                    label,
                    FontId::proportional(13.0 * u),
                    dim,
                );
                x = r.right() + 24.0 * u;
            };
            hint(true, "Select");
            hint(false, "Back");
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

    fn draw_peek(&mut self, pc: &mut PageCtx<'_>) {
        let lay = pc.lay;
        let p = pc.painter;
        let a = pc.appear;
        let u = lay.u;
        let (x, y, wd) = match lay.kind {
            LayoutKind::Desktop => (
                lay.safe.left() + 48.0 * u,
                lay.safe.top() + 110.0 * u,
                250.0 * u,
            ),
            _ => (
                lay.safe.center().x - 130.0 * u,
                lay.safe.top() + 120.0 * u,
                260.0 * u,
            ),
        };
        let rows = pages::DISC_CONTENTS;
        let hgt = (rows.len() as f32 * 26.0 + 52.0) * u;
        let r = Rect::from_min_size(Pos2::new(x, y), Vec2::new(wd, hgt));
        p.rect_filled(r, 2.0, fade(Color32::from_rgb(9, 21, 28), a * 0.92));
        p.rect_filled(
            Rect::from_min_size(r.min, Vec2::new(2.0, r.height())),
            0.0,
            fade(CYAN, a),
        );
        p.text(
            r.min + Vec2::new(18.0 * u, 18.0 * u),
            Align2::LEFT_CENTER,
            "WHAT'S ON THE DISC",
            FontId::proportional(10.5 * u),
            fade(Color32::from_rgb(143, 164, 179), a),
        );
        for (i, name) in rows.iter().enumerate() {
            p.text(
                r.min + Vec2::new(18.0 * u, 46.0 * u + i as f32 * 26.0 * u),
                Align2::LEFT_CENTER,
                *name,
                FontId::proportional(14.5 * u),
                fade(Color32::from_rgb(207, 224, 232), a),
            );
        }
        self.hits.push((r, Click::Peek));
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
            ("PSo", Color32::WHITE),
            ("X", crate::theme::MENU_ACCENT),
            ("ide", Color32::WHITE),
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
        p.add(Shape::galley(logo_rect.min, galley, Color32::WHITE));
        self.hits.push((logo_rect.expand(6.0), Click::Logo));
        // Nav as plain text.
        let mut x = if desktop {
            logo_rect.right() + 34.0 * u
        } else {
            left
        };
        let ny = if desktop {
            logo_rect.center().y
        } else {
            logo_rect.bottom() + 20.0 * u
        };
        let items = nav_items(self.pausing(ms));
        let gap = if desktop { 26.0 * u } else { 18.0 * u };
        for (i, item) in items.iter().enumerate() {
            let focused = self.zone == Zone::Nav && self.nav == i;
            let on = item.page() == self.page || focused;
            let col = if on {
                Color32::WHITE
            } else {
                Color32::from_rgb(111, 129, 146)
            };
            let r = p.text(
                Pos2::new(x, ny),
                Align2::LEFT_CENTER,
                item.label(),
                FontId::proportional(if desktop { 15.5 * u } else { 15.0 * u }),
                fade(col, a),
            );
            if on {
                let c = Pos2::new(r.center().x, r.bottom() + 5.0 * u);
                let d = 3.0 * u;
                p.add(Shape::convex_polygon(
                    vec![
                        c + Vec2::new(0.0, -d),
                        c + Vec2::new(d, 0.0),
                        c + Vec2::new(0.0, d),
                        c + Vec2::new(-d, 0.0),
                    ],
                    fade(
                        if focused {
                            Color32::from_rgb(140, 236, 251)
                        } else {
                            CYAN
                        },
                        a,
                    ),
                    Stroke::NONE,
                ));
            }
            self.hits
                .push((r.expand2(Vec2::new(10.0, 12.0)), Click::Nav(*item)));
            x = r.right() + gap;
        }
        // Controller indicators, top right: small, grey, cyan when in use.
        let mut ix = self
            .feed
            .touch_ui_left
            .map_or(s.right() - if desktop { 48.0 * u } else { 20.0 * u }, |x| {
                x - 10.0 * u
            });
        let iy = top + 9.0 * u;
        let touch_on = self.feed.touch;
        let states = [
            (self.feed.touch || ms.touch_mode, 2),
            (self.feed.gamepads > 0, 1),
            (self.feed.keyboard && !touch_on, 0),
        ];
        for (on, which) in states {
            let col = if on {
                CYAN
            } else {
                Color32::from_rgb(70, 84, 98)
            };
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
                fade(Color32::from_rgb(127, 207, 224), a * 0.8),
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
        if pressed {
            if let Some(pos) = pos {
                let slider = self.hits.iter().rev().find_map(|(r, c)| match c {
                    Click::Slider(i, track) if r.contains(pos) => Some((*i, *track)),
                    _ => None,
                });
                self.press = Some(Press {
                    start: pos,
                    travel: 0.0,
                    scrolled: false,
                    on_slider: slider,
                });
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
            } else if press.travel > 10.0
                && matches!(self.page, Page::Game | Page::Settings | Page::Home)
            {
                press.scrolled = true;
                self.list_scroll = (self.list_scroll - delta.y).max(0.0);
            }
        }
        if released {
            let press = self.press.take();
            let Some(pos) = pos else {
                return;
            };
            // A sideways swipe: the next case on Home, the next section elsewhere.
            if let Some(pr) = press {
                let (dx, dy) = (pos.x - pr.start.x, pos.y - pr.start.y);
                if pr.on_slider.is_none() && dx.abs() > 80.0 && dy.abs() < dx.abs() * 0.6 {
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
            } else if self.peek {
                self.peek = false;
            }
        }
    }

    fn apply_click(&mut self, ms: &mut MenuState, click: Click) -> Option<MenuAction> {
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
            Click::Burn => {
                let cases = self.cases(ms);
                return cases.get(self.case_focus).and_then(|c| c.burn.clone());
            }
            Click::Peek => self.peek = !self.peek,
            Click::Link(url) => return Some(MenuAction::OpenUrl(url.to_string())),
            Click::Welcome => {
                self.page = Page::Welcome;
                self.welcome = 0;
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
            Click::Back => {
                if self.page == Page::Home {
                    ms.open = false;
                } else {
                    self.page = Page::Home;
                }
            }
            Click::Dismiss => {
                self.page = Page::Home;
            }
            Click::Slider(..) => {}
        }
        None
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn home(first_visit: bool, demo: DemoStatus) -> MenuState {
        let mut ms = MenuState::new();
        ms.enable_home(first_visit);
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
        press(&mut ms, key(|i| i.right = true));
        press(&mut ms, key(|i| i.confirm = true));
        assert_eq!(ms.home_probe().0, "Settings");
        assert_eq!(ms.selection_probe().0, "Settings");
        press(&mut ms, key(|i| i.back = true));
        assert_eq!(ms.home_probe().0, "Home");
        // Debug is a page of the shell like the others, and Back leaves it.
        press(&mut ms, key(|i| i.up = true));
        for _ in 0..3 {
            press(&mut ms, key(|i| i.right = true));
        }
        press(&mut ms, key(|i| i.confirm = true));
        assert_eq!(ms.home_probe().0, "Debug");
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
        for want in ["Controls", "Settings", "Debug", "Home", "Controls"] {
            press(&mut ms, next);
            assert_eq!(ms.home_probe().0, want);
        }
        press(&mut ms, prev);
        press(&mut ms, prev);
        assert_eq!(ms.home_probe().0, "Debug", "backwards wraps from Home");
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
    fn settings_arrows_step_a_slider_and_flip_a_toggle() {
        let mut ms = home(false, DemoStatus::Absent);
        ms.sync_video_audio(true, "None", 1.0, false);
        for _ in 0..2 {
            press(
                &mut ms,
                MenuInput {
                    tab_next: true,
                    ..Default::default()
                },
            );
        }
        for _ in 0..4 {
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
        assert_eq!(ms.home_probe().0, "Controls");
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
