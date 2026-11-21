//! Search on the home shelf: type part of a title and only the matching cases stay.
//!
//! The box lives in the header, next to the tabs (a row of its own under the header on a
//! portrait phone), and is the last stop of the tab row for the pad and the arrows.
//!
//! A keyboard opens it with `/` (or Ctrl/Cmd+F) and types into it directly; the window hands
//! the keys over while it is open, so letters bound to pad buttons type instead of pressing
//! them. A controller (Start, or Cross on the box) and a touch screen get a whole keyboard
//! laid out on screen: the D-pad walks it, Cross presses a key, Square erases, Triangle
//! clears, Circle or Start closes it keeping the filter. With a filter up, Back (Esc, Circle)
//! clears it before it does anything else, so getting the whole shelf back is one press.

use egui::{Align2, Color32, FontId, Pos2, Rect, Stroke, Vec2};

use super::hints::{self, Btn};
use super::scene::{fade, CoverKind, CYAN};
use super::{tokens, CaseItem, Click, HomeState, Layout, LayoutKind, NavItem, Page, PageCtx, Zone};
use crate::ui::menu::{MenuInput, MenuState};

/// The longest query kept: a title is never longer than this is useful for.
const MAX_QUERY: usize = 32;

/// A key of the on-screen keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kb {
    Ch(char),
    Space,
    Erase,
    Clear,
    Done,
}

impl Kb {
    /// How wide it is, in letter keys.
    fn width(self) -> f32 {
        match self {
            Self::Ch(_) => 1.0,
            Self::Erase => 2.0,
            Self::Space => 5.0,
            Self::Clear | Self::Done => 2.5,
        }
    }
}

/// The rows of the on-screen keyboard: where each row starts (in letter keys) and its keys.
/// QWERTY, the way every phone and console lays it out, so a key is where the eye looks.
fn rows() -> [(f32, Vec<Kb>); 5] {
    let ch = |s: &str| s.chars().map(Kb::Ch).collect::<Vec<_>>();
    let mut third = ch("ZXCVBNM");
    third.push(Kb::Erase);
    [
        (0.0, ch("1234567890")),
        (0.0, ch("QWERTYUIOP")),
        (0.5, ch("ASDFGHJKL")),
        (0.5, third),
        (0.0, vec![Kb::Space, Kb::Clear, Kb::Done]),
    ]
}

/// The key at `(row, col)`, if there is one.
pub(super) fn key_at(row: usize, col: usize) -> Option<Kb> {
    rows().get(row).and_then(|r| r.1.get(col).copied())
}

/// The left edge and width of every key of a row, in letter keys.
fn row_spans(row: usize) -> Vec<(f32, f32)> {
    let rows = rows();
    let Some((start, keys)) = rows.get(row) else {
        return Vec::new();
    };
    let mut x = *start;
    keys.iter()
        .map(|k| {
            let span = (x, k.width());
            x += k.width();
            span
        })
        .collect()
}

/// The key of `row` under the middle of key `col` of row `from` (moving up or down).
fn nearest(from: usize, col: usize, row: usize) -> usize {
    let mid = row_spans(from).get(col).map_or(0.0, |(x, w)| x + w / 2.0);
    row_spans(row)
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            let da = (a.0 + a.1 / 2.0 - mid).abs();
            let db = (b.0 + b.1 / 2.0 - mid).abs();
            da.total_cmp(&db)
        })
        .map_or(0, |(i, _)| i)
}

/// A key the window passes to the search box while it is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SearchKey {
    Type(char),
    Erase,
    /// Close the box and keep the filter (Enter, the arrows).
    Done,
    /// Clear the filter and close the box (Esc).
    Cancel,
}

#[derive(Debug, Default)]
pub(super) struct Search {
    pub(super) query: String,
    /// The box has the keys: typing goes into it.
    pub(super) editing: bool,
    /// The on-screen keyboard shows (a controller or a finger is typing).
    pub(super) keys: bool,
    /// The key in focus: row, then key along the row.
    cursor: (usize, usize),
    /// The title in focus when the filter started, to come back to when it is cleared.
    before: Option<String>,
}

impl Search {
    /// A filter is narrowing the shelf.
    pub(super) fn active(&self) -> bool {
        !self.query.trim().is_empty()
    }
}

/// Lower case, every run of punctuation one space: "Half-Life" and "half life" read the same.
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut gap = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            gap = false;
        } else if !gap {
            out.push(' ');
            gap = true;
        }
    }
    out.trim_end().to_string()
}

/// Whether `title` matches `query`: every word of the query is part of the title, in any order.
pub(super) fn matches(title: &str, query: &str) -> bool {
    let title = normalize(title);
    normalize(query)
        .split(' ')
        .filter(|w| !w.is_empty())
        .all(|w| title.contains(w))
}

/// The cases the filter keeps: the matching ones, never the folder actions.
pub(super) fn filter(cases: Vec<CaseItem>, query: &str) -> Vec<CaseItem> {
    if normalize(query).is_empty() {
        return cases;
    }
    cases
        .into_iter()
        .filter(|c| c.kind != CoverKind::Ghost && matches(&c.title, query))
        .collect()
}

impl HomeState {
    /// Open the search box on Home. `keys`: a controller or a finger opened it, so the
    /// on-screen keyboard comes up.
    pub(super) fn begin_search(&mut self, ms: &mut MenuState, keys: bool) {
        if self.page != Page::Home {
            self.open_nav(ms, NavItem::Home);
        }
        self.zone = Zone::Body;
        if !self.search.active() {
            self.search.before = self
                .all_cases(ms)
                .get(self.case_focus)
                .map(|c| c.title.clone());
        }
        self.search.editing = true;
        self.search.keys |= keys;
    }

    /// Whether the on-screen keyboard is wanted for what is driving the shell now.
    pub(super) fn wants_screen_keys(&self) -> bool {
        self.feed.gamepads > 0 || self.feed.touch
    }

    /// One key from the window while the box is open.
    pub(super) fn search_key(&mut self, ms: &mut MenuState, key: SearchKey) {
        match key {
            SearchKey::Type(c) => self.search_type(c),
            SearchKey::Erase => self.search_erase(),
            SearchKey::Done => self.search_done(),
            SearchKey::Cancel => self.clear_search(ms, false),
        }
    }

    fn search_type(&mut self, c: char) {
        if c.is_control() || self.search.query.chars().count() >= MAX_QUERY {
            return;
        }
        // No leading space, and no two in a row: they would match nothing more.
        if c == ' ' && (self.search.query.is_empty() || self.search.query.ends_with(' ')) {
            return;
        }
        self.search.query.push(c);
        self.refocus_first();
    }

    fn search_erase(&mut self) {
        if self.search.query.pop().is_some() {
            self.refocus_first();
        }
    }

    fn search_wipe(&mut self) {
        if !self.search.query.is_empty() {
            self.search.query.clear();
            self.refocus_first();
        }
    }

    /// Close the box and keep what it filters.
    pub(super) fn search_done(&mut self) {
        self.search.editing = false;
        self.search.keys = false;
        if self.search.query.trim().is_empty() {
            self.search.query.clear();
        }
    }

    /// The whole shelf again. `keep_pick`: the case in focus among the results keeps the focus
    /// (it was chosen); otherwise the focus goes back to where it was before the search.
    pub(super) fn clear_search(&mut self, ms: &mut MenuState, keep_pick: bool) {
        let now = keep_pick
            .then(|| self.cases(ms).get(self.case_focus).map(|c| c.title.clone()))
            .flatten();
        let want = now.or(self.search.before.take());
        self.search = Search {
            cursor: self.search.cursor,
            ..Search::default()
        };
        let all = self.all_cases(ms);
        if let Some(i) = want.and_then(|t| all.iter().position(|c| c.title == t)) {
            self.case_focus = i;
            self.case_pos = i as f32;
            self.grid_target = None;
        }
    }

    /// The query changed: the first result takes the focus, at once.
    fn refocus_first(&mut self) {
        self.case_focus = 0;
        self.case_pos = 0.0;
        self.grid_scroll = 0.0;
        self.grid_target = None;
    }

    /// Press a key of the on-screen keyboard.
    pub(super) fn press_screen_key(&mut self, ms: &mut MenuState, key: Kb) {
        match key {
            Kb::Ch(c) => self.search_type(c.to_ascii_lowercase()),
            Kb::Space => self.search_type(' '),
            Kb::Erase => self.search_erase(),
            Kb::Clear => self.search_wipe(),
            Kb::Done => self.close_screen_keys(ms),
        }
    }

    /// Close the box from the on-screen keyboard: an empty box gives the shelf back as it was.
    fn close_screen_keys(&mut self, ms: &mut MenuState) {
        self.search_done();
        if self.search.query.is_empty() {
            self.clear_search(ms, false);
        }
    }

    /// A tap on a key: it takes the focus and is pressed.
    pub(super) fn tap_screen_key(&mut self, ms: &mut MenuState, row: usize, col: usize) {
        self.search.keys = true;
        if let Some(key) = key_at(row, col) {
            self.search.cursor = (row, col);
            self.press_screen_key(ms, key);
        }
    }

    /// Put the keyboard's focus on a key (headless previews).
    pub(super) fn search_preview_cursor(&mut self, row: usize, col: usize) {
        if key_at(row, col).is_some() {
            self.search.cursor = (row, col);
        }
    }

    /// A controller (or a touch) while the box is open: every input is the box's.
    pub(super) fn update_search(&mut self, ms: &mut MenuState, input: &MenuInput) {
        let pad = input.left
            || input.right
            || input.up
            || input.down
            || input.confirm
            || input.secondary
            || input.options;
        if pad && !self.search.keys {
            // The first press from a pad brings the keyboard up; it does nothing else.
            self.search.keys = true;
            return;
        }
        let (mut row, mut col) = self.search.cursor;
        let len = |r: usize| rows()[r].1.len();
        if input.left {
            col = (col + len(row) - 1) % len(row);
        }
        if input.right {
            col = (col + 1) % len(row);
        }
        if input.up && row > 0 {
            col = nearest(row, col, row - 1);
            row -= 1;
        }
        if input.down && row + 1 < rows().len() {
            col = nearest(row, col, row + 1);
            row += 1;
        }
        self.search.cursor = (row, col.min(len(row) - 1));
        if input.confirm {
            if let Some(key) = key_at(row, col) {
                self.press_screen_key(ms, key);
            }
        }
        if input.secondary {
            self.search_erase();
        }
        if input.options {
            self.search_wipe();
        }
        if input.back || input.search {
            self.close_screen_keys(ms);
        }
    }

    /// Where the search box goes in the header, given the logo and the left edge of the
    /// device icons. On a portrait phone it is a row of its own under the header.
    pub(super) fn search_rect(&self, lay: &Layout, logo: Rect, icons_left: f32) -> Rect {
        let u = lay.u;
        let h = 34.0 * u;
        match lay.kind {
            LayoutKind::Portrait => Rect::from_min_size(
                Pos2::new(lay.safe.left() + 20.0 * u, logo.bottom() + 14.0 * u),
                Vec2::new(lay.safe.width() - 40.0 * u, h),
            ),
            LayoutKind::Desktop | LayoutKind::Landscape => {
                let after = if lay.kind == LayoutKind::Desktop {
                    self.nav_end
                } else {
                    // The size toggle and its hint sit right of the logo on a phone.
                    logo.right() + 110.0 * u
                };
                let right = icons_left - 20.0 * u;
                let w = (right - after - 28.0 * u).clamp(140.0 * u, 320.0 * u);
                Rect::from_min_size(
                    Pos2::new(right - w, logo.center().y - h / 2.0),
                    Vec2::new(w, h),
                )
            }
        }
    }

    /// The search box in the header, and the on-screen keyboard when it is up.
    pub(super) fn draw_search(&mut self, pc: &PageCtx<'_>, bar: Rect, found: usize) {
        let lay = *pc.lay;
        let p = pc.painter;
        let a = pc.appear;
        let u = lay.u;
        let h = bar.height();
        let editing = self.search.editing;
        // The box is the tab row's last stop: the pad's focus can rest on it.
        let focus = editing || (self.zone == Zone::Nav && self.nav_on_search);
        p.rect_filled(bar, h / 2.0, fade(Color32::from_rgb(8, 18, 26), a * 0.94));
        p.rect_stroke(
            bar,
            h / 2.0,
            Stroke::new(
                if focus { 1.6 } else { 1.0 },
                fade(
                    if focus {
                        tokens::FOCUS
                    } else if self.search.active() {
                        tokens::ACCENT
                    } else {
                        tokens::hairline_strong()
                    },
                    a,
                ),
            ),
            egui::StrokeKind::Inside,
        );
        self.hits.push((bar, Click::Search));
        // The lens.
        let lens = Pos2::new(bar.left() + 18.0 * u, bar.center().y - 1.0 * u);
        let ring = 5.0 * u;
        let lit = focus || self.search.active();
        let ink = Stroke::new(1.6, fade(if lit { tokens::FOCUS } else { tokens::DIM }, a));
        p.circle_stroke(lens, ring, ink);
        let d = Vec2::new(1.0, 1.0) * (ring * 0.72);
        p.line_segment([lens + d, lens + d * 1.9], ink);
        let text_x = bar.left() + 34.0 * u;
        let font = FontId::proportional(14.5 * u);
        let shown = if self.search.query.is_empty() {
            p.text(
                Pos2::new(text_x, bar.center().y),
                Align2::LEFT_CENTER,
                "Search games",
                font,
                fade(tokens::DIM, a),
            );
            Rect::from_min_size(Pos2::new(text_x, bar.center().y), Vec2::ZERO)
        } else {
            p.text(
                Pos2::new(text_x, bar.center().y),
                Align2::LEFT_CENTER,
                &self.search.query,
                font,
                fade(tokens::TEXT, a),
            )
        };
        if editing && (self.time * 1.8).fract() < 0.6 {
            let x = if self.search.query.is_empty() {
                text_x
            } else {
                shown.right() + 2.0 * u
            };
            p.line_segment(
                [
                    Pos2::new(x, bar.center().y - 8.0 * u),
                    Pos2::new(x, bar.center().y + 8.0 * u),
                ],
                Stroke::new(1.4, fade(tokens::FOCUS, a)),
            );
        }
        if self.search.query.is_empty() {
            // The key that opens it, at the box's right end, when nothing is typed.
            if !editing {
                if let Some(style) = self.hint_style(&lay) {
                    let w = hints::btn_width(p, u, style, Btn::Start);
                    hints::draw_btn(
                        p,
                        Pos2::new(bar.right() - 10.0 * u - w / 2.0, bar.center().y),
                        u,
                        a,
                        style,
                        Btn::Start,
                        0.0,
                    );
                }
            }
        } else {
            // How many it found, and the clear button.
            let clear = Pos2::new(bar.right() - 17.0 * u, bar.center().y);
            let x = 4.5 * u;
            let st = Stroke::new(1.5, fade(tokens::MUTED, a));
            p.line_segment([clear + Vec2::new(-x, -x), clear + Vec2::new(x, x)], st);
            p.line_segment([clear + Vec2::new(-x, x), clear + Vec2::new(x, -x)], st);
            self.hits.push((
                Rect::from_center_size(clear, Vec2::splat(28.0 * u)),
                Click::SearchClear,
            ));
            let count = match found {
                0 => "No match".to_string(),
                1 => "1 game".to_string(),
                n => format!("{n} games"),
            };
            p.text(
                Pos2::new(clear.x - 16.0 * u, bar.center().y),
                Align2::RIGHT_CENTER,
                count,
                FontId::proportional(12.5 * u),
                fade(
                    if found == 0 {
                        tokens::MUTED
                    } else {
                        tokens::DIM
                    },
                    a,
                ),
            );
        }
        if self.search.keys && editing {
            self.draw_screen_keys(pc, bar);
        }
    }

    /// The whole keyboard on screen: under the box on a desktop, along the bottom on a phone
    /// (where the thumbs are).
    fn draw_screen_keys(&mut self, pc: &PageCtx<'_>, bar: Rect) {
        let lay = *pc.lay;
        let p = pc.painter;
        let a = pc.appear;
        let u = lay.u;
        let style = self.hint_style(&lay);
        let desktop = lay.kind == LayoutKind::Desktop;
        let pad = 12.0 * u;
        let gap = 5.0 * u;
        let (unit, key_h) = if desktop {
            (46.0 * u, 42.0 * u)
        } else {
            let unit = ((lay.safe.width() - 24.0 * u - 2.0 * pad) / 10.0).min(56.0 * u);
            (unit, (unit * 0.92).clamp(36.0 * u, 48.0 * u))
        };
        let n_rows = rows().len() as f32;
        let hint_h = if style.is_some() { 34.0 * u } else { 0.0 };
        let size = Vec2::new(
            10.0 * unit + 2.0 * pad,
            n_rows * key_h + (n_rows - 1.0) * gap + 2.0 * pad + hint_h,
        );
        let panel = if desktop {
            let left = (bar.right() - size.x).max(lay.safe.left() + 24.0 * u);
            Rect::from_min_size(Pos2::new(left, bar.bottom() + 10.0 * u), size)
        } else {
            let bottom = lay.safe.bottom() - lay.bar_reserve() - 8.0 * u;
            Rect::from_min_size(
                Pos2::new(lay.full.center().x - size.x / 2.0, bottom - size.y),
                size,
            )
        };
        p.rect_filled(
            panel,
            12.0 * u,
            fade(Color32::from_rgb(7, 15, 22), a * 0.97),
        );
        p.rect_stroke(
            panel,
            12.0 * u,
            Stroke::new(1.0, fade(tokens::hairline_strong(), a)),
            egui::StrokeKind::Inside,
        );
        // The panel swallows taps between keys, so they do not reach the cases under it.
        self.hits.push((panel, Click::SearchPanel));
        for (r, (_, keys)) in rows().iter().enumerate() {
            let y = panel.top() + pad + r as f32 * (key_h + gap);
            for (c, ((x, w), key)) in row_spans(r).into_iter().zip(keys.iter()).enumerate() {
                let rect = Rect::from_min_size(
                    Pos2::new(panel.left() + pad + x * unit, y),
                    Vec2::new(w * unit - gap, key_h),
                );
                let focus = self.search.cursor == (r, c);
                p.rect_filled(
                    rect,
                    7.0 * u,
                    fade(
                        if focus {
                            CYAN
                        } else {
                            Color32::from_rgb(16, 30, 40)
                        },
                        a,
                    ),
                );
                let ink = fade(
                    if focus {
                        tokens::ON_ACCENT
                    } else {
                        tokens::TEXT_2
                    },
                    a,
                );
                let label = match key {
                    Kb::Ch(ch) => ch.to_string(),
                    Kb::Space => "Space".into(),
                    Kb::Erase => "Erase".into(),
                    Kb::Clear => "Clear".into(),
                    Kb::Done => "Done".into(),
                };
                let px = if matches!(key, Kb::Ch(_)) { 17.0 } else { 14.0 };
                p.text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    label,
                    FontId::proportional(px * u),
                    ink,
                );
                self.hits.push((rect, Click::SearchKey(r, c)));
            }
        }
        // The buttons that skip the walk across the keys.
        if let Some(style) = style {
            let row = [
                (Btn::Cross, "Press"),
                (Btn::Square, "Erase"),
                (Btn::Triangle, "Clear"),
                (Btn::Circle, "Done"),
            ];
            let width: f32 = row
                .iter()
                .map(|(btn, label)| {
                    let text = p.layout_no_wrap(
                        (*label).to_string(),
                        FontId::proportional(13.0 * u),
                        tokens::DIM,
                    );
                    hints::btn_width(p, u, style, *btn) + 8.0 * u + text.size().x
                })
                .sum::<f32>()
                + 22.0 * u * (row.len() - 1) as f32;
            hints::hint_row(
                p,
                panel.center().x - width / 2.0,
                panel.bottom() - pad - hint_h / 2.0 + 4.0 * u,
                u,
                a,
                style,
                &row,
                13.0 * u,
            );
        }
    }

    /// Nothing on the shelf matches: say so where the cases would be.
    pub(super) fn draw_no_match(&self, pc: &PageCtx<'_>) {
        let lay = *pc.lay;
        let p = pc.painter;
        let a = pc.appear;
        let u = lay.u;
        let c = Pos2::new(lay.full.center().x, lay.full.height() * 0.45);
        p.text(
            c,
            Align2::CENTER_CENTER,
            format!(
                "Nothing matches \u{201c}{}\u{201d}",
                self.search.query.trim()
            ),
            crate::theme::title_font(18.0 * u),
            fade(tokens::TEXT_2, a),
        );
        p.text(
            c + Vec2::new(0.0, 30.0 * u),
            Align2::CENTER_CENTER,
            "Erase a letter, or clear the search to see every game",
            FontId::proportional(14.0 * u),
            fade(tokens::MUTED, a),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_match_in_any_order_ignoring_case_and_punctuation() {
        assert!(matches("Half-Life", "half life"));
        assert!(matches("Half-Life", "LIFE"));
        assert!(matches("Metal Gear Solid (Disc 1)", "solid metal"));
        assert!(matches("Crash Bandicoot", "  crash  "));
        assert!(!matches("Crash Bandicoot", "crash team"));
        assert!(matches("WipEout 2097", "2097"));
        assert!(matches("anything", ""));
    }

    #[test]
    fn the_keyboard_has_every_letter_and_digit_once() {
        let mut seen: Vec<char> = rows()
            .iter()
            .flat_map(|(_, keys)| keys.iter())
            .filter_map(|k| match k {
                Kb::Ch(c) => Some(*c),
                _ => None,
            })
            .collect();
        seen.sort_unstable();
        let want: Vec<char> = ('0'..='9').chain('A'..='Z').collect();
        assert_eq!(seen, want);
        // Every row fits ten letter keys.
        for r in 0..rows().len() {
            let end = row_spans(r).last().map_or(0.0, |(x, w)| x + w);
            assert!(end <= 10.0, "row {r} ends at {end}");
        }
    }

    #[test]
    fn up_and_down_land_on_the_key_underneath() {
        // Q (row 1, first) is under 1; A is between Q and W, nearer Q's right half.
        assert_eq!(nearest(1, 0, 0), 0);
        // P (last of row 1) goes down to L.
        assert_eq!(key_at(2, nearest(1, 9, 2)), Some(Kb::Ch('L')));
        // From V down to Space; from Erase down to Done.
        assert_eq!(key_at(4, nearest(3, 3, 4)), Some(Kb::Space));
        assert_eq!(key_at(4, nearest(3, 7, 4)), Some(Kb::Done));
    }
}
