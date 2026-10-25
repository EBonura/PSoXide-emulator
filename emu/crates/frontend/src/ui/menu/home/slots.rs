//! The slot sheet as a page of the shell: how it opens, how the pad and keys move on it and how
//! Back leaves it. The cards themselves are drawn by [`super::super::slots`].

use super::{nav_items, HomeState, MenuAction, MenuInput, MenuState, Page, Zone};

impl MenuState {
    /// Whether the slot sheet is the page showing.
    pub(in crate::ui::menu) fn home_page_is_slots(&self) -> bool {
        self.open && self.home.as_ref().is_some_and(|h| h.page == Page::Slots)
    }

    /// Show the slot sheet in the shell (the mode is already set on `self.slots`).
    pub(in crate::ui::menu) fn home_open_slots(&mut self, from_shell: bool) {
        let Some(mut shell) = self.home.take() else {
            return;
        };
        shell.open_slots(from_shell);
        self.home = Some(shell);
    }
}

impl HomeState {
    /// Go to the slot sheet. Opened from the Game page, Back returns there; opened from outside
    /// the shell (the toolbar, a touch button), Back closes the shell and the game goes on.
    pub(super) fn open_slots(&mut self, from_shell: bool) {
        self.slots_return = (from_shell && self.page == Page::Game).then_some(Page::Game);
        // Where the shell opens, not the top: the reopening rule must not send it Home.
        self.closed = false;
        // A launch sequence the game ran under is over: the sheet is what shows.
        self.launch = None;
        self.launch_sent = false;
        self.launch_done = false;
        self.page_age = 0.0;
        self.page_dir = 0.0;
        self.page = Page::Slots;
        self.zone = Zone::Body;
        self.list_scroll = 0.0;
        self.last_row = usize::MAX;
        self.peek = false;
    }

    /// The sheet's keys and pad: the grid and its questions are the sheet's own; Back and Up from
    /// the top row are the shell's.
    pub(super) fn update_slots(
        &mut self,
        ms: &mut MenuState,
        input: &MenuInput,
    ) -> Option<MenuAction> {
        if self.zone == Zone::Nav {
            self.update_nav(ms, input);
            return None;
        }
        if !ms.slots.asking() {
            if input.up && ms.slots.on_top_row() {
                self.zone = Zone::Nav;
                self.nav = nav_items(self.pausing(ms))
                    .iter()
                    .position(|i| i.page() == self.page.nav_page())
                    .unwrap_or(0);
                return None;
            }
            if input.back {
                match self.slots_return.take() {
                    Some(page) => self.page = page,
                    // Back from a sheet opened over the game resumes the game.
                    None => ms.open = false,
                }
                return None;
            }
        }
        ms.slots.press(input)
    }
}
