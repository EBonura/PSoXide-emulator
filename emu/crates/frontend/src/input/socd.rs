//! Simultaneous opposing cardinal directions.
//!
//! The real D-pad's rocker makes left+right (or up+down) impossible, so the
//! guest must never see it. When both sides are held, the most recently
//! pressed one wins; releasing it re-exposes the still-held opposite. With no
//! recorded recency the pair resolves to neutral, which is what the rocker
//! does mid-travel.

use emulator_core::button;

use super::model::rising_edges;

const HORIZ: u16 = button::LEFT | button::RIGHT;
const VERT: u16 = button::UP | button::DOWN;

/// Strip simultaneous opposing cardinal directions from `mask`, keeping only
/// the side recorded in `last_horiz` / `last_vert`.
pub(crate) fn socd_resolve(mask: u16, last_horiz: u16, last_vert: u16) -> u16 {
    let mut out = mask;
    if out & HORIZ == HORIZ {
        out = (out & !HORIZ) | (last_horiz & HORIZ);
    }
    if out & VERT == VERT {
        out = (out & !VERT) | (last_vert & VERT);
    }
    out
}

/// Fold a set of rising-edge presses into the recency trackers. Opposite
/// edges arriving in the same poll carry no ordering information, so the
/// pair's recency resets and [`socd_resolve`] yields neutral.
pub(crate) fn update_socd_recency(last_horiz: &mut u16, last_vert: &mut u16, pressed: u16) {
    match pressed & HORIZ {
        button::LEFT => *last_horiz = button::LEFT,
        button::RIGHT => *last_horiz = button::RIGHT,
        HORIZ => *last_horiz = 0,
        _ => {}
    }
    match pressed & VERT {
        button::UP => *last_vert = button::UP,
        button::DOWN => *last_vert = button::DOWN,
        VERT => *last_vert = 0,
        _ => {}
    }
}

/// A mask stream with SOCD resolution: remembers which side of each pair
/// was pressed last across polls, whichever device pressed it.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SocdFilter {
    previous: u16,
    last_horiz: u16,
    last_vert: u16,
}

impl SocdFilter {
    /// Resolve one poll's merged mask.
    pub(crate) fn resolve(&mut self, mask: u16) -> u16 {
        update_socd_recency(
            &mut self.last_horiz,
            &mut self.last_vert,
            rising_edges(self.previous, mask),
        );
        self.previous = mask;
        socd_resolve(mask, self.last_horiz, self.last_vert)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newer_side_wins_and_releasing_it_exposes_the_older() {
        let mut socd = SocdFilter::default();
        assert_eq!(socd.resolve(button::LEFT), button::LEFT);
        assert_eq!(socd.resolve(button::LEFT | button::RIGHT), button::RIGHT);
        assert_eq!(socd.resolve(button::LEFT), button::LEFT);
    }

    #[test]
    fn opposites_arriving_together_resolve_to_neutral() {
        let mut socd = SocdFilter::default();
        assert_eq!(socd.resolve(button::UP | button::DOWN), 0);
        assert_eq!(socd.resolve(0), 0);
    }
}
