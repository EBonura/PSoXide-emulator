//! Box art for the visitor's own discs.
//!
//! Finding the picture of a disc is a short chain, each link tried only when
//! the one before it found nothing:
//!
//! 1. **The disc's own art**: a `<name>.cover.png` or `cover.png` next to a
//!    homebrew disc (desktop). Our own games never get a placeholder when
//!    they have art.
//! 2. **Boot serial** (`SYSTEM.CNF`) in the xlenore/psx-covers collection:
//!    exact for retail discs and cheap (a few sectors).
//! 3. **The Redump catalogue**, which names the game exactly. The serial finds
//!    the name; failing that, the CRC-32 of the data track does (so a renamed
//!    file, a wrong-region name or an odd `SYSTEM.CNF` still match). The
//!    name then picks the libretro-thumbnails boxart, and the catalogue's own
//!    serial is tried in xlenore too. The catalogue is downloaded once and
//!    kept in the cache, never shipped (CC BY-SA 4.0).
//! 4. **The file name**, last, against libretro-thumbnails.
//!
//! A generated case stands in when nothing has it. Pictures are fetched at
//! run time only and kept in a cache (IndexedDB on the web, a folder on the
//! desktop). A miss is remembered for a week so a disc without art does not
//! ask every visit; being offline is not a miss. The chain is a small state
//! machine ([`CoverState`]) over an [`Io`] that does the file and network
//! work off the UI thread, so the logic is tested without either.

use egui::{ColorImage, TextureHandle, TextureId, TextureOptions};
use psoxide_settings::game_group::strip_track;
use psoxide_settings::redump::RedumpDat;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Seconds a remembered miss is believed.
pub(crate) const MISS_TTL_SECS: u64 = 7 * 24 * 3600;
/// Seconds the downloaded catalogue is believed before it is fetched again.
pub(crate) const DAT_TTL_SECS: u64 = 30 * 24 * 3600;
/// Where the Redump PlayStation catalogue lives (libretro-database, CC BY-SA 4.0).
pub(crate) const DAT_URL: &str = "https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/redump/Sony%20-%20PlayStation.dat";
/// Pictures decoded per frame, so a big library cannot stall a frame.
const DECODES_PER_FRAME: usize = 1;
/// Edge length the pictures are cut down to in memory.
const MAX_EDGE: u32 = 256;
/// Seconds a failed network answer waits before the same disc asks again.
const OFFLINE_RETRY_SECS: f64 = 20.0;

/// Ids that start with this are the Demo Disc's own cover, which ships inside the
/// program: no lookup, no network, and it does not depend on the box-art setting.
pub(crate) const DEMO_COVER_PREFIX: &str = "embedded:demo-cover";
/// The Demo Disc's cover. Desktop only: the web build's Demo Disc keeps its drawn cover.
#[cfg(not(target_arch = "wasm32"))]
const DEMO_COVER_PNG: &[u8] =
    include_bytes!("../../../../../../../assets/branding/demo-disc-cover.png");
/// Edge length the shipped cover is kept at: it is the hero case, drawn larger than the rest.
const EMBEDDED_EDGE: u32 = 512;

/// The picture that ships inside the program for an id, if it is one of those.
fn embedded_art(id: &str) -> Option<&'static [u8]> {
    #[cfg(not(target_arch = "wasm32"))]
    if id.starts_with(DEMO_COVER_PREFIX) {
        return Some(DEMO_COVER_PNG);
    }
    let _ = id;
    None
}

/// Where a picture can come from, tried in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    /// Cache key: stable for the same picture.
    pub key: String,
    pub url: String,
}

/// What the work threads (or browser promises) report back.
#[derive(Debug)]
pub(crate) enum Done {
    /// A candidate had a picture.
    Image {
        id: String,
        bytes: Vec<u8>,
        source: String,
    },
    /// Every candidate answered "not there".
    Miss { id: String },
    /// The network did not answer: not a miss, try again later.
    Offline { id: String },
    /// The disc was looked at: its boot serial and the art found beside it.
    Probed {
        id: String,
        serial: Option<String>,
        art: Option<Vec<u8>>,
    },
    /// The data track was hashed: its size and CRC-32.
    Hashed {
        id: String,
        track: Option<(u64, u32)>,
    },
    /// The Redump catalogue arrived (`None`: it could not be had).
    Dat(Option<Box<RedumpDat>>),
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

/// The xlenore picture of a boot serial.
pub(crate) fn serial_candidate(serial: &str) -> Candidate {
    Candidate {
        key: format!("x:{serial}"),
        url: format!(
            "https://raw.githubusercontent.com/xlenore/psx-covers/main/covers/default/{serial}.jpg"
        ),
    }
}

/// The libretro-thumbnails boxart of a game name (a Redump name, or a file stem).
pub(crate) fn name_candidate(name: &str) -> Option<Candidate> {
    let name = libretro_name(name.trim());
    (!name.is_empty()).then(|| Candidate {
        url: format!(
            "https://raw.githubusercontent.com/libretro-thumbnails/Sony_-_PlayStation/master/Named_Boxarts/{}.png",
            encode(&name)
        ),
        key: format!("l:{name}"),
    })
}

/// Which picture work is under way, for the case's loading state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CoverStatus {
    /// Nothing asked yet.
    Idle,
    /// A lookup or a download is in flight.
    Loading,
    /// The picture is on hand.
    Ready,
    /// No source has one: the generated case stays.
    Missing,
}

/// The file and network work behind the state machine. Every call returns at
/// once; the answer comes back through [`Io::poll`].
pub(crate) trait Io {
    /// Try the candidates in order (cache, then network) for `id`.
    fn fetch(&mut self, id: &str, list: &[Candidate]);
    /// Read the disc's boot serial and look for art beside it.
    fn probe(&mut self, id: &str, source: &Source);
    /// Hash the disc's data track. Only called when [`Io::can_hash`].
    fn hash(&mut self, id: &str, source: &Source);
    /// Download or load the catalogue.
    fn dat(&mut self);
    /// Whether this disc's data track can be read here.
    fn can_hash(&self, source: &Source) -> bool;
    /// What has finished since the last call.
    fn poll(&mut self) -> Vec<Done>;
}

/// What the state knows about one disc.
#[derive(Clone, Debug, Default)]
pub(crate) struct Source {
    pub stem: String,
    pub path: Option<PathBuf>,
    /// The launch id, which the browser resolves to a file.
    pub web_id: Option<String>,
    pub serial: Option<String>,
    pub serial_known: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// Look at the disc.
    Probe,
    Probing,
    /// Try the serial's picture.
    Serial,
    /// Ask the catalogue for the serial's name.
    DatSerial,
    /// Hash the data track.
    Hash,
    Hashing,
    /// Ask the catalogue for the hash's name.
    AfterHash,
    /// The file name.
    Name,
    /// Every link answered; a fetch in flight is the last word.
    Last,
}

struct Job {
    source: Source,
    step: Step,
    crc: Option<(u64, u32)>,
    /// Keys already asked, so two links never repeat a picture.
    asked: HashSet<String>,
}

enum DatState {
    Unrequested,
    Loading,
    Ready(Box<RedumpDat>),
    Failed,
}

/// Cover textures and the requests behind them.
pub(crate) struct CoverState {
    enabled: bool,
    io: Box<dyn Io>,
    sources: HashMap<String, Source>,
    jobs: HashMap<String, Job>,
    ready: Vec<(String, Vec<u8>)>,
    textures: HashMap<String, TextureHandle>,
    missing: HashSet<String>,
    /// Ids that wait for the catalogue.
    waiting: Vec<String>,
    dat: DatState,
    /// Retry times (seconds on the egui clock) of discs the network failed.
    offline_until: HashMap<String, f64>,
    now: f64,
    /// Which sources supplied pictures (for the credits and the probe).
    credits: HashSet<&'static str>,
}

impl Default for CoverState {
    fn default() -> Self {
        Self::with_io(Box::new(platform::PlatformIo))
    }
}

impl CoverState {
    pub(crate) fn with_io(io: Box<dyn Io>) -> Self {
        Self {
            enabled: true,
            io,
            sources: HashMap::new(),
            jobs: HashMap::new(),
            ready: Vec::new(),
            textures: HashMap::new(),
            missing: HashSet::new(),
            waiting: Vec::new(),
            dat: DatState::Unrequested,
            offline_until: HashMap::new(),
            now: 0.0,
            credits: HashSet::new(),
        }
    }

    /// Switch fetching on or off (the Settings toggle).
    pub(crate) fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
    }

    /// Tell the state what a launch id is: its file stem and, on the desktop,
    /// the file to read the serial from.
    pub(crate) fn describe(&mut self, id: &str, stem: &str, path: Option<PathBuf>) {
        let entry = self.sources.entry(id.to_string()).or_default();
        entry.stem = strip_track(stem).to_string();
        entry.path = path;
    }

    /// A disc that was booted told its serial.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(crate) fn set_serial(&mut self, id: &str, serial: Option<String>) {
        let entry = self.sources.entry(id.to_string()).or_default();
        if serial.is_some() && entry.serial != serial {
            // A serial is a better key than the file name: look again.
            self.missing.remove(id);
            self.jobs.remove(id);
        }
        entry.serial = serial.or_else(|| entry.serial.take());
        entry.serial_known = true;
    }

    /// The status of an id's picture, for the case's loading state.
    pub(crate) fn status(&self, id: &str) -> CoverStatus {
        if self.textures.contains_key(id) {
            CoverStatus::Ready
        } else if self.missing.contains(id) || (!self.enabled && embedded_art(id).is_none()) {
            CoverStatus::Missing
        } else if self.jobs.contains_key(id) || self.ready.iter().any(|(r, _)| r == id) {
            CoverStatus::Loading
        } else {
            CoverStatus::Idle
        }
    }

    /// Hand the work threads' finished results to the state. Called each frame.
    pub(crate) fn pump(&mut self) {
        for done in self.io.poll() {
            self.on_done(done);
        }
    }

    fn on_done(&mut self, done: Done) {
        match done {
            Done::Image { id, bytes, source } => {
                self.credits.insert(if source.starts_with("x:") {
                    "xlenore/psx-covers"
                } else if source.starts_with("own:") {
                    "your library"
                } else {
                    "libretro-thumbnails"
                });
                self.ready.push((id, bytes));
            }
            Done::Miss { id } => self.advance(&id),
            Done::Offline { id } => {
                // Clear the job so a later look retries, after a pause.
                self.jobs.remove(&id);
                self.offline_until.insert(id, self.now + OFFLINE_RETRY_SECS);
            }
            Done::Probed { id, serial, art } => {
                if let Some(job) = self.jobs.get_mut(&id) {
                    if job.source.serial.is_none() {
                        job.source.serial = serial;
                    }
                    job.source.serial_known = true;
                    job.step = Step::Serial;
                }
                if let Some(bytes) = art {
                    self.credits.insert("your library");
                    self.ready.push((id, bytes));
                } else {
                    self.advance(&id);
                }
            }
            Done::Hashed { id, track } => {
                if let Some(job) = self.jobs.get_mut(&id) {
                    job.crc = track;
                    job.step = Step::AfterHash;
                }
                self.advance(&id);
            }
            Done::Dat(dat) => {
                self.dat = match dat {
                    Some(d) => DatState::Ready(d),
                    None => DatState::Failed,
                };
                for id in std::mem::take(&mut self.waiting) {
                    self.advance(&id);
                }
            }
        }
    }

    /// Start (or continue) a disc's chain from where it stands.
    fn advance(&mut self, id: &str) {
        loop {
            let Some(job) = self.jobs.get_mut(id) else {
                return;
            };
            match job.step {
                Step::Probe => {
                    if job.source.serial_known {
                        job.step = Step::Serial;
                        continue;
                    }
                    job.step = Step::Probing;
                    let source = job.source.clone();
                    self.io.probe(id, &source);
                    return;
                }
                Step::Probing | Step::Hashing => return,
                Step::Serial => {
                    job.step = Step::DatSerial;
                    let Some(serial) = job.source.serial.clone() else {
                        continue;
                    };
                    let list = [serial_candidate(&serial)];
                    if self.ask(id, &list) {
                        return;
                    }
                }
                Step::DatSerial => {
                    let serial = job.source.serial.clone();
                    // Nothing to look up and nothing to hash: straight to the name.
                    if serial.is_none() && !self.io.can_hash(&job.source) {
                        job.step = Step::Name;
                        continue;
                    }
                    // The catalogue only helps from here on; without a serial
                    // the hash step is next anyway.
                    match &self.dat {
                        DatState::Ready(dat) => {
                            let name = serial
                                .as_deref()
                                .and_then(|s| dat.by_serial(s))
                                .and_then(|g| name_candidate(&g.name));
                            job.step = Step::Hash;
                            if let Some(c) = name {
                                if self.ask(id, &[c]) {
                                    return;
                                }
                            }
                        }
                        DatState::Failed => job.step = Step::Name,
                        DatState::Unrequested | DatState::Loading => {
                            self.wait_for_dat(id);
                            return;
                        }
                    }
                }
                Step::Hash => {
                    let source = job.source.clone();
                    if self.io.can_hash(&source) {
                        let job = self.jobs.get_mut(id).expect("job");
                        job.step = Step::Hashing;
                        self.io.hash(id, &source);
                        return;
                    }
                    job.step = Step::Name;
                }
                Step::AfterHash => {
                    job.step = Step::Name;
                    let serial = job.source.serial.clone();
                    let list = match (&self.dat, job.crc) {
                        (DatState::Ready(dat), Some((size, crc))) => {
                            dat.by_track(size, crc).map(|game| {
                                let mut list = Vec::new();
                                if !game.serial.is_empty()
                                    && serial.as_deref() != Some(game.serial.as_str())
                                {
                                    list.push(serial_candidate(&game.serial));
                                }
                                list.extend(name_candidate(&game.name));
                                list
                            })
                        }
                        _ => None,
                    };
                    let Some(list) = list else {
                        continue;
                    };
                    if self.ask(id, &list) {
                        return;
                    }
                }
                Step::Name => {
                    job.step = Step::Last;
                    let list: Vec<Candidate> =
                        name_candidate(&job.source.stem).into_iter().collect();
                    if self.ask(id, &list) {
                        return;
                    }
                }
                Step::Last => {
                    // Every link answered "not there".
                    self.jobs.remove(id);
                    self.missing.insert(id.to_string());
                    return;
                }
            }
        }
    }

    /// Fetch the candidates not asked before. `true` when a fetch is under way.
    fn ask(&mut self, id: &str, list: &[Candidate]) -> bool {
        let Some(job) = self.jobs.get_mut(id) else {
            return false;
        };
        let fresh: Vec<Candidate> = list
            .iter()
            .filter(|c| job.asked.insert(c.key.clone()))
            .cloned()
            .collect();
        if fresh.is_empty() {
            return false;
        }
        self.io.fetch(id, &fresh);
        true
    }

    fn wait_for_dat(&mut self, id: &str) {
        if !self.waiting.iter().any(|w| w == id) {
            self.waiting.push(id.to_string());
        }
        if matches!(self.dat, DatState::Unrequested) {
            self.dat = DatState::Loading;
            self.io.dat();
        }
    }

    /// The picture of a launch id, if it is on hand; starts the lookup when not.
    pub(crate) fn texture(&mut self, ctx: &egui::Context, id: &str) -> Option<TextureId> {
        self.now = ctx.input(|i| i.time);
        if let Some(tex) = self.textures.get(id) {
            return Some(tex.id());
        }
        if !self.enabled && embedded_art(id).is_none() {
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
            self.jobs.remove(&rid);
            let edge = if embedded_art(&rid).is_some() {
                EMBEDDED_EDGE
            } else {
                MAX_EDGE
            };
            match decode(&bytes, edge) {
                Some(image) => {
                    let tex =
                        ctx.load_texture(format!("cover-{rid}"), image, TextureOptions::LINEAR);
                    self.textures.insert(rid, tex);
                }
                None => {
                    self.missing.insert(rid);
                }
            }
        }
        if let Some(tex) = self.textures.get(id) {
            return Some(tex.id());
        }
        self.start(id);
        None
    }

    /// Begin the chain for an id nobody asked about yet.
    fn start(&mut self, id: &str) {
        if self.jobs.contains_key(id)
            || self.missing.contains(id)
            || self.ready.iter().any(|(r, _)| r == id)
            || self.offline_until.get(id).is_some_and(|t| *t > self.now)
        {
            return;
        }
        self.offline_until.remove(id);
        if let Some(bytes) = embedded_art(id) {
            // Nothing to look up: it is already here, and only needs decoding.
            self.ready.push((id.to_string(), bytes.to_vec()));
            return;
        }
        let Some(source) = self.source_of(id) else {
            // Not a disc we can name (a web id before its folder is known).
            self.missing.insert(id.to_string());
            return;
        };
        self.jobs.insert(
            id.to_string(),
            Job {
                source,
                step: Step::Probe,
                crc: None,
                asked: HashSet::new(),
            },
        );
        self.advance(id);
    }

    /// What is known about an id; a path or web launch id says where the disc is.
    fn source_of(&mut self, id: &str) -> Option<Source> {
        if !self.sources.contains_key(id) {
            let from_path = |path: &str| {
                let p = PathBuf::from(path);
                let stem = p
                    .file_stem()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_string();
                (strip_track(&stem).to_string(), p)
            };
            // The launch id says where the disc is: a library path, a path
            // opened by hand, or (web) a path inside the connected folder.
            let guess = if let Some(path) = id
                .strip_prefix("path:")
                .or_else(|| id.strip_prefix("open:"))
            {
                let (stem, p) = from_path(path);
                Source {
                    stem,
                    path: Some(p),
                    ..Source::default()
                }
            } else {
                let (stem, _) = from_path(id.strip_prefix("web:")?);
                Source {
                    stem,
                    ..Source::default()
                }
            };
            self.sources.insert(id.to_string(), guess);
        }
        let mut source = self.sources.get(id)?.clone();
        if id.starts_with("web:") {
            source.web_id = Some(id.to_string());
        }
        Some(source)
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

    /// Discs whose lookup is still running, for the probe.
    #[allow(dead_code)]
    pub(crate) fn pending(&self) -> usize {
        self.jobs.len()
    }
}

/// Decode a PNG or JPEG and cut it down to `edge` pixels on its long side.
fn decode(bytes: &[u8], edge: u32) -> Option<ColorImage> {
    let img = image::load_from_memory(bytes).ok()?;
    let img = if img.width().max(img.height()) > edge {
        img.thumbnail(edge, edge)
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

mod platform {
    //! The platform's [`Io`]: threads and `curl` on the desktop, promises in
    //! the browser.
    use super::{Candidate, Done, Io, Source};

    pub(super) struct PlatformIo;

    impl Io for PlatformIo {
        fn fetch(&mut self, id: &str, list: &[Candidate]) {
            super::io::fetch(id, list);
        }
        fn probe(&mut self, id: &str, source: &Source) {
            super::io::probe(id, source);
        }
        fn hash(&mut self, id: &str, source: &Source) {
            super::io::hash(id, source);
        }
        fn dat(&mut self) {
            super::io::dat();
        }
        fn can_hash(&self, source: &Source) -> bool {
            super::io::can_hash(source)
        }
        fn poll(&mut self) -> Vec<Done> {
            super::io::poll()
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod io {
    //! Desktop work: threads, `curl`, and a folder of files as the cache.

    use std::path::{Path, PathBuf};
    use std::sync::{mpsc, Condvar, Mutex, OnceLock};

    use psoxide_settings::redump::{crc_of_track, data_track, RedumpDat};

    use super::{Candidate, Done, Source, DAT_TTL_SECS, DAT_URL, MISS_TTL_SECS};

    /// Pictures downloaded at the same time.
    const FETCH_SLOTS: usize = 4;

    static DONE: OnceLock<(mpsc::Sender<Done>, Mutex<mpsc::Receiver<Done>>)> = OnceLock::new();
    static SLOTS: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());
    /// Hashing reads whole images: one at a time keeps the disk usable.
    static HASH_LOCK: Mutex<()> = Mutex::new(());

    fn channel() -> &'static (mpsc::Sender<Done>, Mutex<mpsc::Receiver<Done>>) {
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
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    fn age_secs(path: &Path) -> u64 {
        std::fs::metadata(path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|m| m.elapsed().ok())
            .map_or(u64::MAX, |d| d.as_secs())
    }

    fn spawn(name: &str, work: impl FnOnce() -> Done + Send + 'static) {
        let tx = channel().0.clone();
        let spawned = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let _ = tx.send(work());
            });
        if let Err(e) = spawned {
            eprintln!("[frontend] {name} thread: {e}");
        }
    }

    pub(super) fn can_hash(source: &Source) -> bool {
        source.path.is_some()
    }

    pub(super) fn fetch(id: &str, list: &[Candidate]) {
        let id = id.to_string();
        let list = list.to_vec();
        spawn("covers", move || {
            // A few downloads at once, not a hundred.
            let (lock, cv) = &SLOTS;
            let mut used = lock.lock().expect("fetch slots");
            while *used >= FETCH_SLOTS {
                used = cv.wait(used).expect("fetch slots");
            }
            *used += 1;
            drop(used);
            let done = run_fetch(&id, &list);
            *lock.lock().expect("fetch slots") -= 1;
            cv.notify_one();
            done
        });
    }

    fn run_fetch(id: &str, list: &[Candidate]) -> Done {
        let dir = cache_dir();
        let _ = std::fs::create_dir_all(&dir);
        let mut offline = false;
        for cand in list {
            let hit = dir.join(format!("{}.img", file_for(&cand.key)));
            let miss = dir.join(format!("{}.miss", file_for(&cand.key)));
            if let Ok(bytes) = std::fs::read(&hit) {
                return Done::Image {
                    id: id.into(),
                    bytes,
                    source: cand.key.clone(),
                };
            }
            if std::fs::metadata(&miss).is_ok() && age_secs(&miss) < MISS_TTL_SECS {
                continue;
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
                        return Done::Image {
                            id: id.into(),
                            bytes,
                            source: cand.key.clone(),
                        };
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
        if offline {
            Done::Offline { id: id.into() }
        } else {
            Done::Miss { id: id.into() }
        }
    }

    /// Art a homebrew disc ships with: `<name>.cover.png|jpg` beside it, or
    /// `cover.png|jpg` in its folder.
    fn own_art(path: &Path) -> Option<Vec<u8>> {
        let dir = path.parent()?;
        let stem = path.file_stem()?.to_str()?;
        for name in [
            format!("{stem}.cover.png"),
            format!("{stem}.cover.jpg"),
            "cover.png".to_string(),
            "cover.jpg".to_string(),
        ] {
            if let Ok(bytes) = std::fs::read(dir.join(name)) {
                return Some(bytes);
            }
        }
        None
    }

    pub(super) fn probe(id: &str, source: &Source) {
        let id = id.to_string();
        let path = source.path.clone();
        spawn("cover-probe", move || {
            let (serial, art) = match path.as_deref() {
                Some(p) => (psoxide_settings::library::disc_boot_serial(p), own_art(p)),
                None => (None, None),
            };
            Done::Probed { id, serial, art }
        });
    }

    /// The identity cache: a data track's hash, kept by file and modification
    /// time so a library of many discs hashes each image once.
    fn ident_file(track: &psoxide_settings::redump::DataTrack) -> Option<PathBuf> {
        let meta = std::fs::metadata(&track.path).ok()?;
        let mtime = meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs();
        let name = format!(
            "{}|{}|{}|{}",
            track.path.to_string_lossy(),
            meta.len(),
            mtime,
            track.len.unwrap_or(0)
        );
        Some(cache_dir().join(format!("ident-{:016x}.txt", fnv(name.as_bytes()))))
    }

    fn fnv(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
        })
    }

    pub(super) fn hash(id: &str, source: &Source) {
        let id = id.to_string();
        let path = source.path.clone();
        spawn("cover-hash", move || {
            let track = path.as_deref().and_then(data_track).and_then(|t| {
                let cache = ident_file(&t);
                if let Some(text) = cache.as_ref().and_then(|c| std::fs::read_to_string(c).ok()) {
                    let mut it = text.split_whitespace();
                    if let (Some(size), Some(crc)) = (it.next(), it.next()) {
                        if let (Ok(size), Ok(crc)) = (size.parse(), u32::from_str_radix(crc, 16)) {
                            return Some((size, crc));
                        }
                    }
                }
                let _one_at_a_time = HASH_LOCK.lock().expect("hash lock");
                let (size, crc) = crc_of_track(&t)?;
                if let Some(cache) = cache {
                    let _ = std::fs::create_dir_all(cache_dir());
                    let _ = std::fs::write(cache, format!("{size} {crc:08x}\n"));
                }
                Some((size, crc))
            });
            Done::Hashed { id, track }
        });
    }

    pub(super) fn dat() {
        spawn("cover-dat", || {
            let dir = cache_dir();
            let _ = std::fs::create_dir_all(&dir);
            let file = dir.join("redump-playstation.dat");
            if !file.exists() || age_secs(&file) > DAT_TTL_SECS {
                let tmp = dir.join(format!("redump-playstation.part{}", now()));
                let got = std::process::Command::new("curl")
                    .args(["-sfL", "--max-time", "120", "-o"])
                    .arg(&tmp)
                    .arg(DAT_URL)
                    .status()
                    .is_ok_and(|s| s.success());
                if got {
                    let _ = std::fs::rename(&tmp, &file);
                } else {
                    let _ = std::fs::remove_file(&tmp);
                }
            }
            // A stale copy beats none when the network is down.
            let dat = std::fs::read_to_string(&file)
                .ok()
                .map(|text| RedumpDat::parse(&text))
                .filter(|d| !d.is_empty())
                .map(Box::new);
            Done::Dat(dat)
        });
    }

    pub(super) fn poll() -> Vec<Done> {
        let rx = channel().1.lock().expect("cover channel");
        rx.try_iter().collect()
    }
}

#[cfg(target_arch = "wasm32")]
mod io {
    //! Browser work: `fetch` and IndexedDB for pictures and the catalogue, and
    //! the picked folder's files read a slice at a time for the serial and the
    //! hash.

    use psoxide_settings::redump::{Crc32, RedumpDat};
    use std::cell::RefCell;
    use wasm_bindgen::prelude::wasm_bindgen;
    use wasm_bindgen_futures::{spawn_local, JsFuture};

    use super::{Candidate, Done, Source, DAT_TTL_SECS, DAT_URL, MISS_TTL_SECS};

    /// Bytes of the data track read to find `SYSTEM.CNF`: 512 raw sectors.
    const SERIAL_PREFIX_BYTES: f64 = 512.0 * 2352.0;
    /// Bytes hashed per step, so a frame is never held for long.
    const HASH_STEP_BYTES: f64 = 4.0 * 1024.0 * 1024.0;

    thread_local! {
        static DONE: RefCell<Vec<Done>> = const { RefCell::new(Vec::new()) };
        /// Hashing reads whole images: one at a time.
        static HASHING: RefCell<std::collections::VecDeque<(String, Source)>> =
            const { RefCell::new(std::collections::VecDeque::new()) };
        static HASH_BUSY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    fn finish(done: Done) {
        DONE.with(|d| d.borrow_mut().push(done));
    }

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
// The catalogue text, from the cache while it is fresh, else downloaded.
export async function cvDat(url, ttl) {
  const cached = await _get('redump-dat');
  const fresh = cached && cached.text && Date.now() / 1000 - cached.at < ttl;
  if (fresh) return cached.text;
  try {
    const r = await fetch(url, { mode: 'cors' });
    if (r.ok) {
      const text = await r.text();
      await _put('redump-dat', { text, at: Math.floor(Date.now() / 1000) });
      return text;
    }
  } catch (e) {}
  return cached && cached.text ? cached.text : undefined;
}
export async function cvIdentGet(key) {
  const v = await _get('ident:' + key);
  return v && v.size ? v.size + ' ' + v.crc : undefined;
}
export function cvIdentPut(key, size, crc) { return _put('ident:' + key, { size, crc }); }
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
        #[wasm_bindgen(js_name = cvDat)]
        fn cv_dat(url: &str, ttl: f64) -> js_sys::Promise;
        #[wasm_bindgen(js_name = cvIdentGet)]
        fn cv_ident_get(key: &str) -> js_sys::Promise;
        #[wasm_bindgen(js_name = cvIdentPut)]
        fn cv_ident_put(key: &str, size: f64, crc: f64) -> js_sys::Promise;
    }

    pub(super) fn can_hash(source: &Source) -> bool {
        source.web_id.is_some()
    }

    pub(super) fn fetch(id: &str, list: &[Candidate]) {
        cv_request(
            id,
            list.iter().map(|c| c.key.clone()).collect(),
            list.iter().map(|c| c.url.clone()).collect(),
            MISS_TTL_SECS as f64,
        );
    }

    pub(super) fn probe(id: &str, source: &Source) {
        let id = id.to_string();
        let web_id = source.web_id.clone();
        spawn_local(async move {
            let serial = match web_id {
                Some(web) => read_serial(&web).await,
                None => None,
            };
            finish(Done::Probed {
                id,
                serial,
                art: None,
            });
        });
    }

    async fn read_serial(web_id: &str) -> Option<String> {
        let blob = crate::web_files::blob_for_id(web_id).await?;
        let end = SERIAL_PREFIX_BYTES.min(blob.size());
        let part = blob.slice_with_f64_and_f64(0.0, end).ok()?;
        let buf = JsFuture::from(part.array_buffer()).await.ok()?;
        let bytes = js_sys::Uint8Array::new(&buf).to_vec();
        psoxide_settings::library::boot_serial_of_prefix(bytes)
    }

    pub(super) fn hash(id: &str, source: &Source) {
        HASHING.with(|q| q.borrow_mut().push_back((id.to_string(), source.clone())));
        start_next_hash();
    }

    fn start_next_hash() {
        if HASH_BUSY.with(std::cell::Cell::get) {
            return;
        }
        let Some((id, source)) = HASHING.with(|q| q.borrow_mut().pop_front()) else {
            return;
        };
        HASH_BUSY.with(|b| b.set(true));
        spawn_local(async move {
            let track = match source.web_id {
                Some(web) => hash_blob(&web).await,
                None => None,
            };
            HASH_BUSY.with(|b| b.set(false));
            finish(Done::Hashed { id, track });
            start_next_hash();
        });
    }

    async fn hash_blob(web_id: &str) -> Option<(u64, u32)> {
        let blob = crate::web_files::blob_for_id(web_id).await?;
        let size = blob.size();
        let modified = js_sys::Reflect::get(blob.as_ref(), &"lastModified".into())
            .ok()
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let key = format!("{web_id}|{size}|{modified}");
        if let Some(text) = JsFuture::from(cv_ident_get(&key))
            .await
            .ok()
            .and_then(|v| v.as_string())
        {
            let mut it = text.split_whitespace();
            if let (Some(s), Some(c)) = (it.next(), it.next()) {
                if let (Ok(s), Ok(c)) = (s.parse::<u64>(), c.parse::<u32>()) {
                    return Some((s, c));
                }
            }
        }
        let mut crc = Crc32::new();
        let mut at = 0.0;
        while at < size {
            let end = (at + HASH_STEP_BYTES).min(size);
            let part = blob.slice_with_f64_and_f64(at, end).ok()?;
            let buf = JsFuture::from(part.array_buffer()).await.ok()?;
            crc.update(&js_sys::Uint8Array::new(&buf).to_vec());
            at = end;
        }
        let crc = crc.finish();
        let _ = JsFuture::from(cv_ident_put(&key, size, f64::from(crc))).await;
        Some((size as u64, crc))
    }

    pub(super) fn dat() {
        spawn_local(async {
            let text = JsFuture::from(cv_dat(DAT_URL, DAT_TTL_SECS as f64))
                .await
                .ok()
                .and_then(|v| v.as_string());
            let dat = text
                .map(|t| RedumpDat::parse(&t))
                .filter(|d| !d.is_empty())
                .map(Box::new);
            finish(Done::Dat(dat));
        });
    }

    pub(super) fn poll() -> Vec<Done> {
        let mut out: Vec<Done> = DONE.with(|d| std::mem::take(&mut *d.borrow_mut()));
        loop {
            let item = cv_take();
            if item.is_undefined() {
                break;
            }
            let id = cv_id(&item);
            out.push(match cv_bytes(&item) {
                Some(bytes) => Done::Image {
                    id,
                    bytes: bytes.to_vec(),
                    source: cv_source(&item),
                },
                None if cv_missing(&item) => Done::Miss { id },
                None => Done::Offline { id },
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    struct Log {
        fetches: Vec<(String, Vec<String>)>,
        probes: usize,
        hashes: usize,
        dats: usize,
        hashable: bool,
        queue: Vec<Done>,
    }

    struct Fake(Rc<RefCell<Log>>);

    impl Io for Fake {
        fn fetch(&mut self, id: &str, list: &[Candidate]) {
            self.0
                .borrow_mut()
                .fetches
                .push((id.into(), list.iter().map(|c| c.key.clone()).collect()));
        }
        fn probe(&mut self, _: &str, _: &Source) {
            self.0.borrow_mut().probes += 1;
        }
        fn hash(&mut self, _: &str, _: &Source) {
            self.0.borrow_mut().hashes += 1;
        }
        fn dat(&mut self) {
            self.0.borrow_mut().dats += 1;
        }
        fn can_hash(&self, _: &Source) -> bool {
            self.0.borrow().hashable
        }
        fn poll(&mut self) -> Vec<Done> {
            std::mem::take(&mut self.0.borrow_mut().queue)
        }
    }

    fn state(hashable: bool) -> (CoverState, Rc<RefCell<Log>>) {
        let log = Rc::new(RefCell::new(Log {
            hashable,
            ..Log::default()
        }));
        let mut s = CoverState::with_io(Box::new(Fake(log.clone())));
        s.describe(
            "path:/g/WipEout (USA).cue",
            "WipEout (USA) (Track 1)",
            Some("/g/x.cue".into()),
        );
        (s, log)
    }

    const ID: &str = "path:/g/WipEout (USA).cue";
    const DAT: &str = "game (\n\tname \"Wipeout (USA)\"\n\tregion \"USA\"\n\tserial \"SCUS-94301\"\n\trom ( name \"Wipeout (USA) (Track 1).bin\" size 100 crc 0000ABCD md5 1 sha1 2 serial \"SCUS-94301\" )\n)\n";

    fn dat() -> Box<RedumpDat> {
        Box::new(RedumpDat::parse(DAT))
    }

    fn keys(log: &Rc<RefCell<Log>>) -> Vec<Vec<String>> {
        log.borrow().fetches.iter().map(|f| f.1.clone()).collect()
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn the_demo_disc_cover_ships_inside_and_asks_nobody() {
        let (mut s, log) = state(true);
        s.set_enabled(false);
        let id = format!("{DEMO_COVER_PREFIX}:PSoXide Demo Disc v0.47");
        assert_eq!(
            s.status(&id),
            CoverStatus::Idle,
            "not switched off with box art"
        );
        let ctx = egui::Context::default();
        assert!(s.texture(&ctx, &id).is_none(), "decoded on a later call");
        assert_eq!(s.status(&id), CoverStatus::Loading);
        assert!(s.texture(&ctx, &id).is_some());
        assert_eq!(s.status(&id), CoverStatus::Ready);
        let log = log.borrow();
        assert!(
            log.fetches.is_empty() && log.probes == 0 && log.hashes == 0 && log.dats == 0,
            "no lookup of any kind"
        );
    }

    #[test]
    fn libretro_names_replace_forbidden_characters() {
        assert_eq!(
            libretro_name("Spyro: Year of the Dragon (USA)"),
            "Spyro_ Year of the Dragon (USA)"
        );
    }

    #[test]
    fn the_name_candidate_encodes_the_title() {
        let c = name_candidate("Crash Bandicoot (USA)").expect("candidate");
        assert!(c.url.ends_with("Crash%20Bandicoot%20%28USA%29.png"));
        assert!(name_candidate("  ").is_none());
        assert!(serial_candidate("SCUS-94900")
            .url
            .ends_with("/SCUS-94900.jpg"));
    }

    #[test]
    fn a_serial_is_tried_before_anything_else_and_a_hit_stops_the_chain() {
        let (mut s, log) = state(true);
        s.start(ID);
        assert_eq!(log.borrow().probes, 1);
        assert_eq!(s.status(ID), CoverStatus::Loading);
        s.on_done(Done::Probed {
            id: ID.into(),
            serial: Some("SCUS-94301".into()),
            art: None,
        });
        assert_eq!(keys(&log), vec![vec!["x:SCUS-94301".to_string()]]);
        s.on_done(Done::Image {
            id: ID.into(),
            bytes: vec![1],
            source: "x:SCUS-94301".into(),
        });
        assert_eq!(log.borrow().dats, 0, "the catalogue was never needed");
        assert_eq!(log.borrow().hashes, 0);
        assert_eq!(s.credits(), vec!["xlenore/psx-covers"]);
    }

    #[test]
    fn a_serial_miss_asks_the_catalogue_for_the_exact_name() {
        let (mut s, log) = state(true);
        s.start(ID);
        s.on_done(Done::Probed {
            id: ID.into(),
            serial: Some("SCUS-94301".into()),
            art: None,
        });
        s.on_done(Done::Miss { id: ID.into() });
        assert_eq!(log.borrow().dats, 1, "the catalogue is requested once");
        assert_eq!(s.status(ID), CoverStatus::Loading);
        s.on_done(Done::Dat(Some(dat())));
        // The serial's name in the catalogue, before any hashing.
        assert_eq!(
            keys(&log).last().expect("fetch"),
            &vec!["l:Wipeout (USA)".to_string()]
        );
        assert_eq!(log.borrow().hashes, 0);
    }

    #[test]
    fn the_hash_names_a_disc_with_no_serial_and_a_wrong_file_name() {
        let (mut s, log) = state(true);
        s.start(ID);
        s.on_done(Done::Probed {
            id: ID.into(),
            serial: None,
            art: None,
        });
        // No serial: the catalogue is wanted for the hash.
        assert_eq!(log.borrow().dats, 1);
        s.on_done(Done::Dat(Some(dat())));
        assert_eq!(log.borrow().hashes, 1);
        s.on_done(Done::Hashed {
            id: ID.into(),
            track: Some((100, 0xabcd)),
        });
        // The catalogue's serial in xlenore, then its exact name.
        assert_eq!(
            keys(&log).last().expect("fetch"),
            &vec!["x:SCUS-94301".to_string(), "l:Wipeout (USA)".to_string()]
        );
        // And the file name is only tried when all of that missed.
        s.on_done(Done::Miss { id: ID.into() });
        assert_eq!(
            keys(&log).last().expect("fetch"),
            &vec!["l:WipEout (USA)".to_string()]
        );
        s.on_done(Done::Miss { id: ID.into() });
        assert_eq!(s.status(ID), CoverStatus::Missing);
    }

    #[test]
    fn without_a_catalogue_the_file_name_is_the_last_resort() {
        let (mut s, log) = state(true);
        s.start(ID);
        s.on_done(Done::Probed {
            id: ID.into(),
            serial: Some("SCUS-94301".into()),
            art: None,
        });
        s.on_done(Done::Miss { id: ID.into() });
        s.on_done(Done::Dat(None));
        assert_eq!(log.borrow().hashes, 0, "no catalogue, no point hashing");
        assert_eq!(
            keys(&log).last().expect("fetch"),
            &vec!["l:WipEout (USA)".to_string()]
        );
    }

    #[test]
    fn a_picture_found_beside_the_disc_ends_the_chain_at_once() {
        let (mut s, log) = state(true);
        s.start(ID);
        s.on_done(Done::Probed {
            id: ID.into(),
            serial: None,
            art: Some(vec![1, 2, 3]),
        });
        assert!(log.borrow().fetches.is_empty());
        assert_eq!(s.status(ID), CoverStatus::Loading);
        assert_eq!(s.credits(), vec!["your library"]);
    }

    #[test]
    fn an_offline_answer_is_not_a_miss_and_retries_later() {
        let (mut s, _log) = state(false);
        s.start(ID);
        s.on_done(Done::Probed {
            id: ID.into(),
            serial: Some("SCUS-94301".into()),
            art: None,
        });
        s.on_done(Done::Offline { id: ID.into() });
        assert_eq!(s.status(ID), CoverStatus::Idle);
        s.start(ID);
        assert_eq!(s.status(ID), CoverStatus::Idle, "waits before asking again");
        s.now += OFFLINE_RETRY_SECS + 1.0;
        s.start(ID);
        assert_eq!(s.status(ID), CoverStatus::Loading);
    }

    #[test]
    fn a_disc_that_cannot_be_hashed_goes_to_the_name() {
        let (mut s, log) = state(false);
        s.start(ID);
        s.on_done(Done::Probed {
            id: ID.into(),
            serial: None,
            art: None,
        });
        assert_eq!(log.borrow().dats, 0, "no serial and no hash: no catalogue");
        assert_eq!(log.borrow().hashes, 0);
        assert_eq!(
            keys(&log).last().expect("fetch"),
            &vec!["l:WipEout (USA)".to_string()]
        );
    }

    #[test]
    fn an_unknown_id_is_missing_not_loading() {
        let (mut s, _) = state(true);
        s.start("mystery");
        assert_eq!(s.status("mystery"), CoverStatus::Missing);
    }
}
