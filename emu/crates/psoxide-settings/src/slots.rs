//! Save-state slots: what a slot file holds beyond the machine state, and who may load it.
//!
//! A slot is one file (desktop) or one browser record (web) shaped like this:
//!
//! ```text
//! [ SaveStateV1 postcard bytes ][ SlotMeta postcard bytes ][ meta length: u32 LE ][ "PSXMETA1" ]
//! ```
//!
//! The machine state is exactly what [`crate::savestate::SaveStateV1`] writes, so the format
//! version and its checks are untouched. The trailer carries what a slot grid needs without
//! decoding megabytes: when it was made, how far the game had run, a small thumbnail, the title,
//! and the identity of the disc it belongs to. A file without a trailer (a save from before slots)
//! still loads: its header gives the time and the tick, and the disc identity is simply unknown.
//!
//! Loading is refused, with a message a player can read, when the save is from another game, from
//! another disc or build of the same game, from a different state format, or damaged
//! ([`Refusal`]). Nothing here panics on a bad file.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::savestate::{
    SaveStateError, SAVESTATE_FORMAT_VERSION, SAVESTATE_MAGIC, SAVESTATE_MIN_SUPPORTED_VERSION,
};

/// The quick slot: what F5, F7 and the pad shortcuts use.
pub const QUICK_SLOT: u8 = 0;
/// How many numbered slots a game has, `1..=NUMBERED_SLOTS`.
pub const NUMBERED_SLOTS: u8 = 8;
/// Every slot of a game, the quick slot first.
pub const SLOT_COUNT: usize = NUMBERED_SLOTS as usize + 1;

/// Width of a slot thumbnail, in pixels.
pub const THUMB_W: usize = 160;
/// Height of a slot thumbnail, in pixels. The picture is stored at the 4:3 shape a TV shows.
pub const THUMB_H: usize = 120;

/// The PlayStation's CPU clock, for turning a tick count into play time.
const CPU_HZ: u64 = 33_868_800;

const TRAILER_MAGIC: &[u8; 8] = b"PSXMETA1";
/// Bytes after the metadata: its length and the trailer magic.
const TRAILER_TAIL: usize = 4 + TRAILER_MAGIC.len();
/// The most metadata a slot is believed to carry: a thumbnail, some strings, a few numbers.
const MAX_META_BYTES: usize = 256 * 1024;
/// Bytes read from the start of a file to find its header.
const HEAD_BYTES: usize = 4096;
const META_VERSION: u32 = 1;

/// Whether `slot` is one a game has.
#[must_use]
pub fn is_slot(slot: u8) -> bool {
    slot <= NUMBERED_SLOTS
}

/// The name a player sees for a slot.
#[must_use]
pub fn slot_name(slot: u8) -> String {
    if slot == QUICK_SLOT {
        "Quick save".to_string()
    } else {
        format!("Slot {slot}")
    }
}

/// A small picture of the game at the moment of the save: RGB, `w` by `h`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Thumb {
    /// Width in pixels.
    pub w: u16,
    /// Height in pixels.
    pub h: u16,
    /// `w * h * 3` bytes, row by row.
    pub rgb: Vec<u8>,
}

impl Thumb {
    /// Shrink an RGBA picture of `w` by `h` pixels to the thumbnail size, averaging every source
    /// pixel into the thumbnail pixel it lands in. `None` for an empty or inconsistent picture.
    #[must_use]
    pub fn from_rgba(rgba: &[u8], w: u32, h: u32) -> Option<Self> {
        let (w, h) = (w as usize, h as usize);
        if w == 0 || h == 0 || rgba.len() < w * h * 4 {
            return None;
        }
        let mut rgb = Vec::with_capacity(THUMB_W * THUMB_H * 3);
        for ty in 0..THUMB_H {
            let (y0, y1) = span(ty, THUMB_H, h);
            for tx in 0..THUMB_W {
                let (x0, x1) = span(tx, THUMB_W, w);
                let mut sum = [0u32; 3];
                for y in y0..y1 {
                    for x in x0..x1 {
                        let at = (y * w + x) * 4;
                        sum[0] += u32::from(rgba[at]);
                        sum[1] += u32::from(rgba[at + 1]);
                        sum[2] += u32::from(rgba[at + 2]);
                    }
                }
                let n = ((y1 - y0) * (x1 - x0)) as u32;
                rgb.extend(sum.map(|s| (s / n) as u8));
            }
        }
        Some(Self {
            w: THUMB_W as u16,
            h: THUMB_H as u16,
            rgb,
        })
    }

    /// The picture as RGBA, for a texture.
    #[must_use]
    pub fn to_rgba(&self) -> Vec<u8> {
        self.rgb
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect()
    }

    /// Whether the byte count matches the size, so a decoded thumbnail is safe to draw.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        self.rgb.len() == usize::from(self.w) * usize::from(self.h) * 3 && self.w > 0 && self.h > 0
    }
}

/// The source pixels `[start, end)` of one axis that thumbnail pixel `i` of `out` covers.
/// Never empty, so a source smaller than the thumbnail still gives a pixel.
fn span(i: usize, out: usize, src: usize) -> (usize, usize) {
    let start = i * src / out;
    let end = ((i + 1) * src / out).max(start + 1).min(src);
    (start.min(src - 1), end)
}

/// What a slot says about itself, readable without decoding the machine state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SlotMeta {
    /// Schema of this struct.
    pub meta_version: u32,
    /// The library id of the game the save belongs to.
    pub game_id: String,
    /// The game's title when it was saved, for messages.
    pub title: String,
    /// Hash of the whole disc image the save was made on; zero when it was not known.
    pub disc_hash: u64,
    /// Unix seconds when it was saved.
    pub created_at: u64,
    /// CPU cycles the machine had run, a play position.
    pub cpu_tick: u64,
    /// The state format the machine bytes use.
    pub format_version: u32,
    /// The program that wrote it, for example `PSoXide/0.1.0`.
    pub creator: String,
    /// The picture, when one could be taken.
    pub thumb: Option<Thumb>,
}

impl SlotMeta {
    /// The metadata of a save about to be written by this build.
    pub fn for_new_save(
        game_id: &str,
        title: &str,
        disc_hash: Option<u64>,
        created_at: u64,
        cpu_tick: u64,
        thumb: Option<Thumb>,
    ) -> Self {
        Self {
            meta_version: META_VERSION,
            game_id: game_id.to_string(),
            title: title.to_string(),
            disc_hash: disc_hash.unwrap_or(0),
            created_at,
            cpu_tick,
            format_version: SAVESTATE_FORMAT_VERSION,
            creator: concat!("PSoXide/", env!("CARGO_PKG_VERSION")).to_string(),
            thumb: thumb.filter(Thumb::is_consistent),
        }
    }

    /// How long the game had been running at the save, in seconds of emulated time.
    #[must_use]
    pub fn play_seconds(&self) -> u64 {
        self.cpu_tick / CPU_HZ
    }
}

/// Why a slot cannot be loaded here.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refusal {
    /// The bytes are not a readable save: cut short, not ours, or corrupt.
    Damaged,
    /// Written by a PSoXide whose state format this build no longer reads.
    OlderBuild {
        /// The format the save uses.
        found: u32,
    },
    /// Written by a PSoXide newer than this one.
    NewerBuild {
        /// The format the save uses.
        found: u32,
    },
    /// Made for another game.
    OtherGame,
    /// Made on another disc, or another build of this disc.
    OtherDisc,
}

impl Refusal {
    /// A sentence a player can act on.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Damaged => "This save is damaged and cannot be read.".to_string(),
            Self::OlderBuild { found } => format!(
                "This save came from an older PSoXide (state format {found}); this version reads \
                 format {SAVESTATE_MIN_SUPPORTED_VERSION} and newer, so it cannot load it."
            ),
            Self::NewerBuild { found } => format!(
                "This save came from a newer PSoXide (state format {found}). Update PSoXide to \
                 load it."
            ),
            Self::OtherGame => "This save belongs to a different game.".to_string(),
            Self::OtherDisc => "This save was made on a different disc or build of this game, \
                                so it cannot be loaded here."
                .to_string(),
        }
    }

    /// A word or two for a card's badge.
    #[must_use]
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Damaged => "Damaged",
            Self::OlderBuild { .. } | Self::NewerBuild { .. } => "Other version",
            Self::OtherGame => "Other game",
            Self::OtherDisc => "Other disc",
        }
    }

    /// The refusal a failed state decode amounts to.
    #[must_use]
    pub fn from_error(error: &SaveStateError) -> Self {
        match error {
            SaveStateError::UnsupportedVersion { found, .. }
                if *found > SAVESTATE_FORMAT_VERSION =>
            {
                Self::NewerBuild { found: *found }
            }
            SaveStateError::UnsupportedVersion { found, .. } => Self::OlderBuild { found: *found },
            _ => Self::Damaged,
        }
    }
}

/// Judge a slot against the machine that wants to load it.
///
/// `same_game` says whether the slot's game id is the running game's (or an earlier id the same
/// game was known by). `disc_hash` is the running disc's hash when known; an unknown hash on
/// either side means the disc cannot be told apart, and the game id alone decides.
///
/// # Errors
///
/// The [`Refusal`] that says why the slot cannot be loaded here.
pub fn check_loadable(
    meta: &SlotMeta,
    same_game: impl Fn(&str) -> bool,
    disc_hash: Option<u64>,
) -> Result<(), Refusal> {
    if meta.format_version > SAVESTATE_FORMAT_VERSION {
        return Err(Refusal::NewerBuild {
            found: meta.format_version,
        });
    }
    if meta.format_version < SAVESTATE_MIN_SUPPORTED_VERSION {
        return Err(Refusal::OlderBuild {
            found: meta.format_version,
        });
    }
    if !same_game(&meta.game_id) {
        return Err(Refusal::OtherGame);
    }
    match disc_hash {
        Some(now) if now != 0 && meta.disc_hash != 0 && now != meta.disc_hash => {
            Err(Refusal::OtherDisc)
        }
        _ => Ok(()),
    }
}

/// Put the trailer after the machine state bytes.
///
/// # Errors
///
/// [`SaveStateError::Encode`] when the metadata cannot be encoded or is too large.
pub fn pack(mut state: Vec<u8>, meta: &SlotMeta) -> Result<Vec<u8>, SaveStateError> {
    let encoded =
        postcard::to_allocvec(meta).map_err(|error| SaveStateError::Encode(error.to_string()))?;
    let len = u32::try_from(encoded.len())
        .map_err(|_| SaveStateError::Encode("slot metadata too large".to_string()))?;
    state.extend_from_slice(&encoded);
    state.extend_from_slice(&len.to_le_bytes());
    state.extend_from_slice(TRAILER_MAGIC);
    Ok(state)
}

/// Split a slot's bytes into the machine state and, when it has one, the metadata trailer.
/// A damaged trailer reads as none: the state part is still handed back whole, minus nothing.
#[must_use]
pub fn split(bytes: &[u8]) -> (&[u8], Option<SlotMeta>) {
    match trailer_len(bytes) {
        Some(meta_len) => {
            let meta_start = bytes.len() - TRAILER_TAIL - meta_len;
            let meta = decode_meta(&bytes[meta_start..bytes.len() - TRAILER_TAIL]);
            (&bytes[..meta_start], meta)
        }
        None => (bytes, None),
    }
}

/// The length of the trailer's metadata when `bytes` ends in a well-formed trailer.
fn trailer_len(bytes: &[u8]) -> Option<usize> {
    let tail = bytes.len().checked_sub(TRAILER_TAIL)?;
    if &bytes[tail + 4..] != TRAILER_MAGIC {
        return None;
    }
    let len = u32::from_le_bytes(bytes[tail..tail + 4].try_into().ok()?) as usize;
    (len <= MAX_META_BYTES && len <= tail).then_some(len)
}

/// The trailer's metadata bytes alone, for a store that keeps them beside the save.
#[must_use]
pub fn encode_meta(meta: &SlotMeta) -> Vec<u8> {
    postcard::to_allocvec(meta).unwrap_or_default()
}

/// Metadata from the bytes [`encode_meta`] made; `None` when they are not a whole, believable one.
pub fn decode_meta(bytes: &[u8]) -> Option<SlotMeta> {
    let meta: SlotMeta = postcard::from_bytes(bytes).ok()?;
    (meta.thumb.as_ref().is_none_or(Thumb::is_consistent)).then_some(meta)
}

/// The metadata of a slot's bytes: its trailer, or for a save from before slots the little its
/// header knows. `Err` when the bytes are not a PSoXide save at all.
///
/// # Errors
///
/// [`Refusal::Damaged`] when the bytes are not a PSoXide save.
pub fn read_meta(bytes: &[u8]) -> Result<SlotMeta, Refusal> {
    let (state, meta) = split(bytes);
    match meta {
        Some(meta) => Ok(meta),
        None => legacy_meta(state),
    }
}

/// Metadata for a save that has no trailer, from its header alone.
fn legacy_meta(state: &[u8]) -> Result<SlotMeta, Refusal> {
    #[derive(Deserialize)]
    struct HeaderOnly {
        header: crate::savestate::SaveStateHeader,
    }
    if state.len() < SAVESTATE_MAGIC.len() || &state[..SAVESTATE_MAGIC.len()] != SAVESTATE_MAGIC {
        return Err(Refusal::Damaged);
    }
    let (head, _): (HeaderOnly, &[u8]) =
        postcard::take_from_bytes(state).map_err(|_| Refusal::Damaged)?;
    let h = head.header;
    Ok(SlotMeta {
        meta_version: 0,
        game_id: h.game_id,
        title: String::new(),
        disc_hash: 0,
        created_at: h.created_at,
        cpu_tick: h.cpu_tick,
        format_version: h.format_version,
        creator: h.creator,
        thumb: None,
    })
}

/// Read a slot file's metadata without reading the machine state: the start of the file for the
/// header and the end for the trailer.
///
/// # Errors
///
/// [`Refusal::Damaged`] when the file is missing, cut short or not a PSoXide save.
pub fn read_meta_file(path: &Path) -> Result<SlotMeta, Refusal> {
    let mut file = std::fs::File::open(path).map_err(|_| Refusal::Damaged)?;
    let len = file.metadata().map_err(|_| Refusal::Damaged)?.len();
    let tail_len = TRAILER_TAIL as u64;
    if len >= tail_len {
        let mut tail = [0u8; TRAILER_TAIL];
        file.seek(SeekFrom::Start(len - tail_len))
            .and_then(|_| file.read_exact(&mut tail))
            .map_err(|_| Refusal::Damaged)?;
        if &tail[4..] == TRAILER_MAGIC {
            let meta_len = u64::from(u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]));
            if meta_len <= MAX_META_BYTES as u64 && meta_len + tail_len <= len {
                let mut meta = vec![0u8; meta_len as usize];
                file.seek(SeekFrom::Start(len - tail_len - meta_len))
                    .and_then(|_| file.read_exact(&mut meta))
                    .map_err(|_| Refusal::Damaged)?;
                if let Some(meta) = decode_meta(&meta) {
                    return Ok(meta);
                }
            }
        }
    }
    let mut head = vec![0u8; HEAD_BYTES.min(len as usize)];
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.read_exact(&mut head))
        .map_err(|_| Refusal::Damaged)?;
    legacy_meta(&head)
}

/// Write a slot file atomically: a temporary file beside it, then a rename, so a crash or a
/// full disk never leaves half a save where a good one was.
///
/// # Errors
///
/// [`SaveStateError::Io`] when the folder, the temporary file or the rename fails; the old file
/// is then left as it was.
pub fn write_slot_file(path: &Path, bytes: &[u8]) -> Result<(), SaveStateError> {
    let io = |path: &Path, source| SaveStateError::Io {
        path: path.to_path_buf(),
        source,
    };
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
    }
    let tmp = path.with_extension("psx.tmp");
    std::fs::write(&tmp, bytes).map_err(|e| io(&tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io(path, e)
    })
}

/// "3:07" or "1:02:09" for a play position.
#[must_use]
pub fn format_play_time(seconds: u64) -> String {
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::savestate::SaveStateV1;
    use tempfile::TempDir;

    fn picture(w: u32, h: u32, shade: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
        let mut out = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let c = shade(x, y);
                out.extend([c[0], c[1], c[2], 255]);
            }
        }
        out
    }

    fn state_bytes(game: &str) -> Vec<u8> {
        SaveStateV1::new_at(vec![1u32, 2, 3], game, 5_000_000, 1_700_000_000)
            .to_bytes()
            .unwrap()
    }

    fn meta(game: &str, hash: u64) -> SlotMeta {
        SlotMeta::for_new_save(game, "Title", Some(hash), 1_700_000_000, 5_000_000, None)
    }

    #[test]
    fn a_thumbnail_keeps_the_picture() {
        let rgba = picture(
            320,
            240,
            |x, _| if x < 160 { [255, 0, 0] } else { [0, 0, 255] },
        );
        let thumb = Thumb::from_rgba(&rgba, 320, 240).expect("thumbnail");
        assert_eq!((thumb.w, thumb.h), (160, 120));
        assert!(thumb.is_consistent());
        assert_eq!(&thumb.rgb[..3], &[255, 0, 0], "left edge stays red");
        let right = (159 * 3) as usize;
        assert_eq!(
            &thumb.rgb[right..right + 3],
            &[0, 0, 255],
            "right edge stays blue"
        );
        assert_eq!(thumb.to_rgba().len(), 160 * 120 * 4);
    }

    #[test]
    fn a_small_or_odd_picture_still_gives_a_whole_thumbnail() {
        for (w, h) in [(1, 1), (7, 3), (256, 240), (640, 480), (368, 240)] {
            let rgba = picture(w, h, |x, y| [(x % 256) as u8, (y % 256) as u8, 9]);
            let thumb = Thumb::from_rgba(&rgba, w, h).expect("thumbnail");
            assert!(thumb.is_consistent(), "{w}x{h}");
        }
        assert!(Thumb::from_rgba(&[], 0, 0).is_none());
        assert!(Thumb::from_rgba(&[0; 8], 4, 4).is_none(), "too few bytes");
    }

    #[test]
    fn packing_round_trips_and_leaves_the_state_bytes_alone() {
        let state = state_bytes("g");
        let mut m = meta("g", 77);
        m.thumb = Thumb::from_rgba(&picture(4, 4, |_, _| [9, 9, 9]), 4, 4);
        let packed = pack(state.clone(), &m).unwrap();
        let (inner, got) = split(&packed);
        assert_eq!(inner, &state[..]);
        assert_eq!(got.as_ref(), Some(&m));
        // The machine state decodes straight from the packed bytes too: trailing bytes are ignored.
        let decoded: SaveStateV1<Vec<u32>> = SaveStateV1::from_bytes(inner).unwrap();
        assert_eq!(decoded.payload, vec![1, 2, 3]);
        assert_eq!(read_meta(&packed).unwrap(), m);
    }

    #[test]
    fn a_save_from_before_slots_reads_its_header() {
        let state = state_bytes("old-game");
        let m = read_meta(&state).unwrap();
        assert_eq!(m.game_id, "old-game");
        assert_eq!(m.created_at, 1_700_000_000);
        assert_eq!(m.cpu_tick, 5_000_000);
        assert_eq!(m.disc_hash, 0, "the disc is unknown for such a save");
        assert!(m.thumb.is_none());
    }

    #[test]
    fn junk_and_cut_off_files_are_damaged_not_a_panic() {
        assert_eq!(read_meta(b"").unwrap_err(), Refusal::Damaged);
        assert_eq!(
            read_meta(b"hello there, not a save").unwrap_err(),
            Refusal::Damaged
        );
        let packed = pack(state_bytes("g"), &meta("g", 1)).unwrap();
        for cut in [1, 5, 12, 40, packed.len() / 2] {
            let _ = read_meta(&packed[..packed.len() - cut]);
        }
        // A trailer that claims more metadata than the file holds is ignored.
        let mut lying = state_bytes("g");
        lying.extend_from_slice(&u32::MAX.to_le_bytes());
        lying.extend_from_slice(TRAILER_MAGIC);
        assert!(split(&lying).1.is_none());
        assert_eq!(read_meta(&lying).unwrap().game_id, "g");
    }

    #[test]
    fn a_trailer_with_a_lying_thumbnail_is_not_trusted() {
        let mut m = meta("g", 1);
        m.thumb = Some(Thumb {
            w: 160,
            h: 120,
            rgb: vec![0; 10],
        });
        let packed = pack(state_bytes("g"), &m).unwrap();
        assert!(split(&packed).1.is_none());
    }

    #[test]
    fn the_disc_must_match_when_both_sides_know_it() {
        let m = meta("g", 111);
        let ok = |id: &str| id == "g";
        assert_eq!(check_loadable(&m, ok, Some(111)), Ok(()));
        assert_eq!(check_loadable(&m, ok, Some(222)), Err(Refusal::OtherDisc));
        assert_eq!(check_loadable(&m, ok, None), Ok(()));
        assert_eq!(check_loadable(&m, ok, Some(0)), Ok(()));
        let unknown_disc = meta("g", 0);
        assert_eq!(check_loadable(&unknown_disc, ok, Some(222)), Ok(()));
        assert_eq!(
            check_loadable(&m, |_| false, Some(111)),
            Err(Refusal::OtherGame)
        );
    }

    #[test]
    fn another_state_format_is_refused_with_its_own_message() {
        let mut m = meta("g", 1);
        m.format_version = SAVESTATE_FORMAT_VERSION + 1;
        let newer = check_loadable(&m, |_| true, None).unwrap_err();
        assert!(matches!(newer, Refusal::NewerBuild { .. }));
        assert!(newer.message().contains("newer"));
        m.format_version = SAVESTATE_MIN_SUPPORTED_VERSION - 1;
        let older = check_loadable(&m, |_| true, None).unwrap_err();
        assert!(matches!(older, Refusal::OlderBuild { .. }));
        assert!(older.message().contains("older"));
        for refusal in [Refusal::Damaged, Refusal::OtherGame, Refusal::OtherDisc] {
            assert!(!refusal.message().contains('\u{2014}'));
        }
    }

    #[test]
    fn a_failed_decode_maps_to_the_right_refusal() {
        let newer = SaveStateError::UnsupportedVersion {
            found: SAVESTATE_FORMAT_VERSION + 3,
            min: 1,
            max: 2,
        };
        assert!(matches!(
            Refusal::from_error(&newer),
            Refusal::NewerBuild { .. }
        ));
        let older = SaveStateError::UnsupportedVersion {
            found: 2,
            min: 1,
            max: 2,
        };
        assert!(matches!(
            Refusal::from_error(&older),
            Refusal::OlderBuild { found: 2 }
        ));
        assert_eq!(
            Refusal::from_error(&SaveStateError::Decode("x".into())),
            Refusal::Damaged
        );
    }

    #[test]
    fn a_file_is_written_whole_and_its_meta_reads_back_without_the_state() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("deep").join("slot3.psx");
        let mut m = meta("g", 5);
        m.thumb = Thumb::from_rgba(&picture(8, 8, |x, _| [x as u8 * 30, 0, 0]), 8, 8);
        // A state far bigger than the head read, so the trailer really is found from the end.
        let big = SaveStateV1::new_at(vec![7u8; 300_000], "g", 5_000_000, 1_700_000_000)
            .to_bytes()
            .unwrap();
        write_slot_file(&path, &pack(big, &m).unwrap()).unwrap();
        assert!(!path.with_extension("psx.tmp").exists());
        assert_eq!(read_meta_file(&path).unwrap(), m);
        // Overwriting replaces it whole.
        let m2 = meta("g", 6);
        write_slot_file(&path, &pack(state_bytes("g"), &m2).unwrap()).unwrap();
        assert_eq!(read_meta_file(&path).unwrap(), m2);
    }

    #[test]
    fn a_legacy_file_reads_from_its_head() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("slot1.psx");
        let big = SaveStateV1::new_at(vec![7u8; 300_000], "old", 123, 1_650_000_000)
            .to_bytes()
            .unwrap();
        std::fs::write(&path, big).unwrap();
        let m = read_meta_file(&path).unwrap();
        assert_eq!(
            (m.game_id.as_str(), m.cpu_tick, m.created_at),
            ("old", 123, 1_650_000_000)
        );
        std::fs::write(&path, b"nope").unwrap();
        assert_eq!(read_meta_file(&path).unwrap_err(), Refusal::Damaged);
        assert_eq!(
            read_meta_file(&tmp.path().join("missing.psx")).unwrap_err(),
            Refusal::Damaged
        );
    }

    #[test]
    fn slot_numbers_and_play_time() {
        assert!(is_slot(0) && is_slot(NUMBERED_SLOTS) && !is_slot(NUMBERED_SLOTS + 1));
        assert_eq!(slot_name(0), "Quick save");
        assert_eq!(slot_name(3), "Slot 3");
        assert_eq!(format_play_time(0), "0:00");
        assert_eq!(format_play_time(187), "3:07");
        assert_eq!(format_play_time(3729), "1:02:09");
        assert_eq!(meta("g", 1).play_seconds(), 0);
        let mut m = meta("g", 1);
        m.cpu_tick = CPU_HZ * 61;
        assert_eq!(m.play_seconds(), 61);
    }
}
