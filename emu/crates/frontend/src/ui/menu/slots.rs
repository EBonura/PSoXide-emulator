//! The save-state sheet: one card per slot of the running game (the quick slot first), each with
//! the picture taken when it was saved, when, and how far the game had got. Save and Load are two
//! modes of the same sheet, so the slot you load from is the slot you saved to.
//!
//! The model ([`SlotsState`]) is plain data the app fills in from what is stored (files on the
//! desktop, browser storage on the web) and the sheet's rules live here, away from any drawing, so
//! they run as tests: what a click or a button press does in each mode, when it asks first, and
//! how the focus moves over the grid. [`draw`] paints the sheet with egui in the shell's colours
//! and returns what the player chose.

use std::collections::HashMap;

use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use psoxide_settings::slots::{Thumb, QUICK_SLOT, SLOT_COUNT};

use super::home::tokens;
use super::{MenuAction, MenuInput};

/// What the sheet does with a tap on a card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotsMode {
    /// Write the running game into the slot.
    Save,
    /// Replace the running game with what the slot holds.
    Load,
}

impl SlotsMode {
    /// The other mode.
    pub fn other(self) -> Self {
        match self {
            Self::Save => Self::Load,
            Self::Load => Self::Save,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Save => "Save",
            Self::Load => "Load",
        }
    }
}

/// What a filled slot holds, as the sheet shows it.
#[derive(Clone, Debug)]
pub struct SavedInfo {
    /// When it was saved, as a person reads it ("Today 14:32").
    pub when: String,
    /// How long the game had run, "3:07".
    pub played: String,
    /// The save's own time, to tell one picture in a slot from the next.
    pub stamp: u64,
    /// The picture.
    pub thumb: Option<Thumb>,
    /// Why this build cannot load it here, when it cannot.
    pub problem: Option<String>,
    /// A short label for the problem, on the card.
    pub tag: Option<&'static str>,
    /// The program that made it.
    pub creator: String,
    /// The picture's average brightness, 0 to 100, and an 8 by 6 grid of its brightness (0 to 9):
    /// what the browser journeys compare.
    pub luma: u8,
    /// See `luma`.
    pub sig: String,
}

impl SavedInfo {
    /// The card's facts without the picture's bytes, for drawing a card without copying them.
    fn light(&self) -> LightInfo {
        LightInfo {
            when: self.when.clone(),
            played: self.played.clone(),
            problem: self.problem.is_some(),
            tag: self.tag,
            creator: self.creator.clone(),
        }
    }
}

/// What drawing a card needs of a [`SavedInfo`], without the picture.
struct LightInfo {
    when: String,
    played: String,
    problem: bool,
    tag: Option<&'static str>,
    creator: String,
}

/// One slot of the running game.
#[derive(Clone, Debug)]
pub struct SlotCard {
    /// The slot number, 0 for the quick slot.
    pub slot: u8,
    /// What it holds; `None` for an empty slot.
    pub saved: Option<SavedInfo>,
}

impl SlotCard {
    /// An empty slot.
    pub fn empty(slot: u8) -> Self {
        Self { slot, saved: None }
    }
}

/// A question the sheet asks before it does something it cannot take back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ask {
    /// Save over what the slot holds.
    Overwrite(u8),
    /// Throw the slot's save away.
    Delete(u8),
    /// Tell the player why a save cannot be loaded.
    Problem(u8),
}

impl Ask {
    fn slot(self) -> u8 {
        match self {
            Self::Overwrite(s) | Self::Delete(s) | Self::Problem(s) => s,
        }
    }
}

/// The sheet's whole state.
pub struct SlotsState {
    /// Save or Load.
    pub mode: SlotsMode,
    /// The title of the game the slots belong to.
    pub game: String,
    /// One card per slot, the quick slot first.
    pub cards: Vec<SlotCard>,
    /// The focused card, an index into `cards`.
    pub sel: usize,
    /// The open question, if any.
    pub ask: Option<Ask>,
    /// Which answer of the open question has the focus: 0 the action, 1 Cancel.
    pub ask_focus: usize,
    /// A save or load is being worked on (the browser stores a save in the background).
    pub busy: bool,
    /// Columns of the grid as last drawn, for moving the focus by rows.
    pub cols: usize,
    /// The focus moved by key or pad since it was last brought into view.
    pub scroll_to_sel: bool,
    /// Whether the focus ring shows: a key or a pad moved it, not a finger.
    pub focus_visible: bool,
    /// Where each control was drawn last frame, by name, for the browser journeys.
    pub rects: Vec<(String, Rect)>,
    textures: HashMap<(u8, u64), egui::TextureHandle>,
}

impl Default for SlotsState {
    fn default() -> Self {
        Self {
            mode: SlotsMode::Load,
            game: String::new(),
            cards: (0..SLOT_COUNT as u8).map(SlotCard::empty).collect(),
            sel: 0,
            ask: None,
            ask_focus: 0,
            busy: false,
            cols: 3,
            scroll_to_sel: false,
            focus_visible: true,
            rects: Vec::new(),
            textures: HashMap::new(),
        }
    }
}

impl SlotsState {
    /// Replace the cards with what is stored now, keeping the focus where it was.
    pub fn sync(&mut self, game: &str, cards: Vec<SlotCard>) {
        self.game = game.to_string();
        self.cards = cards;
        self.sel = self.sel.min(self.cards.len().saturating_sub(1));
        // A question about a slot that has since changed is no longer the question.
        if self
            .ask
            .is_some_and(|ask| self.card(ask.slot()).is_none_or(|c| c.saved.is_none()))
        {
            self.ask = None;
        }
        let live: Vec<(u8, u64)> = self
            .cards
            .iter()
            .filter_map(|c| c.saved.as_ref().map(|s| (c.slot, s.stamp)))
            .collect();
        self.textures.retain(|key, _| live.contains(key));
    }

    /// Open the sheet in a mode, on the slot most worth acting on: the newest save to load, the
    /// first empty one (else the oldest) to save into.
    pub fn open(&mut self, mode: SlotsMode) {
        self.mode = mode;
        self.ask = None;
        self.ask_focus = 0;
        self.busy = false;
        self.sel = self.best_slot(mode);
        self.scroll_to_sel = true;
    }

    fn best_slot(&self, mode: SlotsMode) -> usize {
        let stamped = |c: &SlotCard| c.saved.as_ref().map(|s| s.stamp);
        match mode {
            SlotsMode::Load => self
                .cards
                .iter()
                .enumerate()
                .filter_map(|(i, c)| stamped(c).map(|t| (t, i)))
                .max()
                .map_or(0, |(_, i)| i),
            SlotsMode::Save => self
                .cards
                .iter()
                .position(|c| c.slot != QUICK_SLOT && c.saved.is_none())
                .or_else(|| {
                    self.cards
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| c.slot != QUICK_SLOT)
                        .filter_map(|(i, c)| stamped(c).map(|t| (t, i)))
                        .min()
                        .map(|(_, i)| i)
                })
                .unwrap_or(0),
        }
    }

    /// The card of a slot.
    pub fn card(&self, slot: u8) -> Option<&SlotCard> {
        self.cards.iter().find(|c| c.slot == slot)
    }

    /// Whether a modal question is open.
    pub fn asking(&self) -> bool {
        self.ask.is_some()
    }

    /// What choosing a card does in the current mode, asking first when it must.
    pub fn choose(&mut self, index: usize) -> Option<MenuAction> {
        if self.busy {
            return None;
        }
        self.sel = index.min(self.cards.len().saturating_sub(1));
        let card = self.cards.get(self.sel)?;
        let slot = card.slot;
        match (self.mode, card.saved.as_ref()) {
            (SlotsMode::Save, None) => Some(MenuAction::SaveSlot(slot)),
            (SlotsMode::Save, Some(_)) => {
                self.ask = Some(Ask::Overwrite(slot));
                self.ask_focus = 1;
                None
            }
            (SlotsMode::Load, None) => None,
            (SlotsMode::Load, Some(saved)) if saved.problem.is_some() => {
                self.ask = Some(Ask::Problem(slot));
                self.ask_focus = 0;
                None
            }
            (SlotsMode::Load, Some(_)) => Some(MenuAction::LoadSlot(slot)),
        }
    }

    /// Ask before throwing the focused card's save away.
    pub fn ask_delete(&mut self, index: usize) {
        if self.busy {
            return;
        }
        if let Some(card) = self.cards.get(index).filter(|c| c.saved.is_some()) {
            self.sel = index;
            self.ask = Some(Ask::Delete(card.slot));
            self.ask_focus = 1;
        }
    }

    /// Answer the open question: the action (`true`) or Cancel.
    pub fn answer(&mut self, confirm: bool) -> Option<MenuAction> {
        let ask = self.ask.take()?;
        if !confirm {
            return None;
        }
        match ask {
            Ask::Overwrite(slot) => Some(MenuAction::SaveSlot(slot)),
            Ask::Delete(slot) => Some(MenuAction::DeleteSlot(slot)),
            Ask::Problem(_) => None,
        }
    }

    /// Move the focus one card in a direction, staying on the grid.
    pub fn step(&mut self, dx: i32, dy: i32) {
        let cols = self.cols.max(1) as i32;
        let count = self.cards.len() as i32;
        let at = self.sel as i32;
        let next = if dx != 0 {
            (at + dx).clamp(0, count - 1)
        } else if at + dy * cols >= 0 && at + dy * cols < count {
            at + dy * cols
        } else {
            at
        };
        if next != at {
            self.sel = next as usize;
            self.scroll_to_sel = true;
        }
    }

    /// Whether the focus is on the first row of the grid.
    pub fn on_top_row(&self) -> bool {
        self.sel < self.cols.max(1)
    }

    /// A press of the keyboard or the pad. `None` for most; an action when one was chosen.
    /// `back` is left to the shell unless a question is open, which Back closes.
    pub fn press(&mut self, input: &MenuInput) -> Option<MenuAction> {
        let any = input.up
            || input.down
            || input.left
            || input.right
            || input.confirm
            || input.back
            || input.secondary
            || input.options;
        if any {
            self.focus_visible = true;
        }
        if self.ask.is_some() {
            if input.left || input.right || input.up || input.down {
                let two = !matches!(self.ask, Some(Ask::Problem(_)));
                if two {
                    self.ask_focus = 1 - self.ask_focus.min(1);
                }
            }
            if input.back {
                self.ask = None;
            } else if input.confirm {
                let confirm = self.ask_focus == 0;
                return self.answer(confirm);
            }
            return None;
        }
        if input.secondary {
            self.mode = self.mode.other();
            return None;
        }
        if input.options {
            self.ask_delete(self.sel);
            return None;
        }
        if input.left {
            self.step(-1, 0);
        }
        if input.right {
            self.step(1, 0);
        }
        if input.down {
            self.step(0, 1);
        }
        if input.up && !self.on_top_row() {
            self.step(0, -1);
        }
        if input.confirm {
            return self.choose(self.sel);
        }
        None
    }

    /// The sheet as JSON, for the browser journeys: the mode, the focus, the question and every
    /// slot with what it holds.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub fn probe_json(&self) -> String {
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let cards: Vec<String> = self
            .cards
            .iter()
            .map(|c| match &c.saved {
                Some(s) => format!(
                    "{{\"slot\":{},\"stamp\":{},\"thumb\":{},\"luma\":{},\"sig\":\"{}\",\"problem\":\"{}\"}}",
                    c.slot,
                    s.stamp,
                    s.thumb.is_some(),
                    s.luma,
                    s.sig,
                    esc(s.problem.as_deref().unwrap_or(""))
                ),
                None => format!("{{\"slot\":{},\"stamp\":0}}", c.slot),
            })
            .collect();
        let ask = match self.ask {
            None => "null".to_string(),
            Some(Ask::Overwrite(s)) => format!("\"overwrite:{s}\""),
            Some(Ask::Delete(s)) => format!("\"delete:{s}\""),
            Some(Ask::Problem(s)) => format!("\"problem:{s}\""),
        };
        let hits: Vec<String> = self
            .rects
            .iter()
            .map(|(name, r)| {
                format!(
                    "[\"{}\",{:.1},{:.1},{:.1},{:.1}]",
                    esc(name),
                    r.left(),
                    r.top(),
                    r.right(),
                    r.bottom()
                )
            })
            .collect();
        format!(
            "{{\"mode\":\"{}\",\"sel\":{},\"ask\":{},\"busy\":{},\"game\":\"{}\",\"cards\":[{}],\"hits\":[{}]}}",
            self.mode.label(),
            self.sel,
            ask,
            self.busy,
            esc(&self.game),
            cards.join(","),
            hits.join(",")
        )
    }
}

// ---- Drawing ---------------------------------------------------------------------------------

/// How the grid lays out in a given width.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GridMetrics {
    pub cols: usize,
    pub card_w: f32,
    pub thumb_h: f32,
    pub card_h: f32,
    pub gap: f32,
}

/// The grid for a sheet `width` by `height` points: two or three columns of 4:3 pictures with two
/// lines of text under each, and on a short screen as many as five, so a row of cards is whole.
/// `u` scales the text and the padding.
pub(crate) fn grid_metrics(width: f32, height: f32, u: f32) -> GridMetrics {
    let gap = 14.0 * u;
    let pad = 8.0 * u;
    let at = |cols: usize| {
        let card_w = (width - gap * (cols as f32 - 1.0)) / cols as f32;
        let thumb_h = (card_w - 2.0 * pad) * 0.75;
        GridMetrics {
            cols,
            card_w,
            thumb_h,
            card_h: thumb_h + 2.0 * pad + 46.0 * u,
            gap,
        }
    };
    let mut cols = (((width + gap) / (200.0 * u + gap)).floor() as usize).clamp(2, 3);
    while cols < 5 && at(cols).card_h > height * 0.95 {
        cols += 1;
    }
    at(cols)
}

const SCRIM: Color32 = Color32::from_rgba_premultiplied(2, 5, 8, 214);
const CARD: Color32 = Color32::from_rgb(10, 20, 28);
const CARD_HOT: Color32 = Color32::from_rgb(14, 30, 40);
const WELL: Color32 = Color32::from_rgb(4, 9, 13);
const WARN: Color32 = Color32::from_rgb(242, 196, 106);

fn with_alpha(c: Color32, a: f32) -> Color32 {
    let [r, g, b, _] = c.to_array();
    Color32::from_rgba_unmultiplied(r, g, b, (a.clamp(0.0, 1.0) * 255.0) as u8)
}

/// Paint the sheet inside `rect` and return what the player chose with the pointer or a finger.
/// `reveal` (0 to 1) fades it in with the page; `touch` makes every target at least 44 points.
pub fn draw(
    ctx: &egui::Context,
    state: &mut SlotsState,
    rect: Rect,
    reveal: f32,
    touch: bool,
) -> Option<MenuAction> {
    let mut chosen = None;
    let u = if touch { 1.1 } else { 1.0 };
    state.rects.clear();
    egui::Area::new(egui::Id::new("slots-sheet"))
        .order(egui::Order::Foreground)
        .fixed_pos(rect.min + Vec2::new(0.0, (1.0 - reveal) * 12.0))
        .show(ctx, |ui| {
            ui.set_opacity(reveal);
            ui.set_clip_rect(rect);
            ui.set_min_size(rect.size());
            ui.set_max_size(rect.size());
            let inner = rect.shrink2(Vec2::new(18.0 * u, 14.0 * u));
            let head = header(ui, state, inner, u, touch, &mut chosen);
            let body =
                Rect::from_min_max(Pos2::new(inner.left(), head.bottom() + 10.0 * u), inner.max);
            grid(ui, state, body, u, touch, &mut chosen);
            if let Some(ask) = state.ask {
                question(ui, state, rect, ask, u, &mut chosen);
            }
        });
    chosen
}

/// The title, the game, and the Save | Load switch. Returns the row's rectangle.
fn header(
    ui: &mut egui::Ui,
    state: &mut SlotsState,
    area: Rect,
    u: f32,
    touch: bool,
    chosen: &mut Option<MenuAction>,
) -> Rect {
    let p = ui.painter().clone();
    let bar_h = if touch { 44.0 } else { 36.0 };
    let seg_w = 84.0 * u;
    let switch = Rect::from_min_size(
        Pos2::new(area.right() - 2.0 * seg_w, area.top()),
        Vec2::new(2.0 * seg_w, bar_h),
    );
    // File chips beside the switch when there is room: Import, and Export of the focused slot.
    let chips = area.width() >= 560.0;
    let chips_w = if chips { 2.0 * (74.0 * u + 8.0) } else { 0.0 };
    let title_w = switch.left() - area.left() - 12.0 - chips_w;
    let title = if state.game.is_empty() {
        "Save states".to_string()
    } else {
        state.game.clone()
    };
    // The title keeps to one line: it shrinks to the room the switch leaves it.
    let mut title_size = 20.0 * u;
    let mut galley = p.layout_no_wrap(
        "Save states".to_string(),
        crate::theme::title_font(title_size),
        tokens::TEXT,
    );
    if galley.size().x > title_w && title_w > 40.0 {
        title_size *= title_w / galley.size().x;
        galley = p.layout_no_wrap(
            "Save states".to_string(),
            crate::theme::title_font(title_size),
            tokens::TEXT,
        );
    }
    p.galley(
        Pos2::new(area.left(), area.top()),
        galley.clone(),
        tokens::TEXT,
    );
    // The game's title, one line, cut short rather than wrapped.
    if title_w > 120.0 {
        let sub = p.layout(
            title,
            FontId::proportional(13.0 * u),
            tokens::MUTED,
            title_w,
        );
        p.galley(
            Pos2::new(area.left(), area.top() + galley.size().y + 2.0),
            sub.clone(),
            tokens::MUTED,
        );
    }
    // The switch: one pill under the active mode.
    p.rect_filled(
        switch,
        bar_h / 2.0,
        Color32::from_rgba_unmultiplied(12, 20, 28, 235),
    );
    p.rect_stroke(
        switch,
        bar_h / 2.0,
        Stroke::new(1.0, Color32::from_rgb(38, 58, 72)),
        egui::StrokeKind::Inside,
    );
    for (i, mode) in [SlotsMode::Save, SlotsMode::Load].into_iter().enumerate() {
        let seg = Rect::from_min_size(
            Pos2::new(switch.left() + i as f32 * seg_w, switch.top()),
            Vec2::new(seg_w, bar_h),
        );
        let on = state.mode == mode;
        if on {
            p.rect_filled(
                seg.shrink(3.0),
                (bar_h - 6.0) / 2.0,
                Color32::from_rgb(16, 52, 62),
            );
        }
        p.text(
            seg.center(),
            Align2::CENTER_CENTER,
            mode.label(),
            FontId::proportional(15.0 * u),
            if on { tokens::FOCUS } else { tokens::DIM },
        );
        let resp = ui.interact(seg, egui::Id::new(("slots-mode", i)), Sense::click());
        state.rects.push((format!("mode:{}", mode.label()), seg));
        if resp.clicked() && !state.asking() {
            state.mode = mode;
            state.focus_visible = false;
        }
    }
    if chips {
        let filled = state
            .cards
            .get(state.sel)
            .is_some_and(|c| c.saved.is_some());
        for (i, (label, name, enabled)) in
            [("Import", "import", true), ("Export", "export", filled)]
                .into_iter()
                .enumerate()
        {
            let x = switch.left() - (2 - i) as f32 * (74.0 * u + 8.0);
            let r = Rect::from_min_size(Pos2::new(x, area.top()), Vec2::new(74.0 * u, bar_h));
            let resp = ui.interact(r, egui::Id::new(("slots-file", i)), Sense::click());
            let live = enabled && !state.asking() && !state.busy;
            let hot = resp.hovered() && live;
            p.rect_filled(
                r,
                bar_h / 2.0,
                if hot {
                    Color32::from_rgb(16, 52, 62)
                } else {
                    Color32::from_rgba_unmultiplied(12, 20, 28, 235)
                },
            );
            p.rect_stroke(
                r,
                bar_h / 2.0,
                Stroke::new(1.0, Color32::from_rgb(38, 58, 72)),
                egui::StrokeKind::Inside,
            );
            p.text(
                r.center(),
                Align2::CENTER_CENTER,
                label,
                FontId::proportional(14.0 * u),
                if live {
                    tokens::TEXT_2
                } else {
                    with_alpha(tokens::DIM, 0.5)
                },
            );
            state.rects.push((format!("file:{name}"), r));
            if resp.clicked() && live {
                state.focus_visible = false;
                *chosen = Some(if i == 0 {
                    MenuAction::ImportSlot
                } else {
                    MenuAction::ExportSlot(state.cards[state.sel].slot)
                });
            }
        }
    }
    Rect::from_min_max(
        area.min,
        Pos2::new(area.right(), area.top() + bar_h.max(galley.size().y + 20.0)),
    )
}

/// The cards, in a scrolling grid.
fn grid(
    ui: &mut egui::Ui,
    state: &mut SlotsState,
    body: Rect,
    u: f32,
    touch: bool,
    chosen: &mut Option<MenuAction>,
) {
    let m = grid_metrics(body.width(), body.height(), u);
    state.cols = m.cols;
    let rows = state.cards.len().div_ceil(m.cols);
    let content_h = rows as f32 * m.card_h + (rows as f32 - 1.0).max(0.0) * m.gap;
    let blocked = state.asking();
    let mut scroll = egui::ScrollArea::vertical()
        .id_salt("slots-scroll")
        .max_height(body.height())
        .auto_shrink([false, false])
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden);
    if blocked {
        scroll = scroll.enable_scrolling(false);
    }
    ui.allocate_new_ui(
        egui::UiBuilder::new()
            .max_rect(body)
            .layout(egui::Layout::top_down(egui::Align::Min)),
        |ui| {
            ui.set_clip_rect(body.expand2(Vec2::new(4.0, 0.0)));
            scroll.show(ui, |ui| {
                let origin = ui.cursor().min;
                ui.allocate_space(Vec2::new(body.width(), content_h + 6.0));
                for index in 0..state.cards.len() {
                    let (row, col) = (index / m.cols, index % m.cols);
                    let min = origin
                        + Vec2::new(
                            col as f32 * (m.card_w + m.gap),
                            row as f32 * (m.card_h + m.gap),
                        );
                    let r = Rect::from_min_size(min, Vec2::new(m.card_w, m.card_h));
                    card(ui, state, index, r, &m, u, touch, chosen);
                    if state.scroll_to_sel && index == state.sel {
                        ui.scroll_to_rect(r.expand(6.0), None);
                        state.scroll_to_sel = false;
                    }
                }
            });
        },
    );
}

#[allow(clippy::too_many_arguments)]
fn card(
    ui: &mut egui::Ui,
    state: &mut SlotsState,
    index: usize,
    r: Rect,
    m: &GridMetrics,
    u: f32,
    touch: bool,
    chosen: &mut Option<MenuAction>,
) {
    let slot = state.cards[index].slot;
    let saved = state.cards[index].saved.as_ref().map(SavedInfo::light);
    let blocked = state.asking();
    let id = egui::Id::new(("slot-card", slot));
    let sense = if blocked {
        Sense::hover()
    } else {
        Sense::click()
    };
    let resp = ui.interact(r, id, sense);
    let visible = ui.clip_rect().intersects(r);
    let focused = index == state.sel && state.focus_visible && !touch;
    let hot = resp.hovered() && !blocked;
    let resp = match saved.as_ref() {
        Some(info) if !info.creator.is_empty() => {
            resp.on_hover_text(format!("Saved by {}", info.creator))
        }
        _ => resp,
    };
    let mode = state.mode;
    let loadable = saved.as_ref().is_some_and(|s| !s.problem);
    // What this card would do, for dimming the ones that would do nothing.
    let live = match mode {
        SlotsMode::Save => true,
        SlotsMode::Load => saved.is_some(),
    };
    state.rects.push((format!("slot:{slot}"), r));
    if visible {
        let p = ui.painter();
        let fill = if hot || focused { CARD_HOT } else { CARD };
        p.rect_filled(r, 8.0, fill);
        let edge = if focused {
            Stroke::new(2.0, tokens::FOCUS)
        } else if hot {
            Stroke::new(1.0, tokens::hairline_strong())
        } else {
            Stroke::new(1.0, tokens::hairline())
        };
        p.rect_stroke(r, 8.0, edge, egui::StrokeKind::Inside);
        let pad = 8.0 * u;
        let well = Rect::from_min_size(
            r.min + Vec2::splat(pad),
            Vec2::new(m.card_w - 2.0 * pad, m.thumb_h),
        );
        p.rect_filled(well, 4.0, WELL);
        let dim = if live { 1.0 } else { 0.4 };
        match saved.as_ref() {
            Some(info) => {
                if let Some(tex) = texture_for(ui.ctx(), state, index) {
                    let tint = if loadable {
                        Color32::WHITE
                    } else {
                        Color32::from_gray(110)
                    };
                    ui.painter().image(
                        tex,
                        well,
                        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                        tint,
                    );
                } else {
                    ui.painter().text(
                        well.center(),
                        Align2::CENTER_CENTER,
                        "No picture",
                        FontId::proportional(13.0 * u),
                        tokens::DIM,
                    );
                }
                if let Some(tag) = info.tag {
                    let t = ui.painter().layout_no_wrap(
                        tag.to_string(),
                        FontId::proportional(12.0 * u),
                        Color32::from_rgb(8, 12, 16),
                    );
                    let chip = Rect::from_min_size(
                        well.left_bottom() + Vec2::new(6.0, -t.size().y - 12.0),
                        t.size() + Vec2::new(12.0, 6.0),
                    );
                    ui.painter().rect_filled(chip, 4.0, WARN);
                    ui.painter().galley(chip.min + Vec2::new(6.0, 3.0), t, WARN);
                }
            }
            None => {
                let (word, col) = match mode {
                    SlotsMode::Save => ("Save here", with_alpha(tokens::ACCENT, 0.9)),
                    SlotsMode::Load => ("Empty", with_alpha(tokens::DIM, dim)),
                };
                ui.painter().text(
                    well.center(),
                    Align2::CENTER_CENTER,
                    word,
                    FontId::proportional(15.0 * u),
                    col,
                );
            }
        }
        // The two lines under the picture.
        let text_x = r.left() + pad + 2.0;
        let name_y = well.bottom() + 8.0 * u;
        let name_col = with_alpha(tokens::TEXT, dim);
        ui.painter().text(
            Pos2::new(text_x, name_y),
            Align2::LEFT_TOP,
            psoxide_settings::slots::slot_name(slot),
            FontId::proportional(15.0 * u),
            name_col,
        );
        if let Some(info) = saved.as_ref() {
            ui.painter().text(
                Pos2::new(text_x, name_y + 20.0 * u),
                Align2::LEFT_TOP,
                &info.when,
                FontId::proportional(12.5 * u),
                tokens::MUTED,
            );
            // How far the game had got, at the end of the name's line.
            if !info.played.is_empty() {
                ui.painter().text(
                    Pos2::new(r.right() - pad - 2.0, name_y + 1.0),
                    Align2::RIGHT_TOP,
                    &info.played,
                    FontId::proportional(13.0 * u),
                    tokens::DIM,
                );
            }
        }
        // The delete mark, top right of a filled card: a small cross.
        if saved.is_some() && !blocked {
            let size = if touch { 40.0 } else { 28.0 };
            let x = Rect::from_min_size(
                Pos2::new(well.right() - size - 2.0, well.top() + 2.0),
                Vec2::splat(size),
            );
            let xr = ui.interact(x, egui::Id::new(("slot-delete", slot)), Sense::click());
            state.rects.push((format!("slot:{slot}:delete"), x));
            let shown = hot || touch || xr.hovered();
            if shown {
                let c = x.center();
                let a = 5.5;
                let col = if xr.hovered() {
                    tokens::FOCUS
                } else {
                    tokens::TEXT_2
                };
                let p = ui.painter();
                p.circle_filled(c, size * 0.42, with_alpha(Color32::BLACK, 0.55));
                let line = Stroke::new(1.6, col);
                p.line_segment([c + Vec2::new(-a, -a), c + Vec2::new(a, a)], line);
                p.line_segment([c + Vec2::new(-a, a), c + Vec2::new(a, -a)], line);
            }
            if xr.clicked() {
                state.ask_delete(index);
                state.focus_visible = false;
                return;
            }
        }
    }
    if resp.clicked() {
        state.focus_visible = false;
        *chosen = state.choose(index).or(chosen.take());
    }
}

/// The picture of a card as a texture, made on first use and kept while the save stays.
fn texture_for(
    ctx: &egui::Context,
    state: &mut SlotsState,
    index: usize,
) -> Option<egui::TextureId> {
    let SlotsState {
        cards, textures, ..
    } = state;
    let card = cards.get(index)?;
    let info = card.saved.as_ref()?;
    let thumb = info.thumb.as_ref()?;
    let key = (card.slot, info.stamp);
    let handle = textures.entry(key).or_insert_with(|| {
        let image =
            egui::ColorImage::from_rgb([usize::from(thumb.w), usize::from(thumb.h)], &thumb.rgb);
        ctx.load_texture(
            format!("slot-{}-{}", card.slot, info.stamp),
            image,
            egui::TextureOptions::LINEAR,
        )
    });
    Some(handle.id())
}

/// The modal question, drawn over the sheet.
fn question(
    ui: &mut egui::Ui,
    state: &mut SlotsState,
    over: Rect,
    ask: Ask,
    u: f32,
    chosen: &mut Option<MenuAction>,
) {
    let card = state.card(ask.slot()).cloned();
    let when = card
        .as_ref()
        .and_then(|c| c.saved.as_ref())
        .map(|s| s.when.clone())
        .unwrap_or_default();
    let name = psoxide_settings::slots::slot_name(ask.slot());
    let (title, body, action) = match ask {
        Ask::Overwrite(_) => (
            format!("Replace {name}?"),
            format!("This writes over the save from {when}. It cannot be undone."),
            Some("Replace"),
        ),
        Ask::Delete(_) => (
            format!("Delete {name}?"),
            format!("The save from {when} will be removed. It cannot be undone."),
            Some("Delete"),
        ),
        Ask::Problem(_) => (
            format!("{name} cannot be loaded"),
            card.and_then(|c| c.saved.and_then(|s| s.problem))
                .unwrap_or_default(),
            None,
        ),
    };
    let p = ui.painter().clone();
    p.rect_filled(over, 0.0, SCRIM);
    // Swallow the pointer over the sheet while the question is open.
    let _ = ui.interact(over, egui::Id::new("slots-scrim"), Sense::click());
    let w = (over.width() - 32.0).min(440.0 * u);
    let body_galley = p.layout(
        body,
        FontId::proportional(14.0 * u),
        tokens::TEXT_2,
        w - 40.0 * u,
    );
    let btn_h = (if u > 1.0 { 46.0 } else { 38.0 }) * 1.0;
    let h = 24.0 * u + 26.0 * u + 10.0 * u + body_galley.size().y + 22.0 * u + btn_h + 20.0 * u;
    let panel = Rect::from_center_size(over.center(), Vec2::new(w, h));
    p.rect_filled(panel, 12.0, Color32::from_rgb(9, 17, 24));
    p.rect_stroke(
        panel,
        12.0,
        Stroke::new(1.0, tokens::hairline_strong()),
        egui::StrokeKind::Inside,
    );
    let x = panel.left() + 20.0 * u;
    let mut y = panel.top() + 22.0 * u;
    let t = p.text(
        Pos2::new(x, y),
        Align2::LEFT_TOP,
        &title,
        crate::theme::title_font(18.0 * u),
        tokens::TEXT,
    );
    y = t.bottom() + 10.0 * u;
    p.galley(Pos2::new(x, y), body_galley.clone(), tokens::TEXT_2);
    y += body_galley.size().y + 22.0 * u;
    let labels: Vec<(&str, bool)> = match action {
        Some(label) => vec![(label, true), ("Cancel", false)],
        None => vec![("OK", false)],
    };
    let bw = ((panel.width() - 40.0 * u - 12.0 * (labels.len() as f32 - 1.0))
        / labels.len() as f32)
        .min(180.0 * u);
    let total = bw * labels.len() as f32 + 12.0 * (labels.len() as f32 - 1.0);
    let mut bx = panel.right() - 20.0 * u - total;
    for (i, (label, confirm)) in labels.iter().enumerate() {
        let b = Rect::from_min_size(Pos2::new(bx, y), Vec2::new(bw, btn_h));
        let primary = *confirm;
        let resp = ui.interact(b, egui::Id::new(("slots-ask", i)), Sense::click());
        let focus = state.focus_visible && state.ask_focus == i;
        let (fill, ink) = if primary {
            (tokens::ACCENT, tokens::ON_ACCENT)
        } else {
            (Color32::from_rgb(16, 30, 40), tokens::TEXT)
        };
        let fill = if resp.hovered() { lighten(fill) } else { fill };
        p.rect_filled(b, btn_h / 2.0, fill);
        if focus {
            p.rect_stroke(
                b.expand(2.0),
                btn_h / 2.0 + 2.0,
                Stroke::new(2.0, tokens::FOCUS),
                egui::StrokeKind::Outside,
            );
        }
        p.text(
            b.center(),
            Align2::CENTER_CENTER,
            *label,
            FontId::proportional(15.0 * u),
            ink,
        );
        let name = if primary {
            "ask:confirm"
        } else if action.is_some() {
            "ask:cancel"
        } else {
            "ask:ok"
        };
        state.rects.push((name.to_string(), b));
        if resp.clicked() {
            *chosen = state.answer(primary).or(chosen.take());
            return;
        }
        bx += bw + 12.0;
    }
}

fn lighten(c: Color32) -> Color32 {
    let [r, g, b, a] = c.to_array();
    Color32::from_rgba_unmultiplied(
        r.saturating_add(22),
        g.saturating_add(22),
        b.saturating_add(22),
        a,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(stamp: u64) -> SavedInfo {
        SavedInfo {
            when: "Today 10:00".into(),
            played: "1:00".into(),
            stamp,
            thumb: None,
            problem: None,
            tag: None,
            creator: "PSoXide/test".into(),
            luma: 0,
            sig: String::new(),
        }
    }

    fn sheet(filled: &[(u8, u64)]) -> SlotsState {
        let mut s = SlotsState::default();
        let cards = (0..SLOT_COUNT as u8)
            .map(|slot| SlotCard {
                slot,
                saved: filled
                    .iter()
                    .find(|(f, _)| *f == slot)
                    .map(|(_, t)| saved(*t)),
            })
            .collect();
        s.sync("Game", cards);
        s
    }

    fn press(f: impl FnOnce(&mut MenuInput)) -> MenuInput {
        let mut input = MenuInput::default();
        f(&mut input);
        input
    }

    #[test]
    fn saving_into_an_empty_slot_just_saves_and_over_a_save_asks() {
        let mut s = sheet(&[(2, 50)]);
        s.mode = SlotsMode::Save;
        assert_eq!(s.choose(1), Some(MenuAction::SaveSlot(1)));
        assert_eq!(s.choose(2), None, "a filled slot asks first");
        assert_eq!(s.ask, Some(Ask::Overwrite(2)));
        assert_eq!(s.ask_focus, 1, "the question starts on Cancel");
        assert_eq!(s.answer(false), None);
        assert!(s.ask.is_none());
        s.choose(2);
        assert_eq!(s.answer(true), Some(MenuAction::SaveSlot(2)));
    }

    #[test]
    fn loading_takes_a_filled_slot_and_ignores_an_empty_one() {
        let mut s = sheet(&[(0, 10), (3, 20)]);
        s.mode = SlotsMode::Load;
        assert_eq!(s.choose(3), Some(MenuAction::LoadSlot(3)));
        assert_eq!(s.choose(0), Some(MenuAction::LoadSlot(0)));
        assert_eq!(s.choose(5), None);
        assert!(s.ask.is_none());
    }

    #[test]
    fn a_save_that_cannot_load_explains_itself_instead() {
        let mut s = sheet(&[(1, 10)]);
        s.cards[1].saved.as_mut().unwrap().problem = Some("different disc".into());
        s.mode = SlotsMode::Load;
        assert_eq!(s.choose(1), None);
        assert_eq!(s.ask, Some(Ask::Problem(1)));
        assert_eq!(s.answer(true), None, "OK just closes it");
        assert!(s.ask.is_none());
    }

    #[test]
    fn delete_asks_and_only_for_a_filled_slot() {
        let mut s = sheet(&[(4, 10)]);
        s.ask_delete(2);
        assert!(s.ask.is_none());
        s.ask_delete(4);
        assert_eq!(s.ask, Some(Ask::Delete(4)));
        assert_eq!(s.answer(true), Some(MenuAction::DeleteSlot(4)));
    }

    #[test]
    fn opening_lands_on_the_newest_save_to_load_and_a_free_slot_to_save() {
        let mut s = sheet(&[(0, 10), (2, 90), (3, 40)]);
        s.open(SlotsMode::Load);
        assert_eq!(s.sel, 2, "the newest save");
        s.open(SlotsMode::Save);
        assert_eq!(s.sel, 1, "the first empty numbered slot");
        let all: Vec<(u8, u64)> = (1..=8).map(|n| (n, u64::from(n) * 10)).collect();
        let mut full = sheet(&all);
        full.open(SlotsMode::Save);
        assert_eq!(
            full.sel, 1,
            "every slot taken: the oldest, never the quick slot"
        );
        let mut none = sheet(&[]);
        none.open(SlotsMode::Load);
        assert_eq!(none.sel, 0);
    }

    #[test]
    fn the_focus_moves_over_the_grid_and_stops_at_its_edges() {
        let mut s = sheet(&[]);
        s.cols = 3;
        s.step(1, 0);
        s.step(0, 1);
        assert_eq!(s.sel, 4);
        s.step(0, 1);
        assert_eq!(s.sel, 7);
        s.step(0, 1);
        assert_eq!(s.sel, 7, "no row below");
        s.step(1, 0);
        s.step(1, 0);
        assert_eq!(s.sel, 8, "the last card is the end");
        s.step(0, -1);
        s.step(0, -1);
        s.step(0, -1);
        assert_eq!(s.sel, 2);
        assert!(s.on_top_row());
    }

    #[test]
    fn keys_choose_switch_and_answer() {
        let mut s = sheet(&[(1, 10)]);
        s.mode = SlotsMode::Save;
        s.sel = 1;
        assert_eq!(
            s.press(&press(|i| i.confirm = true)),
            None,
            "asks to replace"
        );
        assert!(s.asking());
        // Left or right swaps the answer; Confirm on Cancel answers no.
        assert_eq!(s.press(&press(|i| i.confirm = true)), None);
        assert!(!s.asking());
        s.press(&press(|i| i.confirm = true));
        s.press(&press(|i| i.right = true));
        assert_eq!(s.ask_focus, 0);
        assert_eq!(
            s.press(&press(|i| i.confirm = true)),
            Some(MenuAction::SaveSlot(1))
        );
        // Square switches the mode; Back closes a question.
        s.press(&press(|i| i.secondary = true));
        assert_eq!(s.mode, SlotsMode::Load);
        s.press(&press(|i| i.options = true));
        assert!(matches!(s.ask, Some(Ask::Delete(1))));
        s.press(&press(|i| i.back = true));
        assert!(!s.asking());
    }

    #[test]
    fn nothing_happens_while_a_save_is_in_flight() {
        let mut s = sheet(&[(1, 10)]);
        s.busy = true;
        assert_eq!(s.choose(1), None);
        s.ask_delete(1);
        assert!(s.ask.is_none());
    }

    #[test]
    fn a_question_about_a_slot_that_changed_goes_away() {
        let mut s = sheet(&[(1, 10)]);
        s.ask_delete(1);
        assert!(s.asking());
        s.sync("Game", (0..SLOT_COUNT as u8).map(SlotCard::empty).collect());
        assert!(!s.asking());
    }

    #[test]
    fn the_grid_fits_every_width_from_a_small_phone_to_a_desktop() {
        for (w, u) in [
            (320.0, 1.1),
            (360.0, 1.1),
            (369.0, 1.1),
            (700.0, 1.0),
            (1100.0, 1.0),
        ] {
            let m = grid_metrics(w, 900.0, u);
            assert!((2..=3).contains(&m.cols), "{w}: {m:?}");
            let used = m.cols as f32 * m.card_w + (m.cols as f32 - 1.0) * m.gap;
            assert!((used - w).abs() < 0.01, "{w}: used {used}");
            assert!(m.card_w >= 130.0, "{w}: {m:?}");
        }
        assert_eq!(
            grid_metrics(369.0, 700.0, 1.1).cols,
            2,
            "a phone in portrait shows two"
        );
        assert_eq!(grid_metrics(1100.0, 900.0, 1.0).cols, 3);
    }

    #[test]
    fn a_short_screen_gets_more_columns_so_a_whole_row_shows() {
        // A phone held sideways, the sheet 560 by 150 points.
        let m = grid_metrics(560.0, 150.0, 1.1);
        assert!(m.cols >= 4, "{m:?}");
        assert!(m.card_h <= 150.0 * 0.95 + 1.0, "{m:?}");
        assert!(m.card_w >= 95.0, "{m:?}");
        // Plenty of height keeps the big cards.
        assert_eq!(grid_metrics(560.0, 700.0, 1.1).cols, 2);
    }

    #[test]
    fn the_probe_reads_as_json() {
        let mut s = sheet(&[(1, 10)]);
        s.ask_delete(1);
        s.rects.push((
            "slot:1".into(),
            Rect::from_min_size(Pos2::new(1.0, 2.0), Vec2::new(3.0, 4.0)),
        ));
        let json = s.probe_json();
        assert!(json.contains("\"mode\":\"Load\""), "{json}");
        assert!(json.contains("\"ask\":\"delete:1\""), "{json}");
        assert!(json.contains("[\"slot:1\",1.0,2.0,4.0,6.0]"), "{json}");
    }
}
