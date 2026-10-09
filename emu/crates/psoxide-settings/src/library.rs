//! Scanned game library -- discovery, metadata extraction, and
//! on-disk cache.
//!
//! The scanner walks a configured root directory and classifies
//! each file as either a disc image (`.bin`, `.cue`, `.ccd`, `.iso`) or a
//! side-loadable homebrew (`.exe`). For each hit it extracts
//! cheap-to-read metadata so the UI can show a useful label
//! without running the emulator:
//!
//! - **Title** -- from the ISO9660 Primary Volume Descriptor's
//!   *volume identifier* field (for BIN/ISO), or the file stem
//!   (for EXE / anything we can't parse).
//! - **Region** -- inferred from the PSX license-text sector at
//!   LBA 4 (`Licensed by Sony Computer Entertainment America /
//!   Europe / Japan`).
//! - **Stable ID** -- a 16-hex-char FNV-1a-64 fingerprint. Disc IDs
//!   come from the disc's own contents (boot serial, volume, root
//!   directory, the head of the boot executable), so renaming a BIN
//!   doesn't orphan its savestates and two different retail discs never
//!   share an ID. EXE IDs include the file path so project builds with
//!   the same filename remain launch-distinct.
//!
//! Results are cached in `library.ron` alongside the source file's
//! last-modified time. A subsequent scan skips re-parsing files
//! whose mtime hasn't changed -- fast startup even with a big
//! library.
//!
//! Parsing is best-effort: any file that errors out surfaces as
//! [`LibraryEntry::kind`] == [`GameKind::Unknown`] with a reason
//! recorded in [`LibraryEntry::diagnostic`]. A malformed BIN
//! doesn't derail the whole scan.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use psx_iso::TrackSource;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::LIBRARY_VERSION;

/// Discovered game category. Drives the UI grid -- disc images,
/// homebrew, unknown/diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GameKind {
    /// A full PSX disc image in raw 2352-byte-per-sector format.
    DiscBin,
    /// An ISO9660 image (2048 bytes per sector, no raw subchannel).
    /// We don't *boot* these yet -- the CD controller expects BIN
    /// sector layout -- but they show up in the library so the user
    /// can see them.
    DiscIso,
    /// A `.cue` playlist pointing at one or more BIN files.
    DiscCue,
    /// A CloneCD control sheet pointing at a raw `.img` image, with
    /// optional `.sub` subchannel sidecar. An `.img.ecm` sidecar is
    /// decoded in memory at launch.
    DiscCcd,
    /// A PSX-EXE homebrew binary (our SDK's output + many demos).
    Exe,
    /// Didn't match any known format, or parsing failed. The
    /// [`LibraryEntry::diagnostic`] field carries the "why" so
    /// the UI can surface it.
    Unknown,
}

/// Region code inferred from the PSX license-text sector.
/// A real drive refuses non-matching regions; we record the info
/// for the UI but don't *enforce* it (users may swap BIOSes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Region {
    /// NTSC-U (US / Canada).
    NtscU,
    /// PAL (Europe, most of the world).
    Pal,
    /// NTSC-J (Japan + Asia).
    NtscJ,
    /// License text not recognised -- either a pre-release / unlicensed
    /// disc, or our heuristic missed it.
    Unknown,
}

/// A single discovered library entry. Serialised into
/// `library.ron`; small enough to comfortably hold a few thousand
/// entries in RAM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryEntry {
    /// Stable 16-hex-char game ID. Used to name the per-game
    /// directory under `games/<id>/`.
    pub id: String,
    /// Absolute path to the underlying file. Relative paths inside
    /// `library.ron` would be fragile across working directories.
    pub path: PathBuf,
    /// Classification -- what kind of file this is.
    pub kind: GameKind,
    /// Human-readable display name -- prefer the PVD volume
    /// identifier if present, else the file stem.
    pub title: String,
    /// Region code, best-effort.
    pub region: Region,
    /// File size in bytes. Displayed in the UI; also a cheap
    /// sanity-check on corruption.
    pub size: u64,
    /// File mtime (UNIX epoch seconds) captured at scan time.
    /// The next scan skips the parse step if the mtime hasn't
    /// moved -- huge startup win for big libraries.
    pub mtime: u64,
    /// Optional free-text diagnostic -- carries the "why" when
    /// `kind == Unknown`, or any notable warning for other kinds.
    pub diagnostic: Option<String>,
}

/// Full library cache + its version. Top-level of `library.ron`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Library {
    /// Schema version. Bumped when [`LibraryEntry`] changes
    /// incompatibly; older caches are silently discarded and
    /// regenerated on next scan.
    #[serde(default = "default_library_version")]
    pub version: u32,
    /// Discovered entries. Order is file-system walk order -- UI
    /// sorts at display time.
    #[serde(default)]
    pub entries: Vec<LibraryEntry>,
}

fn default_library_version() -> u32 {
    LIBRARY_VERSION
}

/// Errors from library load/save/scan.
#[derive(Debug, Error)]
pub enum LibraryError {
    /// Filesystem error while reading the cache or scanning.
    #[error("library I/O error at {path}: {source}")]
    Io {
        /// The file or directory we were working on.
        path: PathBuf,
        /// The underlying `io::Error`.
        #[source]
        source: io::Error,
    },
    /// Cache file couldn't be parsed. The scanner treats this as
    /// "regenerate from scratch" and logs -- no crash.
    #[error("library parse error at {path}: {source}")]
    Parse {
        /// The file we were parsing.
        path: PathBuf,
        /// RON parser's error.
        #[source]
        source: ron::error::SpannedError,
    },
    /// Serialisation failed.
    #[error("library serialization error: {0}")]
    Serialize(#[from] ron::Error),
}

impl Library {
    /// Load `library.ron` from `path`. Missing file / wrong version
    /// / corrupt file all return an empty [`Library`] rather than
    /// erroring -- the scanner will rebuild it. This keeps a
    /// first-run UX of "just works."
    pub fn load_or_empty(path: &Path) -> Self {
        match Self::load(path) {
            Ok(lib) if lib.version == LIBRARY_VERSION => lib,
            Ok(_) | Err(_) => Self {
                version: LIBRARY_VERSION,
                entries: Vec::new(),
            },
        }
    }

    /// Strict load -- propagates parse errors. Used in tests and by
    /// diagnostics that want to distinguish "missing" from
    /// "corrupt." Most production code wants [`load_or_empty`].
    pub fn load(path: &Path) -> Result<Self, LibraryError> {
        let contents = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(Self {
                    version: LIBRARY_VERSION,
                    entries: Vec::new(),
                });
            }
            Err(source) => {
                return Err(LibraryError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        ron::from_str(&contents).map_err(|source| LibraryError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Write the cache to `path` atomically. Same tmp-and-rename
    /// pattern as [`crate::Settings::save`].
    pub fn save(&self, path: &Path) -> Result<(), LibraryError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|source| LibraryError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
        }
        let body = ron::ser::to_string_pretty(
            self,
            ron::ser::PrettyConfig::new()
                .depth_limit(4)
                .indentor("    ".to_string()),
        )?;
        let tmp = path.with_extension("ron.tmp");
        fs::write(&tmp, body).map_err(|source| LibraryError::Io {
            path: tmp.clone(),
            source,
        })?;
        fs::rename(&tmp, path).map_err(|source| LibraryError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(())
    }

    /// Walk `root` recursively and update the library in place. For
    /// each file whose extension matches a known format:
    ///
    /// - If the cache already has an entry with the same path AND
    ///   the file's mtime hasn't moved, keep the cached metadata.
    /// - Otherwise, re-parse and either insert or replace.
    ///
    /// Entries whose file no longer exists on disk are pruned.
    /// Returns the number of entries added/updated (for diagnostics).
    pub fn scan(&mut self, root: &Path) -> Result<usize, LibraryError> {
        if !root.is_dir() {
            return Err(LibraryError::Io {
                path: root.to_path_buf(),
                source: io::Error::new(io::ErrorKind::NotFound, "library root is not a directory"),
            });
        }

        // Build a quick path → cache-index lookup so we can reuse
        // unchanged entries without re-parsing.
        let mut by_path: std::collections::HashMap<PathBuf, usize> =
            std::collections::HashMap::new();
        for (i, e) in self.entries.iter().enumerate() {
            by_path.insert(e.path.clone(), i);
        }

        let mut fresh: Vec<LibraryEntry> = Vec::new();
        let mut changed = 0usize;
        for path in walk(root) {
            let Some(kind) = classify(&path) else {
                continue;
            };
            let meta = match fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let size = meta.len();

            // Reuse cached entry if mtime hasn't moved.
            if let Some(&idx) = by_path.get(&path) {
                let cached = &self.entries[idx];
                if cached.mtime == mtime && cached.size == size {
                    fresh.push(cached.clone());
                    continue;
                }
            }

            let parsed = parse_entry(&path, kind, size, mtime);
            changed += 1;
            fresh.push(parsed);
        }

        self.entries = fresh;
        self.version = LIBRARY_VERSION;
        Ok(changed)
    }

    /// Walk multiple roots and union their scan results into a
    /// single library. Roots that don't exist are silently skipped
    /// (common case: the SDK-examples path only exists on developer
    /// machines with the nightly toolchain; end users won't have
    /// it). Roots that fail with a real error abort the whole
    /// scan and surface the error.
    ///
    /// Internally it does what `scan` does but across every
    /// provided root, using a single `fresh` accumulator so the
    /// final `self.entries` is the union. Useful for the
    /// frontend's "scan retail games + SDK examples together" case.
    pub fn scan_roots(&mut self, roots: &[&Path]) -> Result<usize, LibraryError> {
        // Reuse the existing cache map so mtime-match still short-
        // circuits re-parses across ALL roots.
        let mut by_path: std::collections::HashMap<PathBuf, usize> =
            std::collections::HashMap::new();
        for (i, e) in self.entries.iter().enumerate() {
            by_path.insert(e.path.clone(), i);
        }

        let mut fresh: Vec<LibraryEntry> = Vec::new();
        let mut changed = 0usize;

        for root in roots {
            if !root.exists() {
                // Missing roots are a normal condition (end-user
                // install without SDK builds). Silently skip.
                continue;
            }
            if !root.is_dir() {
                return Err(LibraryError::Io {
                    path: root.to_path_buf(),
                    source: io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "library root exists but is not a directory",
                    ),
                });
            }

            for path in walk(root) {
                let Some(kind) = classify(&path) else {
                    continue;
                };
                let meta = match fs::metadata(&path) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let size = meta.len();

                if let Some(&idx) = by_path.get(&path) {
                    let cached = &self.entries[idx];
                    if cached.mtime == mtime && cached.size == size {
                        fresh.push(cached.clone());
                        continue;
                    }
                }

                let parsed = parse_entry(&path, kind, size, mtime);
                changed += 1;
                fresh.push(parsed);
            }
        }

        self.entries = fresh;
        self.version = LIBRARY_VERSION;
        Ok(changed)
    }
}

/// Classify a file by extension only. Returns `None` for files we
/// don't care about (images, archives, etc). Keeps `scan()` cheap --
/// we only hit the disk to parse files we'll actually show.
fn classify(path: &Path) -> Option<GameKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "bin" => Some(GameKind::DiscBin),
        "iso" => Some(GameKind::DiscIso),
        "cue" => Some(GameKind::DiscCue),
        "ccd" => Some(GameKind::DiscCcd),
        "exe" => Some(GameKind::Exe),
        _ => None,
    }
}

/// Directory names the recursive walker skips. Primarily cargo's
/// per-target build-tree siblings -- if the user points a library
/// scanner at a cargo target-dir (the SDK-examples dir is exactly
/// this case), we'd otherwise surface every intermediate
/// `hello_tri-<hash>.exe` living under `deps/` as a separate
/// library entry. Names match cargo's layout; `.fingerprint` +
/// hidden dirs are caught by the leading-dot filter in `walk`.
///
/// Also kept short because a false positive (user has a folder
/// legitimately named `deps/`) is strictly worse than scanning
/// through it -- keep the list tight and explainable.
const SKIP_DIRS: &[&str] = &["deps", "incremental", "build"];

/// Recursive directory walk. Returns a flat list of file paths --
/// a full-blown `WalkDir` dep feels over-engineered for a few
/// dozen lines of plain `read_dir`. Skips directories in
/// [`SKIP_DIRS`] and anything starting with `.`, so cargo's
/// per-target build siblings don't show up as library entries
/// when the scanner is pointed at an SDK-build output tree.
fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                // Skip cargo's build-artifact siblings (`deps/`,
                // `incremental/`, `build/`) and any hidden dir
                // (`.fingerprint/`, dotfiles). Using `file_name()`
                // rather than full-path matching means a user's
                // real `deps/` folder anywhere in the walk is also
                // skipped -- an acceptable trade for keeping the
                // scanner noise-free when rooted at a cargo
                // target-dir like `build/examples/.../release/`.
                let skip = p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with('.') || SKIP_DIRS.contains(&n))
                    .unwrap_or(false);
                if !skip {
                    stack.push(p);
                }
            } else if p.is_file() {
                out.push(p);
            }
        }
    }
    out
}

/// Parse one file into a `LibraryEntry`. Uses cheap heuristics --
/// read one or two well-known sectors and the file stem. Never
/// loads the whole file (discs reach 600 MiB; scanning
/// wouldn't finish).
fn parse_entry(path: &Path, kind: GameKind, size: u64, mtime: u64) -> LibraryEntry {
    let fallback_title = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("<unknown>")
        .to_string();

    match kind {
        GameKind::DiscBin => parse_bin(path, size, mtime, &fallback_title),
        GameKind::DiscIso => LibraryEntry {
            id: SectorReader::open(path)
                .and_then(|reader| disc_identity_id(&reader))
                .unwrap_or_else(|| raw_image_id(path, size, &fallback_title)),
            path: path.to_path_buf(),
            kind,
            title: fallback_title,
            region: Region::Unknown,
            size,
            mtime,
            // ISO parsing shares the PVD path with BIN once we
            // handle 2048-byte sectors -- just not today.
            diagnostic: Some("ISO sector-size parsing not yet implemented".into()),
        },
        GameKind::DiscCue => parse_cue(path, size, mtime, &fallback_title),
        GameKind::DiscCcd => parse_ccd(path, size, mtime, &fallback_title),
        GameKind::Exe => LibraryEntry {
            id: exe_fingerprint(path, &fallback_title),
            path: path.to_path_buf(),
            kind,
            title: fallback_title,
            region: Region::Unknown,
            size,
            mtime,
            diagnostic: None,
        },
        GameKind::Unknown => LibraryEntry {
            id: fingerprint(&[fallback_title.as_bytes()]),
            path: path.to_path_buf(),
            kind,
            title: fallback_title,
            region: Region::Unknown,
            size,
            mtime,
            diagnostic: Some("unknown file kind".into()),
        },
    }
}

/// Sector-addressed reader over a disc image file, 2352-byte raw or
/// 2048-byte cooked, plain or ECM-packed. Each read touches one sector, so
/// the identity probes below cost a few KiB of IO however big the image is.
struct SectorReader {
    image: crate::disc_image::SharedImage,
    stride: u64,
    user_offset: u64,
}

impl SectorReader {
    fn open(path: &Path) -> Option<Self> {
        use psx_iso::{SECTOR_BYTES, SECTOR_USER_DATA_BYTES, SECTOR_USER_DATA_OFFSET};
        let image = crate::disc_image::open_image(path).ok()?;
        let len = image.len();
        let (stride, user_offset) = if len != 0 && len % SECTOR_BYTES as u64 == 0 {
            (SECTOR_BYTES as u64, SECTOR_USER_DATA_OFFSET as u64)
        } else if len != 0 && len % SECTOR_USER_DATA_BYTES as u64 == 0 {
            (SECTOR_USER_DATA_BYTES as u64, 0)
        } else {
            // Neither shape: read it as raw sectors, as before.
            (SECTOR_BYTES as u64, SECTOR_USER_DATA_OFFSET as u64)
        };
        Some(Self {
            image,
            stride,
            user_offset,
        })
    }

    fn sector_count(&self) -> u64 {
        self.image.len() / self.stride
    }

    /// The 2048-byte user-data payload of `lba`; `None` past the image.
    fn user(&self, lba: u64) -> Option<Vec<u8>> {
        let offset = lba
            .checked_mul(self.stride)?
            .checked_add(self.user_offset)?;
        let mut buf = vec![0u8; psx_iso::SECTOR_USER_DATA_BYTES];
        self.image.read_at(offset, &mut buf).then_some(buf)
    }

    /// `count` consecutive sectors from `lba`, cut to `len` bytes.
    fn extent(&self, lba: u64, count: u64, len: usize) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(len.min(1 << 20));
        for i in 0..count {
            out.extend_from_slice(&self.user(lba.checked_add(i)?)?);
        }
        out.truncate(len);
        Some(out)
    }
}

/// One ISO9660 directory record's identity-relevant fields.
struct IsoRecord {
    name: String,
    lba: u64,
    size: u64,
    is_dir: bool,
}

fn le_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// Parse the records of a directory extent. Stops at the first malformed
/// record rather than guessing.
fn iso_records(dir: &[u8]) -> Vec<IsoRecord> {
    let sector = psx_iso::SECTOR_USER_DATA_BYTES;
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < dir.len() {
        let len = dir[at] as usize;
        if len == 0 {
            // Records never straddle sectors: padding runs to the next one.
            at = (at / sector + 1) * sector;
            continue;
        }
        let Some(rec) = dir.get(at..at + len) else {
            break;
        };
        let name_len = rec.get(32).copied().unwrap_or(0) as usize;
        let (Some(name), Some(lba), Some(size)) =
            (rec.get(33..33 + name_len), le_u32(rec, 2), le_u32(rec, 10))
        else {
            break;
        };
        out.push(IsoRecord {
            name: String::from_utf8_lossy(name).into_owned(),
            lba: lba as u64,
            size: size as u64,
            is_dir: rec.get(25).is_some_and(|flags| flags & 0x02 != 0),
        });
        at += len;
    }
    out
}

/// The boot executable name from SYSTEM.CNF text (`BOOT = cdrom:\SLUS_006.78;1`).
fn boot_exe_name(system_cnf: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(system_cnf);
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("BOOT") {
            continue;
        }
        let value = value.trim();
        let value = value
            .get(..6)
            .filter(|head| head.eq_ignore_ascii_case("cdrom:"))
            .map_or(value, |_| &value[6..]);
        let value = value.trim_start_matches(['\\', '/']);
        let value = value.split(';').next().unwrap_or(value).trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

/// Sectors of the boot executable folded into a disc's identity.
const IDENTITY_EXE_SECTORS: u64 = 256;

/// A disc's own identity, independent of where its file lives or what it
/// is called. Folds in the license sector, the ISO9660 volume (id, size),
/// the root directory listing, SYSTEM.CNF (the boot serial) and the first
/// [`IDENTITY_EXE_SECTORS`] sectors of the boot executable. That tells every
/// retail disc and revision apart for a few dozen KiB of IO. It is not a
/// hash of the whole track (a library of 650 MB images would take minutes to
/// index); two images that differ only past the first part of the boot
/// executable would share an identity.
///
/// `None` when the image has no ISO9660 volume descriptor (an audio track or
/// a capture), where the caller falls back to [`raw_image_id`].
fn disc_identity_id(reader: &SectorReader) -> Option<String> {
    let pvd = reader.user(16)?;
    if pvd[0] != 1 || &pvd[1..6] != b"CD001" {
        return None;
    }
    let license = reader.user(4);

    let mut hasher = psx_hw::hash::Fnv1a64::new();
    hasher.update(b"psoxide-disc-v2");
    if let Some(license) = &license {
        hasher.update(&license[..256]);
    }
    hasher.update(&pvd[40..72]);
    hasher.update(&pvd[80..88]);

    let root_lba = le_u32(&pvd, 158).unwrap_or(0) as u64;
    let root_size = le_u32(&pvd, 166).unwrap_or(0) as usize;
    let sector = psx_iso::SECTOR_USER_DATA_BYTES as u64;
    // A root directory is a handful of sectors; cap a corrupt size.
    let root_sectors = (root_size as u64).div_ceil(sector).min(32);
    let root = reader
        .extent(root_lba, root_sectors, root_size)
        .unwrap_or_default();
    hasher.update(&root);

    let records = iso_records(&root);
    let system_cnf = records
        .iter()
        .find(|r| !r.is_dir && r.name.to_ascii_uppercase().starts_with("SYSTEM.CNF"))
        .and_then(|r| reader.extent(r.lba, r.size.div_ceil(sector).min(2), r.size as usize))
        .unwrap_or_default();
    hasher.update(&system_cnf);

    if let Some(exe) = boot_exe_name(&system_cnf).and_then(|boot| {
        records.into_iter().find(|r| {
            !r.is_dir
                && r.name
                    .split(';')
                    .next()
                    .unwrap_or(&r.name)
                    .eq_ignore_ascii_case(&boot)
        })
    }) {
        let sectors = exe
            .size
            .div_ceil(sector)
            .min(IDENTITY_EXE_SECTORS)
            .min(reader.sector_count().saturating_sub(exe.lba));
        for i in 0..sectors {
            if let Some(data) = reader.user(exe.lba + i) {
                hasher.update(&data);
            }
        }
        hasher.update(&exe.size.to_le_bytes());
    }
    Some(format!("{:016x}", hasher.finish()))
}

/// Identity for an image with no ISO9660 volume: the file stem, its length
/// and its first 64 KiB. Audio tracks of a multi-BIN rip land here, and the
/// "(Track N)" in the stem keeps them apart.
fn raw_image_id(path: &Path, size: u64, fallback_title: &str) -> String {
    let mut head = vec![0u8; 64 * 1024];
    let filled = crate::disc_image::open_image(path)
        .ok()
        .map(|image| {
            let n = (head.len() as u64).min(image.len()) as usize;
            head.truncate(n);
            image.read_at(0, &mut head)
        })
        .unwrap_or(false);
    if !filled {
        head.clear();
    }
    fingerprint(&[
        fallback_title.as_bytes(),
        &size.to_le_bytes(),
        &head,
        b"raw",
    ])
}

/// Parse a BIN (or an ISO / ECM-packed image). Reads:
///
/// - LBA 4 user-data → PSX license text → region
/// - LBA 16 user-data → ISO9660 PVD → volume identifier → title
/// - the identity probe in [`disc_identity_id`] → stable ID
///
/// Total disk IO for one image is bounded (well under a megabyte)
/// regardless of its size.
fn parse_bin(path: &Path, size: u64, mtime: u64, fallback_title: &str) -> LibraryEntry {
    let reader = SectorReader::open(path);
    let license_user = reader.as_ref().and_then(|r| r.user(4));
    let pvd_user = reader.as_ref().and_then(|r| r.user(16));
    let region = license_user
        .as_deref()
        .map(region_from_license_text)
        .unwrap_or(Region::Unknown);
    let title = pvd_user
        .as_deref()
        .and_then(pvd_volume_identifier)
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| fallback_title.to_string());

    let id = reader
        .as_ref()
        .and_then(disc_identity_id)
        .unwrap_or_else(|| raw_image_id(path, size, fallback_title));

    LibraryEntry {
        id,
        path: path.to_path_buf(),
        kind: GameKind::DiscBin,
        title,
        region,
        size,
        mtime,
        diagnostic: None,
    }
}

/// The id [`parse_bin`] produced before disc identities: the license text
/// plus the PVD volume identifier. Many retail discs share both, so those ids
/// collide; they are kept only to find the per-game data saved under them.
fn legacy_bin_id(path: &Path, fallback_title: &str) -> String {
    use psx_iso::{SECTOR_BYTES, SECTOR_USER_DATA_BYTES, SECTOR_USER_DATA_OFFSET};
    let read_user = |lba: u64| -> Option<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = fs::File::open(path).ok()?;
        let byte_offset = lba
            .checked_mul(SECTOR_BYTES as u64)?
            .checked_add(SECTOR_USER_DATA_OFFSET as u64)?;
        f.seek(SeekFrom::Start(byte_offset)).ok()?;
        let mut buf = vec![0u8; SECTOR_USER_DATA_BYTES];
        f.read_exact(&mut buf).ok()?;
        Some(buf)
    };
    let license_user = read_user(4);
    let pvd_user = read_user(16);
    let mut parts: Vec<&[u8]> = Vec::new();
    if let Some(ref bytes) = license_user {
        parts.push(&bytes[..bytes.len().min(256)]);
    }
    if let Some(ref bytes) = pvd_user {
        parts.push(&bytes[40..bytes.len().min(72)]);
    }
    if parts.is_empty() {
        parts.push(fallback_title.as_bytes());
    }
    fingerprint(&parts)
}

/// The id this entry's per-game data was saved under before disc
/// identities, when that differs from [`LibraryEntry::id`]. `None` when the
/// id never changed (EXEs, ISOs, unreadable sheets) or nothing was ever
/// saved under a different one.
pub fn legacy_id(entry: &LibraryEntry) -> Option<String> {
    let fallback_title = entry
        .path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("<unknown>");
    let old = match entry.kind {
        GameKind::DiscBin => legacy_bin_id(&entry.path, fallback_title),
        GameKind::DiscCue => {
            let bin = primary_bin_from_cue(&entry.path)?;
            fingerprint(&[legacy_bin_id(&bin, fallback_title).as_bytes(), b"cue"])
        }
        GameKind::DiscCcd => {
            let img = ccd_decoded_img_path(&entry.path);
            if !img.exists() {
                return None;
            }
            fingerprint(&[legacy_bin_id(&img, fallback_title).as_bytes(), b"ccd"])
        }
        GameKind::DiscIso => fingerprint(&[fallback_title.as_bytes()]),
        GameKind::Exe | GameKind::Unknown => return None,
    };
    (old != entry.id).then_some(old)
}

/// Build a library entry for one file picked outside any scanned folder
/// ("Open disc..."). Never stored in the cache.
pub fn entry_for_path(path: &Path) -> Result<LibraryEntry, String> {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let name = path.to_string_lossy().to_ascii_lowercase();
    // A CloneCD `.img.ecm` is launched through the `.ccd` sheet beside it.
    let path = if name.ends_with(".img.ecm") {
        let ccd = path.with_extension("").with_extension("ccd");
        if ccd.is_file() {
            ccd
        } else {
            return Err(format!(
                "{}: no .ccd sheet beside this image",
                path.display()
            ));
        }
    } else {
        path
    };
    let kind = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("ecm") => GameKind::DiscBin,
        _ => classify(&path).ok_or_else(|| {
            format!(
                "{}: not a disc image (.cue .bin .iso .ccd .ecm) or .exe",
                path.display()
            )
        })?,
    };
    let meta = fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    Ok(parse_entry(&path, kind, meta.len(), mtime))
}

fn parse_cue(path: &Path, size: u64, mtime: u64, fallback_title: &str) -> LibraryEntry {
    let Some(bin_path) = primary_bin_from_cue(path) else {
        return LibraryEntry {
            id: fingerprint(&[fallback_title.as_bytes(), b"cue"]),
            path: path.to_path_buf(),
            kind: GameKind::DiscCue,
            title: fallback_title.to_string(),
            region: Region::Unknown,
            size,
            mtime,
            diagnostic: Some("could not resolve a data-track BIN from CUE".into()),
        };
    };

    let mut entry = parse_bin(&bin_path, size, mtime, fallback_title);
    entry.id = fingerprint(&[entry.id.as_bytes(), b"cue"]);
    entry.path = path.to_path_buf();
    entry.kind = GameKind::DiscCue;
    entry.title = fallback_title.to_string();
    entry.size = size;
    entry.mtime = mtime;
    entry.diagnostic = None;
    entry
}

fn parse_ccd(path: &Path, size: u64, mtime: u64, fallback_title: &str) -> LibraryEntry {
    let decoded_img = ccd_decoded_img_path(path);
    if decoded_img.exists() {
        let mut entry = parse_bin(&decoded_img, size, mtime, fallback_title);
        entry.id = fingerprint(&[entry.id.as_bytes(), b"ccd"]);
        entry.path = path.to_path_buf();
        entry.kind = GameKind::DiscCcd;
        entry.title = fallback_title.to_string();
        entry.size = size;
        entry.mtime = mtime;
        entry.diagnostic = None;
        return entry;
    }

    let ecm_img = ecm_sidecar_path(&decoded_img);
    let diagnostic = if ecm_img.exists() {
        Some("ECM-compressed CloneCD image; decoded in memory at launch".into())
    } else {
        Some(format!(
            "missing CloneCD image sidecar {}",
            decoded_img
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("<image>.img")
        ))
    };

    LibraryEntry {
        id: fingerprint(&[fallback_title.as_bytes(), b"ccd"]),
        path: path.to_path_buf(),
        kind: GameKind::DiscCcd,
        title: fallback_title.to_string(),
        region: Region::Unknown,
        size,
        mtime,
        diagnostic,
    }
}

fn ccd_decoded_img_path(ccd_path: &Path) -> PathBuf {
    ccd_path.with_extension("img")
}

fn ecm_sidecar_path(decoded_img_path: &Path) -> PathBuf {
    let mut path = decoded_img_path.as_os_str().to_os_string();
    path.push(".ecm");
    PathBuf::from(path)
}

/// Cheap region heuristic -- look for any of the three canonical
/// license strings anywhere in LBA 4's user data. The BIOS checks
/// for these too; if none match, we say `Unknown` rather than
/// guess.
fn region_from_license_text(bytes: &[u8]) -> Region {
    let as_str = String::from_utf8_lossy(bytes);
    if as_str.contains("Sony Computer Entertainment Amer") {
        Region::NtscU
    } else if as_str.contains("Sony Computer Entertainment Euro")
        || as_str.contains("Sony Computer Entertainment Inc. for U.K.")
    {
        Region::Pal
    } else if as_str.contains("Sony Computer Entertainment Inc.") {
        Region::NtscJ
    } else {
        Region::Unknown
    }
}

/// Read the ISO9660 volume identifier out of a Primary Volume
/// Descriptor sector (LBA 16). The spec places it at offset 40 for
/// 32 ASCII bytes, space-padded. We trim trailing whitespace and
/// replace non-printables with `?` so we never return binary
/// garbage as a title.
fn pvd_volume_identifier(user_data: &[u8]) -> Option<String> {
    if user_data.len() < 72 {
        return None;
    }
    // Must be a Primary Volume Descriptor: type=1, magic "CD001",
    // version=1. Otherwise this isn't a valid ISO9660 PVD sector.
    if user_data[0] != 1 || &user_data[1..6] != b"CD001" || user_data[6] != 1 {
        return None;
    }
    let raw = &user_data[40..72];
    let cleaned: String = raw
        .iter()
        .map(|&b| {
            if (0x20..=0x7E).contains(&b) {
                b as char
            } else {
                ' '
            }
        })
        .collect();
    Some(cleaned.trim().to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CueTrackSpec {
    number: u8,
    track_type: psx_iso::TrackType,
    path: PathBuf,
    /// Pregap sectors in disc space before INDEX 01.
    pregap: u32,
    /// Pregap sectors physically present at the start of the track file.
    file_pregap: u32,
    /// File-relative sector where this track's physical data begins.
    file_start_sector: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CcdToc {
    tracks: Vec<CcdTrackSpec>,
    leadout_lba: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CcdTrackSpec {
    number: u8,
    track_type: psx_iso::TrackType,
    start_lba: u32,
    /// `INDEX 0` from the track's `[TRACK n]` section: where its pregap
    /// starts, when it has one in the image.
    index0_lba: Option<u32>,
}

#[derive(Default)]
struct CcdEntry {
    point: Option<i32>,
    control: Option<i32>,
    plba: Option<i32>,
}

fn starts_with_keyword(line: &str, keyword: &str) -> bool {
    let bytes = line.as_bytes();
    bytes.len() >= keyword.len() && bytes[..keyword.len()].eq_ignore_ascii_case(keyword.as_bytes())
}

fn parse_cue_filename(rest: &str) -> Option<&str> {
    if let Some(rest) = rest.strip_prefix('"') {
        let end = rest.find('"')?;
        Some(&rest[..end])
    } else {
        rest.split_whitespace().next()
    }
}

fn parse_cue_msf(s: &str) -> u32 {
    let mut parts = s.split(':');
    let m = parts
        .next()
        .and_then(|p| p.parse::<u32>().ok())
        .unwrap_or(0);
    let s = parts
        .next()
        .and_then(|p| p.parse::<u32>().ok())
        .unwrap_or(0);
    let f = parts
        .next()
        .and_then(|p| p.parse::<u32>().ok())
        .unwrap_or(0);
    if parts.next().is_some() {
        return 0;
    }
    // The sheet is untrusted: saturate rather than overflow on absurd fields.
    m.saturating_mul(60 * 75)
        .saturating_add(s.saturating_mul(75))
        .saturating_add(f)
}

fn parse_ccd_int(s: &str) -> Option<i32> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        i32::from_str_radix(hex, 16).ok()
    } else {
        s.parse().ok()
    }
}

fn parse_ccd_toc(ccd_path: &Path) -> Result<CcdToc, String> {
    let contents =
        fs::read_to_string(ccd_path).map_err(|e| format!("{}: {e}", ccd_path.display()))?;
    let mut tracks: Vec<CcdTrackSpec> = Vec::new();
    let mut leadout_lba = None;
    let mut current: Option<CcdEntry> = None;
    // `[TRACK n]` sections: track number and its INDEX 0.
    let mut index0: Vec<(u8, u32)> = Vec::new();
    let mut section_track: Option<u8> = None;

    let flush_entry =
        |entry: CcdEntry, tracks: &mut Vec<CcdTrackSpec>, leadout_lba: &mut Option<u32>| {
            let Some(point) = entry.point else { return };
            let Some(plba) = entry.plba else { return };
            if point == 0xA2 {
                if plba >= 0 {
                    *leadout_lba = Some(plba as u32);
                }
                return;
            }
            let track_number = if (0x01..=0x99).contains(&point) {
                psx_iso::bcd_to_bin(point as u8)
            } else {
                0xFF
            };
            if track_number == 0xFF || track_number == 0 || plba < 0 {
                return;
            }
            let control = entry.control.unwrap_or(0);
            let track_type = if control & 0x04 != 0 {
                psx_iso::TrackType::Data
            } else {
                psx_iso::TrackType::Audio
            };
            tracks.push(CcdTrackSpec {
                number: track_number,
                track_type,
                start_lba: plba as u32,
                index0_lba: None,
            });
        };

    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            if let Some(entry) = current.take() {
                flush_entry(entry, &mut tracks, &mut leadout_lba);
            }
            section_track = trimmed
                .strip_prefix("[TRACK ")
                .and_then(|rest| rest.strip_suffix(']'))
                .and_then(|n| n.trim().parse().ok());
            if trimmed.starts_with("[Entry ") {
                current = Some(CcdEntry::default());
            }
            continue;
        }
        if let Some(number) = section_track {
            if let Some((key, value)) = trimmed.split_once('=') {
                if key.trim().eq_ignore_ascii_case("INDEX 0") {
                    if let Some(lba) = parse_ccd_int(value).filter(|lba| *lba >= 0) {
                        index0.push((number, lba as u32));
                    }
                }
            }
            continue;
        }
        let Some(entry) = current.as_mut() else {
            continue;
        };
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        match key.trim().to_ascii_lowercase().as_str() {
            "point" => entry.point = parse_ccd_int(value),
            "control" => entry.control = parse_ccd_int(value),
            "plba" => entry.plba = parse_ccd_int(value),
            _ => {}
        }
    }
    if let Some(entry) = current {
        flush_entry(entry, &mut tracks, &mut leadout_lba);
    }

    tracks.sort_by_key(|track| track.number);
    tracks.dedup_by_key(|track| track.number);
    for (number, lba) in index0 {
        if let Some(track) = tracks.iter_mut().find(|t| t.number == number) {
            track.index0_lba = Some(lba).filter(|lba| *lba < track.start_lba);
        }
    }
    if tracks.is_empty() {
        Err(format!("{} contains no track entries", ccd_path.display()))
    } else {
        Ok(CcdToc {
            tracks,
            leadout_lba,
        })
    }
}

fn detect_track1_embedded_pregap(bytes: &[u8]) -> u32 {
    if bytes.len() < psx_iso::SECTOR_BYTES {
        return 0;
    }
    let sector = &bytes[..psx_iso::SECTOR_BYTES];
    if sector[0] != 0x00 || sector[11] != 0x00 || sector[1..11] != [0xFF; 10] {
        return 0;
    }
    let m = psx_iso::bcd_to_bin(sector[12]);
    let s = psx_iso::bcd_to_bin(sector[13]);
    let f = psx_iso::bcd_to_bin(sector[14]);
    if [m, s, f].contains(&0xFF) {
        return 0;
    }
    let abs_frame = (m as u32) * 60 * 75 + (s as u32) * 75 + (f as u32);
    150u32.saturating_sub(abs_frame)
}

fn parse_cue_tracks(cue_path: &Path) -> Result<Vec<CueTrackSpec>, String> {
    let contents =
        fs::read_to_string(cue_path).map_err(|e| format!("{}: {e}", cue_path.display()))?;
    let dir = cue_path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", cue_path.display()))?;
    parse_cue_tracks_str(&contents, dir).map_err(|e| format!("{}: {e}", cue_path.display()))
}

/// The pure half of [`parse_cue_tracks`]: no filesystem, so the web build can
/// feed it a fetched CUE sheet. `dir` is only joined onto FILE names.
fn parse_cue_tracks_str(contents: &str, dir: &Path) -> Result<Vec<CueTrackSpec>, String> {
    let mut tracks: Vec<CueTrackSpec> = Vec::new();
    let mut current_file: Option<PathBuf> = None;
    let mut current_track_num: Option<u8> = None;
    let mut current_track_type = psx_iso::TrackType::Data;
    let mut current_index0: Option<u32> = None;
    let mut cue_pregap = 0u32;

    for line in contents.lines() {
        let trimmed = line.trim();
        if starts_with_keyword(trimmed, "FILE") {
            let rest = trimmed.get(4..).unwrap_or("").trim_start();
            let Some(filename) = parse_cue_filename(rest) else {
                continue;
            };
            current_file = Some(dir.join(filename));
        } else if starts_with_keyword(trimmed, "TRACK") {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() >= 3 {
                current_track_num = parts[1].parse().ok();
                current_track_type = if parts[2].eq_ignore_ascii_case("AUDIO") {
                    psx_iso::TrackType::Audio
                } else {
                    psx_iso::TrackType::Data
                };
                current_index0 = None;
                cue_pregap = 0;
            }
        } else if starts_with_keyword(trimmed, "PREGAP") {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if let Some(msf) = parts.get(1) {
                cue_pregap = parse_cue_msf(msf);
            }
        } else if starts_with_keyword(trimmed, "INDEX 00") {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if let Some(msf) = parts.get(2) {
                current_index0 = Some(parse_cue_msf(msf));
            }
        } else if starts_with_keyword(trimmed, "INDEX 01") {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            let file_index1_sector = parts.get(2).map(|msf| parse_cue_msf(msf)).unwrap_or(0);
            let Some(path) = current_file.clone() else {
                continue;
            };
            let Some(number) = current_track_num else {
                continue;
            };
            let new_file_for_track = match tracks.last() {
                Some(track) => track.path != path,
                None => true,
            };
            let file_start_sector = current_index0.unwrap_or(if new_file_for_track {
                0
            } else {
                file_index1_sector
            });
            let file_pregap = file_index1_sector.saturating_sub(file_start_sector);
            tracks.push(CueTrackSpec {
                number,
                track_type: current_track_type,
                path,
                pregap: cue_pregap.saturating_add(file_pregap),
                file_pregap,
                file_start_sector,
            });
            current_index0 = None;
            cue_pregap = 0;
        }
    }

    if tracks.is_empty() {
        Err("contains no INDEX 01 tracks".to_string())
    } else {
        Ok(tracks)
    }
}

/// Load a full multitrack disc model from a CUE sheet. Track timing
/// comes from the CUE; per-track bytes come from the referenced files.
///
/// The referenced files stay on disk and are read a sector at a time as the
/// drive asks for them (see [`crate::disc_image`]).
pub fn load_disc_from_cue(cue_path: &Path) -> Result<psx_iso::Disc, String> {
    let specs = parse_cue_tracks(cue_path)?;
    disc_from_cue_specs(&specs, &mut crate::disc_image::open_image)
}

/// Mount a raw single-file image (`.bin` / `.iso`, or an ECM-packed one)
/// read from disk on demand. An image shorter than one sector mounts as an
/// empty disc, as it did when images were read whole; callers that need a
/// sector check the length.
pub fn load_disc_from_bin(path: &Path) -> Result<psx_iso::Disc, String> {
    let image = crate::disc_image::open_image(path)?;
    let len = image.len();
    Ok(psx_iso::Disc::from_source(Box::new(
        crate::disc_image::ImageSlice::new(image, 0, len),
    )))
}

/// Build a disc from a CUE sheet already in memory, reading the referenced
/// files through `read_file`. This is the web build's path: the CUE and BIN
/// arrive over HTTP, so there is no filesystem to consult. The track/LBA math
/// is exactly [`load_disc_from_cue`]'s -- both funnel through
/// [`disc_from_cue_specs`], so the two targets cannot drift.
pub fn disc_from_cue_str(
    contents: &str,
    read_file: &mut dyn FnMut(&Path) -> Result<Vec<u8>, String>,
) -> Result<psx_iso::Disc, String> {
    let specs = parse_cue_tracks_str(contents, Path::new(""))?;
    disc_from_cue_specs(&specs, &mut |path| {
        let image: crate::disc_image::SharedImage = std::sync::Arc::new(read_file(path)?);
        Ok(image)
    })
}

/// Build a disc from a CUE sheet plus one byte buffer per track, each
/// holding that track's file extent exactly as the single-BIN layout would
/// slice it. This is the streaming web build's path: the delivery splits the
/// pressed BIN at track boundaries, ships the pieces separately, and hands
/// them here (zero-filled placeholders for tracks still in flight -- silence
/// until the download lands). The LBA math is [`disc_from_cue_specs`]'s,
/// applied to pre-cut extents.
pub fn disc_from_cue_pieces(
    contents: &str,
    mut piece: impl FnMut(u8) -> Result<Vec<u8>, String>,
) -> Result<psx_iso::Disc, String> {
    let specs = parse_cue_tracks_str(contents, Path::new(""))?;
    let mut tracks = Vec::with_capacity(specs.len());
    for spec in &specs {
        let track_bytes = piece(spec.number)?;
        if !track_bytes.len().is_multiple_of(psx_iso::SECTOR_BYTES) || track_bytes.is_empty() {
            return Err(format!(
                "track {}: piece is not a whole number of raw sectors",
                spec.number
            ));
        }
        let mut file_pregap = spec.file_pregap;
        let mut pregap = spec.pregap;
        if spec.number == 1 && file_pregap == 0 {
            file_pregap = detect_track1_embedded_pregap(&track_bytes);
            pregap = pregap.max(file_pregap);
        }
        let track_file_sectors = track_bytes.len() / psx_iso::SECTOR_BYTES;
        if file_pregap as usize >= track_file_sectors {
            return Err(format!("track {} has no INDEX 01 sectors", spec.number));
        }
        let sector_count = track_file_sectors.saturating_sub(file_pregap as usize) as u32;
        let start_lba = tracks
            .last()
            .map(|prev: &psx_iso::Track| {
                prev.start_lba
                    .saturating_add(prev.sector_count)
                    .saturating_add(pregap)
            })
            .unwrap_or(0);
        tracks.push(psx_iso::Track {
            number: spec.number,
            track_type: spec.track_type,
            start_lba,
            sector_count,
            pregap,
            file_pregap,
            source: Box::new(track_bytes),
        });
    }
    Ok(psx_iso::Disc::from_tracks(tracks))
}

/// Shared back half of the CUE loaders: cut each track's extent out of the
/// referenced files and lay the tracks onto the disc LBA line. Every track of
/// one file shares one opened image.
fn disc_from_cue_specs(
    specs: &[CueTrackSpec],
    open: &mut dyn FnMut(&Path) -> Result<crate::disc_image::SharedImage, String>,
) -> Result<psx_iso::Disc, String> {
    let mut images: HashMap<PathBuf, crate::disc_image::SharedImage> = HashMap::new();
    let mut tracks = Vec::with_capacity(specs.len());

    for (index, spec) in specs.iter().enumerate() {
        if !images.contains_key(&spec.path) {
            images.insert(spec.path.clone(), open(&spec.path)?);
        }
        let image = images.get(&spec.path).expect("opened cue file");
        let len = image.len();
        if !len.is_multiple_of(psx_iso::SECTOR_BYTES as u64) {
            return Err(format!(
                "{} is not a whole number of raw 2352-byte sectors",
                spec.path.display()
            ));
        }
        let file_sectors = len / psx_iso::SECTOR_BYTES as u64;
        if file_sectors == 0 {
            return Err(format!(
                "{} is too small to contain a raw PS1 sector",
                spec.path.display()
            ));
        }
        if u64::from(spec.file_start_sector) >= file_sectors {
            return Err(format!(
                "{} track {} points outside {} sectors",
                spec.path.display(),
                spec.number,
                file_sectors
            ));
        }

        let next_file_start_sector = specs
            .iter()
            .skip(index + 1)
            .find(|next| next.path == spec.path)
            .map(|next| u64::from(next.file_start_sector))
            .unwrap_or(file_sectors)
            .min(file_sectors);
        if next_file_start_sector <= u64::from(spec.file_start_sector) {
            return Err(format!(
                "{} track {} has an invalid CUE extent",
                spec.path.display(),
                spec.number
            ));
        }
        let sector = psx_iso::SECTOR_BYTES as u64;
        let start = u64::from(spec.file_start_sector) * sector;
        let source = crate::disc_image::ImageSlice::new(
            image.clone(),
            start,
            next_file_start_sector * sector - start,
        );

        let mut file_pregap = spec.file_pregap;
        let mut pregap = spec.pregap;
        if spec.number == 1 && file_pregap == 0 {
            let mut first = [0u8; psx_iso::SECTOR_BYTES];
            if source.read_at(0, &mut first) {
                file_pregap = detect_track1_embedded_pregap(&first);
            }
            pregap = pregap.max(file_pregap);
        }
        let track_file_sectors = source.len() / sector;
        if u64::from(file_pregap) >= track_file_sectors {
            return Err(format!(
                "{} track {} has no INDEX 01 sectors",
                spec.path.display(),
                spec.number
            ));
        }
        let sector_count =
            (track_file_sectors - u64::from(file_pregap)).min(u64::from(u32::MAX)) as u32;
        let start_lba = tracks
            .last()
            .map(|prev: &psx_iso::Track| {
                prev.start_lba
                    .saturating_add(prev.sector_count)
                    .saturating_add(pregap)
            })
            .unwrap_or(0);
        tracks.push(psx_iso::Track {
            number: spec.number,
            track_type: spec.track_type,
            start_lba,
            sector_count,
            pregap,
            file_pregap,
            source: Box::new(source),
        });
    }

    Ok(psx_iso::Disc::from_tracks(tracks))
}

/// Load a full disc model from a CloneCD `.ccd` sheet and sibling
/// `.img` image. If the `.img` is absent but `.img.ecm` exists, the ECM
/// container is decoded on demand (nothing is written next to the disc).
pub fn load_disc_from_ccd(ccd_path: &Path) -> Result<psx_iso::Disc, String> {
    let toc = parse_ccd_toc(ccd_path)?;
    let (img_path, image) = open_ccd_image(ccd_path)?;
    let sector = psx_iso::SECTOR_BYTES as u64;
    let image_sectors = image.len() / sector;
    if image_sectors == 0 {
        return Err(format!(
            "{} is too small to contain a raw PS1 sector",
            img_path.display()
        ));
    }
    if image.len() % sector != 0 {
        return Err(format!(
            "{} is not a whole number of raw 2352-byte sectors",
            img_path.display()
        ));
    }

    let mut tracks = Vec::with_capacity(toc.tracks.len());
    for (idx, spec) in toc.tracks.iter().enumerate() {
        // A pregap in the image (INDEX 0) belongs to its own track, so a
        // track's bytes run from its INDEX 0 to the next one's.
        let start = u64::from(spec.index0_lba.unwrap_or(spec.start_lba));
        let next_lba = toc
            .tracks
            .get(idx + 1)
            .map(|track| track.index0_lba.unwrap_or(track.start_lba))
            .or(toc.leadout_lba)
            .map(u64::from)
            .unwrap_or(image_sectors);
        let end = next_lba.min(image_sectors);
        if start >= image_sectors || end <= u64::from(spec.start_lba) {
            return Err(format!(
                "{} track {} points outside {} sectors",
                ccd_path.display(),
                spec.number,
                image_sectors
            ));
        }
        let pregap = spec.start_lba - start as u32;
        tracks.push(psx_iso::Track {
            number: spec.number,
            track_type: spec.track_type,
            start_lba: spec.start_lba,
            sector_count: (end - start) as u32 - pregap,
            pregap,
            file_pregap: pregap,
            source: Box::new(crate::disc_image::ImageSlice::new(
                image.clone(),
                start * sector,
                (end - start) * sector,
            )),
        });
    }

    Ok(psx_iso::Disc::from_tracks(tracks))
}

/// The raw image behind a `.ccd`: the `.img` when present, otherwise the
/// `.img.ecm` decoded on demand. Returns the path opened (for messages).
fn open_ccd_image(ccd_path: &Path) -> Result<(PathBuf, crate::disc_image::SharedImage), String> {
    let decoded_img = ccd_decoded_img_path(ccd_path);
    if decoded_img.exists() {
        let image = crate::disc_image::open_image(&decoded_img)?;
        return Ok((decoded_img, image));
    }
    let ecm_img = ecm_sidecar_path(&decoded_img);
    if !ecm_img.exists() {
        return Err(format!(
            "{} needs sibling {} or {}",
            ccd_path.display(),
            decoded_img.display(),
            ecm_img.display()
        ));
    }
    let image = crate::disc_image::open_ecm(&ecm_img)?;
    Ok((ecm_img, image))
}

/// The LibCrypt subchannel file for a disc sheet: `<stem>.sbi` next to it.
pub fn sbi_path_for(sheet: &Path) -> PathBuf {
    sheet.with_extension("sbi")
}

/// Sectors listed in an `.sbi` file (PSX LibCrypt subchannel patches), as
/// LBAs counted from 00:02:00. The file is `"SBI\0"` followed by records of
/// a BCD absolute MSF, a type byte and the replacement Q data: 10 bytes for
/// type 1, 3 bytes (a relative or absolute MSF) for types 2 and 3. Every
/// listed sector has a deliberately bad Q CRC, so only the positions matter.
pub fn parse_sbi(bytes: &[u8]) -> Result<Vec<u32>, String> {
    if bytes.len() < 4 || &bytes[..4] != b"SBI\0" {
        return Err("not an SBI file (missing SBI header)".into());
    }
    let bcd = |b: u8| -> Result<u32, String> {
        let (hi, lo) = (u32::from(b >> 4), u32::from(b & 0x0F));
        if hi > 9 || lo > 9 {
            return Err(format!("SBI position byte {b:#04x} is not BCD"));
        }
        Ok(hi * 10 + lo)
    };
    let mut lbas = Vec::new();
    let mut at = 4;
    while at < bytes.len() {
        let record = bytes
            .get(at..at + 4)
            .ok_or_else(|| format!("SBI record at byte {at} is truncated"))?;
        let frames = (bcd(record[0])? * 60 + bcd(record[1])?) * 75 + bcd(record[2])?;
        let payload = match record[3] {
            1 => 10,
            2 | 3 => 3,
            other => return Err(format!("SBI record at byte {at} has unknown type {other}")),
        };
        if bytes.len() < at + 4 + payload {
            return Err(format!("SBI record at byte {at} is truncated"));
        }
        lbas.push(
            frames
                .checked_sub(150)
                .ok_or_else(|| format!("SBI record at byte {at} lies before 00:02:00"))?,
        );
        at += 4 + payload;
    }
    Ok(lbas)
}

/// Read the `.sbi` next to `sheet`, if there is one.
pub fn load_sbi_for(sheet: &Path) -> Result<Option<Vec<u32>>, String> {
    let path = sbi_path_for(sheet);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_sbi(&bytes)
        .map(Some)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Parse a CUE sheet to find the path of its first data track's BIN.
/// Used to collapse CUE + BIN pairs in the UI and to inherit region
/// metadata from the bootable track during library scans.
pub fn primary_bin_from_cue(cue_path: &Path) -> Option<PathBuf> {
    let tracks = parse_cue_tracks(cue_path).ok()?;
    tracks
        .into_iter()
        .find(|track| track.track_type == psx_iso::TrackType::Data)
        .map(|track| track.path)
}

/// Parse a CUE sheet and return every referenced image file path once,
/// preserving sheet order.
pub fn cue_referenced_files(cue_path: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    for track in parse_cue_tracks(cue_path)? {
        if !files.iter().any(|path| path == &track.path) {
            files.push(track.path);
        }
    }
    Ok(files)
}

/// FNV-1a-64 over any number of input slices, rendered as a
/// 16-hex-char string. Same algorithm the parity-cache uses -- no
/// adversarial input, just a stable fingerprint.
fn fingerprint(parts: &[&[u8]]) -> String {
    let mut h = psx_hw::hash::Fnv1a64::new();
    for p in parts {
        h.update(p);
    }
    format!("{:016x}", h.finish())
}

fn exe_fingerprint(path: &Path, fallback_title: &str) -> String {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let path_label = canonical.to_string_lossy();
    fingerprint(&[path_label.as_bytes(), fallback_title.as_bytes(), b"exe"])
}

/// Scratch helper used in tests: build a synthetic 2352-byte
/// sector with the given user-data payload at the standard
/// Mode-2-Form-1 offset. Real BIN parsing uses the same layout.
#[cfg(test)]
fn synth_sector(user_data: &[u8]) -> Vec<u8> {
    use psx_iso::{SECTOR_BYTES, SECTOR_USER_DATA_BYTES, SECTOR_USER_DATA_OFFSET};
    let mut out = vec![0u8; SECTOR_BYTES];
    let n = user_data.len().min(SECTOR_USER_DATA_BYTES);
    out[SECTOR_USER_DATA_OFFSET..SECTOR_USER_DATA_OFFSET + n].copy_from_slice(&user_data[..n]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_test_disc(path: &Path, title: &str, region_msg: &[u8]) {
        let mut bin = vec![0u8; psx_iso::SECTOR_BYTES * 20];

        let mut license = [0u8; psx_iso::SECTOR_USER_DATA_BYTES];
        license[..region_msg.len()].copy_from_slice(region_msg);
        let sec4 = synth_sector(&license);
        let off4 = 4 * psx_iso::SECTOR_BYTES;
        bin[off4..off4 + psx_iso::SECTOR_BYTES].copy_from_slice(&sec4);

        let mut pvd = [0u8; psx_iso::SECTOR_USER_DATA_BYTES];
        pvd[0] = 1;
        pvd[1..6].copy_from_slice(b"CD001");
        pvd[6] = 1;
        pvd[8..19].copy_from_slice(b"PLAYSTATION");
        let title_bytes = title.as_bytes();
        pvd[40..40 + title_bytes.len().min(32)]
            .copy_from_slice(&title_bytes[..title_bytes.len().min(32)]);
        let sec16 = synth_sector(&pvd);
        let off16 = 16 * psx_iso::SECTOR_BYTES;
        bin[off16..off16 + psx_iso::SECTOR_BYTES].copy_from_slice(&sec16);

        std::fs::write(path, &bin).unwrap();
    }

    #[test]
    fn round_trip_empty_library() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("library.ron");
        let lib = Library::default();
        lib.save(&path).unwrap();
        let loaded = Library::load(&path).unwrap();
        assert_eq!(loaded.entries.len(), 0);
    }

    #[test]
    fn missing_file_loads_as_empty() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("no.ron");
        let lib = Library::load_or_empty(&missing);
        assert_eq!(lib.entries.len(), 0);
    }

    #[test]
    fn corrupt_cache_falls_back_to_empty_via_load_or_empty() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.ron");
        std::fs::write(&path, "{not ron").unwrap();
        let lib = Library::load_or_empty(&path);
        assert_eq!(lib.entries.len(), 0);
    }

    #[test]
    fn classify_matches_known_extensions() {
        assert_eq!(classify(Path::new("g.bin")), Some(GameKind::DiscBin));
        assert_eq!(classify(Path::new("G.BIN")), Some(GameKind::DiscBin));
        assert_eq!(classify(Path::new("g.iso")), Some(GameKind::DiscIso));
        assert_eq!(classify(Path::new("g.cue")), Some(GameKind::DiscCue));
        assert_eq!(classify(Path::new("g.ccd")), Some(GameKind::DiscCcd));
        assert_eq!(classify(Path::new("hello.exe")), Some(GameKind::Exe));
        assert_eq!(classify(Path::new("notes.txt")), None);
        assert_eq!(classify(Path::new("NOEXT")), None);
    }

    #[test]
    fn region_from_license_text_recognises_sce_variants() {
        let us = b"   Licensed  by   Sony Computer Entertainment America ";
        let eu = b"Licensed by Sony Computer Entertainment Europe";
        let jp = b"Licensed by Sony Computer Entertainment Inc.";
        let unknown = b"Some other text";
        assert_eq!(region_from_license_text(us), Region::NtscU);
        assert_eq!(region_from_license_text(eu), Region::Pal);
        assert_eq!(region_from_license_text(jp), Region::NtscJ);
        assert_eq!(region_from_license_text(unknown), Region::Unknown);
    }

    #[test]
    fn pvd_volume_identifier_trims_padding() {
        // Build a synthetic PVD: type=1, "CD001", ver=1, then 32
        // bytes of system identifier followed by the 32-byte volume
        // identifier at offset 40.
        let mut pvd = vec![0u8; 2048];
        pvd[0] = 1;
        pvd[1..6].copy_from_slice(b"CD001");
        pvd[6] = 1;
        pvd[8..19].copy_from_slice(b"PLAYSTATION");
        pvd[40..72].copy_from_slice(b"CRASH_BANDICOOT                 ");
        assert_eq!(
            pvd_volume_identifier(&pvd),
            Some("CRASH_BANDICOOT".to_string())
        );
    }

    #[test]
    fn pvd_volume_identifier_rejects_non_pvd_sector() {
        // Zero sector isn't a PVD -- type byte is 0.
        let zeros = vec![0u8; 2048];
        assert_eq!(pvd_volume_identifier(&zeros), None);
    }

    #[test]
    fn scan_finds_exe_files_and_reuses_cache_on_mtime_match() {
        let tmp = TempDir::new().unwrap();
        // Create two fake EXEs.
        std::fs::write(tmp.path().join("a.exe"), b"fake exe 1").unwrap();
        std::fs::write(tmp.path().join("b.exe"), b"fake exe 2").unwrap();
        let mut lib = Library::default();
        let changed = lib.scan(tmp.path()).unwrap();
        assert_eq!(changed, 2);
        assert_eq!(lib.entries.len(), 2);

        // Second scan -- no files changed, so nothing should be
        // re-parsed.
        let changed2 = lib.scan(tmp.path()).unwrap();
        assert_eq!(changed2, 0);
        assert_eq!(lib.entries.len(), 2);
    }

    #[test]
    fn exe_ids_include_path_to_disambiguate_project_builds() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a").join("baked");
        let b = tmp.path().join("b").join("baked");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("untitled_ps1_project.exe"), b"fake exe 1").unwrap();
        std::fs::write(b.join("untitled_ps1_project.exe"), b"fake exe 2").unwrap();

        let mut lib = Library::default();
        lib.scan(tmp.path()).unwrap();
        assert_eq!(lib.entries.len(), 2);
        assert_ne!(lib.entries[0].id, lib.entries[1].id);
    }

    #[test]
    fn scan_prunes_entries_whose_files_are_gone() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a.exe");
        let b = tmp.path().join("b.exe");
        std::fs::write(&a, b"1").unwrap();
        std::fs::write(&b, b"2").unwrap();
        let mut lib = Library::default();
        lib.scan(tmp.path()).unwrap();
        assert_eq!(lib.entries.len(), 2);
        std::fs::remove_file(&b).unwrap();
        lib.scan(tmp.path()).unwrap();
        assert_eq!(lib.entries.len(), 1);
        assert_eq!(lib.entries[0].path, a);
    }

    #[test]
    fn scan_skips_cargo_build_artifact_dirs() {
        // Regression: when the library scanner is pointed at an SDK
        // build-output tree (`build/examples/mipsel-sony-psx/release/`),
        // cargo's `deps/` subdirectory contains intermediate
        // `<crate>-<hash>.exe` artifacts. Those used to surface as
        // separate library entries -- the user saw both
        // `hello-tri` and `hello_tri-<hash>` in the Examples column.
        //
        // Fix is in `walk()`: skip `deps/`, `incremental/`, `build/`,
        // and any hidden dir. This test nails down the invariant.
        let tmp = TempDir::new().unwrap();
        // Main release output -- the file the user should see.
        std::fs::write(tmp.path().join("hello-tri.exe"), b"final").unwrap();
        // Cargo's intermediate layout next to it.
        let deps = tmp.path().join("deps");
        std::fs::create_dir_all(&deps).unwrap();
        std::fs::write(deps.join("hello_tri-0123456789abcdef.exe"), b"dep").unwrap();
        std::fs::write(deps.join("hello_tri-0123456789abcdef.d"), b"").unwrap();
        let incr = tmp.path().join("incremental");
        std::fs::create_dir_all(&incr).unwrap();
        std::fs::write(incr.join("hello_tri-abcd.exe"), b"inc").unwrap();
        let hidden = tmp.path().join(".fingerprint");
        std::fs::create_dir_all(&hidden).unwrap();
        std::fs::write(hidden.join("hello_tri-xxxx.exe"), b"fp").unwrap();
        let builddir = tmp.path().join("build");
        std::fs::create_dir_all(&builddir).unwrap();
        std::fs::write(builddir.join("some-artifact.exe"), b"b").unwrap();

        let mut lib = Library::default();
        lib.scan(tmp.path()).unwrap();
        assert_eq!(
            lib.entries.len(),
            1,
            "only the top-level hello-tri.exe should be surfaced; \
             got entries: {:?}",
            lib.entries.iter().map(|e| &e.path).collect::<Vec<_>>(),
        );
        assert!(lib.entries[0].path.file_name().and_then(|n| n.to_str()) == Some("hello-tri.exe"),);
    }

    #[test]
    fn scan_rejects_non_directory_root() {
        let tmp = TempDir::new().unwrap();
        let not_a_dir = tmp.path().join("file.txt");
        std::fs::write(&not_a_dir, b"").unwrap();
        let mut lib = Library::default();
        assert!(lib.scan(&not_a_dir).is_err());
    }

    fn put_sector(image: &mut [u8], lba: usize, user: &[u8]) {
        let off = lba * psx_iso::SECTOR_BYTES;
        image[off..off + psx_iso::SECTOR_BYTES].copy_from_slice(&synth_sector(user));
    }

    fn dir_record(name: &str, lba: u32, size: u32) -> Vec<u8> {
        let mut rec = vec![0u8; 33 + name.len() + usize::from(name.len().is_multiple_of(2))];
        rec[0] = rec.len() as u8;
        rec[2..6].copy_from_slice(&lba.to_le_bytes());
        rec[10..14].copy_from_slice(&size.to_le_bytes());
        rec[32] = name.len() as u8;
        rec[33..33 + name.len()].copy_from_slice(name.as_bytes());
        rec
    }

    /// A retail-shaped disc: license text, an ISO9660 volume with a root
    /// directory holding SYSTEM.CNF and the boot executable. Every disc made
    /// here has the same license text and the same volume identifier, which
    /// is exactly what made the old ids collide.
    fn write_retail_disc(path: &Path, serial: &str, exe_fill: u8) {
        let mut image = vec![0u8; psx_iso::SECTOR_BYTES * 40];
        let msg = b"          Licensed  by          Sony Computer Entertainment Amer  ica ";
        put_sector(&mut image, 4, msg);

        let mut pvd = [0u8; 2048];
        pvd[0] = 1;
        pvd[1..6].copy_from_slice(b"CD001");
        pvd[40..43].copy_from_slice(b"PSX");
        pvd[80..84].copy_from_slice(&40u32.to_le_bytes());
        pvd[158..162].copy_from_slice(&22u32.to_le_bytes());
        pvd[166..170].copy_from_slice(&2048u32.to_le_bytes());
        put_sector(&mut image, 16, &pvd);

        let cnf = format!("BOOT = cdrom:\\{serial};1\r\nTCB = 4\r\nEVENT = 10\r\n");
        let mut root = Vec::new();
        root.extend(dir_record("SYSTEM.CNF;1", 23, cnf.len() as u32));
        root.extend(dir_record(&format!("{serial};1"), 24, 4096));
        root.resize(2048, 0);
        put_sector(&mut image, 22, &root);
        put_sector(&mut image, 23, cnf.as_bytes());
        put_sector(&mut image, 24, &[exe_fill; 2048]);
        put_sector(&mut image, 25, &[exe_fill; 2048]);
        std::fs::write(path, image).unwrap();
    }

    fn entry_of(path: &Path, kind: GameKind) -> LibraryEntry {
        let size = std::fs::metadata(path).unwrap().len();
        parse_entry(path, kind, size, 0)
    }

    #[test]
    fn discs_sharing_license_and_volume_id_get_distinct_ids() {
        let tmp = TempDir::new().unwrap();
        let (a, b, c) = (
            tmp.path().join("a.bin"),
            tmp.path().join("b.bin"),
            tmp.path().join("c.bin"),
        );
        write_retail_disc(&a, "SLUS_000.01", 0x11);
        write_retail_disc(&b, "SLUS_000.02", 0x11);
        // Same serial, different executable body: another revision.
        write_retail_disc(&c, "SLUS_000.01", 0x22);
        let (ea, eb, ec) = (
            entry_of(&a, GameKind::DiscBin),
            entry_of(&b, GameKind::DiscBin),
            entry_of(&c, GameKind::DiscBin),
        );
        assert_ne!(ea.id, eb.id);
        assert_ne!(ea.id, ec.id);
        assert_ne!(eb.id, ec.id);
        // The old scheme could not tell any of them apart.
        let old = legacy_id(&ea).expect("id changed");
        assert_eq!(legacy_id(&eb), Some(old.clone()));
        assert_eq!(legacy_id(&ec), Some(old));
    }

    #[test]
    fn disc_id_follows_the_content_not_the_file() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a.bin");
        write_retail_disc(&a, "SLUS_000.01", 0x11);
        let renamed = tmp.path().join("Renamed (USA).bin");
        std::fs::copy(&a, &renamed).unwrap();
        let ea = entry_of(&a, GameKind::DiscBin);
        let er = entry_of(&renamed, GameKind::DiscBin);
        assert_eq!(ea.id, er.id);
        assert_eq!(ea.id.len(), 16);
    }

    #[test]
    fn cue_and_ccd_ids_stay_distinct_from_the_bin_and_map_back() {
        let tmp = TempDir::new().unwrap();
        let bin = tmp.path().join("g.bin");
        write_retail_disc(&bin, "SLUS_000.01", 0x11);
        let cue = tmp.path().join("g.cue");
        std::fs::write(
            &cue,
            "FILE \"g.bin\" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:00\n",
        )
        .unwrap();
        let bin_entry = entry_of(&bin, GameKind::DiscBin);
        let cue_entry = entry_of(&cue, GameKind::DiscCue);
        assert_ne!(bin_entry.id, cue_entry.id);
        let old_cue = legacy_id(&cue_entry).expect("cue id changed");
        assert_eq!(
            old_cue,
            fingerprint(&[legacy_bin_id(&bin, "g").as_bytes(), b"cue"])
        );
    }

    #[test]
    fn images_without_a_volume_get_ids_from_their_name_and_size() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("Game (Track 02).bin");
        let b = tmp.path().join("Game (Track 03).bin");
        std::fs::write(&a, vec![7u8; psx_iso::SECTOR_BYTES * 20]).unwrap();
        std::fs::write(&b, vec![7u8; psx_iso::SECTOR_BYTES * 20]).unwrap();
        let (ea, eb) = (
            entry_of(&a, GameKind::DiscBin),
            entry_of(&b, GameKind::DiscBin),
        );
        assert_ne!(ea.id, eb.id);
        // Two audio tracks used to share one id.
        assert_eq!(legacy_id(&ea), legacy_id(&eb));
        assert!(legacy_id(&ea).is_some());
    }

    #[test]
    fn boot_exe_name_reads_the_usual_system_cnf_spellings() {
        for (text, want) in [
            (
                "BOOT = cdrom:\\SLUS_006.78;1\r\nTCB = 4",
                Some("SLUS_006.78"),
            ),
            ("boot=cdrom:SCES_012.34;1", Some("SCES_012.34")),
            ("BOOT = cdrom:/SLPS_000.01;1", Some("SLPS_000.01")),
            ("TCB = 4\nEVENT = 10", None),
            ("", None),
        ] {
            assert_eq!(boot_exe_name(text.as_bytes()).as_deref(), want, "{text:?}");
        }
    }

    #[test]
    fn entry_for_path_builds_an_entry_outside_any_library() {
        let tmp = TempDir::new().unwrap();
        let bin = tmp.path().join("open.bin");
        write_retail_disc(&bin, "SLUS_000.01", 0x11);
        let entry = entry_for_path(&bin).unwrap();
        assert_eq!(entry.kind, GameKind::DiscBin);
        assert_eq!(entry.id, entry_of(&bin, GameKind::DiscBin).id);

        let exe = tmp.path().join("hello.EXE");
        std::fs::write(&exe, b"PS-X EXE").unwrap();
        assert_eq!(entry_for_path(&exe).unwrap().kind, GameKind::Exe);

        let text = tmp.path().join("notes.txt");
        std::fs::write(&text, b"x").unwrap();
        assert!(entry_for_path(&text).is_err());
        assert!(entry_for_path(&tmp.path().join("missing.cue")).is_err());
    }

    #[test]
    fn entry_for_path_launches_an_img_ecm_through_its_ccd_sheet() {
        let tmp = TempDir::new().unwrap();
        let ccd = tmp.path().join("Disc.ccd");
        std::fs::write(&ccd, b"[CloneCD]\nVersion=3\n").unwrap();
        let ecm = tmp.path().join("Disc.img.ecm");
        std::fs::write(&ecm, b"ecm").unwrap();
        let entry = entry_for_path(&ecm).unwrap();
        assert_eq!(entry.kind, GameKind::DiscCcd);
        assert_eq!(entry.path.file_name().unwrap(), "Disc.ccd");
        std::fs::remove_file(&ccd).unwrap();
        assert!(entry_for_path(&ecm).is_err());
    }

    #[test]
    fn rescan_picks_up_new_files_and_leaves_cached_ones_alone() {
        let tmp = TempDir::new().unwrap();
        let first = tmp.path().join("first.bin");
        write_retail_disc(&first, "SLUS_000.01", 0x11);
        let mut lib = Library::default();
        assert_eq!(lib.scan_roots(&[tmp.path()]).unwrap(), 1);
        // Nothing changed: nothing is parsed again.
        assert_eq!(lib.scan_roots(&[tmp.path()]).unwrap(), 0);
        let before = lib.clone();
        let second = tmp.path().join("second.bin");
        write_retail_disc(&second, "SLUS_000.02", 0x11);
        assert_eq!(lib.scan_roots(&[tmp.path()]).unwrap(), 1);
        assert_eq!(lib.entries.len(), 2);
        assert_ne!(lib, before);
        let ids: std::collections::HashSet<_> = lib.entries.iter().map(|e| e.id.clone()).collect();
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn parse_bin_extracts_region_and_title_from_synthetic_disc() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.bin");
        write_test_disc(
            &path,
            "TEST_TITLE",
            b"    Licensed  by   Sony Computer Entertainment America",
        );
        let e = parse_entry(
            &path,
            GameKind::DiscBin,
            (psx_iso::SECTOR_BYTES * 20) as u64,
            0,
        );
        assert_eq!(e.title, "TEST_TITLE");
        assert_eq!(e.region, Region::NtscU);
        assert_eq!(e.kind, GameKind::DiscBin);
        assert_eq!(e.id.len(), 16);
    }

    #[test]
    fn fingerprint_is_stable() {
        assert_eq!(fingerprint(&[b"hello"]), fingerprint(&[b"hello"]));
        assert_ne!(fingerprint(&[b"hello"]), fingerprint(&[b"world"]));
    }

    #[test]
    fn primary_bin_from_cue_extracts_first_file_line() {
        let tmp = TempDir::new().unwrap();
        let cue_path = tmp.path().join("game.cue");
        // Realistic multi-track PSX CUE -- the data track is track 1.
        std::fs::write(
            &cue_path,
            concat!(
                "FILE \"game (Track 01).bin\" BINARY\n",
                "  TRACK 01 MODE2/2352\n",
                "    INDEX 01 00:00:00\n",
                "FILE \"game (Track 02).bin\" BINARY\n",
                "  TRACK 02 AUDIO\n",
                "    INDEX 00 00:00:00\n",
            ),
        )
        .unwrap();
        let bin = primary_bin_from_cue(&cue_path).unwrap();
        assert_eq!(bin, tmp.path().join("game (Track 01).bin"));
    }

    #[test]
    fn cue_referenced_files_dedups_reused_images_in_sheet_order() {
        let tmp = TempDir::new().unwrap();
        let cue_path = tmp.path().join("game.cue");
        std::fs::write(
            &cue_path,
            concat!(
                "FILE \"game.bin\" BINARY\n",
                "  TRACK 01 MODE2/2352\n",
                "    INDEX 01 00:00:00\n",
                "  TRACK 02 AUDIO\n",
                "    INDEX 01 10:00:00\n",
                "FILE \"bonus.bin\" BINARY\n",
                "  TRACK 03 AUDIO\n",
                "    INDEX 01 00:00:00\n",
            ),
        )
        .unwrap();

        assert_eq!(
            cue_referenced_files(&cue_path).unwrap(),
            vec![tmp.path().join("game.bin"), tmp.path().join("bonus.bin")]
        );
    }

    #[test]
    fn primary_bin_from_cue_handles_lowercase_keyword() {
        let tmp = TempDir::new().unwrap();
        let cue_path = tmp.path().join("g.cue");
        std::fs::write(
            &cue_path,
            "file \"g.bin\" BINARY\n  track 01 mode2/2352\n    index 01 00:00:00\n",
        )
        .unwrap();
        assert_eq!(
            primary_bin_from_cue(&cue_path).unwrap(),
            tmp.path().join("g.bin")
        );
    }

    #[test]
    fn primary_bin_from_cue_returns_none_on_garbage() {
        let tmp = TempDir::new().unwrap();
        let cue_path = tmp.path().join("bad.cue");
        std::fs::write(&cue_path, "REM some comment with no FILE\n").unwrap();
        assert!(primary_bin_from_cue(&cue_path).is_none());
    }

    #[test]
    fn parse_entry_disc_cue_uses_data_track_metadata() {
        let tmp = TempDir::new().unwrap();
        let cue_path = tmp.path().join("Crash Test.cue");
        let track1_path = tmp.path().join("track1.bin");
        write_test_disc(
            &track1_path,
            "CRASH_TEST_DISC",
            b"    Licensed  by   Sony Computer Entertainment Europe",
        );
        std::fs::write(
            &cue_path,
            concat!(
                "FILE \"track1.bin\" BINARY\n",
                "  TRACK 01 MODE2/2352\n",
                "    INDEX 01 00:00:00\n",
            ),
        )
        .unwrap();

        let entry = parse_entry(&cue_path, GameKind::DiscCue, 1234, 5678);
        assert_eq!(entry.kind, GameKind::DiscCue);
        assert_eq!(entry.title, "Crash Test");
        assert_eq!(entry.region, Region::Pal);
        assert_eq!(entry.path, cue_path);
        assert_eq!(entry.size, 1234);
        assert_eq!(entry.mtime, 5678);
        assert_eq!(entry.diagnostic, None);
    }

    #[test]
    fn parse_entry_disc_ccd_uses_decoded_img_metadata() {
        let tmp = TempDir::new().unwrap();
        let ccd_path = tmp.path().join("Tomb Test.ccd");
        let img_path = tmp.path().join("Tomb Test.img");
        write_test_disc(
            &img_path,
            "TOMB_TEST",
            b"    Licensed  by   Sony Computer Entertainment America",
        );
        std::fs::write(
            &ccd_path,
            concat!(
                "[Entry 0]\n",
                "Point=0x01\n",
                "Control=0x04\n",
                "PLBA=0\n",
                "[Entry 1]\n",
                "Point=0xa2\n",
                "Control=0x00\n",
                "PLBA=20\n",
            ),
        )
        .unwrap();

        let entry = parse_entry(&ccd_path, GameKind::DiscCcd, 1234, 5678);
        assert_eq!(entry.kind, GameKind::DiscCcd);
        assert_eq!(entry.title, "Tomb Test");
        assert_eq!(entry.region, Region::NtscU);
        assert_eq!(entry.path, ccd_path);
        assert_eq!(entry.size, 1234);
        assert_eq!(entry.mtime, 5678);
        assert_eq!(entry.diagnostic, None);
    }

    #[test]
    fn parse_entry_disc_ccd_reports_ecm_sidecar() {
        let tmp = TempDir::new().unwrap();
        let ccd_path = tmp.path().join("Compressed.ccd");
        std::fs::write(tmp.path().join("Compressed.img.ecm"), b"ecm").unwrap();
        std::fs::write(&ccd_path, "[Entry 0]\nPoint=0x01\nControl=0x04\nPLBA=0\n").unwrap();

        let entry = parse_entry(&ccd_path, GameKind::DiscCcd, 1234, 5678);
        assert_eq!(entry.kind, GameKind::DiscCcd);
        assert_eq!(entry.title, "Compressed");
        assert_eq!(entry.region, Region::Unknown);
        assert!(entry
            .diagnostic
            .as_deref()
            .is_some_and(|msg| msg.contains("ECM-compressed")));
    }

    #[test]
    fn load_disc_from_cue_positions_later_track_after_pregap() {
        let tmp = TempDir::new().unwrap();
        let cue_path = tmp.path().join("disc.cue");
        let track1_path = tmp.path().join("track1.bin");
        let track2_path = tmp.path().join("track2.bin");
        let mut track1 = vec![0u8; psx_iso::SECTOR_BYTES * 10];
        track1[12] = 0x00;
        track1[13] = 0x02;
        track1[14] = 0x00;
        std::fs::write(&track1_path, track1).unwrap();
        let mut track2 = vec![0u8; psx_iso::SECTOR_BYTES * 4];
        track2[0] = 0xAB;
        std::fs::write(&track2_path, track2).unwrap();
        std::fs::write(
            &cue_path,
            concat!(
                "FILE \"track1.bin\" BINARY\n",
                "  TRACK 01 MODE2/2352\n",
                "    INDEX 01 00:00:00\n",
                "FILE \"track2.bin\" BINARY\n",
                "  TRACK 02 AUDIO\n",
                "    PREGAP 00:00:02\n",
                "    INDEX 01 00:00:00\n",
            ),
        )
        .unwrap();

        let disc = load_disc_from_cue(&cue_path).unwrap();
        let pos = disc.track_position_for_lba(10).unwrap();
        assert_eq!(pos.track_number, 2);
        assert_eq!(pos.index_number, 0);
        assert_eq!(pos.relative_msf, (0, 0, 1));
        assert!(disc.read_sector_raw(10).is_none());
        assert_eq!(disc.read_sector_raw(12).unwrap()[0], 0xAB);
    }

    #[test]
    fn load_disc_from_cue_treats_new_file_index1_as_embedded_pregap() {
        let tmp = TempDir::new().unwrap();
        let cue_path = tmp.path().join("disc.cue");
        let track1_path = tmp.path().join("track1.bin");
        let track2_path = tmp.path().join("track2.bin");
        std::fs::write(&track1_path, vec![0u8; psx_iso::SECTOR_BYTES * 10]).unwrap();
        let mut track2 = vec![0u8; psx_iso::SECTOR_BYTES * (150 + 4)];
        track2[150 * psx_iso::SECTOR_BYTES] = 0xAB;
        std::fs::write(&track2_path, track2).unwrap();
        std::fs::write(
            &cue_path,
            concat!(
                "FILE \"track1.bin\" BINARY\n",
                "  TRACK 01 MODE2/2352\n",
                "    INDEX 01 00:00:00\n",
                "FILE \"track2.bin\" BINARY\n",
                "  TRACK 02 AUDIO\n",
                "    INDEX 01 00:02:00\n",
            ),
        )
        .unwrap();

        let disc = load_disc_from_cue(&cue_path).unwrap();
        assert_eq!(disc.track(2).unwrap().start_lba, 160);
        assert_eq!(disc.track(2).unwrap().sector_count, 4);
        assert!(disc.read_cdda_sector(10).is_none());
        assert_eq!(disc.read_cdda_sector(160).unwrap()[0], 0xAB);
    }

    #[test]
    fn load_disc_from_cue_slices_single_bin_multitrack_layout() {
        let tmp = TempDir::new().unwrap();
        let cue_path = tmp.path().join("disc.cue");
        let bin_path = tmp.path().join("disc.bin");
        let mut image = vec![0u8; psx_iso::SECTOR_BYTES * (10 + 150 + 4)];
        image[9 * psx_iso::SECTOR_BYTES] = 0x11;
        image[160 * psx_iso::SECTOR_BYTES] = 0xAB;
        std::fs::write(&bin_path, image).unwrap();
        std::fs::write(
            &cue_path,
            concat!(
                "FILE \"disc.bin\" BINARY\n",
                "  TRACK 01 MODE2/2352\n",
                "    INDEX 01 00:00:00\n",
                "  TRACK 02 AUDIO\n",
                "    INDEX 00 00:00:10\n",
                "    INDEX 01 00:02:10\n",
            ),
        )
        .unwrap();

        let disc = load_disc_from_cue(&cue_path).unwrap();
        assert_eq!(disc.track_count(), 2);
        assert_eq!(disc.track(1).unwrap().sector_count, 10);
        assert_eq!(disc.track(2).unwrap().start_lba, 160);
        assert_eq!(disc.track(2).unwrap().sector_count, 4);
        assert_eq!(disc.read_sector_raw(9).unwrap()[0], 0x11);
        assert!(disc.read_cdda_sector(10).is_none());
        assert_eq!(disc.read_cdda_sector(160).unwrap()[0], 0xAB);
    }

    // The streaming path: the same sheet with the image pre-cut at track
    // boundaries must model identically to the whole-image loader.
    #[test]
    fn disc_from_cue_pieces_matches_the_single_bin_loader() {
        let mut image = vec![0u8; psx_iso::SECTOR_BYTES * (10 + 150 + 4)];
        image[9 * psx_iso::SECTOR_BYTES] = 0x11;
        image[160 * psx_iso::SECTOR_BYTES] = 0xAB;
        let cue = concat!(
            "FILE \"disc.bin\" BINARY\n",
            "  TRACK 01 MODE2/2352\n",
            "    INDEX 01 00:00:00\n",
            "  TRACK 02 AUDIO\n",
            "    INDEX 00 00:00:10\n",
            "    INDEX 01 00:02:10\n",
        );
        let mut whole = Some(image.clone());
        let reference = disc_from_cue_str(cue, &mut |_| {
            whole.take().ok_or_else(|| "second file".to_string())
        })
        .unwrap();

        let cut = 10 * psx_iso::SECTOR_BYTES;
        let pieces = disc_from_cue_pieces(cue, |n| match n {
            1 => Ok(image[..cut].to_vec()),
            2 => Ok(image[cut..].to_vec()),
            other => Err(format!("unexpected track {other}")),
        })
        .unwrap();

        assert_eq!(reference.track_count(), pieces.track_count());
        let summary = |t: &psx_iso::Track| {
            let mut bytes = vec![0u8; t.source.len() as usize];
            assert!(t.source.read_at(0, &mut bytes));
            (
                t.number,
                t.track_type,
                t.start_lba,
                t.sector_count,
                t.pregap,
                t.file_pregap,
                bytes,
            )
        };
        for n in 1..=2 {
            assert_eq!(
                reference.track(n).map(summary),
                pieces.track(n).map(summary),
                "track {n}"
            );
        }
    }

    // The web build's path: same sheet, same image, no filesystem. Must slice
    // identically to `load_disc_from_cue` above, and take the image from the
    // reader exactly once.
    #[test]
    fn disc_from_cue_str_matches_the_fs_loader_on_a_single_bin() {
        let mut image = vec![0u8; psx_iso::SECTOR_BYTES * (10 + 150 + 4)];
        image[9 * psx_iso::SECTOR_BYTES] = 0x11;
        image[160 * psx_iso::SECTOR_BYTES] = 0xAB;
        let cue = concat!(
            "FILE \"disc.bin\" BINARY\n",
            "  TRACK 01 MODE2/2352\n",
            "    INDEX 01 00:00:00\n",
            "  TRACK 02 AUDIO\n",
            "    INDEX 00 00:00:10\n",
            "    INDEX 01 00:02:10\n",
        );

        let mut handed = Some(image);
        let disc = disc_from_cue_str(cue, &mut |_| {
            handed.take().ok_or_else(|| "second file".to_string())
        })
        .unwrap();
        assert_eq!(disc.track_count(), 2);
        assert_eq!(disc.track(1).unwrap().sector_count, 10);
        assert_eq!(disc.track(2).unwrap().start_lba, 160);
        assert_eq!(disc.track(2).unwrap().sector_count, 4);
        assert_eq!(disc.read_sector_raw(9).unwrap()[0], 0x11);
        assert!(disc.read_cdda_sector(10).is_none());
        assert_eq!(disc.read_cdda_sector(160).unwrap()[0], 0xAB);
    }

    #[test]
    fn load_disc_from_ccd_slices_single_img_by_toc() {
        let tmp = TempDir::new().unwrap();
        let ccd_path = tmp.path().join("disc.ccd");
        let img_path = tmp.path().join("disc.img");
        let mut image = vec![0u8; psx_iso::SECTOR_BYTES * 14];
        image[9 * psx_iso::SECTOR_BYTES] = 0x11;
        image[10 * psx_iso::SECTOR_BYTES] = 0xAB;
        std::fs::write(&img_path, image).unwrap();
        std::fs::write(
            &ccd_path,
            concat!(
                "[Entry 0]\n",
                "Point=0x01\n",
                "Control=0x04\n",
                "PLBA=0\n",
                "[Entry 1]\n",
                "Point=0x02\n",
                "Control=0x00\n",
                "PLBA=10\n",
                "[Entry 2]\n",
                "Point=0xa2\n",
                "Control=0x00\n",
                "PLBA=14\n",
            ),
        )
        .unwrap();

        let disc = load_disc_from_ccd(&ccd_path).unwrap();
        assert_eq!(disc.track_count(), 2);
        assert_eq!(disc.track(1).unwrap().track_type, psx_iso::TrackType::Data);
        assert_eq!(disc.track(2).unwrap().track_type, psx_iso::TrackType::Audio);
        assert_eq!(disc.read_sector_raw(9).unwrap()[0], 0x11);
        assert_eq!(disc.read_sector_raw(10).unwrap()[0], 0xAB);
        let pos = disc.track_position_for_lba(10).unwrap();
        assert_eq!(pos.track_number, 2);
        assert_eq!(pos.index_number, 1);
    }

    #[test]
    fn load_disc_from_ccd_keeps_an_index_0_pregap_in_its_own_track() {
        // Tomb Raider's CloneCD sheet: [TRACK 2] has INDEX 0 before INDEX 1.
        // The pregap sectors are in the image and belong to track 2 (index
        // 00, relative time counting down), not to the end of track 1.
        let tmp = TempDir::new().unwrap();
        let ccd_path = tmp.path().join("disc.ccd");
        let mut image = vec![0u8; psx_iso::SECTOR_BYTES * 14];
        image[7 * psx_iso::SECTOR_BYTES] = 0x77;
        image[10 * psx_iso::SECTOR_BYTES] = 0xAB;
        std::fs::write(tmp.path().join("disc.img"), image).unwrap();
        std::fs::write(
            &ccd_path,
            concat!(
                "[Entry 0]\nPoint=0x01\nControl=0x04\nPLBA=0\n",
                "[Entry 1]\nPoint=0x02\nControl=0x00\nPLBA=10\n",
                "[Entry 2]\nPoint=0xa2\nControl=0x00\nPLBA=14\n",
                "[TRACK 1]\nMODE=2\nINDEX 1=0\n",
                "[TRACK 2]\nMODE=0\nINDEX 0=7\nINDEX 1=10\n",
            ),
        )
        .unwrap();

        let disc = load_disc_from_ccd(&ccd_path).unwrap();
        assert_eq!(disc.track(1).unwrap().sector_count, 7);
        let track2 = disc.track(2).unwrap();
        assert_eq!(
            (track2.start_lba, track2.pregap, track2.sector_count),
            (10, 3, 4)
        );
        let pos = disc.track_position_for_lba(8).unwrap();
        assert_eq!((pos.track_number, pos.index_number), (2, 0));
        assert_eq!(pos.relative_msf, (0, 0, 1));
        let pos = disc.track_position_for_lba(6).unwrap();
        assert_eq!((pos.track_number, pos.index_number), (1, 1));
        assert_eq!(disc.read_sector_raw(10).unwrap()[0], 0xAB);
        assert_eq!(disc.read_cdda_sector(10).unwrap()[0], 0xAB);
    }

    #[test]
    fn parse_ccd_toc_decodes_bcd_track_numbers() {
        let tmp = TempDir::new().unwrap();
        let ccd_path = tmp.path().join("many.ccd");
        std::fs::write(&ccd_path, "[Entry 0]\nPoint=0x10\nControl=0x00\nPLBA=123\n").unwrap();

        let toc = parse_ccd_toc(&ccd_path).unwrap();
        assert_eq!(toc.tracks.len(), 1);
        assert_eq!(toc.tracks[0].number, 10);
    }

    #[test]
    fn load_disc_from_ccd_decodes_an_ecm_sidecar_in_memory() {
        let tmp = TempDir::new().unwrap();
        let ccd_path = tmp.path().join("disc.ccd");
        let mut image = vec![0u8; psx_iso::SECTOR_BYTES * 2];
        image[0] = 0xCD;
        // A literal-only ECM stream: header, one type-0 record, end
        // marker, checksum of the decoded bytes.
        let mut ecm = b"ECM\0".to_vec();
        let mut n = image.len() as u32 - 1;
        ecm.push(((n & 0x1F) as u8) << 2);
        n >>= 5;
        while n != 0 {
            *ecm.last_mut().unwrap() |= 0x80;
            ecm.push((n & 0x7F) as u8);
            n >>= 7;
        }
        ecm.extend_from_slice(&image);
        ecm.extend_from_slice(&[0xFC, 0xFF, 0xFF, 0xFF, 0x3F]);
        ecm.extend_from_slice(&crate::ecm::edc_update(0, &image).to_le_bytes());
        std::fs::write(tmp.path().join("disc.img.ecm"), ecm).unwrap();
        std::fs::write(
            &ccd_path,
            "[Entry 0]\nPoint=0x01\nControl=0x04\nPLBA=0\n[Entry 1]\nPoint=0xa2\nPLBA=2\n",
        )
        .unwrap();

        let disc = load_disc_from_ccd(&ccd_path).unwrap();
        assert_eq!(disc.read_sector_raw(0).unwrap()[0], 0xCD);
        assert!(!tmp.path().join("disc.img").exists());
    }

    #[test]
    fn sbi_records_become_lbas_for_every_record_type() {
        let mut sbi = b"SBI\0".to_vec();
        // 03:08:05 type 1 (10 bytes of Q), 03:08:10 type 3, 09:20:45 type 2.
        sbi.extend([0x03, 0x08, 0x05, 1]);
        sbi.extend([0u8; 10]);
        sbi.extend([0x03, 0x08, 0x10, 3, 0x03, 0x08, 0x10]);
        sbi.extend([0x09, 0x20, 0x45, 2, 0x00, 0x00, 0x00]);
        // psx-spx lists these LibCrypt sectors as 14105, 14110 and 42045 in
        // absolute frames; LBAs start at 00:02:00.
        assert_eq!(
            parse_sbi(&sbi).unwrap(),
            vec![14105 - 150, 14110 - 150, 42045 - 150]
        );
    }

    #[test]
    fn sbi_rejects_bad_headers_types_and_truncation() {
        assert!(parse_sbi(b"XYZ\0").is_err());
        assert!(parse_sbi(b"SBI\0\x03\x08\x05\x07").is_err());
        assert!(parse_sbi(b"SBI\0\x03\x08\x05\x01\x00").is_err());
        assert!(parse_sbi(b"SBI\0\x03\x08\x5A\x02\x00\x00\x00").is_err());
    }

    #[test]
    fn sbi_is_found_next_to_the_sheet() {
        let tmp = TempDir::new().unwrap();
        let cue = tmp.path().join("Game (Europe).cue");
        assert_eq!(load_sbi_for(&cue).unwrap(), None);
        std::fs::write(
            tmp.path().join("Game (Europe).sbi"),
            b"SBI\0\x03\x09\x56\x02\x00\x00\x00",
        )
        .unwrap();
        assert_eq!(load_sbi_for(&cue).unwrap(), Some(vec![14231 - 150]));
    }

    #[test]
    fn scan_walks_recursively_into_subdirs() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("game.exe"), b"").unwrap();
        let mut lib = Library::default();
        lib.scan(tmp.path()).unwrap();
        assert_eq!(lib.entries.len(), 1);
        assert!(lib.entries[0].path.ends_with("sub/game.exe"));
    }
}
