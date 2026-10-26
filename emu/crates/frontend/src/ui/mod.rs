//! UI panel orchestration.
//!
//! Individual panels live in submodules; `draw_layout` composes them in
//! the order that makes the visual layering work: docked panels first
//! (bottom/side get clipped to remaining space), then the central area,
//! then free-floating overlays (Menu, HUD) on top.

pub mod burn;
pub mod debug_sidebar;
pub mod framebuffer;
pub mod memory;
pub mod menu;
pub(crate) mod momentum;
pub mod profiler;
pub mod registers;
pub mod splash;
pub mod toolbar;
pub mod touch_overlay;
pub mod vram;

use crate::app::{AppState, ScaleMode};
use crate::input::{InputRouter, PortChoice};

/// Paint every panel for this frame, in layering order.
pub fn draw_layout(
    ctx: &egui::Context,
    state: &mut AppState,
    input_router: &mut InputRouter,
    vram_tex: egui::TextureId,
    display_tex: egui::TextureId,
    #[cfg(feature = "editor")] editor_viewport: psxed_ui::EditorViewport3dPresentation,
    display_uv: egui::Rect,
    dt: f32,
) {
    state.guest_stats.note_host_frame(f64::from(dt));
    state.tick_status(dt);
    let recording_input = state.input_recording_status().0;
    state.menu.sync_input_recording_label(recording_input);
    state.menu.set_game_loaded(state.bus.is_some());
    state.feed_home(input_router, ctx);
    state.menu.set_deinterlace(state.deinterlace.label());
    state.menu.sync_video_audio(
        state.scale_mode == ScaleMode::Window,
        state.texture_filter.label(),
        state.audio_volume,
        state.audio_muted,
    );

    // An embedded player (web `?embed=1`) is just the game screen: the host
    // page supplies the chrome.
    let embed = crate::app::embed_mode();

    // When the editor workspace owns the central UI it takes over the whole
    // frame; the emulator panels below never run. Compiled out without the
    // editor feature (the workspace can never be the editor then).
    #[cfg(feature = "editor")]
    if state.workspace.is_editor() {
        let playtest_status = state.editor_playtest_status();
        // During Play the guest performance panel can take the Inspector's
        // column (F3 / the bars button toggles between the two).
        let mut export = None;
        let stats = &mut state.guest_stats;
        let mut play_panel = |ui: &mut egui::Ui| {
            export = play_panel_contents(ui, stats, vram_tex);
        };
        state.editor.draw_with_play_panel(
            ctx,
            editor_viewport,
            playtest_status,
            Some(&mut play_panel),
        );
        if let Some(psoxide_debug_ui::PanelAction::ExportCsv { csv, seconds }) = export {
            state.export_guest_stats_csv(&csv, seconds);
        }
        if state.editor.take_emulator_menu_request() {
            state.menu.open = true;
        }
        state.sync_embedded_playtest_with_editor_project();
        let menu_warning = state.menu_setup_warning();
        state.menu.draw(ctx, dt, menu_warning);
        burn::draw(ctx, state);
        draw_recording_indicator(ctx, state);
        draw_freecam_indicator(ctx, state);
        draw_status_toast(ctx, state);
        return;
    }

    // One-shot boot splash on a foreground layer. The home screen is its own
    // first impression.
    if !embed && !state.menu.home_enabled() {
        splash::draw(ctx);
    }

    // Top-bar controls go first so the central panel (framebuffer)
    // clips to what's left under them. The unified debug sidebar
    // docks next so the framebuffer gives it room.
    if !embed {
        toolbar::draw(ctx, state);
    }

    // With the on-screen controller up, the game keeps clear of the notch
    // and, in portrait, of the controller's own panel under the picture.
    if !embed {
        draw_touch_reservations(ctx, input_router.touch());
    }

    // The live debug panel docks inside those: beside the game on a wide window, between the
    // game and the controller's panel on a tall one. Always called: it animates itself open or
    // closed from the `debug_sidebar` flag and returns at once when fully closed.
    debug_sidebar::draw(ctx, state, vram_tex);
    if !embed {
        draw_touch_bottom_panel(ctx, input_router.touch());
        draw_touch_right_bar(ctx, input_router.touch());
    }

    // Zero-margin frame: the game screen gets every pixel between the
    // toolbar and the sidebar; the 4:3 letterbox bars are plain black,
    // CRT-bezel style, with no inset or border chrome.
    let native_rows = state
        .bus
        .as_ref()
        .map_or(240, |bus| u32::from(bus.gpu.display_area().height));
    egui::CentralPanel::default()
        .frame(egui::Frame::NONE.fill(egui::Color32::BLACK))
        .show(ctx, |ui| {
            framebuffer::draw(
                ui,
                display_tex,
                display_uv,
                &mut state.framebuffer_present_size_px,
                &mut state.game_rect,
                state.settings.video.integer_scale,
                native_rows,
            );
        });

    let menu_warning = state
        .menu_setup_warning()
        .filter(|_| !state.menu.home_enabled());
    // The Controls page shows the live devices and says what to change on them.
    if state.menu.home_wants_devices() {
        state.menu.home_set_devices(input_router.devices());
    }
    state.menu.draw(ctx, dt, menu_warning);
    for command in state.menu.home_take_device_commands() {
        use menu::DeviceCommand;
        let message = match command {
            DeviceCommand::Choice(id, choice) => input_router
                .set_device_choice(&id, choice)
                .then(|| format!("Port: {}", choice.label())),
            DeviceCommand::Profile(id, profile) => input_router
                .set_device_profile(&id, profile)
                .then(|| format!("{} mode", profile.label())),
        };
        if let Some(message) = message {
            state.status_message_set(message);
        }
    }
    // A controller's analog mode the player changed (on the Controls page or the
    // Analog button) is saved, so the next visit starts the same way.
    if let Some(choices) = input_router.take_remembered_analog() {
        state.settings.input.analog_choice = choices;
        if let Err(error) = state.save_settings() {
            eprintln!("[frontend] {error}");
        }
    }
    if let Some((kind, pct)) = state.menu.take_touch_slider() {
        use crate::ui::menu::TouchSliderKind;
        state.edit_touch_prefs(|p| match kind {
            TouchSliderKind::Opacity => p.opacity_pct = pct,
            TouchSliderKind::Size => p.size_pct = pct,
        });
    }
    let mut port_message = None;
    state
        .menu
        .draw_controls(ctx, |ui| port_message = controller_ports(ui, input_router));
    // The slot sheet is a page of the shell, drawn in the space it leaves.
    if let Some((_, rect)) = state.menu.home_section() {
        let reveal = state.menu.home_section_reveal();
        let touch = state.menu.touch_mode();
        if let Some(action) = state.menu.draw_slots(ctx, rect, reveal, touch) {
            state.menu.queue_pointer_action(action);
        }
    }
    if let Some(message) = port_message {
        state.status_message_set(message);
    }
    burn::draw(ctx, state);
    draw_recording_indicator(ctx, state);
    draw_freecam_indicator(ctx, state);
    if !embed {
        draw_status_toast(ctx, state);
        touch_overlay::draw(
            ctx,
            input_router.touch(),
            state.menu.open,
            state.panels.debug_sidebar,
            state.running,
        );
    }
}

/// Black panels that keep the game out of the notch and the portrait
/// controller panel. Empty (and free) when the controller is hidden.
fn draw_touch_reservations(ctx: &egui::Context, touch: &crate::input::backend::touch::TouchDevice) {
    let reserved = touch_overlay::reserved(ctx, touch);
    let black = || egui::Frame::NONE.fill(egui::Color32::BLACK);
    if reserved.top > 0.5 {
        egui::TopBottomPanel::top("touch-safe-top")
            .exact_height(reserved.top)
            .frame(black())
            .show(ctx, |_| {});
    }
    if reserved.left > 0.5 {
        egui::SidePanel::left("touch-safe-left")
            .exact_width(reserved.left)
            .frame(black())
            .show(ctx, |_| {});
    }
    if reserved.right > 0.5 {
        egui::SidePanel::right("touch-safe-right")
            .exact_width(reserved.right)
            .frame(black())
            .show(ctx, |_| {});
    }
}

/// The black panel under the game in portrait that holds the controller's buttons. It is declared
/// after the debug strip, which sits under it at the window's edge.
fn draw_touch_bottom_panel(ctx: &egui::Context, touch: &crate::input::backend::touch::TouchDevice) {
    let reserved = touch_overlay::reserved(ctx, touch);
    if reserved.bottom > 0.5 {
        egui::TopBottomPanel::bottom("touch-controls-panel")
            .exact_height(reserved.bottom)
            .frame(egui::Frame::NONE.fill(egui::Color32::BLACK))
            .show(ctx, |_| {});
    }
}

/// The black bar of controls right of the game in landscape. It is declared after the debug
/// panel, which docks between the notch and this bar.
fn draw_touch_right_bar(ctx: &egui::Context, touch: &crate::input::backend::touch::TouchDevice) {
    let reserved = touch_overlay::reserved(ctx, touch);
    if reserved.right_bar > 0.5 {
        egui::SidePanel::right("touch-bar-right")
            .exact_width(reserved.right_bar)
            .frame(egui::Frame::NONE.fill(egui::Color32::BLACK))
            .show(ctx, |_| {});
    }
}

/// The guest performance panel as it fills the Inspector's column during
/// editor Play; shared with the headless `dump-editor-ui --play-disc`.
#[cfg(feature = "editor")]
pub fn play_panel_contents(
    ui: &mut egui::Ui,
    stats: &mut psoxide_debug_ui::GuestStats,
    vram_tex: egui::TextureId,
) -> Option<psoxide_debug_ui::PanelAction> {
    crate::theme::viz_frame(ui, "", |ui| {
        psoxide_debug_ui::draw(ui, stats, Some(vram_tex))
    })
}

pub fn apply_menu_action(state: &mut AppState, action: menu::MenuAction) -> MenuOutcome {
    use menu::MenuAction::*;
    match action {
        ToggleRun => {
            state.running = !state.running;
            state.menu.sync_run_label(state.running);
            // Auto-close the overlay so Run is observable immediately.
            if state.running {
                state.menu.open = false;
            }
            MenuOutcome::None
        }
        Reset => {
            state.reset_game();
            MenuOutcome::None
        }
        ToggleInputRecording => {
            state.toggle_input_recording();
            MenuOutcome::None
        }
        LoadInputReplay => {
            state.pick_input_replay();
            MenuOutcome::None
        }
        ReplayLastInput => {
            state.replay_last_input();
            MenuOutcome::None
        }
        SaveSlot(slot) => {
            state.save_slot(slot);
            MenuOutcome::None
        }
        LoadSlot(slot) => {
            state.load_slot(slot);
            MenuOutcome::None
        }
        DeleteSlot(slot) => {
            state.delete_slot(slot);
            MenuOutcome::None
        }
        ExportSlot(slot) => {
            state.export_slot(slot);
            MenuOutcome::None
        }
        ImportSlot => {
            state.import_slot();
            MenuOutcome::None
        }
        OpenSlots(mode) => {
            state.open_slots(mode);
            MenuOutcome::None
        }
        OpenControls => {
            if state.menu.home_enabled() {
                state.open_shell_page("Controls");
            } else {
                state.menu.open_controls();
            }
            MenuOutcome::None
        }
        ResetControls => {
            state.reset_controls();
            // The shell caches pad state keyed by the just-replaced
            // bindings; it must throw that cache away or bits set
            // under the old mapping can never be released.
            MenuOutcome::ClearHostKeyboardInput
        }
        ToggleLibraryFolder(folder) => {
            state.menu.toggle_library_folder(&folder);
            MenuOutcome::None
        }
        LaunchGame(id) => {
            // The disc that is already loaded (or already on its way in)
            // restarts from the image in hand: no second download.
            #[cfg(target_arch = "wasm32")]
            if state.pick_web_disc(&id) {
                state.menu.open = false;
                return MenuOutcome::None;
            }
            // Game-launch rebuilds Bus + Cpu from scratch. Close
            // the Menu on success so the user sees the game boot.
            match state.launch_by_id(&id) {
                Ok(()) => {
                    state.menu.open = false;
                    // A download in flight leaves the current game running.
                    #[cfg(target_arch = "wasm32")]
                    state.menu_closed_by_back();
                }
                Err(e) => {
                    eprintln!("[frontend] launch failed: {e}");
                    state.status_message_set(format!("Launch failed: {e}"));
                }
            }
            MenuOutcome::None
        }
        OpenBurnMenu(id) => {
            if let Err(e) = state.open_burn_menu_by_id(&id) {
                eprintln!("[frontend] burn menu failed: {e}");
                state.status_message_set(format!("Burn menu failed: {e}"));
            }
            MenuOutcome::None
        }
        RescanLibrary => {
            match state.rescan_library() {
                Ok(n) => {
                    state.status_message_set(format!(
                        "Scan complete: {} entries ({n} new/changed)",
                        state.library.entries.len()
                    ));
                }
                Err(e) => {
                    eprintln!("[frontend] rescan failed: {e}");
                    state.status_message_set(format!("Rescan failed: {e}"));
                }
            }
            MenuOutcome::None
        }
        OpenDisc => {
            #[cfg(not(target_arch = "wasm32"))]
            state.open_disc_dialog();
            MenuOutcome::None
        }
        BuildExamples => {
            state.start_examples_build();
            MenuOutcome::None
        }
        #[cfg(feature = "editor")]
        ToggleEditorWorkspace => {
            state.toggle_editor_workspace();
            state.menu.open = false;
            MenuOutcome::None
        }
        ChooseGamesPath => {
            state.choose_games_path();
            MenuOutcome::None
        }
        CycleVideoScale => {
            state.scale_mode = match state.scale_mode {
                ScaleMode::Window => ScaleMode::Native,
                ScaleMode::Native => ScaleMode::Window,
            };
            MenuOutcome::None
        }
        CycleTextureFilter => {
            state.cycle_texture_filter();
            MenuOutcome::None
        }
        CycleDeinterlace => {
            state.cycle_deinterlace();
            MenuOutcome::None
        }
        CycleVolume => {
            const STEPS: [f32; 7] = [0.0, 0.25, 0.5, 0.75, 1.0, 1.25, 1.5];
            state.audio_volume = STEPS
                .into_iter()
                .find(|&step| step > state.audio_volume + 0.01)
                .unwrap_or(STEPS[0]);
            state.audio_muted = false;
            MenuOutcome::None
        }
        ToggleMute => {
            state.audio_muted = !state.audio_muted;
            MenuOutcome::None
        }
        CycleMenuOpacity => {
            state.cycle_menu_opacity();
            MenuOutcome::None
        }
        CycleUiScale => {
            state.cycle_ui_scale();
            MenuOutcome::None
        }
        CycleTouchMode => {
            state.edit_touch_prefs(|p| p.mode = p.mode.next());
            MenuOutcome::None
        }
        CycleTouchOpacity => {
            state.edit_touch_prefs(|p| p.cycle_opacity());
            MenuOutcome::None
        }
        CycleTouchSize => {
            state.edit_touch_prefs(|p| p.cycle_size());
            MenuOutcome::None
        }
        ToggleTouchHaptics => {
            state.edit_touch_prefs(|p| p.haptics = !p.haptics);
            MenuOutcome::None
        }
        ToggleTouchLeftHanded => {
            state.edit_touch_prefs(|p| p.left_handed = !p.left_handed);
            MenuOutcome::None
        }
        ToggleFullscreen => {
            state.fullscreen_requested = true;
            MenuOutcome::None
        }
        ToggleSmoothSlowHost => {
            state.toggle_smooth_slow_host();
            MenuOutcome::None
        }
        #[cfg(target_arch = "wasm32")]
        Reconnect => {
            state.reconnect_web_files();
            MenuOutcome::None
        }
        ShowAbout => {
            state.menu.show_about();
            MenuOutcome::None
        }
        StartDemoDisc => {
            state.start_demo_disc();
            MenuOutcome::None
        }
        ResumeGame => {
            state.menu.open = false;
            state.menu_closed_by_back();
            MenuOutcome::None
        }
        OpenUrl(url) => {
            menu::open_url(&url);
            MenuOutcome::None
        }
        SetVolume(pct) => {
            state.audio_volume = f32::from(pct) / 100.0;
            state.audio_muted = false;
            MenuOutcome::None
        }
        ToggleBoxArt => {
            state.toggle_box_art();
            MenuOutcome::None
        }
        ToggleReduceMotion => {
            state.toggle_reduce_motion();
            MenuOutcome::None
        }
        ToggleMenuSounds => {
            state.toggle_menu_sounds();
            MenuOutcome::None
        }
        Quit => MenuOutcome::Quit,
    }
}

/// The Controls panel's device section: every connected input device with
/// its live state, the guest port it drives, and the controller identity it
/// presents. The keyboard is listed like any pad. Returns a status message
/// when a setting changed.
fn controller_ports(ui: &mut egui::Ui, input_router: &mut InputRouter) -> Option<String> {
    let mut message = None;
    ui.label("Auto gives each controller a port on its first button press.");
    ui.label(
        egui::RichText::new("Digital is compatible with early games such as Crash Bandicoot.")
            .small()
            .color(crate::theme::MENU_TEXT_DIM),
    );
    ui.add_space(6.0);
    for device in input_router.devices() {
        let name = &device.identity.name;
        ui.horizontal_wrapped(|ui| {
            ui.add_sized(
                [(ui.available_width() - 12.0).clamp(80.0, 220.0), 26.0],
                egui::Label::new(egui::RichText::new(name).strong()).truncate(),
            )
            .on_hover_text(device.identity.describe());

            let mut choice = device.choice;
            let selected = match device.choice {
                PortChoice::Auto => format!("Auto ({})", device.port.label()),
                PortChoice::Pinned(port) => port.label().to_string(),
            };
            egui::ComboBox::from_id_salt(("controller_port", &device.identity.id))
                .selected_text(selected)
                .width(110.0)
                .show_ui(ui, |ui| {
                    for option in PortChoice::ALL {
                        ui.selectable_value(&mut choice, option, option.label());
                    }
                });
            if choice != device.choice
                && input_router.set_device_choice(&device.identity.id, choice)
            {
                message = Some(format!("{name}: {}", choice.label()));
            }

            // A keyboard has no sticks: it never offers analog mode.
            if device.identity.backend != crate::input::model::BackendKind::Keyboard
                && ui
                    .add_sized([78.0, 26.0], egui::Button::new(device.profile.label()))
                    .on_hover_text(device.profile.description())
                    .clicked()
                && input_router.set_device_profile(&device.identity.id, device.profile.next())
            {
                message = Some(format!(
                    "{name} will use {} mode",
                    device.profile.next().label()
                ));
            }
        });
        let dim = crate::theme::MENU_TEXT_DIM;
        let status = if !device.identity.mapped {
            egui::RichText::new("Unmapped device: its buttons are not recognised")
                .small()
                .color(egui::Color32::from_rgb(230, 150, 60))
        } else if device.ignored {
            egui::RichText::new("Ignored: sending constant input (stuck or faulty)")
                .small()
                .color(egui::Color32::from_rgb(230, 150, 60))
        } else {
            egui::RichText::new(device.live.describe())
                .small()
                .color(crate::theme::MENU_TEXT_VALUE)
        };
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            ui.label(status);
            ui.label(
                egui::RichText::new(format!(
                    "  {} · {}",
                    device.identity.backend.label(),
                    match device.identity.vid_pid {
                        Some((vid, pid)) => format!("{vid:04x}:{pid:04x}"),
                        None => "no id".to_string(),
                    }
                ))
                .small()
                .color(dim),
            );
        });
        ui.add_space(4.0);
    }
    message
}

/// What the shell needs to do after an Menu action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuOutcome {
    None,
    /// The action invalidated the bindings the shell's cached keyboard
    /// pad state was built under (Reset Controls). The shell must drop
    /// that cache and rebuild the current frame's merged input from
    /// the gamepad alone.
    ClearHostKeyboardInput,
    Quit,
}

pub(crate) fn draw_status_toast(ctx: &egui::Context, state: &AppState) {
    let Some((msg, ttl)) = state.status_message.as_ref() else {
        return;
    };
    let alpha = (*ttl / 0.35).clamp(0.0, 1.0);
    let bg = egui::Color32::from_rgba_premultiplied(5, 9, 13, (240.0 * alpha) as u8);
    let stroke = egui::Stroke::new(
        1.0,
        egui::Color32::from_rgba_premultiplied(34, 200, 229, (170.0 * alpha) as u8),
    );
    let text = egui::Color32::from_rgba_premultiplied(244, 248, 251, (255.0 * alpha) as u8);

    // Sit below whichever persistent indicators are up, so a toast never
    // lands on top of the freecam or recording badge.
    let mut toast_y = 48.0 + state.toast_inset_top;
    if state.input_recording_status().0 {
        toast_y += 46.0;
    }
    if state.freelook.enabled {
        toast_y += 46.0;
    }
    // Beside the debug panel, not over it.
    let docked = debug_sidebar::right_dock_points(
        ctx.screen_rect().size(),
        state.sidebar_width,
        state.debug_slide,
        state.menu.touch_mode(),
    );
    egui::Area::new("status-toast".into())
        .anchor(
            egui::Align2::RIGHT_TOP,
            egui::vec2(-(18.0 + docked), toast_y),
        )
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(bg)
                .stroke(stroke)
                .corner_radius(egui::CornerRadius::same(4))
                .inner_margin(egui::Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.set_max_width(420.0);
                    ui.add(
                        egui::Label::new(egui::RichText::new(msg).color(text).size(13.0)).wrap(),
                    );
                });
        });
}

/// Persistent freecam badge.
///
/// Freecam mutes the guest pad entirely, so without a standing indicator a
/// forgotten freecam looks exactly like a hung game: buttons do nothing and
/// nothing on screen says why. The toolbar EYE button already shows the state,
/// but it is not always visible and it does not explain the muted controller.
fn draw_freecam_indicator(ctx: &egui::Context, state: &AppState) {
    if !state.freelook.enabled {
        return;
    }
    let y = if state.input_recording_status().0 {
        94.0
    } else {
        48.0
    };
    egui::Area::new("freecam-indicator".into())
        .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-18.0, y))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(egui::Color32::from_rgba_premultiplied(12, 22, 30, 235))
                .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(0, 191, 230)))
                .corner_radius(egui::CornerRadius::same(4))
                .inner_margin(egui::Margin::symmetric(10, 6))
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(
                            "FREECAM  game input paused · tap L3+R3 to exit · hold to reset",
                        )
                        .color(egui::Color32::WHITE)
                        .strong(),
                    );
                });
        });
}

pub(crate) fn draw_recording_indicator(ctx: &egui::Context, state: &AppState) {
    let (recording, frames) = state.input_recording_status();
    if !recording {
        return;
    }
    // Sit left of the debug sidebar rather than over its buttons.
    let docked = debug_sidebar::right_dock_points(
        ctx.screen_rect().size(),
        state.sidebar_width,
        state.debug_slide,
        state.menu.touch_mode(),
    );
    let x = -(18.0 + docked);
    egui::Area::new("input-recording-indicator".into())
        .anchor(egui::Align2::RIGHT_TOP, egui::vec2(x, 48.0))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(egui::Color32::from_rgba_premultiplied(30, 12, 14, 235))
                .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(235, 62, 74)))
                .corner_radius(egui::CornerRadius::same(4))
                .inner_margin(egui::Margin::symmetric(10, 6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.colored_label(egui::Color32::from_rgb(245, 54, 68), "●");
                        ui.label(
                            egui::RichText::new(format!("REC  {frames} frames · F8 to stop"))
                                .color(egui::Color32::WHITE)
                                .strong(),
                        );
                    });
                });
        });
}
