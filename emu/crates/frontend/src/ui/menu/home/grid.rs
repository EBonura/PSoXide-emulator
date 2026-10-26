//! The compact grid of the home screen, the letter jump and its badge.
//!
//! Home is one screen at two scales: big cases on the floor, or a grid of
//! small cases for a big folder. The cases, the focus and the actions are the
//! same; only the layout differs.

use egui::{Align2, Color32, Pos2, Rect, Shape, Stroke, Vec2};

use super::scene::{fade, Camera, CaseDraw, CoverKind, CYAN, V3};
use super::{tokens, CaseItem, Click, HomeState, LayoutKind, PageCtx};
use crate::ui::menu::MenuState;

/// The first letter a case is filed under: `#` for digits and symbols.
pub(super) fn letter_of(title: &str) -> char {
    title
        .chars()
        .find(|c| c.is_alphanumeric())
        .map_or('#', |c| {
            if c.is_ascii_alphabetic() {
                c.to_ascii_uppercase()
            } else {
                '#'
            }
        })
}

/// The index of the first shelf case of each letter, in order.
pub(super) fn letter_groups(cases: &[CaseItem]) -> Vec<(char, usize)> {
    let mut groups: Vec<(char, usize)> = Vec::new();
    for (i, c) in cases.iter().enumerate().filter(|(_, c)| c.shelf) {
        let l = letter_of(&c.title);
        if groups.last().is_none_or(|g| g.0 != l) {
            groups.push((l, i));
        }
    }
    groups
}

impl HomeState {
    /// Jump to the previous or next first letter of the shelf (L2 and R2).
    pub(super) fn jump_letter(&mut self, cases: &[CaseItem], step: i32) {
        let groups = letter_groups(cases);
        if groups.is_empty() {
            return;
        }
        let cur = cases
            .get(self.case_focus)
            .filter(|c| c.shelf)
            .map(|c| letter_of(&c.title));
        let pick = match (cur, step > 0) {
            (None, true) => groups.first(),
            (None, false) => groups.last(),
            (Some(l), true) => groups.iter().find(|g| g.0 > l).or(groups.first()),
            (Some(l), false) => groups.iter().rev().find(|g| g.0 < l).or(groups.last()),
        };
        if let Some(&(letter, index)) = pick {
            self.case_focus = index;
            self.badge = Some((letter, 1.2));
        }
    }

    /// Jump to a letter by name (the rail on a phone).
    pub(super) fn jump_to(&mut self, cases: &[CaseItem], letter: char) {
        let groups = letter_groups(cases);
        let pick = groups
            .iter()
            .find(|g| g.0 >= letter)
            .or_else(|| groups.last());
        if let Some(&(l, index)) = pick {
            self.case_focus = index;
            self.badge = Some((l, 0.8));
        }
    }

    /// The big letter that shows for a moment after a jump.
    pub(super) fn draw_badge(&mut self, pc: &PageCtx<'_>, dt: f32) {
        let Some((letter, left)) = self.badge else {
            return;
        };
        let alpha = (left / 0.5).min(1.0) * pc.appear;
        let c = pc.lay.full.center();
        let r = 64.0 * pc.lay.u;
        pc.painter.rect_filled(
            Rect::from_center_size(c, Vec2::splat(r * 2.0)),
            12.0,
            fade(Color32::from_rgba_unmultiplied(8, 16, 22, 230), alpha),
        );
        pc.painter.rect_stroke(
            Rect::from_center_size(c, Vec2::splat(r * 2.0)),
            12.0,
            Stroke::new(1.5, fade(CYAN, alpha)),
            egui::StrokeKind::Inside,
        );
        pc.painter.text(
            c,
            Align2::CENTER_CENTER,
            letter.to_string(),
            crate::theme::title_font(r * 1.1),
            fade(tokens::TEXT, alpha),
        );
        self.badge = (left - dt > 0.0).then_some((letter, left - dt));
    }

    /// The grid of small cases. Returns the rect below it for the focused
    /// case's title and button.
    pub(super) fn draw_grid(
        &mut self,
        ms: &mut MenuState,
        ctx: &egui::Context,
        pc: &mut PageCtx<'_>,
        cases: &[CaseItem],
    ) -> Rect {
        let lay = *pc.lay;
        let p = pc.painter;
        let a = pc.appear;
        let u = lay.u;
        let (cols, cell_w, top) = match lay.kind {
            LayoutKind::Desktop => (6usize, 168.0 * u, lay.safe.top() + 96.0 * u),
            LayoutKind::Portrait => (
                3,
                (lay.safe.width() - 56.0 * u) / 3.0,
                lay.safe.top() + 70.0 * u,
            ),
            LayoutKind::Landscape => (
                6,
                (lay.safe.width() - 96.0 * u) / 6.0,
                lay.safe.top() + 44.0 * u,
            ),
        };
        self.grid_cols = cols;
        let cell_h = cell_w * 1.18;
        let bottom_info = match lay.kind {
            LayoutKind::Desktop => 118.0 * u,
            LayoutKind::Portrait => 120.0 * u,
            LayoutKind::Landscape => 8.0 * u,
        };
        let area = Rect::from_min_max(
            Pos2::new(lay.safe.left() + 28.0 * u, top),
            Pos2::new(
                lay.safe.right()
                    - 28.0 * u
                    - if lay.kind == LayoutKind::Portrait {
                        22.0 * u
                    } else {
                        0.0
                    },
                lay.safe.bottom() - lay.bar_reserve() - bottom_info,
            ),
        );
        let rows_total = cases.len().div_ceil(cols);
        let width = cols as f32 * cell_w;
        let left = area.center().x - width / 2.0;
        // Keep the focused row in view, easing.
        let focus_row = self.case_focus / cols;
        let want = (focus_row as f32 * cell_h - (area.height() - cell_h) / 2.0)
            .clamp(0.0, (rows_total as f32 * cell_h - area.height()).max(0.0));
        self.grid_scroll += (want - self.grid_scroll) * 0.25;
        let clip = p.with_clip_rect(area.expand2(Vec2::new(8.0, 0.0)));
        let cam = Camera {
            origin: Pos2::new(area.center().x, area.center().y - 400.0 * u),
            focal: 2600.0 * u,
        };
        for (i, item) in cases.iter().enumerate() {
            let (row, col) = (i / cols, i % cols);
            let cell = Rect::from_min_size(
                Pos2::new(
                    left + col as f32 * cell_w,
                    area.top() + row as f32 * cell_h - self.grid_scroll,
                ),
                Vec2::new(cell_w, cell_h),
            );
            if cell.bottom() < area.top() - 4.0 || cell.top() > area.bottom() + 4.0 {
                continue;
            }
            let focused = i == self.case_focus;
            let inner = cell.shrink(6.0 * u);
            if focused {
                super::focus_cage(&clip, inner, a, u);
            }
            let art = item
                .cover_key
                .as_deref()
                .and_then(|k| self.covers.texture(ctx, k));
            let kind = if art.is_some() {
                CoverKind::Art
            } else {
                item.kind
            };
            let hero = matches!(item.kind, CoverKind::Demo);
            let size = if hero {
                V3::new(236.0, 210.0, 17.0)
            } else {
                V3::new(150.0, 134.0, 12.0)
            };
            let scale = (cell_w - 28.0 * u) / size.x;
            let center = V3::new(cell.center().x, cell.top() + cell_h * 0.42, 0.0);
            let draw = CaseDraw {
                kind,
                title: &item.title,
                art,
                size,
                center,
                yaw: -0.12,
                pitch: -0.05,
                lid: 0.0,
                scale,
                opacity: a * if focused { 1.0 } else { 0.85 },
                seed: super::hash_str(&item.title),
            };
            super::scene::draw_case(&clip, &cam, &draw);
            let mut job = egui::text::LayoutJob::simple(
                item.title.clone(),
                egui::FontId::proportional(12.0 * u),
                fade(if focused { tokens::TEXT } else { tokens::MUTED }, a),
                cell_w - 14.0 * u,
            );
            job.wrap.max_rows = 1;
            job.wrap.break_anywhere = true;
            let label = clip.layout_job(job);
            let lp = Pos2::new(
                cell.center().x - label.size().x / 2.0,
                cell.bottom() - 8.0 * u - label.size().y,
            );
            clip.add(Shape::galley(lp, label, tokens::TEXT));
            let hit = cell.intersect(area);
            if hit.width() > 4.0 && hit.height() > 4.0 {
                self.hits.push((hit, Click::Case(i)));
            }
        }
        // The rail on a phone: tap or drag along it to jump by letter.
        if lay.kind == LayoutKind::Portrait {
            let groups = letter_groups(cases);
            if groups.len() >= 2 {
                let rail = Rect::from_min_max(
                    Pos2::new(lay.safe.right() - 26.0 * u, area.top()),
                    Pos2::new(lay.safe.right() - 4.0 * u, area.bottom()),
                );
                let letters: Vec<char> = "#ABCDEFGHIJKLMNOPQRSTUVWXYZ".chars().collect();
                let step = rail.height() / letters.len() as f32;
                let cur = cases.get(self.case_focus).map(|c| letter_of(&c.title));
                for (k, l) in letters.iter().enumerate() {
                    let on = groups.iter().any(|g| g.0 == *l);
                    let here = cur == Some(*l);
                    p.text(
                        Pos2::new(rail.center().x, rail.top() + step * (k as f32 + 0.5)),
                        Align2::CENTER_CENTER,
                        l.to_string(),
                        egui::FontId::proportional(12.0),
                        fade(
                            if here {
                                CYAN
                            } else if on {
                                tokens::MUTED
                            } else {
                                tokens::RING
                            },
                            a,
                        ),
                    );
                }
                self.rail = Some((rail, letters));
                self.hits
                    .push((rail.expand2(Vec2::new(8.0, 0.0)), Click::Rail));
            }
        } else {
            self.rail = None;
        }
        let _ = ms;
        Rect::from_min_max(
            Pos2::new(lay.safe.left(), area.bottom()),
            Pos2::new(lay.safe.right(), lay.safe.bottom() - lay.bar_reserve()),
        )
    }
}
