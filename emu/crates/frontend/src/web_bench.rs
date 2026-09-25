//! Measurement hooks for the web build.
//!
//! A headless browser cannot drive the egui menu reliably, so two small
//! hooks let a script measure the real frame loop:
//!
//! - `?disc=<path>` in the page URL fetches a disc image (or PS-EXE) served
//!   next to the page and boots it through the same path as a picked file.
//!   Only same-origin relative paths are accepted.
//! - `psoxideBenchStats()` (exported to JS) returns running totals of the
//!   redraw profile as JSON; `psoxideBenchReset()` zeroes them so a script can
//!   measure a steady-state window.
//!
//! - `?smooth=1` turns on `video.smooth_slow_host` for the session, so the
//!   slow-host alternative can be measured without clicking the menu.
//!
//! All are inert unless called: no URL parameter, no work.

use std::cell::RefCell;

use wasm_bindgen::prelude::*;

use crate::ui::profiler::FrameProfileSample;

#[derive(Default)]
struct Totals {
    redraws: u64,
    guest_frames: u64,
    total_ms: f64,
    emu_ms: f64,
    audio_ms: f64,
    hw_render_ms: f64,
    hw_vram_clone_ms: f64,
    egui_ms: f64,
    max_total_ms: f32,
    /// Redraws whose handler took longer than one 60 Hz frame.
    long_redraws: u64,
    audio_underrun_frames: u64,
    audio_queue_len: usize,
    /// Guest frames held back waiting for disc sectors.
    disc_waits: u64,
    /// Drive-side counts since boot: sector deliveries that had to wait, and
    /// CD-DA pieces played as silence (see `CdRom::late_sector_counts`).
    late_sectors: u64,
    late_cdda: u64,
}

thread_local! {
    static TOTALS: RefCell<Totals> = RefCell::new(Totals::default());
}

/// Fold one redraw's profile into the totals.
pub fn note_redraw(
    sample: &FrameProfileSample,
    underrun_frames: u64,
    audio_queue_len: usize,
    late_sectors: (u64, u64),
) {
    TOTALS.with(|t| {
        let mut t = t.borrow_mut();
        t.redraws += 1;
        t.guest_frames += sample.frames_run as u64;
        t.total_ms += sample.total_ms as f64;
        t.emu_ms += sample.emu_ms as f64;
        t.audio_ms += sample.audio_ms as f64;
        t.hw_render_ms += sample.hw_render_ms as f64;
        t.hw_vram_clone_ms += sample.hw_vram_clone_ms as f64;
        t.egui_ms += sample.egui.total_ms as f64;
        t.max_total_ms = t.max_total_ms.max(sample.total_ms);
        if sample.total_ms > 1000.0 / 60.0 {
            t.long_redraws += 1;
        }
        t.audio_underrun_frames = underrun_frames;
        t.audio_queue_len = audio_queue_len;
        t.disc_waits += sample.disc_waits as u64;
        (t.late_sectors, t.late_cdda) = late_sectors;
    });
}

/// Running totals since the last reset, as JSON.
#[wasm_bindgen(js_name = psoxideBenchStats)]
pub fn bench_stats() -> String {
    TOTALS.with(|t| {
        let t = t.borrow();
        format!(
            "{{\"redraws\":{},\"guest_frames\":{},\"total_ms\":{:.3},\"emu_ms\":{:.3},\
             \"audio_ms\":{:.3},\"hw_render_ms\":{:.3},\"hw_vram_clone_ms\":{:.3},\
             \"egui_ms\":{:.3},\"max_total_ms\":{:.3},\"long_redraws\":{},\
             \"audio_underrun_frames\":{},\"audio_queue_len\":{},\"disc_waits\":{},\
             \"late_sectors\":{},\"late_cdda\":{}}}",
            t.redraws,
            t.guest_frames,
            t.total_ms,
            t.emu_ms,
            t.audio_ms,
            t.hw_render_ms,
            t.hw_vram_clone_ms,
            t.egui_ms,
            t.max_total_ms,
            t.long_redraws,
            t.audio_underrun_frames,
            t.audio_queue_len,
            t.disc_waits,
            t.late_sectors,
            t.late_cdda
        )
    })
}

/// Zero the totals (the underrun counter is cumulative in the audio callback,
/// so a script diffs it instead).
#[wasm_bindgen(js_name = psoxideBenchReset)]
pub fn bench_reset() {
    TOTALS.with(|t| *t.borrow_mut() = Totals::default());
}

/// The `disc` URL parameter, when it names a same-origin relative path.
pub fn disc_param() -> Option<String> {
    let search = web_sys::window()?.location().search().ok()?;
    let value = search
        .trim_start_matches('?')
        .split('&')
        .find_map(|pair| pair.strip_prefix("disc="))?;
    let path = js_sys::decode_uri_component(value).ok()?.as_string()?;
    let relative = !path.is_empty()
        && !path.contains("//")
        && !path.contains(':')
        && !path.starts_with('/')
        && !path.contains('\\');
    relative.then_some(path)
}

/// Whether the page URL carries `name=1`.
pub fn flag(name: &str) -> bool {
    web_sys::window()
        .and_then(|w| w.location().search().ok())
        .is_some_and(|search| {
            search
                .trim_start_matches('?')
                .split('&')
                .any(|pair| pair == format!("{name}=1"))
        })
}
