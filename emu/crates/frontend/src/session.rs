//! The player's transport: what Reset, the PS button, closing the menu and
//! picking a disc do to a machine that is, or is not, already loaded.
//!
//! Everything here is a pure function of a small value type, so the journeys a
//! player walks (open the menu, reset, pick the same disc again, close the menu)
//! run as plain tests. The shell reads its fields from `AppState`, applies one
//! of these transitions and writes the result back; it never decides on its own
//! whether a press resumes, restarts or downloads.
//!
//! The rules, in one place:
//!
//! - Reset restarts the loaded game in place. It never opens the menu and never
//!   downloads anything.
//! - Picking the disc that is already loaded restarts it from the image in hand
//!   (the in-memory chunks or the browser cache), with no new download.
//! - Closing the menu, by the PS button or by Back, resumes the game if the menu
//!   is what paused it, and leaves a game the player paused themselves paused.
//! - The menu only ever opens because the player asked, or because the guest
//!   crashed ([`Transport::guest_crashed`]).

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

/// The four facts the transport decides on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Transport {
    /// The guest is being stepped.
    pub running: bool,
    /// The overlay menu is on screen.
    pub menu_open: bool,
    /// A machine is loaded (booted at least once and not torn down).
    pub game_loaded: bool,
    /// The menu paused a running game, so closing it should resume.
    pub paused_by_menu: bool,
}

impl Transport {
    /// The PS button (Escape, Select+Start, the touch Menu button): pause and
    /// open the menu while playing, resume and close it while it is open.
    pub fn ps_button(&mut self) {
        if self.running {
            self.running = false;
            self.menu_open = true;
            self.paused_by_menu = true;
        } else if self.menu_open {
            self.menu_open = false;
            if self.game_loaded {
                self.running = true;
            }
            self.paused_by_menu = false;
        } else {
            self.menu_open = true;
        }
    }

    /// Back (Circle, or Back on a touch menu) closed the menu.
    pub fn menu_back(&mut self) {
        if !self.menu_open {
            return;
        }
        self.menu_open = false;
        if self.paused_by_menu && self.game_loaded {
            self.running = true;
        }
        self.paused_by_menu = false;
    }

    /// A machine finished booting (first load, restart or reset): it runs and
    /// the menu gets out of the way.
    pub fn game_booted(&mut self) {
        self.running = true;
        self.menu_open = false;
        self.game_loaded = true;
        self.paused_by_menu = false;
    }

    /// The guest stopped on a fault: show the menu so the player can reset.
    pub fn guest_crashed(&mut self) {
        self.running = false;
        self.menu_open = true;
        self.paused_by_menu = false;
    }

    /// The machine was torn down (a different disc is loading).
    pub fn game_unloaded(&mut self) {
        self.running = false;
        self.game_loaded = false;
        self.paused_by_menu = false;
    }
}

/// What the player's Reset does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetPlan {
    /// Cold-boot the loaded game from the image already in hand.
    RestartInPlace,
    /// Nothing is loaded, so there is nothing to reset.
    NothingLoaded,
}

/// Decide a Reset. `can_restart` says the loaded machine kept what a cold boot
/// needs (the disc, or the executable bytes).
pub fn plan_reset(game_loaded: bool, can_restart: bool) -> ResetPlan {
    if game_loaded && can_restart {
        ResetPlan::RestartInPlace
    } else {
        ResetPlan::NothingLoaded
    }
}

/// What picking a disc in the Library does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickPlan {
    /// The picked disc is the loaded one: restart it from the image in hand.
    RestartInPlace,
    /// Fetch and boot the picked disc (its data comes from the browser cache
    /// when it was fetched before).
    Load,
    /// The picked disc is already on its way in; do nothing a second time.
    AlreadyLoading,
}

/// Decide a Library pick. `loaded` is the id of the machine now loaded,
/// `can_restart` whether it kept what a cold boot needs, and `loading` the id
/// of a disc whose boot is still in flight.
pub fn plan_pick(
    loaded: Option<&str>,
    can_restart: bool,
    loading: Option<&str>,
    picked: &str,
) -> PickPlan {
    if loading == Some(picked) {
        PickPlan::AlreadyLoading
    } else if loaded == Some(picked) && can_restart {
        PickPlan::RestartInPlace
    } else {
        PickPlan::Load
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playing() -> Transport {
        Transport {
            running: true,
            menu_open: false,
            game_loaded: true,
            paused_by_menu: false,
        }
    }

    /// Menu open, then closed by the PS button: back exactly where it was.
    #[test]
    fn ps_button_round_trip_resumes() {
        let mut t = playing();
        t.ps_button();
        assert!(!t.running && t.menu_open);
        t.ps_button();
        assert!(t.running && !t.menu_open);
    }

    /// The reported bug: closing the menu with Back left the game paused behind
    /// a closed menu, and the next press reopened it.
    #[test]
    fn back_resumes_a_game_the_menu_paused() {
        let mut t = playing();
        t.ps_button();
        t.menu_back();
        assert!(t.running && !t.menu_open && !t.paused_by_menu);
        // And the next PS press opens the menu again, once.
        t.ps_button();
        assert!(t.menu_open && !t.running);
    }

    /// A game the player paused with the toolbar stays paused when a menu opened
    /// over it is closed again.
    #[test]
    fn back_leaves_a_user_pause_alone() {
        let mut t = playing();
        t.running = false;
        t.ps_button();
        assert!(t.menu_open);
        t.menu_back();
        assert!(!t.running && !t.menu_open);
    }

    #[test]
    fn menu_never_reopens_by_itself() {
        let mut t = playing();
        t.ps_button();
        t.game_booted();
        assert!(!t.menu_open, "a booted, running game keeps the menu closed");
        assert!(t.running);
    }

    /// Reset from the menu: the boot lands, the game runs and the menu closes;
    /// closing the (already closed) menu changes nothing.
    #[test]
    fn reset_from_the_menu_returns_to_the_running_game() {
        let mut t = playing();
        t.ps_button();
        assert_eq!(plan_reset(t.game_loaded, true), ResetPlan::RestartInPlace);
        t.game_booted();
        assert!(t.running && !t.menu_open);
        t.menu_back();
        assert!(t.running && !t.menu_open);
    }

    #[test]
    fn a_guest_crash_opens_the_menu_and_a_restart_recovers() {
        let mut t = playing();
        t.guest_crashed();
        assert!(t.menu_open && !t.running);
        t.game_booted();
        assert!(t.running && !t.menu_open);
    }

    #[test]
    fn ps_button_with_nothing_loaded_only_opens_the_menu() {
        let mut t = Transport::default();
        t.ps_button();
        assert!(t.menu_open && !t.running);
        t.ps_button();
        assert!(!t.menu_open && !t.running, "nothing to resume");
    }

    #[test]
    fn reset_needs_a_restartable_game() {
        assert_eq!(plan_reset(false, false), ResetPlan::NothingLoaded);
        assert_eq!(plan_reset(true, false), ResetPlan::NothingLoaded);
        assert_eq!(plan_reset(true, true), ResetPlan::RestartInPlace);
    }

    #[test]
    fn picking_the_loaded_disc_restarts_it_without_a_download() {
        assert_eq!(
            plan_pick(Some("demo"), true, None, "demo"),
            PickPlan::RestartInPlace
        );
    }

    #[test]
    fn picking_another_disc_loads_it() {
        assert_eq!(plan_pick(Some("demo"), true, None, "other"), PickPlan::Load);
        assert_eq!(plan_pick(None, false, None, "demo"), PickPlan::Load);
    }

    /// The loaded machine lost its disc (a failed restart): pick again loads.
    #[test]
    fn picking_a_disc_that_cannot_restart_loads_it_again() {
        assert_eq!(plan_pick(Some("demo"), false, None, "demo"), PickPlan::Load);
    }

    #[test]
    fn a_double_pick_while_loading_is_ignored() {
        assert_eq!(
            plan_pick(None, false, Some("demo"), "demo"),
            PickPlan::AlreadyLoading
        );
        assert_eq!(
            plan_pick(Some("demo"), true, Some("other"), "demo"),
            PickPlan::RestartInPlace
        );
    }

    /// Full journey: boot, play, menu, pick the same disc, pick another, come
    /// back to the first: the menu is closed and the game runs after each boot.
    #[test]
    fn the_whole_journey() {
        let mut t = Transport::default();
        t.game_booted();
        t.ps_button();
        assert_eq!(
            plan_pick(Some("demo"), true, None, "demo"),
            PickPlan::RestartInPlace
        );
        t.game_booted();
        t.ps_button();
        assert_eq!(plan_pick(Some("demo"), true, None, "web:x"), PickPlan::Load);
        t.game_unloaded();
        assert!(!t.running);
        t.game_booted();
        t.ps_button();
        t.menu_back();
        assert!(t.running && !t.menu_open);
        t.ps_button();
        assert_eq!(plan_pick(Some("web:x"), true, None, "demo"), PickPlan::Load);
    }
}
