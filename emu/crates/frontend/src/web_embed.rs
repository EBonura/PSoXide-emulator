//! `?embed=1`: the player a host page shows in an iframe next to code.
//!
//! The page script (index.html) owns the embed protocol: the click-to-start
//! poster, the postMessage API to the parent, visibility pausing, and the
//! overlay buttons. It loads the wasm only after the click, so by the time
//! this runs the user has asked for the program named by `?disc=`.
//!
//! This side stays small. Commands arrive through `psoxideEmbedCommand`
//! and are applied once per tick; the emulator's run state is reported back
//! through `globalThis.psoxideEmbedEvent` whenever it changes. The shell
//! skips the menu, toolbar, splash and toasts, and never autoboots the demo
//! disc, while this mode is on (see `crate::app::embed_mode`).

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

use wasm_bindgen::prelude::*;

use crate::app::AppState;

#[wasm_bindgen(inline_js = r#"
export function embedEmit(kind, message) {
  const f = globalThis.psoxideEmbedEvent;
  if (typeof f === 'function') f(kind, message);
}
"#)]
extern "C" {
    #[wasm_bindgen(js_name = embedEmit)]
    fn embed_emit(kind: &str, message: &str);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
    Pause,
    Resume,
    Reset,
    Mute,
    Unmute,
}

impl Command {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "pause" => Self::Pause,
            "resume" => Self::Resume,
            "reset" => Self::Reset,
            "mute" => Self::Mute,
            "unmute" => Self::Unmute,
            _ => return None,
        })
    }
}

thread_local! {
    static ENABLED: Cell<Option<bool>> = const { Cell::new(None) };
    static QUEUE: RefCell<VecDeque<Command>> = const { RefCell::new(VecDeque::new()) };
    /// Held by `pause` and released by `resume`. Sticky across boots, so a
    /// pause that lands while the program is still loading keeps it paused.
    static PAUSED: Cell<bool> = const { Cell::new(false) };
    /// Last run state reported to the page, `None` before the first boot.
    static REPORTED: Cell<Option<bool>> = const { Cell::new(None) };
}

/// Whether the page URL carries `embed=1`. Read once.
pub fn enabled() -> bool {
    ENABLED.with(|cell| {
        if let Some(on) = cell.get() {
            return on;
        }
        let on = crate::web_bench::flag("embed");
        cell.set(Some(on));
        on
    })
}

/// Queue a command from the page script. Returns false for an unknown name.
#[wasm_bindgen(js_name = psoxideEmbedCommand)]
pub fn command(name: &str) -> bool {
    match Command::parse(name) {
        Some(cmd) if enabled() => {
            QUEUE.with(|q| q.borrow_mut().push_back(cmd));
            true
        }
        _ => false,
    }
}

/// Report a load or boot failure to the page (which forwards it to the host).
pub fn error(message: &str) {
    if enabled() {
        embed_emit("error", message);
    }
}

/// Apply queued commands and report run-state changes. Called every tick.
pub fn tick(state: &mut AppState) {
    if !enabled() {
        return;
    }
    let commands: Vec<Command> = QUEUE.with(|q| q.borrow_mut().drain(..).collect());
    for cmd in commands {
        match cmd {
            Command::Pause => PAUSED.set(true),
            Command::Resume => PAUSED.set(false),
            Command::Reset => {
                if let Err(message) = state.reboot_current_web_game() {
                    // Nothing loaded yet: there is nothing to restart.
                    if state.bus.is_some() {
                        error(&format!("reset: {message}"));
                    }
                }
            }
            Command::Mute => state.audio_muted = true,
            Command::Unmute => state.audio_muted = false,
        }
    }
    // There is no menu in embed mode; nothing may leave it open.
    state.menu.open = false;
    let booted = state.bus.is_some() && state.current_game.is_some();
    if !booted {
        return;
    }
    let running = !PAUSED.get();
    if state.running != running {
        state.running = running;
        state.menu.sync_run_label(running);
    }
    if REPORTED.get() != Some(running) {
        REPORTED.set(Some(running));
        embed_emit(if running { "running" } else { "paused" }, "");
    }
}
