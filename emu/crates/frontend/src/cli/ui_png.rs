//! Offscreen PNG of the emulator window chrome: the menu overlay, or the
//! toolbar, debug sidebar and recording badge over an empty screen.
//!
//! `ui-png --out FILE --view menu --category Game` lays the real widgets out
//! in an egui context and renders them through egui-wgpu into a texture that
//! is read back as a PNG. No window is created, so it is safe for agents and
//! CI, and the evidence is the same code the window draws.

use std::path::{Path, PathBuf};

use psoxide_settings::library::{GameKind, Region};
use psoxide_settings::LibraryEntry;

use super::debug_ui_png::{input, paint};
use crate::app::{AppState, TextureFilter};
use crate::theme;
use crate::ui;

/// What to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum View {
    /// The menu overlay with one column selected.
    Menu,
    /// Toolbar, debug sidebar, REC badge and status toast.
    Window,
}

/// Arguments for `ui-png`.
#[derive(Debug, clap::Args)]
pub struct UiPngArgs {
    /// PNG to write.
    #[arg(long)]
    pub out: PathBuf,
    #[arg(long, value_enum, default_value = "menu")]
    pub view: View,
    /// Menu column to select: Library, Game or Settings.
    #[arg(long, default_value = "Game")]
    pub category: String,
    #[arg(long, default_value_t = 1100)]
    pub width: u32,
    #[arg(long, default_value_t = 640)]
    pub height: u32,
    /// Boot this PS1 EXE first so the Game column and the tape controls are
    /// live (the Game column only exists while a game is loaded).
    #[arg(long)]
    pub exe: Option<PathBuf>,
    /// Start an input recording (needs `--exe`).
    #[arg(long, requires = "exe")]
    pub record: bool,
    /// Use a copy of this `library.ron` for the Library column. The file is
    /// only read; the render works in a private config tree.
    #[arg(long)]
    pub library_ron: Option<PathBuf>,
    /// Games folder the Library column groups by (`paths.game_library`).
    #[arg(long, requires = "library_ron")]
    pub games_root: Option<PathBuf>,
    /// Library folder to show expanded, relative to the games folder
    /// (repeatable; `/homebrew` is the Homebrew folder).
    #[arg(long)]
    pub expand: Vec<PathBuf>,
    /// Texture filter to show: none or edge.
    #[arg(long, default_value = "none")]
    pub filter: String,
}

pub(super) fn render(args: UiPngArgs) -> Result<(), String> {
    // A private config tree: rendering must never touch the user's settings.
    let root = std::env::temp_dir().join(format!("psoxide-ui-png-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).map_err(|e| format!("{}: {e}", root.display()))?;
    let result = render_in(&args, &root);
    let _ = std::fs::remove_dir_all(&root);
    result
}

fn render_in(args: &UiPngArgs, root: &Path) -> Result<(), String> {
    if let Some(library) = args.library_ron.as_ref() {
        std::fs::copy(library, root.join("library.ron"))
            .map_err(|e| format!("copy {}: {e}", library.display()))?;
        if let Some(games) = args.games_root.as_ref() {
            let mut settings = psoxide_settings::Settings::default();
            settings.paths.game_library = games.to_string_lossy().into_owned();
            settings
                .save(&root.join("settings.ron"))
                .map_err(|e| format!("write settings: {e}"))?;
        }
    }
    let mut state = AppState::with_config_dir(Some(root.to_path_buf()));
    for folder in &args.expand {
        state.menu.toggle_library_folder(folder);
    }
    state.texture_filter = TextureFilter::from_setting(&args.filter);
    if let Some(exe) = args.exe.as_ref() {
        let entry = LibraryEntry {
            id: "0123456789abcdef".to_string(),
            path: exe.clone(),
            kind: GameKind::Exe,
            title: "preview".to_string(),
            region: Region::Unknown,
            size: 0,
            mtime: 0,
            diagnostic: None,
        };
        state.launch_entry(&entry)?;
        if args.record {
            state.toggle_input_recording();
        }
    }
    state.menu.set_game_loaded(state.bus.is_some());
    state.menu.sync_video_audio(
        true,
        state.texture_filter.label(),
        state.audio_volume,
        state.audio_muted,
    );
    state
        .menu
        .sync_input_recording_label(state.input_recording_status().0);
    match args.view {
        View::Menu => {
            state.menu.select_category(&args.category);
            state.menu.open = true;
        }
        View::Window => {
            state.menu.open = false;
            state.panels.debug_sidebar = true;
            state.sidebar_width = 380.0;
            state.status_message_set("Texture filter: Edge");
        }
    }

    let ctx = egui::Context::default();
    theme::apply(&ctx);
    let vram = ctx.load_texture(
        "ui-png-vram",
        egui::ColorImage::new([4, 4], egui::Color32::BLACK),
        egui::TextureOptions::NEAREST,
    );
    let mut textures = egui::TexturesDelta::default();
    let mut last = None;
    // Enough frames for the menu dissolve, the column slide and the sidebar
    // slide to settle.
    for frame in 0..40u32 {
        let output = ctx.run(
            input(
                args.width,
                args.height,
                1.0 + f64::from(frame) * 0.1,
                Vec::<egui::Event>::new(),
            ),
            |ctx| {
                if args.view == View::Window {
                    ui::toolbar::draw(ctx, &mut state);
                    ui::debug_sidebar::draw(ctx, &mut state, vram.id());
                }
                state.menu.draw(ctx, 0.1, None);
                ui::draw_recording_indicator(ctx, &state);
                if args.view == View::Window {
                    ui::draw_status_toast(ctx, &state);
                }
            },
        );
        textures.append(output.textures_delta.clone());
        last = Some(output);
    }
    let output = last.expect("ran at least one frame");
    let jobs = ctx.tessellate(output.shapes, output.pixels_per_point);
    let rgba = paint(
        args.width,
        args.height,
        output.pixels_per_point,
        &jobs,
        textures,
    )?;
    if let Some(parent) = args.out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    image::save_buffer(
        &args.out,
        &rgba,
        args.width,
        args.height,
        image::ExtendedColorType::Rgba8,
    )
    .map_err(|e| format!("write {}: {e}", args.out.display()))
}
