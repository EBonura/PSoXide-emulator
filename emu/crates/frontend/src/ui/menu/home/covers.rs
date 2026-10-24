//! Box art for the visitor's own discs.
//!
//! The picture is looked up by the disc's boot serial in the xlenore/psx-covers
//! collection, then by the disc's file name in libretro-thumbnails, and a
//! generated case stands in when neither has it. Pictures are fetched at run
//! time only, kept in a cache (IndexedDB on the web, a folder on the desktop),
//! and never shipped with the program. A miss is remembered for a week so a
//! disc without art does not ask every visit; being offline is not a miss.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use egui::{ColorImage, TextureHandle, TextureId, TextureOptions};

/// Seconds a remembered miss is believed.
pub(crate) const MISS_TTL_SECS: u64 = 7 * 24 * 3600;
/// Pictures decoded per frame, so a big library cannot stall a frame.
const DECODES_PER_FRAME: usize = 1;
/// Edge length the pictures are cut down to in memory.
const MAX_EDGE: u32 = 256;

/// Where a picture can come from, tried in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    /// Cache key: stable for the same picture.
    pub key: String,
    pub url: String,
}

/// What the network layer reports back.
#[derive(Debug)]
pub(crate) struct Fetched {
    pub id: String,
    /// The picture bytes, when a candidate had one.
    pub bytes: Option<Vec<u8>>,
    /// The candidate key that produced them.
    pub source: String,
    /// Every candidate answered "not there" (as opposed to a network failure).
    pub missing: bool,
}

/// libretro writes names with `&*/:`<>?\|` replaced by `_`.
pub(crate) fn libretro_name(stem: &str) -> String {
    stem.chars()
        .map(|c| if "&*/:`<>?\\|\"".contains(c) { '_' } else { c })
        .collect()
}

fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The URLs to try for a disc.
pub(crate) fn candidates(serial: Option<&str>, stem: &str) -> Vec<Candidate> {
    let mut out = Vec::new();
    if let Some(serial) = serial {
        out.push(Candidate {
            key: format!("x:{serial}"),
            url: format!(
                "https://raw.githubusercontent.com/xlenore/psx-covers/main/covers/default/{serial}.jpg"
            ),
        });
    }
    let name = libretro_name(stem.trim());
    if !name.is_empty() {
        out.push(Candidate {
            key: format!("l:{name}"),
            url: format!(
                "https://raw.githubusercontent.com/libretro-thumbnails/Sony_-_PlayStation/master/Named_Boxarts/{}.png",
                encode(&name)
            ),
        });
    }
    out
}

/// What the state knows about one disc.
#[derive(Clone, Debug, Default)]
struct Source {
    stem: String,
    path: Option<PathBuf>,
    serial: Option<String>,
    serial_read: bool,
}

/// Cover textures and the requests behind them.
pub(crate) struct CoverState {
    enabled: bool,
    sources: HashMap<String, Source>,
    requested: HashSet<String>,
    ready: Vec<(String, Vec<u8>)>,
    textures: HashMap<String, TextureHandle>,
    failed: HashSet<String>,
    /// Which sources supplied pictures (for the credits and the probe).
    credits: HashSet<&'static str>,
}

impl Default for CoverState {
    fn default() -> Self {
        Self {
            enabled: true,
            sources: HashMap::new(),
            requested: HashSet::new(),
            ready: Vec::new(),
            textures: HashMap::new(),
            failed: HashSet::new(),
            credits: HashSet::new(),
        }
    }
}

impl CoverState {
    /// Switch fetching on or off (the Settings toggle).
    pub(crate) fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
    }

    /// Tell the state what a launch id is: its file stem and, on the desktop,
    /// the file to read the serial from.
    pub(crate) fn describe(&mut self, id: &str, stem: &str, path: Option<PathBuf>) {
        let entry = self.sources.entry(id.to_string()).or_default();
        entry.stem = stem.to_string();
        entry.path = path;
    }

    /// A disc that was booted told its serial.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(crate) fn set_serial(&mut self, id: &str, serial: Option<String>) {
        let entry = self.sources.entry(id.to_string()).or_default();
        if serial.is_some() && entry.serial != serial {
            // A serial is a better key than the file name: look again.
            self.failed.remove(id);
            self.requested.remove(id);
        }
        entry.serial = serial.or_else(|| entry.serial.take());
        entry.serial_read = true;
    }

    /// A picture delivered by the network layer.
    pub(crate) fn deliver(&mut self, fetched: Fetched) {
        match fetched.bytes {
            Some(bytes) => {
                self.credits.insert(if fetched.source.starts_with("x:") {
                    "xlenore/psx-covers"
                } else {
                    "libretro-thumbnails"
                });
                self.ready.push((fetched.id, bytes));
            }
            None => {
                if fetched.missing {
                    self.failed.insert(fetched.id.clone());
                }
                // An offline answer clears the request so a later look retries.
                self.requested.remove(&fetched.id);
            }
        }
    }

    /// Requests that should be started now: (id, candidates).
    fn want(&mut self, id: &str) -> Option<Vec<Candidate>> {
        if !self.enabled || self.requested.contains(id) || self.failed.contains(id) {
            return None;
        }
        let source = self.sources.get_mut(id)?;
        #[cfg(not(target_arch = "wasm32"))]
        if !source.serial_read {
            source.serial_read = true;
            if let Some(path) = source.path.as_ref() {
                source.serial = psoxide_settings::library::disc_boot_serial(path);
            }
        }
        let list = candidates(source.serial.as_deref(), &source.stem);
        self.requested.insert(id.to_string());
        (!list.is_empty()).then_some(list)
    }

    /// The picture of a launch id, if it is on hand; starts a fetch when not.
    pub(crate) fn texture(&mut self, ctx: &egui::Context, id: &str) -> Option<TextureId> {
        if let Some(tex) = self.textures.get(id) {
            return Some(tex.id());
        }
        if !self.enabled {
            return None;
        }
        // Decode what has arrived, a picture or two per frame.
        let mut decoded = 0;
        while decoded < DECODES_PER_FRAME {
            let Some(at) = self.ready.iter().position(|(rid, _)| rid == id) else {
                break;
            };
            let (rid, bytes) = self.ready.swap_remove(at);
            decoded += 1;
            match decode(&bytes) {
                Some(image) => {
                    let tex = ctx.load_texture(format!("cover-{rid}"), image, TextureOptions::LINEAR);
                    self.textures.insert(rid, tex);
                }
                None => {
                    self.failed.insert(rid);
                }
            }
        }
        if let Some(tex) = self.textures.get(id) {
            return Some(tex.id());
        }
        if let Some(list) = self.want(id) {
            io::request(id, &list);
        }
        None
    }

    /// Hand the network layer's finished work to the state. Called each frame.
    pub(crate) fn pump(&mut self) {
        for fetched in io::poll() {
            self.deliver(fetched);
        }
    }

    /// The collections that supplied a picture this session, for the credits.
    #[allow(dead_code)]
    pub(crate) fn credits(&self) -> Vec<&'static str> {
        let mut v: Vec<_> = self.credits.iter().copied().collect();
        v.sort_unstable();
        v
    }

    /// Pictures in memory, for the probe.
    #[allow(dead_code)]
    pub(crate) fn loaded(&self) -> usize {
        self.textures.len()
    }
}

/// Decode a PNG or JPEG and cut it down to [`MAX_EDGE`].
fn decode(bytes: &[u8]) -> Option<ColorImage> {
    let img = image::load_from_memory(bytes).ok()?;
    let img = if img.width().max(img.height()) > MAX_EDGE {
        img.thumbnail(MAX_EDGE, MAX_EDGE)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some(ColorImage::from_rgba_unmultiplied(
        [w as usize, h as usize],
        rgba.as_raw(),
    ))
}

#[cfg(not(target_arch = "wasm32"))]
mod io {
    //! Desktop fetch: `curl` in a thread, a folder of files as the cache.

    use std::path::PathBuf;
    use std::sync::{mpsc, Mutex, OnceLock};

    use super::{Candidate, Fetched, MISS_TTL_SECS};

    static DONE: OnceLock<(mpsc::Sender<Fetched>, Mutex<mpsc::Receiver<Fetched>>)> =
        OnceLock::new();

    fn channel() -> &'static (mpsc::Sender<Fetched>, Mutex<mpsc::Receiver<Fetched>>) {
        DONE.get_or_init(|| {
            let (tx, rx) = mpsc::channel();
            (tx, Mutex::new(rx))
        })
    }

    /// The cache folder: next to the other per-user data.
    fn cache_dir() -> PathBuf {
        let base = std::env::var_os("PSOXIDE_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("XDG_CACHE_HOME")
                    .map(PathBuf::from)
                    .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
            })
            .unwrap_or_else(std::env::temp_dir);
        base.join("psoxide").join("covers")
    }

    fn file_for(key: &str) -> String {
        key.chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
            .collect()
    }

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    pub(super) fn request(id: &str, list: &[Candidate]) {
        let id = id.to_string();
        let list = list.to_vec();
        let tx = channel().0.clone();
        let spawned = std::thread::Builder::new()
            .name("covers".into())
            .spawn(move || {
                let _ = tx.send(run(&id, &list));
            });
        if let Err(e) = spawned {
            eprintln!("[frontend] cover thread: {e}");
        }
    }

    fn run(id: &str, list: &[Candidate]) -> Fetched {
        let dir = cache_dir();
        let _ = std::fs::create_dir_all(&dir);
        let mut offline = false;
        for cand in list {
            let hit = dir.join(format!("{}.img", file_for(&cand.key)));
            let miss = dir.join(format!("{}.miss", file_for(&cand.key)));
            if let Ok(bytes) = std::fs::read(&hit) {
                return Fetched { id: id.into(), bytes: Some(bytes), source: cand.key.clone(), missing: false };
            }
            if let Ok(meta) = std::fs::metadata(&miss) {
                let age = meta
                    .modified()
                    .ok()
                    .and_then(|m| m.elapsed().ok())
                    .map_or(u64::MAX, |d| d.as_secs());
                if age < MISS_TTL_SECS {
                    continue;
                }
            }
            let tmp = dir.join(format!("{}.part{}", file_for(&cand.key), now()));
            let status = std::process::Command::new("curl")
                .args(["-sfL", "--max-time", "25", "-o"])
                .arg(&tmp)
                .arg(&cand.url)
                .status();
            match status {
                Ok(s) if s.success() => {
                    if let Ok(bytes) = std::fs::read(&tmp) {
                        let _ = std::fs::rename(&tmp, &hit);
                        return Fetched { id: id.into(), bytes: Some(bytes), source: cand.key.clone(), missing: false };
                    }
                }
                // curl exit 22: the server answered with an HTTP error, which
                // for these collections means "no picture for this disc".
                Ok(s) if s.code() == Some(22) => {
                    let _ = std::fs::write(&miss, b"");
                }
                _ => offline = true,
            }
            let _ = std::fs::remove_file(&tmp);
        }
        Fetched { id: id.into(), bytes: None, source: String::new(), missing: !offline }
    }

    pub(super) fn poll() -> Vec<Fetched> {
        let rx = channel().1.lock().expect("cover channel");
        rx.try_iter().collect()
    }
}

#[cfg(target_arch = "wasm32")]
mod io {
    //! Browser fetch: `fetch` with an IndexedDB cache, in one small script.

    use wasm_bindgen::prelude::wasm_bindgen;

    use super::{Candidate, Fetched, MISS_TTL_SECS};

    #[wasm_bindgen(inline_js = r#"
let _db = null;
const _done = [];
function _open() {
  if (_db) return _db;
  _db = new Promise((resolve, reject) => {
    const req = indexedDB.open('psoxide-covers', 1);
    req.onupgradeneeded = () => req.result.createObjectStore('c');
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
  return _db;
}
function _get(key) {
  return _open().then((db) => new Promise((resolve) => {
    const r = db.transaction('c').objectStore('c').get(key);
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => resolve(undefined);
  })).catch(() => undefined);
}
function _put(key, value) {
  return _open().then((db) => new Promise((resolve) => {
    const tx = db.transaction('c', 'readwrite');
    tx.objectStore('c').put(value, key);
    tx.oncomplete = () => resolve();
    tx.onerror = () => resolve();
  })).catch(() => undefined);
}
export function cvRequest(id, keys, urls, ttl) {
  (async () => {
    let offline = false;
    for (let i = 0; i < keys.length; i++) {
      const cached = await _get(keys[i]);
      if (cached && cached.bytes) { _done.push({ id, bytes: cached.bytes, source: keys[i], missing: false }); return; }
      if (cached && cached.miss && Date.now() / 1000 - cached.miss < ttl) continue;
      try {
        const r = await fetch(urls[i], { mode: 'cors' });
        if (r.ok) {
          const bytes = new Uint8Array(await r.arrayBuffer());
          await _put(keys[i], { bytes });
          _done.push({ id, bytes, source: keys[i], missing: false });
          return;
        }
        if (r.status === 404) await _put(keys[i], { miss: Math.floor(Date.now() / 1000) });
        else offline = true;
      } catch (e) { offline = true; }
    }
    _done.push({ id, bytes: undefined, source: '', missing: !offline });
  })();
}
export function cvTake() { return _done.length ? _done.shift() : undefined; }
export function cvId(item) { return item.id; }
export function cvBytes(item) { return item.bytes; }
export function cvSource(item) { return item.source; }
export function cvMissing(item) { return item.missing; }
"#)]
    extern "C" {
        #[wasm_bindgen(js_name = cvRequest)]
        fn cv_request(id: &str, keys: Vec<String>, urls: Vec<String>, ttl: f64);
        #[wasm_bindgen(js_name = cvTake)]
        fn cv_take() -> wasm_bindgen::JsValue;
        #[wasm_bindgen(js_name = cvId)]
        fn cv_id(item: &wasm_bindgen::JsValue) -> String;
        #[wasm_bindgen(js_name = cvBytes)]
        fn cv_bytes(item: &wasm_bindgen::JsValue) -> Option<js_sys::Uint8Array>;
        #[wasm_bindgen(js_name = cvSource)]
        fn cv_source(item: &wasm_bindgen::JsValue) -> String;
        #[wasm_bindgen(js_name = cvMissing)]
        fn cv_missing(item: &wasm_bindgen::JsValue) -> bool;
    }

    pub(super) fn request(id: &str, list: &[Candidate]) {
        cv_request(
            id,
            list.iter().map(|c| c.key.clone()).collect(),
            list.iter().map(|c| c.url.clone()).collect(),
            MISS_TTL_SECS as f64,
        );
    }

    pub(super) fn poll() -> Vec<Fetched> {
        let mut out = Vec::new();
        loop {
            let item = cv_take();
            if item.is_undefined() {
                break;
            }
            out.push(Fetched {
                id: cv_id(&item),
                bytes: cv_bytes(&item).map(|b| b.to_vec()),
                source: cv_source(&item),
                missing: cv_missing(&item),
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libretro_names_replace_forbidden_characters() {
        assert_eq!(libretro_name("Spyro: Year of the Dragon (USA)"), "Spyro_ Year of the Dragon (USA)");
    }

    #[test]
    fn candidates_try_the_serial_before_the_name() {
        let c = candidates(Some("SCUS-94900"), "Crash Bandicoot (USA)");
        assert_eq!(c.len(), 2);
        assert!(c[0].url.ends_with("/SCUS-94900.jpg"));
        assert!(c[1].url.ends_with("Crash%20Bandicoot%20%28USA%29.png"));
        assert_eq!(candidates(None, "").len(), 0);
    }
}
