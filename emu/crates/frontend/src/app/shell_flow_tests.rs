//! Sequences through the shell and the controller button on a game started the way
//! `PSOXIDE_DISC` + `PSOXIDE_AUTORUN` starts it: what is on screen and whether the game runs, at
//! every step. The steps go through the real toolbar button (a pointer press on it), the same
//! menu input step the window runs, the real `build_ui` draw, and a pad plugged into the router.

use super::tests::{bootable_test_bin, frontend_test_temp_dir};
use super::*;
use crate::ui::menu::home::quick;
use crate::ui::menu::MenuInput;
use emulator_core::pad::PadMode;

/// What a step left on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Look {
    /// The shell page ("" when the shell is closed).
    page: &'static str,
    /// The quick popup is up.
    popup: bool,
    running: bool,
    menu_open: bool,
}

const fn look(page: &'static str, popup: bool, running: bool, menu_open: bool) -> Look {
    Look {
        page,
        popup,
        running,
        menu_open,
    }
}

/// A running game, nothing else on screen.
const GAME: Look = look("", false, true, false);
/// The popup over the paused game.
const POPUP: Look = look("", true, false, false);
/// The Controls page over the paused game.
const CONTROLS: Look = look("Controls", false, false, true);
/// The shell's home over the paused game.
const HOME: Look = look("Home", false, false, true);
/// A game the player had paused themselves, nothing else on screen.
const PAUSED: Look = look("", false, false, false);

struct Rig {
    state: AppState,
    router: crate::input::InputRouter,
    ctx: egui::Context,
    time: f64,
    layout_stamp: (u64, u64),
    root: PathBuf,
    /// A made-up picture standing in for the game, so a frame shows what covers what.
    game: egui::TextureHandle,
    /// Every texture change so far and the last frame drawn, for the step frames.
    textures: egui::TexturesDelta,
    last: Option<egui::FullOutput>,
}

impl Rig {
    /// A game running from the environment launch with the home shell enabled, as `main` does,
    /// and a controller connected.
    fn running_game(name: &str) -> Self {
        Self::with_pad(name, true)
    }

    /// The same with or without a controller plugged in.
    fn with_pad(name: &str, pad: bool) -> Self {
        let root = frontend_test_temp_dir(name);
        let bin = root.join("Env Game.bin");
        std::fs::write(&bin, bootable_test_bin()).unwrap();
        let mut state = AppState::with_config_dir(Some(root.join("config")));
        let disc = psoxide_settings::library::load_disc_from_bin(&bin).unwrap();
        let mut bus = Bus::new_without_bios();
        fast_boot_disc(&mut bus, &mut state.cpu, &disc).unwrap();
        state.bus = Some(bus);
        state.adopt_env_launch(&bin);
        state.enable_home_screen();
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let game = ctx.load_texture("game", game_picture(), egui::TextureOptions::LINEAR);
        let mut router = crate::input::InputRouter::new();
        if pad {
            router.connect_test_pad("pad", "Wireless Controller");
        }
        let mut rig = Self {
            state,
            router,
            ctx,
            time: 1.0,
            layout_stamp: (u64::MAX, u64::MAX),
            root,
            game,
            textures: egui::TexturesDelta::default(),
            last: None,
        };
        rig.settle();
        rig
    }

    /// Draw frames with no input, so the shell's fades settle.
    fn settle(&mut self) {
        for _ in 0..30 {
            self.frame(MenuInput::default(), Vec::new());
        }
    }

    /// One window frame: the menu input step, the real `build_ui` draw, the pointer action the
    /// draw produced, and the guest pad layout, as the window loop does them.
    fn frame(&mut self, input: MenuInput, events: Vec<egui::Event>) {
        if let Some(action) = self.state.step_menu(input) {
            let _ = crate::ui::apply_menu_action(&mut self.state, action);
        }
        self.time += 0.1;
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1100.0, 640.0));
        let raw = egui::RawInput {
            screen_rect: Some(rect),
            time: Some(self.time),
            focused: true,
            events,
            ..Default::default()
        };
        let state = &mut self.state;
        let router = &mut self.router;
        let game = self.game.id();
        let output = self.ctx.run(raw, |ctx| {
            crate::app::build_ui(
                ctx,
                state,
                router,
                egui::TextureId::default(),
                game,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                0.1,
            );
        });
        self.textures.append(output.textures_delta.clone());
        self.last = Some(output);
        if let Some(action) = self.state.menu.take_pending_pointer_action() {
            let _ = crate::ui::apply_menu_action(&mut self.state, action);
        }
        let stamp = (
            self.state.gpu_resync_generation,
            self.router.routing_generation(),
        );
        if self.layout_stamp != stamp {
            if let Some(bus) = self.state.bus.as_mut() {
                self.router.apply_layout(bus);
            }
            self.layout_stamp = stamp;
        }
    }

    fn press(&mut self, input: MenuInput) {
        self.frame(input, Vec::new());
        self.settle();
    }

    fn escape(&mut self) {
        self.press(MenuInput {
            toggle_open: true,
            ..MenuInput::default()
        });
    }

    fn back(&mut self) {
        self.press(MenuInput {
            back: true,
            ..MenuInput::default()
        });
    }

    fn down(&mut self) {
        self.press(MenuInput {
            down: true,
            ..MenuInput::default()
        });
    }

    fn up(&mut self) {
        self.press(MenuInput {
            up: true,
            ..MenuInput::default()
        });
    }

    fn confirm(&mut self) {
        self.press(MenuInput {
            confirm: true,
            ..MenuInput::default()
        });
    }

    /// A pointer click at a point.
    fn click(&mut self, at: egui::Pos2) {
        self.frame(MenuInput::default(), vec![egui::Event::PointerMoved(at)]);
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        self.frame(MenuInput::default(), vec![button(true)]);
        self.frame(MenuInput::default(), vec![button(false)]);
        self.settle();
    }

    /// Press the toolbar's controller button with the pointer.
    fn controller_button(&mut self) {
        let rect: egui::Rect = self
            .ctx
            .data(|d| d.get_temp(egui::Id::new(crate::ui::toolbar::CONTROLS_BUTTON_RECT)))
            .expect("the toolbar drew its controller button");
        self.click(rect.center());
    }

    /// Click a control of the popup.
    fn click_popup(&mut self, name: &str) {
        let rect = quick::rect_of(&self.ctx, name).expect("the popup drew that control");
        self.click(rect.center());
    }

    fn look(&self) -> Look {
        let (page, _) = self.state.menu.home_probe();
        Look {
            page: if self.state.menu.open { page } else { "" },
            popup: self.state.quick.is_some(),
            running: self.state.running,
            menu_open: self.state.menu.open,
        }
    }

    fn analog_on(&self) -> bool {
        self.state.bus.as_ref().and_then(Bus::port1_pad_mode) == Some(PadMode::Analog)
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The reported flow: the controller button from a running game, Escape, Escape.
#[test]
fn the_controller_button_then_escape_returns_to_the_running_game() {
    let mut rig = Rig::running_game("shell-flow-a");
    assert_eq!(rig.look(), GAME);
    rig.controller_button();
    assert_eq!(
        rig.look(),
        POPUP,
        "the button opens the small popup, not the shell"
    );
    rig.escape();
    assert_eq!(
        rig.look(),
        GAME,
        "Escape from the popup gives the game back"
    );
    // A second Escape is the PS button again: the shell, as it always was.
    rig.escape();
    assert_eq!(rig.look(), HOME);
    rig.escape();
    assert_eq!(rig.look(), GAME);
}

/// The button, "Full controller settings", then Escape: the game as it was, not the shell's home.
#[test]
fn escape_from_the_full_page_the_popup_opened_returns_to_the_game() {
    let mut rig = Rig::running_game("shell-flow-b");
    rig.controller_button();
    rig.click_popup("full");
    assert_eq!(rig.look(), CONTROLS);
    rig.escape();
    assert_eq!(rig.look(), GAME);
    // And with Back (Circle) instead of Escape.
    rig.controller_button();
    rig.click_popup("full");
    rig.back();
    assert_eq!(rig.look(), GAME);
}

/// A game the player paused with the toolbar stays paused through every way out.
#[test]
fn a_game_that_was_paused_stays_paused() {
    let mut rig = Rig::running_game("shell-flow-c");
    rig.state.running = false;
    assert_eq!(rig.look(), PAUSED);
    rig.controller_button();
    assert_eq!(rig.look(), POPUP);
    rig.escape();
    assert_eq!(rig.look(), PAUSED);
    rig.controller_button();
    rig.click_popup("full");
    assert_eq!(rig.look(), CONTROLS);
    rig.escape();
    assert_eq!(rig.look(), PAUSED);
    rig.controller_button();
    rig.controller_button();
    assert_eq!(rig.look(), PAUSED, "the button again closes the popup");
}

/// The button again, a click outside, and Back all close the popup.
#[test]
fn every_way_to_close_the_popup_gives_the_game_back() {
    let mut rig = Rig::running_game("shell-flow-d");
    rig.controller_button();
    rig.controller_button();
    assert_eq!(rig.look(), GAME, "the controller button toggles");
    rig.controller_button();
    rig.click(egui::pos2(60.0, 400.0));
    assert_eq!(rig.look(), GAME, "a click outside closes it");
    rig.controller_button();
    rig.back();
    assert_eq!(rig.look(), GAME, "Back closes it");
    rig.controller_button();
    // A click inside the panel that hits nothing keeps it open.
    let full = quick::rect_of(&rig.ctx, "full").unwrap();
    rig.click(egui::pos2(full.left() + 4.0, full.top() - 6.0));
    assert_eq!(rig.look(), POPUP);
}

/// The popup is a mode of its own: nothing the game or shell listens to leaks through it.
#[test]
fn while_the_popup_is_up_the_shell_and_the_game_stay_out() {
    let mut rig = Rig::running_game("shell-flow-e");
    rig.controller_button();
    let tick = rig.state.cpu.tick();
    for _ in 0..3 {
        rig.down();
        rig.up();
    }
    assert_eq!(rig.look(), POPUP);
    assert_eq!(
        rig.state.cpu.tick(),
        tick,
        "the paused game did not advance"
    );
}

/// The analog switch with the pad alone and with the pointer: the guest pad follows.
#[test]
fn the_analog_switch_works_from_a_pad_and_from_the_pointer() {
    let mut rig = Rig::running_game("shell-flow-f");
    let start = rig.analog_on();
    rig.controller_button();
    rig.confirm();
    assert_eq!(rig.look(), POPUP);
    assert_ne!(
        rig.analog_on(),
        start,
        "Confirm on the focused switch flips analog"
    );
    rig.confirm();
    assert_eq!(rig.analog_on(), start, "and flips it back");
    rig.click_popup("switch-0");
    assert_ne!(rig.analog_on(), start, "a click flips it");
    // Left means off, Right means on, whatever it was.
    rig.press(MenuInput {
        left: true,
        ..MenuInput::default()
    });
    assert!(!rig.analog_on());
    rig.press(MenuInput {
        right: true,
        ..MenuInput::default()
    });
    assert!(rig.analog_on());
    // Down is the full-settings button, Confirm opens the page, Escape is the game.
    rig.down();
    rig.confirm();
    assert_eq!(rig.look(), CONTROLS);
    rig.escape();
    assert_eq!(rig.look(), GAME);
}

/// The shell's own Escape journey is untouched by all of this.
#[test]
fn the_shell_opened_by_escape_still_goes_back_a_page_at_a_time() {
    let mut rig = Rig::running_game("shell-flow-g");
    rig.escape();
    assert_eq!(rig.look(), HOME);
    rig.state.menu.home_goto("Controls");
    rig.settle();
    assert_eq!(rig.look().page, "Controls");
    rig.escape();
    assert_eq!(
        rig.look(),
        HOME,
        "from a page opened inside the shell, Escape is Back"
    );
    rig.escape();
    assert_eq!(rig.look(), GAME);
}

/// Going to another page from the full Controls page ends its special Escape.
#[test]
fn leaving_the_controls_page_ends_the_game_button_return() {
    let mut rig = Rig::running_game("shell-flow-h");
    rig.controller_button();
    rig.click_popup("full");
    rig.state.menu.home_goto("Settings");
    rig.settle();
    rig.escape();
    assert_eq!(rig.look(), HOME);
    assert!(!rig.state.full_from_quick);
}

/// A made-up game picture: a sky, a floor and a few blocks, so a frame shows what the popup and
/// the shell cover.
fn game_picture() -> egui::ColorImage {
    let (w, h) = (320usize, 240usize);
    let mut pixels = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let horizon = 150;
            let mut c = if y < horizon {
                let t = y as f32 / horizon as f32;
                egui::Color32::from_rgb(40 + (90.0 * t) as u8, 70 + (80.0 * t) as u8, 140)
            } else {
                let checker = ((x / 16) + (y / 8)) % 2 == 0;
                if checker {
                    egui::Color32::from_rgb(70, 110, 60)
                } else {
                    egui::Color32::from_rgb(58, 96, 50)
                }
            };
            for (bx, by, bw, bh, col) in [
                (60, 90, 40, 70, egui::Color32::from_rgb(200, 90, 60)),
                (150, 110, 50, 50, egui::Color32::from_rgb(230, 200, 90)),
                (240, 70, 30, 90, egui::Color32::from_rgb(160, 80, 170)),
            ] {
                if x >= bx && x < bx + bw && y >= by && y < by + bh {
                    c = col;
                }
            }
            pixels.push(c);
        }
    }
    egui::ColorImage {
        size: [w, h],
        pixels,
    }
}

impl Rig {
    /// Keep the frame on screen now as `<dir>/<name>.png`, when `PSOXIDE_UI_FRAMES` names a
    /// directory. Without it the tests do not render.
    fn shot(&mut self, name: &str) {
        let Some(dir) = std::env::var_os("PSOXIDE_UI_FRAMES") else {
            return;
        };
        let output = self.last.clone().expect("a frame was drawn");
        crate::cli::ui_png::write_frame_png(
            &PathBuf::from(dir).join(format!("{name}.png")),
            &self.ctx,
            output,
            self.textures.clone(),
            (1100, 640),
        )
        .expect("frame written");
    }
}

/// The reported flow, one frame per step, for the review page: run with
/// `PSOXIDE_UI_FRAMES=<dir> cargo test -p psoxide-emulator frames_of`.
#[test]
fn frames_of_the_controller_flow() {
    let mut rig = Rig::running_game("shell-flow-frames");
    rig.shot("01-game-running");
    rig.controller_button();
    rig.shot("02-after-controller-button");
    rig.escape();
    rig.shot("03-after-escape");
    rig.escape();
    rig.shot("04-after-second-escape");
    rig.escape();
    rig.controller_button();
    rig.click_popup("full");
    rig.shot("05-full-settings-from-popup");
    rig.escape();
    rig.shot("06-escape-from-full-settings");

    // Nothing but the keyboard plugged in.
    let mut rig = Rig::with_pad("shell-flow-frames-keys", false);
    rig.controller_button();
    rig.shot("07-popup-keyboard-only");
}
