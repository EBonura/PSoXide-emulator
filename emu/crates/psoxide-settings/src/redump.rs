//! The Redump PlayStation catalogue: telling a disc by its data track.
//!
//! The libretro database publishes the Redump PlayStation dump list as a
//! clrmamepro `.dat` file. Each game lists the name, region and boot serial
//! of the disc and the size and CRC-32 of its first (data) track. Hashing the
//! data track of a disc image and looking it up there names the game exactly,
//! whatever the file is called and whatever its `SYSTEM.CNF` says, which is
//! what cover lookup needs when the file name is wrong or the serial is
//! missing from a cover collection.
//!
//! The catalogue is data under CC BY-SA 4.0: it is never compiled in. The
//! frontend downloads it once and keeps it in its cache. This module is the
//! pure half: parsing, lookup and the CRC, with no network and no GUI.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// One game of the catalogue.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DatGame {
    /// The exact Redump name, e.g. `Wipeout (USA)`.
    pub name: String,
    /// The boot serial in the cover collections' form (`SCUS-94301`).
    pub serial: String,
    /// The region the dump list gives (`USA`, `Europe`, ...).
    pub region: String,
}

/// The parsed catalogue.
#[derive(Debug, Default)]
pub struct RedumpDat {
    games: Vec<DatGame>,
    by_track: HashMap<(u64, u32), u32>,
    by_serial: HashMap<String, u32>,
    by_name: HashMap<String, u32>,
}

/// `SCUS-94426CE` and `SCES-02105/P` and `SLES-02375-P` are all
/// `SCUS-94426` / `SCES-02105` / `SLES-02375`: the letters, a dash and five
/// digits. Anything else (homebrew, `LSP-99012101`) comes back trimmed.
#[must_use]
pub fn normalize_serial(raw: &str) -> String {
    let upper = raw.trim().to_ascii_uppercase();
    let letters = upper.bytes().take_while(u8::is_ascii_uppercase).count();
    let bytes = upper.as_bytes();
    if (3..=4).contains(&letters)
        && bytes.get(letters) == Some(&b'-')
        && bytes.len() >= letters + 6
        && bytes[letters + 1..letters + 6]
            .iter()
            .all(u8::is_ascii_digit)
        && bytes.get(letters + 6).is_none_or(|b| !b.is_ascii_digit())
    {
        return upper[..letters + 6].to_string();
    }
    upper
}

/// The text between the first pair of quotes after `key` on a line.
fn quoted<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let at = line.find(key)? + key.len();
    let rest = line[at..].trim_start().strip_prefix('"')?;
    Some(&rest[..rest.find('"')?])
}

/// The word that follows `key` on a line (`size 123` gives `123`).
fn word<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let at = line.find(key)? + key.len();
    line[at..].split_whitespace().next()
}

impl RedumpDat {
    /// Parse the text of the `.dat`. Lines that do not fit are skipped, so a
    /// future format change loses games instead of failing the whole file.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut dat = Self::default();
        let mut name = String::new();
        let mut region = String::new();
        let mut serial = String::new();
        let mut in_game = false;
        for line in text.lines() {
            let line = line.trim();
            if line == "game (" {
                in_game = true;
                name.clear();
                region.clear();
                serial.clear();
            } else if in_game && line.starts_with("name ") {
                name = quoted(line, "name").unwrap_or_default().to_string();
            } else if in_game && line.starts_with("region ") {
                region = quoted(line, "region").unwrap_or_default().to_string();
            } else if in_game && line.starts_with("serial ") {
                serial = normalize_serial(quoted(line, "serial").unwrap_or_default());
            } else if in_game && line.starts_with("rom (") {
                let size = word(line, " size ").and_then(|s| s.parse::<u64>().ok());
                let crc = word(line, " crc ").and_then(|s| u32::from_str_radix(s, 16).ok());
                if name.is_empty() {
                    continue;
                }
                let index = dat.games.len() as u32;
                dat.games.push(DatGame {
                    name: name.clone(),
                    serial: serial.clone(),
                    region: region.clone(),
                });
                if let (Some(size), Some(crc)) = (size, crc) {
                    dat.by_track.entry((size, crc)).or_insert(index);
                }
                dat.by_name.entry(name.clone()).or_insert(index);
                if !serial.is_empty() {
                    dat.by_serial.entry(serial.clone()).or_insert(index);
                }
            }
        }
        dat
    }

    /// Games in the catalogue (one per data-track entry).
    #[must_use]
    pub fn len(&self) -> usize {
        self.games.len()
    }

    /// Whether the catalogue has no games.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.games.is_empty()
    }

    /// The game whose data track has this size and CRC-32.
    #[must_use]
    pub fn by_track(&self, size: u64, crc: u32) -> Option<&DatGame> {
        self.by_track
            .get(&(size, crc))
            .and_then(|i| self.games.get(*i as usize))
    }

    /// The game with exactly this Redump name (case matters, as on the thumbnail server).
    #[must_use]
    pub fn by_name(&self, name: &str) -> Option<&DatGame> {
        self.by_name
            .get(name)
            .and_then(|i| self.games.get(*i as usize))
    }

    /// The game with this boot serial (any spelling [`normalize_serial`] folds).
    #[must_use]
    pub fn by_serial(&self, serial: &str) -> Option<&DatGame> {
        self.by_serial
            .get(&normalize_serial(serial))
            .and_then(|i| self.games.get(*i as usize))
    }
}

/// A streaming CRC-32 (the zlib/PKZIP polynomial the `.dat` uses).
#[derive(Clone, Debug)]
pub struct Crc32 {
    state: u32,
}

/// Slice-by-8 tables, built at compile time.
const TABLES: [[u32; 256]; 8] = {
    let mut tables = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        tables[0][i] = c;
        i += 1;
    }
    let mut t = 1;
    while t < 8 {
        let mut i = 0;
        while i < 256 {
            let prev = tables[t - 1][i];
            tables[t][i] = (prev >> 8) ^ tables[0][(prev & 0xff) as usize];
            i += 1;
        }
        t += 1;
    }
    tables
};

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc32 {
    /// A fresh checksum.
    #[must_use]
    pub const fn new() -> Self {
        Self { state: 0xffff_ffff }
    }

    /// Fold in more bytes.
    pub fn update(&mut self, data: &[u8]) {
        let mut crc = self.state;
        let mut chunks = data.chunks_exact(8);
        for c in &mut chunks {
            let lo = u32::from_le_bytes([c[0], c[1], c[2], c[3]]) ^ crc;
            let hi = u32::from_le_bytes([c[4], c[5], c[6], c[7]]);
            crc = TABLES[7][(lo & 0xff) as usize]
                ^ TABLES[6][((lo >> 8) & 0xff) as usize]
                ^ TABLES[5][((lo >> 16) & 0xff) as usize]
                ^ TABLES[4][(lo >> 24) as usize]
                ^ TABLES[3][(hi & 0xff) as usize]
                ^ TABLES[2][((hi >> 8) & 0xff) as usize]
                ^ TABLES[1][((hi >> 16) & 0xff) as usize]
                ^ TABLES[0][(hi >> 24) as usize];
        }
        for &b in chunks.remainder() {
            crc = TABLES[0][((crc ^ u32::from(b)) & 0xff) as usize] ^ (crc >> 8);
        }
        self.state = crc;
    }

    /// The checksum of everything folded in so far.
    #[must_use]
    pub const fn finish(&self) -> u32 {
        !self.state
    }
}

/// The data track of a disc image, as the catalogue hashes it: a file and
/// how many of its bytes belong to track 1.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DataTrack {
    /// The image file.
    pub path: PathBuf,
    /// Bytes of track 1 in it (`None`: the whole file).
    pub len: Option<u64>,
}

/// Where the data track of `path` (a cue sheet, or an image) is.
///
/// A cue with one file per track names the data track's own file. A cue that
/// lays every track into one file has track 1 end where track 2's pregap
/// starts, which is where Redump cut it.
#[must_use]
pub fn data_track(path: &Path) -> Option<DataTrack> {
    let is_cue = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("cue"));
    if !is_cue {
        // A lone image is one track only when it is a plain raw image; a
        // CloneCD set or a cooked ISO holds more or less than Redump's track 1.
        let raw = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("bin") || e.eq_ignore_ascii_case("img"));
        return raw.then(|| DataTrack {
            path: path.to_path_buf(),
            len: None,
        });
    }
    let tracks = crate::library::cue_track_files(path).ok()?;
    let first = tracks.first()?;
    let len = tracks
        .get(1)
        .filter(|next| next.path == first.path)
        .map(|next| u64::from(next.file_start_sector) * psx_iso::SECTOR_BYTES as u64);
    Some(DataTrack {
        path: first.path.clone(),
        len,
    })
}

/// Size and CRC-32 of a data track, reading it once. `None` when the file
/// cannot be read. Reads in 1 MiB steps.
#[must_use]
pub fn crc_of_track(track: &DataTrack) -> Option<(u64, u32)> {
    let mut file = File::open(&track.path).ok()?;
    let total = file.metadata().ok()?.len();
    let want = track.len.map_or(total, |l| l.min(total));
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut crc = Crc32::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut left = want;
    while left > 0 {
        let n = buf.len().min(left as usize);
        file.read_exact(&mut buf[..n]).ok()?;
        crc.update(&buf[..n]);
        left -= n as u64;
    }
    Some((want, crc.finish()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"clrmamepro (
	name "Sony - PlayStation"
)

game (
	name "Wipeout (USA)"
	region "USA"
	serial "SCUS-94301"
	rom ( name "Wipeout (USA) (Track 1).bin" size 58026192 crc 52A07C67 md5 6153AD6A5C70491D3A9C82E9B4F23123 sha1 A796FB38CC98620E3CE793CE3788E5F186A34F67 serial "SCUS-94301" )
)
game (
	name "CTR - Crash Team Racing (USA)"
	region "USA"
	serial "SCUS-94426CE"
	rom ( name "CTR - Crash Team Racing (USA).bin" size 605698800 crc 9A742438 md5 AB sha1 1B serial "SCUS-94426CE" )
)
"#;

    #[test]
    fn serials_fold_to_the_collection_form() {
        assert_eq!(normalize_serial("SCUS-94426CE"), "SCUS-94426");
        assert_eq!(normalize_serial("SCES-02105/P"), "SCES-02105");
        assert_eq!(normalize_serial("sles-02375-p"), "SLES-02375");
        assert_eq!(normalize_serial("SCUS-94900"), "SCUS-94900");
        assert_eq!(normalize_serial("LSP-99012101"), "LSP-99012101");
    }

    #[test]
    fn the_catalogue_finds_a_game_by_track_hash_or_serial() {
        let dat = RedumpDat::parse(SAMPLE);
        assert_eq!(dat.len(), 2);
        let by_hash = dat.by_track(58_026_192, 0x52a0_7c67).expect("hash hit");
        assert_eq!(by_hash.name, "Wipeout (USA)");
        assert!(dat.by_track(58_026_192, 1).is_none());
        let by_serial = dat.by_serial("SCUS_94426".replace('_', "-").as_str());
        assert_eq!(
            by_serial.map(|g| g.name.as_str()),
            Some("CTR - Crash Team Racing (USA)")
        );
    }

    #[test]
    fn crc32_matches_the_standard_check_value() {
        let mut crc = Crc32::new();
        crc.update(b"123456789");
        assert_eq!(crc.finish(), 0xcbf4_3926);
        // Split updates and the 8-byte fast path agree with one pass.
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7 + 3) as u8).collect();
        let mut whole = Crc32::new();
        whole.update(&data);
        let mut parts = Crc32::new();
        parts.update(&data[..13]);
        parts.update(&data[13..]);
        assert_eq!(whole.finish(), parts.finish());
    }

    #[test]
    fn a_data_track_is_hashed_up_to_the_second_tracks_pregap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("g.bin");
        let sector = psx_iso::SECTOR_BYTES;
        // Three data sectors, then audio: track 2's INDEX 00 is at sector 3.
        let mut image = vec![0xa5u8; sector * 3];
        image.extend(vec![0x11u8; sector * 2]);
        std::fs::write(&bin, &image).expect("write");
        let cue = dir.path().join("g.cue");
        std::fs::write(
            &cue,
            "FILE \"g.bin\" BINARY\n TRACK 01 MODE2/2352\n  INDEX 01 00:00:00\n TRACK 02 AUDIO\n  INDEX 00 00:00:03\n  INDEX 01 00:00:04\n",
        )
        .expect("cue");
        let track = data_track(&cue).expect("track");
        assert_eq!(track.len, Some(3 * sector as u64));
        let (size, crc) = crc_of_track(&track).expect("crc");
        let mut expect = Crc32::new();
        expect.update(&image[..3 * sector]);
        assert_eq!((size, crc), (3 * sector as u64, expect.finish()));
    }
}
