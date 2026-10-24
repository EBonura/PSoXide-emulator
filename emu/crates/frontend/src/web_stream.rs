//! Same-origin streamed discs for the web build.
//!
//! A full disc image is far too large to bake into the wasm, and even one
//! straight download is a long sit: the pressing is mostly CD-DA. So the
//! deploy stages a split delivery next to the wasm (see the demo-disc repo's
//! `disc-tools web-delivery`): the data track gzipped, each audio track as
//! lossless FLAC, and a small text manifest naming the pieces with sizes and
//! FNV-1a-32 checksums. The boot gate is the data track alone; every song,
//! the first included, streams in behind the running emulator and is patched
//! into the mounted disc as it lands. A track whose bytes have not arrived
//! plays as digital silence -- the geometry is complete from the first frame,
//! so the drive model never sees anything a real disc could not do. A song
//! patched in late changes sector contents only, never the drive's state or
//! position, so the launcher's status-based stall watchdog has nothing to
//! react to.
//!
//! A disc whose audio is XA has no audio tracks to split off: the music is in
//! the data track, which is far too big to gate a boot on. Its manifest has a
//! single `image` line instead, and the disc is served as the raw image (in
//! numbered parts when a host caps file size) and read lazily over HTTP Range
//! requests (see `web_disc::open_lazy`); only a small sidecar of sector headers
//! is downloaded up front.
//!
//! A third delivery, the compact one, serves the same XA discs smaller. Its
//! manifest line names an index file (downloaded whole, a few tens of KB) and
//! a payload of deflated 32-sector chunks that psx-webdisc rebuilds into the
//! raw sectors; chunks are fetched with range requests as the drive reaches
//! for them and kept in IndexedDB for the next visit (see `web_disc`).
//!
//! Ordering note: pieces download in disc order and the launcher cycles its
//! menu tracks in the same order, so delivery stays ahead of the playhead on
//! any connection that can stream audio at all.
//!
//! FLAC decoding runs in Rust (claxon, see `web_decode`), time-budgeted per
//! frame from the shell's poll so a 30 MB track never stalls a redraw.
//! Fetches run in JS; bytes cross the boundary once per piece.
//!
//! The page can start the delivery before the wasm has even compiled:
//! `index.html` fetches the manifest, the data track and then the first song into
//! `window.psoxidePrefetch`, keyed by URL, and `wsFetch` adopts a prefetched
//! response instead of asking again. Both sides build the same URLs; a
//! mismatch only costs the head start.
//!
//! Every piece after the manifest is requested as `file?v=<fnv>` with the
//! checksum the manifest names. The CDN serves these files with a cache
//! lifetime of weeks and the manifest, cue and payloads expire on different
//! days, so a returning visitor could otherwise pair a cached manifest with
//! a refreshed data track. A new build changes the checksum and so the URL;
//! the old cache entry is simply never asked for.

use std::cell::RefCell;
use std::time::Duration;

use wasm_bindgen::prelude::*;

use crate::web_decode::{fnv1a32, Decode};

/// One disc served next to the wasm, streamed on demand.
pub struct StreamedDisc {
    /// Menu launch id; the `stream:` prefix is how launch routing spots it.
    /// Also the IndexedDB save-state key, so renaming it orphans saves.
    pub id: &'static str,
    /// Menu title.
    pub title: &'static str,
    /// Menu subtitle. Honest about what a click starts.
    pub subtitle: &'static str,
    /// Manifest URL, relative to the page. Everything else is named by it.
    pub manifest_url: &'static str,
    /// CUE sheet URL, relative to the page.
    pub cue_url: &'static str,
}

/// The streamed catalog. The deploy workflow stages the files from the
/// demo-disc repo's rolling `web-disc` release; a missing file surfaces as
/// an HTTP error in the status bar rather than a build failure.
pub static DISCS: &[StreamedDisc] = &[StreamedDisc {
    id: crate::app::DEMO_DISC_ID,
    title: "PSoXide Demo Disc",
    subtitle: "homebrew - music streams in while you play",
    manifest_url: "web-manifest.txt",
    cue_url: "demo-disc.cue",
}];

/// Look up a streamed disc by its menu launch id.
pub fn find(id: &str) -> Option<&'static StreamedDisc> {
    DISCS.iter().find(|d| d.id == id)
}

// ---- JS: one XHR slot per piece, polled from Rust ---------------------------
//
// XHR rather than fetch-with-reader: the emulator's rAF loop starves a stream
// reader's microtask ping-pong to one chunk per frame, while XHR assembles
// natively and still reports byte progress. Gunzip uses the browser's own
// DecompressionStream. Slots hold their payload until Rust takes it.
#[wasm_bindgen(inline_js = r#"
let _slots = {};
export function wsFetch(id, url) {
  if (_slots[id]) return;
  // The page may already have this response in flight or landed (index.html
  // starts the delivery while the wasm compiles). Adopt it, once.
  const pre = window.psoxidePrefetch;
  if (pre && pre[url]) {
    _slots[id] = pre[url];
    delete pre[url];
    return;
  }
  const s = { state: 'running', received: 0, total: 0 };
  _slots[id] = s;
  const xhr = new XMLHttpRequest();
  xhr.open('GET', url);
  xhr.responseType = 'arraybuffer';
  xhr.onprogress = (e) => { s.received = e.loaded; if (e.lengthComputable) s.total = e.total; };
  xhr.onload = () => {
    if (xhr.status >= 200 && xhr.status < 300) { s.buf = new Uint8Array(xhr.response); s.state = 'done'; }
    else { s.msg = url + ': HTTP ' + xhr.status; s.state = 'error'; }
  };
  xhr.onerror = () => { s.msg = url + ': network error'; s.state = 'error'; };
  xhr.send();
}
export function wsGunzip(id) {
  const s = _slots[id];
  if (!s || s.state !== 'done' || s.unzipping) return;
  // Some CDNs (itch.io's, for one) serve a .gz file with Content-Encoding:
  // gzip, so the browser has already inflated it by the time XHR hands it
  // over. The magic bytes say which case this is; the FNV check downstream
  // vouches for the payload either way.
  if (!(s.buf.length > 2 && s.buf[0] === 0x1f && s.buf[1] === 0x8b)) return;
  s.unzipping = true;
  s.state = 'running';
  (async () => {
    try {
      const out = await new Response(
        new Blob([s.buf]).stream().pipeThrough(new DecompressionStream('gzip'))
      ).arrayBuffer();
      s.buf = new Uint8Array(out);
      s.state = 'done';
    } catch (e) { s.msg = 'gunzip: ' + String(e); s.state = 'error'; }
  })();
}
export function wsState(id) { const s = _slots[id]; return s ? s.state : 'idle'; }
export function wsReceived(id) { const s = _slots[id]; return s ? s.received : 0; }
export function wsTotal(id) { const s = _slots[id]; return s ? s.total : 0; }
export function wsTake(id) { const s = _slots[id]; const b = s.buf; delete _slots[id]; return b; }
export function wsError(id) { const s = _slots[id]; const m = s ? s.msg : 'no slot'; delete _slots[id]; return m || 'fetch failed'; }
"#)]
extern "C" {
    #[wasm_bindgen(js_name = wsFetch)]
    fn ws_fetch(id: &str, url: &str);
    #[wasm_bindgen(js_name = wsGunzip)]
    fn ws_gunzip(id: &str);
    #[wasm_bindgen(js_name = wsState)]
    fn ws_state(id: &str) -> String;
    #[wasm_bindgen(js_name = wsReceived)]
    fn ws_received(id: &str) -> f64;
    #[wasm_bindgen(js_name = wsTotal)]
    fn ws_total(id: &str) -> f64;
    #[wasm_bindgen(js_name = wsTake)]
    fn ws_take(id: &str) -> Vec<u8>;
    #[wasm_bindgen(js_name = wsError)]
    fn ws_error(id: &str) -> String;
}

/// One audio piece from the manifest.
struct TrackMeta {
    number: u8,
    title: String,
    file: String,
    raw_bytes: usize,
    fnv: u32,
}

/// A whole raw image delivered for lazy HTTP Range reads, from the manifest.
struct ImageMeta {
    file: String,
    bytes: u64,
    /// `emulator_core::game_image_hash` of the image, computed at press time.
    hash: u64,
    /// Gzipped sidecar: bytes 12..20 of every sector.
    hdr_file: String,
    hdr_raw_bytes: usize,
    hdr_fnv: u32,
    /// Sectors the first frames of a boot read, fetched in one request.
    boot_sectors: u64,
    /// Bytes per file when the image is split to fit a host's file size cap
    /// (itch.io refuses 200 MB); 0 when `file` holds the whole image.
    part_bytes: u64,
}

impl ImageMeta {
    /// The URL of every file the image is served as, in order, each tied to
    /// the image hash. A split image is `file.001`, `file.002`, ... of
    /// `part_bytes` each (the last one shorter).
    fn urls(&self) -> Vec<String> {
        if self.part_bytes == 0 {
            return vec![piece_url(&self.file, self.hash)];
        }
        let parts = self.bytes.div_ceil(self.part_bytes);
        (1..=parts)
            .map(|n| piece_url(&format!("{}.{n:03}", self.file), self.hash))
            .collect()
    }
}

/// A compact disc delivery, from the manifest.
struct CompactMeta {
    /// The index file, and its size and FNV-1a-32 checksum.
    index_file: String,
    index_bytes: usize,
    index_fnv: u32,
    /// The payload file, or the stem of `.001`, `.002`... when split.
    payload_file: String,
    /// Bytes per payload file when the payload is split, else 0.
    part_bytes: u64,
}

/// Where a background track stands, for the shell's progress line.
#[derive(Clone, PartialEq, Eq)]
pub enum TrackState {
    /// Not started.
    Pending,
    /// Bytes on the wire, percent of the FLAC fetched.
    Fetching(u8),
    /// Fetched, decoding back into sectors.
    Decoding,
    /// Decoded, verified, patched into the disc.
    Ready,
    /// Fetch or checksum failure; the track stays silent.
    Failed,
}

/// A decode in progress, and which track it belongs to.
struct ActiveDecode {
    index: usize,
    decode: Decode,
}

/// Decode time per frame while nothing is running yet. The page is waiting
/// on the disc anyway, so this is as fast as the host allows short of
/// starving the redraw that shows the progress line.
const BOOT_DECODE_BUDGET: Duration = Duration::from_millis(8);

/// Decode time per frame beside a running guest. The guest emulates a
/// PlayStation on the same thread, and a frame that runs long underruns the
/// host's CD audio. A few milliseconds is small beside a 16.7 ms frame, and
/// still turns a 20 MB track around in a couple of seconds where the old
/// three blocks per frame needed over ten.
const BACKGROUND_DECODE_BUDGET: Duration = Duration::from_millis(3);

enum Phase {
    /// Fetching manifest, cue and data piece.
    Boot,
    /// The shell took the boot payload; remaining tracks stream in.
    Background,
    /// Every track is home (or given up on).
    Done,
    /// The boot payload could not be assembled.
    Failed,
}

#[derive(Default)]
struct Pieces {
    cue: Option<String>,
    data: Option<Vec<u8>>,
}

struct Stream {
    disc: &'static StreamedDisc,
    phase: Phase,
    tracks: Vec<TrackMeta>,
    states: Vec<TrackState>,
    /// The data piece's file name, from the manifest.
    data_file: String,
    image: Option<ImageMeta>,
    compact: Option<CompactMeta>,
    /// The manifest has been read and parsed.
    manifest_parsed: bool,
    data_raw_bytes: usize,
    data_fnv: u32,
    gunzip_requested: bool,
    pieces: Pieces,
    decode: Option<ActiveDecode>,
    /// Decoded-and-verified tracks waiting for the shell to patch in.
    ready: Vec<(u8, Vec<u8>)>,
    error: String,
}

thread_local! {
    static STREAM: RefCell<Option<Stream>> = const { RefCell::new(None) };
}

/// What the shell hears each frame while the boot payload assembles.
pub enum BootStatus {
    /// Nothing in flight.
    Idle,
    /// Human-readable progress for the status line.
    Progress(String),
    /// Everything the boot needs, handed over exactly once.
    Ready {
        /// The disc being streamed.
        disc: &'static StreamedDisc,
        /// CUE sheet text.
        cue: String,
        /// Track 1's file extent, decompressed.
        data: Vec<u8>,
        /// `(number, raw_bytes)` for every audio track, for placeholders.
        /// Every one starts silent; the songs are patched in as they land.
        layout: Vec<(u8, usize)>,
    },
    /// A whole image to read on demand, handed over exactly once. Nothing
    /// else is downloaded up front: the shell opens it and boots as soon as
    /// the sectors the boot reads have arrived.
    Lazy {
        /// The disc being streamed.
        disc: &'static StreamedDisc,
        /// What to open, and where it is served.
        image: crate::web_disc::LazyImage,
    },
    /// A compact disc to read on demand, handed over exactly once, like
    /// [`BootStatus::Lazy`].
    Compact {
        /// The disc being streamed.
        disc: &'static StreamedDisc,
        /// What to open, and where it is served.
        source: crate::web_disc::CompactSource,
    },
    /// The stream is dead; message handed over exactly once.
    Failed(String),
    /// The manifest is not served (or not a manifest): this page has no
    /// delivery staged, as on a dev build. Handed over exactly once.
    NotServed(String),
}

/// A background delivery event for the shell to apply.
pub enum BgEvent {
    /// A track's exact sectors, verified, ready to patch into the disc.
    TrackReady(u8, Vec<u8>),
}

/// Kick off a stream. Idempotent while one is running: double-clicking the
/// menu entry cannot fork the pipeline. A failed stream can be retried.
pub fn start(disc: &'static StreamedDisc) {
    STREAM.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot
            .as_ref()
            .is_some_and(|s| !matches!(s.phase, Phase::Failed))
        {
            return;
        }
        ws_fetch("manifest", disc.manifest_url);
        *slot = Some(Stream {
            disc,
            phase: Phase::Boot,
            tracks: Vec::new(),
            states: Vec::new(),
            data_file: String::new(),
            image: None,
            compact: None,
            manifest_parsed: false,
            data_raw_bytes: 0,
            data_fnv: 0,
            gunzip_requested: false,
            pieces: Pieces::default(),
            decode: None,
            ready: Vec::new(),
            error: String::new(),
        });
    });
}

fn parse_manifest(text: &str, s: &mut Stream) -> Result<(), String> {
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("data") => {
                s.data_file = parts
                    .next()
                    .ok_or("manifest: data file missing")?
                    .to_string();
                let _gz = parts
                    .next()
                    .and_then(|v| v.parse::<u64>().ok())
                    .ok_or("manifest: gz bytes")?;
                s.data_raw_bytes = parts
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or("manifest: raw bytes")?;
                s.data_fnv = parts
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or("manifest: data fnv")?;
            }
            Some("image") => {
                // `image FILE BYTES GAME_IMAGE_HASH HDR_FILE HDR_RAW_BYTES
                // HDR_FNV32 BOOT_SECTORS [PART_BYTES]`: a whole-image delivery
                // read on demand, in place of the data and track pieces. With
                // PART_BYTES the image is the files FILE.001, FILE.002, ...
                let mut next = || parts.next().ok_or("manifest: short image line");
                let file = next()?.to_string();
                let bytes = next()?.parse().map_err(|_| "manifest: image bytes")?;
                let hash = next()?.parse().map_err(|_| "manifest: image hash")?;
                let hdr_file = next()?.to_string();
                let hdr_raw_bytes = next()?.parse().map_err(|_| "manifest: header bytes")?;
                let hdr_fnv = next()?.parse().map_err(|_| "manifest: header fnv")?;
                let boot_sectors = next()?.parse().map_err(|_| "manifest: boot sectors")?;
                let part_bytes: u64 = match parts.next() {
                    Some(v) => v.parse().map_err(|_| "manifest: part bytes")?,
                    None => 0,
                };
                // A part is whole sectors, or one sector would straddle two
                // files.
                if !part_bytes.is_multiple_of(psx_iso::SECTOR_BYTES as u64) {
                    return Err("manifest: part bytes are not whole sectors".into());
                }
                s.image = Some(ImageMeta {
                    file,
                    bytes,
                    hash,
                    hdr_file,
                    hdr_raw_bytes,
                    hdr_fnv,
                    boot_sectors,
                    part_bytes,
                });
            }
            Some("compact") => {
                // `compact INDEX_FILE INDEX_BYTES INDEX_FNV32 PAYLOAD_FILE
                // BOOT_BYTES PART_BYTES`: see psx-webdisc.
                let mut next = || parts.next().ok_or("manifest: short compact line");
                let index_file = next()?.to_string();
                let index_bytes = next()?.parse().map_err(|_| "manifest: index bytes")?;
                let index_fnv = next()?.parse().map_err(|_| "manifest: index fnv")?;
                let payload_file = next()?.to_string();
                // Payload bytes the boot reads first: only index.html uses them.
                let _boot_bytes: u64 = next()?.parse().map_err(|_| "manifest: boot bytes")?;
                let part_bytes = next()?.parse().map_err(|_| "manifest: part bytes")?;
                s.compact = Some(CompactMeta {
                    index_file,
                    index_bytes,
                    index_fnv,
                    payload_file,
                    part_bytes,
                });
            }
            Some("track") => {
                let number: u8 = parts
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or("manifest: track number")?;
                let file = parts.next().ok_or("manifest: track file")?.to_string();
                let _flac = parts
                    .next()
                    .and_then(|v| v.parse::<u64>().ok())
                    .ok_or("manifest: flac bytes")?;
                let raw_bytes: usize = parts
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or("manifest: track raw")?;
                let fnv: u32 = parts
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or("manifest: track fnv")?;
                let title = parts.collect::<Vec<_>>().join(" ");
                s.tracks.push(TrackMeta {
                    number,
                    title,
                    file,
                    raw_bytes,
                    fnv,
                });
                s.states.push(TrackState::Pending);
            }
            _ => {}
        }
    }
    // A lazily read image needs no data or audio pieces; a split delivery
    // needs both.
    if s.image.is_none() && s.compact.is_none() && (s.data_raw_bytes == 0 || s.tracks.is_empty()) {
        return Err("manifest: empty or unreadable".into());
    }
    Ok(())
}

/// URL of a piece named by the manifest, tied to its checksum so a cached
/// copy from another build is never mistaken for this one. `index.html`
/// builds the same strings for its early fetches.
fn piece_url(file: &str, fnv: impl std::fmt::Display) -> String {
    format!("{file}?v={fnv}")
}

/// Begin fetching track `index` if it exists and has not started.
fn start_fetch(s: &mut Stream, index: usize) {
    if index < s.tracks.len() && s.states[index] == TrackState::Pending {
        let url = piece_url(&s.tracks[index].file, s.tracks[index].fnv);
        ws_fetch(&format!("track{index}"), &url);
        s.states[index] = TrackState::Fetching(0);
    }
}

/// Move a fetched track into the budgeted decoder; called when the decoder
/// is free.
fn start_decode(s: &mut Stream, index: usize) {
    let slot = format!("track{index}");
    if ws_state(&slot) != "done" {
        return;
    }
    let flac = ws_take(&slot);
    match Decode::new(flac, s.tracks[index].raw_bytes) {
        Ok(decode) => {
            s.decode = Some(ActiveDecode { index, decode });
            s.states[index] = TrackState::Decoding;
        }
        Err(e) => {
            s.states[index] = TrackState::Failed;
            s.error = format!("{}: FLAC header: {e}", s.tracks[index].title);
        }
    }
}

/// Run the decoder for one time slice. On completion, verify and queue the
/// track for patching.
fn pump_decode(s: &mut Stream, budget: Duration) {
    let Some(active) = s.decode.as_mut() else {
        return;
    };
    let Some(decoded) = active.decode.pump(budget) else {
        return;
    };
    let index = active.index;
    s.decode = None;
    let meta = &s.tracks[index];
    // One console line per song, for timing the decode on a real browser.
    web_sys::console::log_1(
        &format!(
            "[web] track {}: decoded in {} ms of work over {} ms",
            meta.number,
            decoded.busy.as_millis(),
            decoded.wall.as_millis()
        )
        .into(),
    );
    if decoded.pcm.len() == meta.raw_bytes && decoded.fnv == meta.fnv {
        s.states[index] = TrackState::Ready;
        s.ready.push((meta.number, decoded.pcm));
    } else {
        s.states[index] = TrackState::Failed;
        s.error = format!("{}: decode mismatch, leaving it silent", meta.title);
    }
}

/// Drop a stream that is still assembling, so it never boots. Fetches in
/// flight land in their slots and are reused if the disc is started again.
pub fn abandon() {
    STREAM.with(|cell| *cell.borrow_mut() = None);
}

fn fail(s: &mut Stream, message: String) -> BootStatus {
    s.phase = Phase::Failed;
    BootStatus::Failed(message)
}

/// Per-frame poll while the boot payload assembles. `Ready` and `Failed`
/// hand over exactly once; afterwards use [`poll_background`].
pub fn poll_boot() -> BootStatus {
    STREAM.with(|cell| {
        let mut slot = cell.borrow_mut();
        let Some(s) = slot.as_mut() else {
            return BootStatus::Idle;
        };
        if !matches!(s.phase, Phase::Boot) {
            return BootStatus::Idle;
        }

        // A song that lands while the boot payload is still assembling
        // decodes at the fast, nothing-is-running rate.
        advance_tracks(s, BOOT_DECODE_BUDGET);

        // The manifest, tiny. It names everything else, so the data piece
        // and the cue start the moment it parses.
        if !s.manifest_parsed {
            match ws_state("manifest").as_str() {
                "done" => {
                    let text = String::from_utf8_lossy(&ws_take("manifest")).into_owned();
                    if let Err(e) = parse_manifest(&text, s) {
                        s.phase = Phase::Failed;
                        return BootStatus::NotServed(e);
                    }
                    s.manifest_parsed = true;
                    if let Some(compact) = &s.compact {
                        ws_fetch("idx", &piece_url(&compact.index_file, compact.index_fnv));
                    } else if let Some(image) = &s.image {
                        ws_fetch("hdr", &piece_url(&image.hdr_file, image.hdr_fnv));
                    } else {
                        ws_fetch("data", &piece_url(&s.data_file, s.data_fnv));
                        ws_fetch("cue", &piece_url(s.disc.cue_url, s.data_fnv));
                    }
                }
                "error" => {
                    s.phase = Phase::Failed;
                    return BootStatus::NotServed(ws_error("manifest"));
                }
                _ => return BootStatus::Progress(format!("{}: reading manifest...", s.disc.title)),
            }
        }

        if s.compact.is_some() {
            return poll_compact(s);
        }
        if s.image.is_some() {
            return poll_lazy(s);
        }

        // The data piece: fetch, then browser-native gunzip, then verify.
        if s.pieces.data.is_none() {
            match ws_state("data").as_str() {
                "done" if !s.gunzip_requested => {
                    s.gunzip_requested = true;
                    // The wire is free again: the first song starts coming
                    // down while the data track unpacks. It no longer gates
                    // the boot, but it is the next thing the launcher plays.
                    start_fetch(s, 0);
                    ws_gunzip("data");
                    return BootStatus::Progress(format!(
                        "{}: unpacking data track...",
                        s.disc.title
                    ));
                }
                "done" => {
                    let raw = ws_take("data");
                    if raw.len() != s.data_raw_bytes || fnv1a32(&raw) != s.data_fnv {
                        return fail(
                            s,
                            "data track: checksum mismatch (try reloading the page)".into(),
                        );
                    }
                    s.pieces.data = Some(raw);
                }
                "error" => return fail(s, ws_error("data")),
                _ => {
                    let mb = ws_received("data") / (1024.0 * 1024.0);
                    return BootStatus::Progress(format!(
                        "{}: data track... {mb:.1} MB",
                        s.disc.title
                    ));
                }
            }
        }
        if s.pieces.cue.is_none() {
            match ws_state("cue").as_str() {
                "done" => {
                    s.pieces.cue = Some(String::from_utf8_lossy(&ws_take("cue")).into_owned())
                }
                "error" => return fail(s, ws_error("cue")),
                _ => return BootStatus::Progress(format!("{}: reading cue...", s.disc.title)),
            }
        }

        // The data track is the whole boot gate. The songs follow.
        s.phase = Phase::Background;
        start_fetch(s, 0);
        BootStatus::Ready {
            disc: s.disc,
            cue: s.pieces.cue.take().expect("cue fetched"),
            data: s.pieces.data.take().expect("data fetched"),
            layout: s.tracks.iter().map(|t| (t.number, t.raw_bytes)).collect(),
        }
    })
}

/// The compact delivery's only up-front piece: the index. Fetch, verify,
/// parse, and hand the disc over.
fn poll_compact(s: &mut Stream) -> BootStatus {
    let meta = s.compact.as_ref().expect("compact delivery");
    match ws_state("idx").as_str() {
        "done" => {
            let bytes = ws_take("idx");
            if bytes.len() != meta.index_bytes || fnv1a32(&bytes) != meta.index_fnv {
                return fail(
                    s,
                    "disc index: checksum mismatch (try reloading the page)".into(),
                );
            }
            let index = match psx_webdisc::Index::parse(&bytes) {
                Ok(index) => index,
                Err(e) => return fail(s, format!("disc index: {e}")),
            };
            let urls = if meta.part_bytes == 0 {
                vec![piece_url(&meta.payload_file, meta.index_fnv)]
            } else {
                (1..=index.payload_bytes().div_ceil(meta.part_bytes))
                    .map(|n| {
                        piece_url(
                            &psx_webdisc::part_name(&meta.payload_file, n),
                            meta.index_fnv,
                        )
                    })
                    .collect()
            };
            let cache_key = format!("{:016x}-{:08x}", index.image_hash(), meta.index_fnv);
            let handoff = BootStatus::Compact {
                disc: s.disc,
                source: crate::web_disc::CompactSource {
                    index,
                    urls,
                    cache_key,
                },
            };
            s.phase = Phase::Done;
            handoff
        }
        "error" => fail(s, ws_error("idx")),
        _ => BootStatus::Progress(format!("{}: reading disc index...", s.disc.title)),
    }
}

/// The lazy delivery's only up-front piece: the header sidecar. Fetch,
/// gunzip when the CDN did not, verify, and hand the image over.
fn poll_lazy(s: &mut Stream) -> BootStatus {
    let image = s.image.as_ref().expect("lazy delivery");
    match ws_state("hdr").as_str() {
        "done" if !s.gunzip_requested => {
            s.gunzip_requested = true;
            ws_gunzip("hdr");
            BootStatus::Progress(format!("{}: indexing disc...", s.disc.title))
        }
        "done" => {
            let headers = ws_take("hdr");
            if headers.len() != image.hdr_raw_bytes || fnv1a32(&headers) != image.hdr_fnv {
                return fail(
                    s,
                    "disc index: checksum mismatch (try reloading the page)".into(),
                );
            }
            let handoff = BootStatus::Lazy {
                disc: s.disc,
                image: crate::web_disc::LazyImage {
                    urls: image.urls(),
                    part_bytes: image.part_bytes,
                    len: image.bytes,
                    hash: image.hash,
                    headers,
                    boot_sectors: image.boot_sectors,
                },
            };
            s.phase = Phase::Done;
            handoff
        }
        "error" => fail(s, ws_error("hdr")),
        _ => BootStatus::Progress(format!("{}: reading disc index...", s.disc.title)),
    }
}

/// Hand finished fetches to the decoder, keep the wire busy, and run the
/// decoder for one time slice. The next fetch starts the moment the current
/// one lands.
fn advance_tracks(s: &mut Stream, budget: Duration) {
    for index in 0..s.tracks.len() {
        if let TrackState::Fetching(_) = s.states[index] {
            let id = format!("track{index}");
            match ws_state(&id).as_str() {
                "done" => {
                    if s.decode.is_none() {
                        start_decode(s, index);
                    }
                    start_fetch(s, index + 1);
                }
                "error" => {
                    let _ = ws_error(&id);
                    s.states[index] = TrackState::Failed;
                    // One lost song must not strand the ones behind it.
                    start_fetch(s, index + 1);
                }
                _ => {
                    let pct = if ws_total(&id) > 0.0 {
                        (ws_received(&id) / ws_total(&id) * 100.0) as u8
                    } else {
                        0
                    };
                    s.states[index] = TrackState::Fetching(pct);
                }
            }
        }
    }
    pump_decode(s, budget);
    // A finished decode frees the decoder for the next fetched track.
    if s.decode.is_none() {
        for index in 0..s.tracks.len() {
            if matches!(s.states[index], TrackState::Fetching(_))
                && ws_state(&format!("track{index}")) == "done"
            {
                start_decode(s, index);
                break;
            }
        }
    }
}

/// Per-frame poll after boot: advances fetches and decodes under budget and
/// returns tracks ready to be patched into the mounted disc.
pub fn poll_background() -> Vec<BgEvent> {
    STREAM.with(|cell| {
        let mut slot = cell.borrow_mut();
        let Some(s) = slot.as_mut() else {
            return Vec::new();
        };
        if !matches!(s.phase, Phase::Background) {
            return Vec::new();
        }

        advance_tracks(s, BACKGROUND_DECODE_BUDGET);

        let events: Vec<BgEvent> = s
            .ready
            .drain(..)
            .map(|(number, pcm)| BgEvent::TrackReady(number, pcm))
            .collect();

        if s.decode.is_none()
            && s.ready.is_empty()
            && s.states
                .iter()
                .all(|t| matches!(t, TrackState::Ready | TrackState::Failed))
        {
            s.phase = Phase::Done;
        }
        events
    })
}

/// One line for the status bar while tracks are still arriving, `None` once
/// the whole disc is home (or before a stream starts).
pub fn progress_line() -> Option<String> {
    STREAM.with(|cell| {
        let slot = cell.borrow();
        let s = slot.as_ref()?;
        if !matches!(s.phase, Phase::Background) {
            return None;
        }
        let ready = s
            .states
            .iter()
            .filter(|t| matches!(t, TrackState::Ready))
            .count();
        let active = s.states.iter().enumerate().find_map(|(i, t)| match t {
            TrackState::Fetching(pct) => Some(format!("{} {pct}%", s.tracks[i].title)),
            TrackState::Decoding => Some(format!("{} decoding", s.tracks[i].title)),
            _ => None,
        });
        match active {
            Some(what) => Some(format!(
                "Music streaming in: {what} ({ready}/{} ready)",
                s.tracks.len()
            )),
            None => Some(format!(
                "Music streaming in: {ready}/{} ready",
                s.tracks.len()
            )),
        }
    })
}
