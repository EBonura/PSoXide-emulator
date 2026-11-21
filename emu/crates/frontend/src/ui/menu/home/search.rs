//! Search on the home shelf: type part of a title and only the matching cases stay.
//!
//! A keyboard opens it with `/` (or Ctrl/Cmd+F) and types into it directly; the window hands
//! the keys over while it is open, so letters bound to pad buttons type instead of pressing
//! them. A controller opens it with Start and types on a strip of letters under the box:
//! left and right pick, Cross types, Square erases, Triangle clears, Circle or Down closes the
//! box and keeps the filter. With a filter up, Back (Esc, Circle) clears it before it does
//! anything else, so getting the whole shelf back is one press.

use egui::{Align2, Color32, FontId, Pos2, Rect, Stroke, Vec2};

use super::hints::{self, Btn, HintStyle};
use super::scene::{fade, CoverKind, CYAN};
use super::{tokens, CaseItem, Click, HomeState, LayoutKind, NavItem, Page, PageCtx, Zone};
use crate::ui::menu::{MenuInput, MenuState};

/// The longest query kept: a title is never longer than this is useful for.
const MAX_QUERY: usize = 32;

/// What the controller's letter strip offers, in order: the space comes last.
pub(super) fn strip_chars() -> impl Iterator<Item = char> {
    ('A'..='Z').chain('0'..='9').chain(std::iter::once(' '))
}

fn strip_len() -> usize {
    strip_chars().count()
}

fn strip_char(i: usize) -> char {
    strip_chars().nth(i).unwrap_or(' ')
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
    /// A controller is typing: the letter strip shows.
    pub(super) strip: bool,
    /// The letter of the strip in focus.
    cursor: usize,
    /// The title in focus when the filter started, to come back to when it is cleared.
    before: Option<String>,
}

impl Search {
    /// A filter is narrowing the shelf.
    pub(super) fn active(&self) -> bool {
        !self.query.trim().is_empty()
    }

    fn shown(&self) -> bool {
        self.editing || !self.query.is_empty()
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
    /// Open the search box on Home. `strip`: a controller opened it, so its letters show.
    pub(super) fn begin_search(&mut self, ms: &mut MenuState, strip: bool) {
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
        self.search.strip |= strip;
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

    /// Close the box and keep what it filters.
    pub(super) fn search_done(&mut self) {
        self.search.editing = false;
        self.search.strip = false;
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

    /// A controller (or a touch on the strip) while the box is open. Every input is the box's.
    pub(super) fn update_search(&mut self, ms: &mut MenuState, input: &MenuInput) {
        let pad = input.left
            || input.right
            || input.confirm
            || input.secondary
            || input.options
            || input.back
            || input.down;
        if pad {
            self.search.strip = true;
        }
        let n = strip_len();
        if input.left {
            self.search.cursor = (self.search.cursor + n - 1) % n;
        }
        if input.right {
            self.search.cursor = (self.search.cursor + 1) % n;
        }
        if input.confirm {
            self.search_type(strip_char(self.search.cursor).to_ascii_lowercase());
        }
        if input.secondary {
            self.search_erase();
        }
        if input.options {
            self.search.query.clear();
            self.refocus_first();
        }
        if input.back || input.down || input.search {
            self.search_done();
            if self.search.query.is_empty() {
                self.clear_search(ms, false);
            }
        }
    }

    /// Put the strip's focus on a letter (headless previews).
    pub(super) fn search_preview_cursor(&mut self, i: usize) {
        self.search.cursor = i.min(strip_len() - 1);
    }

    /// A tap on a letter of the strip: it takes the focus and types.
    pub(super) fn update_search_char(&mut self, i: usize) {
        self.search.cursor = i.min(strip_len() - 1);
        self.search_type(strip_char(self.search.cursor).to_ascii_lowercase());
    }

    /// The search box (and a controller's letter strip) over the top of the shelf.
    pub(super) fn draw_search(&mut self, pc: &PageCtx<'_>, found: usize) {
        if !self.search.shown() {
            return;
        }
        let lay = *pc.lay;
        let p = pc.painter;
        let a = pc.appear;
        let u = lay.u;
        let w = match lay.kind {
            LayoutKind::Desktop => 440.0 * u,
            _ => (lay.safe.width() - 40.0 * u).min(440.0 * u),
        };
        let top = match lay.kind {
            LayoutKind::Desktop => lay.safe.top() + 50.0 * u,
            LayoutKind::Portrait => lay.safe.top() + 22.0 * u,
            LayoutKind::Landscape => lay.safe.top() + 6.0 * u,
        };
        let h = 36.0 * u;
        let bar = Rect::from_min_size(
            Pos2::new(lay.full.center().x - w / 2.0, top),
            Vec2::new(w, h),
        );
        let editing = self.search.editing;
        p.rect_filled(bar, h / 2.0, fade(Color32::from_rgb(8, 18, 26), a * 0.94));
        p.rect_stroke(
            bar,
            h / 2.0,
            Stroke::new(
                if editing { 1.6 } else { 1.0 },
                fade(
                    if editing {
                        tokens::FOCUS
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
        let lens = Pos2::new(bar.left() + 20.0 * u, bar.center().y - 1.0 * u);
        let ring = 5.5 * u;
        let ink = Stroke::new(
            1.6,
            fade(if editing { tokens::FOCUS } else { tokens::DIM }, a),
        );
        p.circle_stroke(lens, ring, ink);
        let d = Vec2::new(1.0, 1.0) * (ring * 0.72);
        p.line_segment([lens + d, lens + d * 1.9], ink);
        let text_x = bar.left() + 38.0 * u;
        let font = FontId::proportional(15.0 * u);
        let shown = if self.search.query.is_empty() {
            p.text(
                Pos2::new(text_x, bar.center().y),
                Align2::LEFT_CENTER,
                "Search your games",
                font.clone(),
                fade(tokens::DIM, a),
            );
            Rect::from_min_size(Pos2::new(text_x, bar.center().y), Vec2::ZERO)
        } else {
            p.text(
                Pos2::new(text_x, bar.center().y),
                Align2::LEFT_CENTER,
                &self.search.query,
                font.clone(),
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
                    Pos2::new(x, bar.center().y - 9.0 * u),
                    Pos2::new(x, bar.center().y + 9.0 * u),
                ],
                Stroke::new(1.4, fade(tokens::FOCUS, a)),
            );
        }
        // How many it found, and the clear button.
        if !self.search.query.is_empty() {
            let clear = Pos2::new(bar.right() - 18.0 * u, bar.center().y);
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
                Pos2::new(clear.x - 18.0 * u, bar.center().y),
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
        if self.search.strip && editing {
            self.draw_strip(pc, bar);
        }
    }

    /// The controller's letters, one row under the box, with what the buttons do.
    fn draw_strip(&mut self, pc: &PageCtx<'_>, bar: Rect) {
        let lay = *pc.lay;
        let p = pc.painter;
        let a = pc.appear;
        let u = lay.u;
        let n = strip_len();
        let span = (lay.safe.width() - 48.0 * u).min(n as f32 * 26.0 * u);
        let cell = span / n as f32;
        let y = bar.bottom() + 24.0 * u;
        let left = lay.full.center().x - span / 2.0;
        let back = Rect::from_min_max(
            Pos2::new(left - 10.0 * u, y - 16.0 * u),
            Pos2::new(left + span + 10.0 * u, y + 16.0 * u),
        );
        p.rect_filled(back, 8.0 * u, fade(Color32::from_rgb(8, 18, 26), a * 0.9));
        for (i, c) in strip_chars().enumerate() {
            let at = Pos2::new(left + cell * (i as f32 + 0.5), y);
            let focus = i == self.search.cursor;
            let r = Rect::from_center_size(at, Vec2::new(cell - 2.0 * u, 26.0 * u));
            if focus {
                p.rect_filled(r, 5.0 * u, fade(CYAN, a));
            }
            let ink = if focus {
                tokens::ON_ACCENT
            } else {
                tokens::TEXT_2
            };
            if c == ' ' {
                // The space key, drawn: the shell's fonts have no open-box glyph.
                let (x, s) = (4.5 * u, Stroke::new(1.4, fade(ink, a)));
                let base = at + Vec2::new(0.0, 3.0 * u);
                p.line_segment([base - Vec2::new(x, 0.0), base + Vec2::new(x, 0.0)], s);
                p.line_segment([base - Vec2::new(x, 0.0), base - Vec2::new(x, 4.0 * u)], s);
                p.line_segment([base + Vec2::new(x, 0.0), base + Vec2::new(x, -4.0 * u)], s);
            } else {
                p.text(
                    at,
                    Align2::CENTER_CENTER,
                    c.to_string(),
                    FontId::proportional(14.0 * u),
                    fade(ink, a),
                );
            }
            self.hits.push((r, Click::SearchChar(i)));
        }
        let style = self.hint_style(&lay).unwrap_or(HintStyle::Pad);
        let row = [
            (Btn::Cross, "Type"),
            (Btn::Square, "Erase"),
            (Btn::Triangle, "Clear"),
            (Btn::Circle, "Done"),
        ];
        // Centre the row: glyph, gap, label, and the gap between hints.
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
        let x = lay.full.center().x - width / 2.0;
        hints::hint_row(p, x, back.bottom() + 18.0 * u, u, a, style, &row, 13.0 * u);
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
    fn strip_has_letters_digits_and_space() {
        assert_eq!(strip_len(), 37);
        assert_eq!(strip_char(0), 'A');
        assert_eq!(strip_char(26), '0');
        assert_eq!(strip_char(36), ' ');
    }
}
