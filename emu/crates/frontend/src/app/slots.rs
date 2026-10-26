//! Save slots of the running game: saving, loading and deleting, and the storage behind them.
//!
//! A game has a quick slot and eight numbered ones. The desktop keeps each as a file in the
//! game's folder under the settings directory; the browser keeps each in IndexedDB (see
//! `web_files`). Either way a slot is the machine state plus a small trailer that holds a
//! thumbnail, the time, how far the game had run and the identity of the disc (see
//! `psoxide_settings::slots`), so the sheet can list every slot without decoding any state and a
//! save can never be loaded into the wrong game or build.
//!
//! Loading validates everything (the trailer, the format, the game, the disc, a full decode of
//! the state) before it touches the running machine, so a refusal or a damaged file leaves the
//! game exactly as it was.

use psoxide_settings::slots::{
    check_loadable, format_play_time, is_slot, pack, read_meta, slot_name, split, Refusal,
    SlotMeta, Thumb, NUMBERED_SLOTS, QUICK_SLOT, SLOT_COUNT,
};

use psoxide_settings::savestate::SaveStateV1;

use psoxide_settings::LibraryEntry;

use super::{unix_now_secs, AppState};
use crate::ui::menu::slots::{SavedInfo, SlotCard};
use crate::ui::menu::SlotsMode;

/// The slots' bookkeeping that lives beside the machine.
#[derive(Default)]
pub(super) struct SlotBook {
    /// Browser: what each slot of the running game held when last listed.
    #[cfg(target_arch = "wasm32")]
    web: Vec<Option<Result<SlotMeta, Refusal>>>,
    /// Browser: the game those entries belong to.
    #[cfg(target_arch = "wasm32")]
    web_game: Option<String>,
    /// Browser: the operation waiting on storage.
    #[cfg(target_arch = "wasm32")]
    pending: Option<WebOp>,
}

/// A browser storage operation in flight.
#[cfg(target_arch = "wasm32")]
enum WebOp {
    Save(Box<SlotMeta>),
    Load,
    Delete,
    /// Load a slot's bytes to hand them over as a file of this name.
    Export(String),
}

impl AppState {
    /// Show the slot sheet over the running game, pausing it behind the shell.
    pub fn open_slots(&mut self, mode: SlotsMode) {
        if self.current_game.is_none() || self.bus.is_none() {
            self.status_message_set("Start a game to use save states");
            return;
        }
        let from_shell = self.menu.open;
        self.refresh_slot_cards();
        if !from_shell {
            let mut transport = self.transport(true);
            transport.ps_button();
            self.set_transport(transport);
        }
        self.menu.open_slots(mode, from_shell);
    }

    /// F5 and the pad shortcut: save into the quick slot.
    pub fn quick_save(&mut self) {
        self.save_slot(QUICK_SLOT);
    }

    /// F7 and the pad shortcut: load the quick slot.
    pub fn quick_load(&mut self) {
        if self.current_game.is_none() || self.bus.is_none() {
            self.status_message_set("Start a game to use save states");
            return;
        }
        if !self.slot_is_stored(QUICK_SLOT) {
            self.status_message_set("No quick save yet. Press F5 to make one.");
            return;
        }
        self.load_slot(QUICK_SLOT);
    }

    /// Write the running game into a slot, replacing what it held.
    pub fn save_slot(&mut self, slot: u8) {
        if !is_slot(slot) {
            return;
        }
        let Some(game) = self.current_game.clone() else {
            self.status_message_set("Start a game to use save states");
            return;
        };
        if self.bus.is_none() {
            self.status_message_set("Start a game to use save states");
            return;
        }
        #[cfg(target_arch = "wasm32")]
        if self.slots.pending.is_some() {
            self.status_message_set("Still saving. One moment.");
            return;
        }
        let created_at = unix_now_secs();
        let tick = self.cpu.tick();
        let meta = SlotMeta::for_new_save(
            &game.id,
            &game.title,
            self.current_game_hash,
            created_at,
            tick,
            None,
        );
        let state = match self.encode_state(&game.id, tick, created_at) {
            Ok(bytes) => bytes,
            Err(message) => {
                self.status_message_set(format!("Save failed: {message}"));
                return;
            }
        };
        let thumb = self.capture_thumb();
        self.store_save(slot, game.id.clone(), state, meta, thumb);
    }

    /// Pack a save with its picture and store it.
    fn store_save(
        &mut self,
        slot: u8,
        game_id: String,
        state: Vec<u8>,
        mut meta: SlotMeta,
        thumb: Option<Thumb>,
    ) {
        meta.thumb = thumb.filter(Thumb::is_consistent);
        let packed = match pack(state, &meta) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.status_message_set(format!("Save failed: {error}"));
                return;
            }
        };
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = self.paths.savestate_file(&game_id, slot);
            match psoxide_settings::slots::write_slot_file(&path, &packed) {
                Ok(()) => {
                    self.refresh_slot_cards();
                    self.status_message_set(saved_message(slot));
                }
                Err(error) => {
                    self.status_message_set(format!("Save failed: {error}"));
                }
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            let meta_bytes = psoxide_settings::slots::encode_meta(&meta);
            crate::web_files::slot_save(game_id, slot, &packed, &meta_bytes);
            self.slots.pending = Some(WebOp::Save(Box::new(meta)));
            self.menu.slots.busy = true;
        }
    }

    /// Replace the running game with what a slot holds.
    pub fn load_slot(&mut self, slot: u8) {
        if !is_slot(slot) {
            return;
        }
        let Some(game) = self.current_game.clone() else {
            self.status_message_set("Start a game to use save states");
            return;
        };
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = self.paths.savestate_file(&game.id, slot);
            match std::fs::read(&path) {
                Ok(bytes) => self.restore_slot(slot, &bytes),
                Err(_) => self.status_message_set(format!("{} is empty", slot_name(slot))),
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            if self.slots.pending.is_some() {
                self.status_message_set("Still working on a save. One moment.");
                return;
            }
            crate::web_files::slot_load(game.id.clone(), slot);
            self.slots.pending = Some(WebOp::Load);
            self.menu.slots.busy = true;
        }
    }

    /// Remove a slot's save.
    pub fn delete_slot(&mut self, slot: u8) {
        if !is_slot(slot) {
            return;
        }
        let Some(game) = self.current_game.clone() else {
            return;
        };
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = self.paths.savestate_file(&game.id, slot);
            let result = match std::fs::remove_file(&path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
                _ => Ok(()),
            };
            // A picture from before slots lived in its own file beside the save.
            let _ = std::fs::remove_file(self.paths.savestate_thumbnail_file(&game.id, slot));
            match result {
                Ok(()) => self.status_message_set(format!("Deleted {}", slot_name(slot))),
                Err(error) => self.status_message_set(format!("Delete failed: {error}")),
            }
            self.refresh_slot_cards();
        }
        #[cfg(target_arch = "wasm32")]
        {
            if self.slots.pending.is_some() {
                return;
            }
            crate::web_files::slot_delete(game.id.clone(), slot);
            self.slots.pending = Some(WebOp::Delete);
            self.menu.slots.busy = true;
        }
    }

    /// Hand a slot's save to the player as a file: a save dialog on the desktop, a download in the
    /// browser. The file is the slot as stored, so another PSoXide reads it (and checks it) like
    /// any save.
    pub fn export_slot(&mut self, slot: u8) {
        let Some(game) = self.current_game.clone() else {
            return;
        };
        let name = export_file_name(&game.title, slot);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = self.paths.savestate_file(&game.id, slot);
            let Ok(bytes) = std::fs::read(&path) else {
                self.status_message_set(format!("{} is empty", slot_name(slot)));
                return;
            };
            let Some(target) = rfd::FileDialog::new()
                .set_title("Export save state")
                .set_file_name(&name)
                .add_filter("PSoXide save state", &["psxstate"])
                .save_file()
            else {
                return;
            };
            match std::fs::write(&target, bytes) {
                Ok(()) => self.status_message_set(format!("Exported to {}", target.display())),
                Err(error) => self.status_message_set(format!("Export failed: {error}")),
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            if self.slots.pending.is_some() {
                return;
            }
            crate::web_files::slot_load(game.id.clone(), slot);
            self.slots.pending = Some(WebOp::Export(name));
            self.menu.slots.busy = true;
        }
    }

    /// Read a save file the player picks into the first empty numbered slot of the running
    /// game, after the same checks a load makes (game, disc, format). It never replaces a save.
    pub fn import_slot(&mut self) {
        if self.current_game.is_none() {
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let Some(source) = rfd::FileDialog::new()
                .set_title("Import save state")
                .add_filter("PSoXide save state", &["psxstate", "psx"])
                .pick_file()
            else {
                return;
            };
            match std::fs::read(&source) {
                Ok(bytes) => self.import_bytes(&bytes),
                Err(error) => self.status_message_set(format!("Import failed: {error}")),
            }
        }
        #[cfg(target_arch = "wasm32")]
        crate::web_files::pick_save();
    }

    /// Check a save file and store it in the first empty numbered slot.
    pub(super) fn import_bytes(&mut self, bytes: &[u8]) {
        let Some(game) = self.current_game.clone() else {
            return;
        };
        let meta = match read_meta(bytes) {
            Ok(meta) => meta,
            Err(refusal) => {
                self.ui_sound
                    .trigger(crate::ui_sound::UiSound::Refused, 1.0);
                self.status_message_set(refusal.message());
                return;
            }
        };
        let known_as = self.earlier_ids(&game.id);
        let same_game = |id: &str| id == game.id || known_as.iter().any(|old| old == id);
        if let Err(refusal) = check_loadable(&meta, same_game, self.current_game_hash) {
            self.ui_sound
                .trigger(crate::ui_sound::UiSound::Refused, 1.0);
            self.status_message_set(refusal.message());
            return;
        }
        let taken: Vec<bool> = (0..=NUMBERED_SLOTS)
            .map(|slot| {
                self.menu
                    .slots
                    .card(slot)
                    .is_some_and(|c| c.saved.is_some())
            })
            .collect();
        let Some(slot) = (1..=NUMBERED_SLOTS).find(|slot| !taken[usize::from(*slot)]) else {
            self.status_message_set("All slots are full. Delete one to import a save.");
            return;
        };
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = self.paths.savestate_file(&game.id, slot);
            match psoxide_settings::slots::write_slot_file(&path, bytes) {
                Ok(()) => {
                    self.refresh_slot_cards();
                    self.status_message_set(format!("Imported into {}", slot_name(slot)));
                }
                Err(error) => self.status_message_set(format!("Import failed: {error}")),
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            if self.slots.pending.is_some() {
                return;
            }
            let meta_bytes = psoxide_settings::slots::encode_meta(&meta);
            crate::web_files::slot_save(game.id.clone(), slot, bytes, &meta_bytes);
            self.slots.pending = Some(WebOp::Save(Box::new(meta)));
            self.menu.slots.busy = true;
        }
    }

    /// Whether the running game has something in a slot (as far as is known without reading it).
    fn slot_is_stored(&self, slot: u8) -> bool {
        let Some(game) = self.current_game.as_ref() else {
            return false;
        };
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.paths.savestate_file(&game.id, slot).is_file()
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = game;
            self.slots
                .web
                .get(usize::from(slot))
                .is_some_and(Option::is_some)
        }
    }

    /// A small picture of what the game is showing, from the emulated frame buffer. The part of
    /// VRAM on show is read straight from the CPU copy, which the CPU rasterizer keeps exact in
    /// the browser and on the desktop alike; the modes that image cannot show as plain 15-bit
    /// pixels (24-bit colour, interlaced fields, screen offsets) use the machine's own display
    /// image.
    fn capture_thumb(&self) -> Option<Thumb> {
        let gpu = &self.bus.as_ref()?.gpu;
        let area = gpu.display_area();
        let plain = !area.bpp24
            && !gpu.field_rendering_active()
            && gpu.horizontal_display_offset_px() == 0
            && gpu.vertical_display_offset_px() == 0;
        if plain {
            let (width, height) = (usize::from(area.width), usize::from(area.height));
            let words = gpu.vram.words();
            let mut rgba = Vec::with_capacity(width * height * 4);
            for row in 0..height {
                let y = (usize::from(area.y) + row) % 512;
                for col in 0..width {
                    let x = (usize::from(area.x) + col) % 1024;
                    rgba.extend(bgr15_to_rgba(words[y * 1024 + x]));
                }
            }
            return Thumb::from_rgba(&rgba, width as u32, height as u32);
        }
        let (rgba, width, height) = gpu.display_rgba8_with(self.deinterlace);
        Thumb::from_rgba(&rgba, width, height)
    }

    /// The machine's state as bytes.
    fn encode_state(&self, game_id: &str, tick: u64, created_at: u64) -> Result<Vec<u8>, String> {
        let (Some(bus), cpu) = (self.bus.as_ref(), &self.cpu) else {
            return Err("no game is running".to_string());
        };
        let encode = || {
            let state = SaveStateV1::new_at(
                emulator_core::EmulatorStateRef { cpu, bus },
                game_id,
                tick,
                created_at,
            );
            state.to_bytes()
        };
        run_with_big_stack(encode)?.map_err(|e| e.to_string())
    }

    /// Put a slot's save into the running machine. Nothing changes unless every check passes.
    fn restore_slot(&mut self, slot: u8, bytes: &[u8]) {
        let Some(game) = self.current_game.clone() else {
            return;
        };
        match self.try_restore(&game, bytes) {
            Ok(()) => {
                self.refresh_slot_cards();
                self.status_message_set(loaded_message(slot));
            }
            Err(refusal) => {
                self.ui_sound
                    .trigger(crate::ui_sound::UiSound::Refused, 1.0);
                self.status_message_set(refusal.message());
                self.refresh_slot_cards();
            }
        }
    }

    fn try_restore(&mut self, game: &LibraryEntry, bytes: &[u8]) -> Result<(), Refusal> {
        let meta = read_meta(bytes)?;
        let known_as = self.earlier_ids(&game.id);
        let same_game = |id: &str| id == game.id || known_as.iter().any(|old| old == id);
        check_loadable(&meta, same_game, self.current_game_hash)?;
        let (state, _) = split(bytes);
        let loaded =
            run_with_big_stack(|| SaveStateV1::<emulator_core::EmulatorState>::from_bytes(state))
                .map_err(|_| Refusal::Damaged)?
                .map_err(|error| Refusal::from_error(&error))?;
        // The header is the authority on the game; the trailer was only a summary of it.
        if !same_game(&loaded.header.game_id) {
            return Err(Refusal::OtherGame);
        }
        let Some(donor) = self.bus.as_mut() else {
            return Err(Refusal::Damaged);
        };
        let mut payload = loaded.payload;
        // The save leaves out the BIOS image and the disc: the machine that is running has both.
        payload.bus.restore_excluded_from(donor);
        #[cfg(not(target_arch = "wasm32"))]
        {
            // A memory card is the game's own file, not part of a state: loading a state must
            // never roll back what the player saved to the card since.
            if let Err(error) = self.flush_memcard_port1() {
                eprintln!("[frontend] memcard flush before a load: {error}");
            }
            let card = self
                .memcard_port1_path
                .as_ref()
                .and_then(|path| std::fs::read(path).ok())
                .unwrap_or_default();
            payload.bus.attach_memcard_port1(card);
        }
        self.cpu = payload.cpu;
        crate::install_native_tier(&mut self.cpu);
        self.bus = Some(payload.bus);
        self.gpu_resync_generation = self.gpu_resync_generation.wrapping_add(1);
        self.exec_history.clear();
        self.gpr_snapshot = None;
        // The game runs on from the loaded frame and the shell steps out of the way.
        let mut transport = self.transport(true);
        transport.game_booted();
        self.set_transport(transport);
        Ok(())
    }

    /// Ids this game was known by before, whose saves still belong to it.
    fn earlier_ids(&self, id: &str) -> Vec<String> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.paths.migrated_from(id)
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = id;
            Vec::new()
        }
    }

    /// Rebuild the sheet's cards from what is stored for the running game.
    pub(super) fn refresh_slot_cards(&mut self) {
        let Some(game) = self.current_game.clone() else {
            self.menu.sync_slots("", empty_cards());
            return;
        };
        let known_as = self.earlier_ids(&game.id);
        let same_game = |id: &str| id == game.id || known_as.iter().any(|old| old == id);
        let hash = self.current_game_hash;
        let mut cards = Vec::with_capacity(SLOT_COUNT);
        for slot in 0..=NUMBERED_SLOTS {
            let stored = self.stored_meta(&game, slot);
            cards.push(card_for(slot, stored, &same_game, hash));
        }
        self.menu.sync_slots(&game.title, cards);
    }

    /// What a slot holds: `None` when nothing is stored there.
    #[cfg(not(target_arch = "wasm32"))]
    fn stored_meta(&self, game: &LibraryEntry, slot: u8) -> Option<Result<SlotMeta, Refusal>> {
        let path = self.paths.savestate_file(&game.id, slot);
        if !path.is_file() {
            return None;
        }
        let mut meta = psoxide_settings::slots::read_meta_file(&path);
        // A save from before slots kept its picture in a file beside it.
        if let Ok(meta) = meta.as_mut() {
            if meta.thumb.is_none() {
                meta.thumb = std::fs::read(self.paths.savestate_thumbnail_file(&game.id, slot))
                    .ok()
                    .and_then(|png| image::load_from_memory(&png).ok())
                    .and_then(|png| {
                        let rgba = png.to_rgba8();
                        Thumb::from_rgba(rgba.as_raw(), rgba.width(), rgba.height())
                    });
            }
        }
        Some(meta)
    }

    #[cfg(target_arch = "wasm32")]
    fn stored_meta(&self, game: &LibraryEntry, slot: u8) -> Option<Result<SlotMeta, Refusal>> {
        if self.slots.web_game.as_deref() != Some(game.id.as_str()) {
            return None;
        }
        self.slots.web.get(usize::from(slot)).cloned().flatten()
    }

    /// The running game changed: forget the old game's slots and read the new one's.
    pub(super) fn slot_game_changed(&mut self, id: &str) {
        #[cfg(target_arch = "wasm32")]
        {
            self.slots.web = vec![None; SLOT_COUNT];
            self.slots.web_game = Some(id.to_string());
            self.slots.pending = None;
            self.menu.slots.busy = false;
            crate::web_files::slot_list(id.to_string());
        }
        #[cfg(not(target_arch = "wasm32"))]
        let _ = id;
        self.refresh_slot_cards();
    }

    /// Browser: apply what the storage has finished since last frame.
    #[cfg(target_arch = "wasm32")]
    pub(super) fn poll_web_slots(&mut self) {
        use crate::web_files::SlotEvent;
        for event in crate::web_files::drain_slot_events() {
            let current = self.current_game.as_ref().map(|g| g.id.clone());
            match event {
                SlotEvent::Listed {
                    game_id,
                    slots,
                    result,
                } => {
                    if current.as_deref() != Some(game_id.as_str()) {
                        continue;
                    }
                    if let Err(error) = result {
                        self.status_message_set(format!("Could not read saves: {error}"));
                        continue;
                    }
                    let mut web = vec![None; SLOT_COUNT];
                    for (slot, bytes) in slots {
                        if let Some(entry) = web.get_mut(usize::from(slot)) {
                            *entry = Some(parse_stored_meta(&bytes));
                        }
                    }
                    self.slots.web = web;
                    self.slots.web_game = Some(game_id);
                    self.refresh_slot_cards();
                }
                SlotEvent::Saved {
                    game_id,
                    slot,
                    result,
                } => {
                    let op = self.slots.pending.take();
                    self.menu.slots.busy = false;
                    if current.as_deref() != Some(game_id.as_str()) {
                        continue;
                    }
                    match (result, op) {
                        (Ok(()), Some(WebOp::Save(meta))) => {
                            if let Some(entry) = self.slots.web.get_mut(usize::from(slot)) {
                                *entry = Some(Ok(*meta));
                            }
                            self.refresh_slot_cards();
                            self.status_message_set(saved_message(slot));
                        }
                        (Ok(()), _) => self.slot_game_changed(&game_id),
                        (Err(error), _) => {
                            self.status_message_set(format!("Save failed: {error}"));
                        }
                    }
                }
                SlotEvent::Loaded {
                    game_id,
                    slot,
                    result,
                } => {
                    let mut export = self.slots.pending.take();
                    if !matches!(export, Some(WebOp::Export(_))) {
                        export = None;
                    }
                    self.menu.slots.busy = false;
                    if current.as_deref() != Some(game_id.as_str()) {
                        self.status_message_set("Load cancelled: a different game is running now");
                        continue;
                    }
                    if let Some(WebOp::Export(name)) = export.take() {
                        match result {
                            Ok(Some(bytes)) => {
                                crate::web_files::download_bytes(&name, &bytes);
                                self.status_message_set(format!("Exported {name}"));
                            }
                            Ok(None) => {
                                self.status_message_set(format!("{} is empty", slot_name(slot)));
                            }
                            Err(error) => {
                                self.status_message_set(format!("Export failed: {error}"));
                            }
                        }
                        continue;
                    }
                    match result {
                        Ok(Some(bytes)) => self.restore_slot(slot, &bytes),
                        Ok(None) => {
                            self.status_message_set(format!("{} is empty", slot_name(slot)));
                        }
                        Err(error) => {
                            self.status_message_set(format!("Load failed: {error}"));
                        }
                    }
                }
                SlotEvent::Deleted {
                    game_id,
                    slot,
                    result,
                } => {
                    self.slots.pending = None;
                    self.menu.slots.busy = false;
                    if current.as_deref() != Some(game_id.as_str()) {
                        continue;
                    }
                    match result {
                        Ok(()) => {
                            if let Some(entry) = self.slots.web.get_mut(usize::from(slot)) {
                                *entry = None;
                            }
                            self.refresh_slot_cards();
                            self.status_message_set(format!("Deleted {}", slot_name(slot)));
                        }
                        Err(error) => {
                            self.status_message_set(format!("Delete failed: {error}"));
                        }
                    }
                }
            }
        }
    }
}

/// A browser slot's metadata from the bytes stored beside it.
#[cfg(target_arch = "wasm32")]
fn parse_stored_meta(bytes: &[u8]) -> Result<SlotMeta, Refusal> {
    psoxide_settings::slots::decode_meta(bytes).map_or_else(|| read_meta(bytes), Ok)
}

/// What the sheet shows for a slot of the running game.
fn card_for(
    slot: u8,
    stored: Option<Result<SlotMeta, Refusal>>,
    same_game: &impl Fn(&str) -> bool,
    disc_hash: Option<u64>,
) -> SlotCard {
    let offset = |at: u64| local_offset_secs(at);
    let now = unix_now_secs();
    let saved = match stored {
        None => None,
        Some(Err(refusal)) => Some(SavedInfo {
            when: "Unreadable".to_string(),
            played: String::new(),
            stamp: 0,
            thumb: None,
            tag: Some(refusal.tag()),
            problem: Some(refusal.message()),
            creator: String::new(),
            luma: 0,
            sig: String::new(),
        }),
        Some(Ok(meta)) => {
            let refusal = check_loadable(&meta, same_game, disc_hash).err();
            let (luma, sig) = meta.thumb.as_ref().map_or((0, String::new()), brightness);
            Some(SavedInfo {
                when: when_label(meta.created_at, now, offset(meta.created_at)),
                played: format_play_time(meta.play_seconds()),
                stamp: meta.created_at,
                thumb: meta.thumb,
                tag: refusal.as_ref().map(Refusal::tag),
                problem: refusal.map(|r| r.message()),
                creator: meta.creator,
                luma,
                sig,
            })
        }
    };
    SlotCard { slot, saved }
}

/// A picture's average brightness (0 to 100) and its 8 by 6 grid of brightness digits (0 to 9).
fn brightness(thumb: &Thumb) -> (u8, String) {
    let (w, h) = (usize::from(thumb.w), usize::from(thumb.h));
    let at = |x: usize, y: usize| {
        let i = (y * w + x) * 3;
        (u32::from(thumb.rgb[i]) * 299
            + u32::from(thumb.rgb[i + 1]) * 587
            + u32::from(thumb.rgb[i + 2]) * 114)
            / 1000
    };
    let mut total = 0u64;
    for y in 0..h {
        for x in 0..w {
            total += u64::from(at(x, y));
        }
    }
    let mean = (total / (w * h).max(1) as u64) as u32;
    let mut sig = String::new();
    for gy in 0..6 {
        for gx in 0..8 {
            let (x0, x1, y0, y1) = (gx * w / 8, (gx + 1) * w / 8, gy * h / 6, (gy + 1) * h / 6);
            let mut sum = 0u64;
            for y in y0..y1 {
                for x in x0..x1 {
                    sum += u64::from(at(x, y));
                }
            }
            let cell = sum / ((x1 - x0) * (y1 - y0)).max(1) as u64;
            sig.push(char::from(b'0' + (cell * 10 / 256) as u8));
        }
    }
    ((mean * 100 / 255) as u8, sig)
}

/// The file name an exported save gets: the game and the slot, safe in any file system.
fn export_file_name(title: &str, slot: u8) -> String {
    let clean: String = title
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let clean = clean.trim_matches('-');
    let clean = if clean.is_empty() { "game" } else { clean };
    let which = if slot == QUICK_SLOT {
        "quick".to_string()
    } else {
        format!("slot{slot}")
    };
    format!("{clean}-{which}.psxstate")
}

/// One 15-bit pixel as RGBA.
fn bgr15_to_rgba(pixel: u16) -> [u8; 4] {
    let expand = |v: u16| ((v << 3) | (v >> 2)) as u8;
    [
        expand(pixel & 0x1f),
        expand((pixel >> 5) & 0x1f),
        expand((pixel >> 10) & 0x1f),
        0xff,
    ]
}

fn empty_cards() -> Vec<SlotCard> {
    (0..=NUMBERED_SLOTS).map(SlotCard::empty).collect()
}

fn saved_message(slot: u8) -> String {
    if slot == QUICK_SLOT {
        "Quick saved".to_string()
    } else {
        format!("Saved to {}", slot_name(slot))
    }
}

fn loaded_message(slot: u8) -> String {
    if slot == QUICK_SLOT {
        "Quick loaded".to_string()
    } else {
        format!("Loaded {}", slot_name(slot))
    }
}

/// Run `work` where the deep recursion of encoding or decoding the machine's state has room: a
/// big stack of its own on the desktop (the window's thread can be small), the page's own stack
/// in the browser.
fn run_with_big_stack<T: Send>(work: impl FnOnce() -> T + Send) -> Result<T, String> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .stack_size(64 * 1024 * 1024)
                .spawn_scoped(scope, work)
                .map_err(|error| format!("could not start a worker: {error}"))?
                .join()
                .map_err(|_| "the state could not be processed".to_string())
        })
    }
    #[cfg(target_arch = "wasm32")]
    {
        Ok(work())
    }
}

/// Seconds the local clock is ahead of UTC at a moment.
#[cfg(not(target_arch = "wasm32"))]
fn local_offset_secs(at: u64) -> i64 {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_opt(at as i64, 0)
        .single()
        .map_or(0, |time| i64::from(time.offset().local_minus_utc()))
}

#[cfg(target_arch = "wasm32")]
fn local_offset_secs(at: u64) -> i64 {
    // `getTimezoneOffset` is minutes UTC is ahead of local time, so the sign flips.
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(at as f64 * 1000.0));
    -(date.get_timezone_offset() as i64) * 60
}

/// "Today 14:32", "Yesterday 09:10", "Oct 22, 18:05", or with the year for an older one. `offset`
/// is the local clock's distance ahead of UTC in seconds.
pub(crate) fn when_label(created_at: u64, now: u64, offset: i64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let local = created_at as i64 + offset;
    let (day, secs) = (local.div_euclid(86_400), local.rem_euclid(86_400));
    let today = (now as i64 + offset).div_euclid(86_400);
    let (y, m, d) = civil_from_days(day);
    let (year_now, _, _) = civil_from_days(today);
    let clock = format!("{:02}:{:02}", secs / 3600, secs / 60 % 60);
    if day == today {
        format!("Today {clock}")
    } else if day == today - 1 {
        format!("Yesterday {clock}")
    } else if y == year_now {
        format!("{} {d}, {clock}", MONTHS[(m - 1) as usize])
    } else {
        format!("{} {d} {y}, {clock}", MONTHS[(m - 1) as usize])
    }
}

/// The calendar date of a day count since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Howard Hinnant's algorithm, exact for every date in the proleptic Gregorian calendar.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use psoxide_settings::library::{GameKind, Region};

    use super::super::step_one_frame;
    use super::*;

    // 2026-10-25 12:00:00 UTC.
    const NOON: u64 = 1_792_929_600;

    #[test]
    fn dates_are_right_across_a_calendar() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(NOON as i64 / 86_400), (2026, 10, 25));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn a_save_reads_as_today_yesterday_or_a_date() {
        assert_eq!(when_label(NOON - 3_600, NOON, 0), "Today 11:00");
        assert_eq!(when_label(NOON - 86_400, NOON, 0), "Yesterday 12:00");
        assert_eq!(when_label(NOON - 3 * 86_400, NOON, 0), "Oct 22, 12:00");
        assert_eq!(
            when_label(NOON - 400 * 86_400, NOON, 0),
            "Sep 20 2025, 12:00"
        );
    }

    #[test]
    fn the_local_clock_decides_what_day_it_was() {
        // 23:30 UTC the evening before is already after midnight two hours east.
        let late = NOON - 12 * 3_600 - 1_800;
        assert_eq!(when_label(late, NOON, 0), "Yesterday 23:30");
        assert_eq!(when_label(late, NOON, 2 * 3_600), "Today 01:30");
        assert_eq!(when_label(late, NOON, -5 * 3_600), "Yesterday 18:30");
    }

    fn meta(game: &str, hash: u64) -> SlotMeta {
        SlotMeta::for_new_save(game, "T", Some(hash), NOON, 33_868_800 * 187, None)
    }

    #[test]
    fn a_card_shows_when_and_how_far_and_flags_what_cannot_load() {
        let same = |id: &str| id == "g";
        let ok = card_for(2, Some(Ok(meta("g", 7))), &same, Some(7));
        let info = ok.saved.expect("filled");
        assert_eq!(info.played, "3:07");
        assert!(info.problem.is_none() && info.tag.is_none());

        let other_disc = card_for(2, Some(Ok(meta("g", 7))), &same, Some(8));
        let info = other_disc.saved.expect("filled");
        assert_eq!(info.tag, Some("Other disc"));
        assert!(info.problem.is_some());

        let other_game = card_for(1, Some(Ok(meta("h", 7))), &same, Some(7));
        assert_eq!(other_game.saved.expect("filled").tag, Some("Other game"));

        let damaged = card_for(3, Some(Err(Refusal::Damaged)), &same, None);
        assert_eq!(damaged.saved.expect("filled").tag, Some("Damaged"));
        assert!(card_for(4, None, &same, None).saved.is_none());
    }

    #[test]
    fn the_messages_name_the_slot() {
        assert_eq!(saved_message(0), "Quick saved");
        assert_eq!(saved_message(3), "Saved to Slot 3");
        assert_eq!(loaded_message(0), "Quick loaded");
        assert_eq!(loaded_message(8), "Loaded Slot 8");
    }

    // ---- A whole game: save, change, load, refuse, overwrite, delete ----

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "psoxide-slots-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A tiny bootable disc: SYSTEM.CNF plus a 4-byte PSX-EXE.
    fn bootable_bin(tag: u8) -> Vec<u8> {
        let mut exe = vec![0u8; psx_iso::EXE_HEADER_BYTES];
        exe[..8].copy_from_slice(b"PS-X EXE");
        exe[0x10..0x14].copy_from_slice(&0x8001_0000u32.to_le_bytes());
        exe[0x18..0x1c].copy_from_slice(&0x8001_0000u32.to_le_bytes());
        exe[0x1c..0x20].copy_from_slice(&4u32.to_le_bytes());
        exe.extend_from_slice(&[tag; 4]);
        let mut builder = psx_iso::IsoBuilder::new();
        builder.add_file("SYSTEM.CNF", b"BOOT = cdrom:\\GAME.EXE;1\r\n".to_vec());
        builder.add_file("GAME.EXE", exe);
        builder.build_bin()
    }

    fn running_game(root: &Path) -> AppState {
        let bin = root.join("game.bin");
        std::fs::write(&bin, bootable_bin(0)).unwrap();
        let mut state = AppState::with_config_dir(Some(root.join("config")));
        let entry = LibraryEntry {
            id: "slotgame".into(),
            path: bin,
            kind: GameKind::DiscBin,
            title: "Slot Game".into(),
            region: Region::Unknown,
            size: 0,
            mtime: 0,
            diagnostic: None,
        };
        state.launch_entry(&entry).expect("boots");
        state
    }

    fn run_frames(state: &mut AppState, frames: usize) {
        for _ in 0..frames {
            step_one_frame(state);
        }
    }

    fn status(state: &AppState) -> String {
        state
            .status_message
            .as_ref()
            .map(|(m, _)| m.clone())
            .unwrap_or_default()
    }

    #[test]
    fn a_save_is_a_slot_that_loads_back_the_exact_machine() {
        let root = test_dir("roundtrip");
        let mut state = running_game(&root);
        run_frames(&mut state, 3);
        let saved_tick = state.cpu.tick();
        state.save_slot(2);
        assert_eq!(status(&state), "Saved to Slot 2");
        let card = state
            .menu
            .slots
            .card(2)
            .and_then(|c| c.saved.clone())
            .expect("card");
        assert!(card.problem.is_none());
        assert!(card.when.starts_with("Today"), "{}", card.when);

        run_frames(&mut state, 5);
        assert!(state.cpu.tick() > saved_tick);
        state.load_slot(2);
        assert_eq!(status(&state), "Loaded Slot 2");
        assert_eq!(
            state.cpu.tick(),
            saved_tick,
            "the machine is back where it was saved"
        );
        assert!(
            state.running && !state.menu.open,
            "the game runs on from the loaded frame"
        );
        // The loaded machine runs.
        run_frames(&mut state, 2);
        assert!(state.cpu.tick() > saved_tick);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn quick_save_and_quick_load_use_slot_zero_and_say_so() {
        let root = test_dir("quick");
        let mut state = running_game(&root);
        state.quick_load();
        assert!(
            status(&state).starts_with("No quick save yet"),
            "{}",
            status(&state)
        );
        run_frames(&mut state, 2);
        let tick = state.cpu.tick();
        state.quick_save();
        assert_eq!(status(&state), "Quick saved");
        run_frames(&mut state, 4);
        state.quick_load();
        assert_eq!(status(&state), "Quick loaded");
        assert_eq!(state.cpu.tick(), tick);
        assert!(state.paths.savestate_file("slotgame", QUICK_SLOT).is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn saving_over_a_slot_replaces_it_whole_and_delete_removes_it() {
        let root = test_dir("overwrite");
        let mut state = running_game(&root);
        run_frames(&mut state, 1);
        let first = state.cpu.tick();
        state.save_slot(1);
        run_frames(&mut state, 4);
        let second = state.cpu.tick();
        state.save_slot(1);
        run_frames(&mut state, 3);
        state.load_slot(1);
        assert_eq!(state.cpu.tick(), second, "the second save won");
        assert_ne!(first, second);
        let leftovers: Vec<_> = std::fs::read_dir(state.paths.savestates_dir("slotgame"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, ["slot1.psx"], "no temporary file is left behind");

        state.delete_slot(1);
        assert!(!state.paths.savestate_file("slotgame", 1).exists());
        assert!(state.menu.slots.card(1).unwrap().saved.is_none());
        state.load_slot(1);
        assert!(status(&state).contains("empty"), "{}", status(&state));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_save_never_loads_into_another_disc_game_or_build() {
        let root = test_dir("refusals");
        let mut state = running_game(&root);
        run_frames(&mut state, 2);
        state.save_slot(3);
        run_frames(&mut state, 4);
        let tick_now = state.cpu.tick();

        // The same game id, but another build of the disc.
        let real = state.current_game_hash;
        state.current_game_hash = Some(real.unwrap_or(1) ^ 0xdead_beef);
        state.load_slot(3);
        assert!(
            status(&state).contains("different disc"),
            "{}",
            status(&state)
        );
        assert_eq!(state.cpu.tick(), tick_now, "a refused load changes nothing");
        let card = state.menu.slots.card(3).unwrap().saved.clone().unwrap();
        assert_eq!(
            card.tag,
            Some("Other disc"),
            "the sheet says so before the click"
        );
        state.current_game_hash = real;

        // A save from a newer or older state format.
        let path = state.paths.savestate_file("slotgame", 3);
        let good = std::fs::read(&path).unwrap();
        for (version, word) in [
            (
                psoxide_settings::savestate::SAVESTATE_FORMAT_VERSION + 1,
                "newer",
            ),
            (
                psoxide_settings::savestate::SAVESTATE_MIN_SUPPORTED_VERSION - 1,
                "older",
            ),
        ] {
            let (inner, meta) = split(&good);
            let mut meta = meta.unwrap();
            meta.format_version = version;
            std::fs::write(&path, pack(inner.to_vec(), &meta).unwrap()).unwrap();
            state.load_slot(3);
            assert!(
                status(&state).contains(word),
                "{version}: {}",
                status(&state)
            );
            assert_eq!(state.cpu.tick(), tick_now);
        }

        // A save made for another game.
        let (inner, meta) = split(&good);
        let mut meta = meta.unwrap();
        meta.game_id = "another-game".into();
        std::fs::write(&path, pack(inner.to_vec(), &meta).unwrap()).unwrap();
        state.load_slot(3);
        assert!(
            status(&state).contains("different game"),
            "{}",
            status(&state)
        );
        assert_eq!(state.cpu.tick(), tick_now);

        // Damaged: cut off, or not a save at all.
        std::fs::write(&path, &good[..good.len() / 3]).unwrap();
        state.load_slot(3);
        assert!(status(&state).contains("damaged"), "{}", status(&state));
        std::fs::write(&path, b"this is not a save state").unwrap();
        state.load_slot(3);
        assert!(status(&state).contains("damaged"), "{}", status(&state));
        assert_eq!(state.cpu.tick(), tick_now);
        // The machine still runs after every refusal.
        run_frames(&mut state, 1);
        assert!(state.cpu.tick() > tick_now);

        // Put the good file back: it loads again.
        std::fs::write(&path, &good).unwrap();
        state.load_slot(3);
        assert_eq!(status(&state), "Loaded Slot 3");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn saves_survive_a_restart_of_the_app() {
        let root = test_dir("restart");
        let mut state = running_game(&root);
        run_frames(&mut state, 2);
        let tick = state.cpu.tick();
        state.save_slot(5);
        drop(state);
        // A new process: a fresh state on the same settings directory and the same disc.
        let mut again = running_game(&root);
        let card = again.menu.slots.card(5).and_then(|c| c.saved.clone());
        assert!(card.is_some(), "the slot is listed after a restart");
        assert!(card.unwrap().problem.is_none());
        again.load_slot(5);
        assert_eq!(again.cpu.tick(), tick);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_exported_save_imports_into_the_first_empty_slot_after_the_same_checks() {
        let root = test_dir("import");
        let mut state = running_game(&root);
        run_frames(&mut state, 2);
        let tick = state.cpu.tick();
        state.save_slot(1);
        let file = std::fs::read(state.paths.savestate_file("slotgame", 1)).unwrap();
        state.import_bytes(&file);
        assert!(
            status(&state).starts_with("Imported into Slot 2"),
            "{}",
            status(&state)
        );
        run_frames(&mut state, 3);
        state.load_slot(2);
        assert_eq!(state.cpu.tick(), tick, "the import is the same machine");
        // Not a save, another game, another disc: refused, nothing written.
        state.import_bytes(b"not a save");
        assert!(status(&state).contains("damaged"), "{}", status(&state));
        let (inner, meta) = split(&file);
        let mut other = meta.unwrap();
        other.game_id = "elsewhere".into();
        state.import_bytes(&pack(inner.to_vec(), &other).unwrap());
        assert!(
            status(&state).contains("different game"),
            "{}",
            status(&state)
        );
        assert!(!state.paths.savestate_file("slotgame", 3).exists());
        // Every numbered slot taken: nothing is overwritten.
        for slot in 3..=NUMBERED_SLOTS {
            state.save_slot(slot);
        }
        state.import_bytes(&file);
        assert!(status(&state).contains("full"), "{}", status(&state));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn export_names_are_safe_file_names() {
        assert_eq!(
            export_file_name("Crash Bandicoot (USA)", 3),
            "Crash-Bandicoot--USA-slot3.psxstate"
        );
        assert_eq!(export_file_name("???", 0), "game-quick.psxstate");
    }

    #[test]
    fn the_sheet_opens_over_the_running_game_and_pauses_it() {
        let root = test_dir("open");
        let mut state = running_game(&root);
        state.menu.open = false;
        assert!(state.running);
        state.open_slots(SlotsMode::Load);
        assert!(
            state.menu.open && !state.running,
            "the shell pauses the game behind it"
        );
        assert_eq!(state.menu.slots.mode, SlotsMode::Load);
        let mut none = AppState::with_config_dir(Some(root.join("other")));
        none.menu.open = false;
        none.open_slots(SlotsMode::Save);
        assert!(!none.menu.open, "no game, no sheet");
        assert!(status(&none).contains("Start a game"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn there_is_a_card_for_every_slot_and_all_start_empty() {
        let cards = empty_cards();
        assert_eq!(cards.len(), SLOT_COUNT);
        assert!(cards.iter().all(|c| c.saved.is_none()));
        assert_eq!(cards[0].slot, QUICK_SLOT);
    }
}
