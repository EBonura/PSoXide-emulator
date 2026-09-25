//! Property tests: the disc-sheet and container parsers take untrusted
//! files, so random and mutated input must come back as `Ok` or `Err`,
//! never as a panic, and never as an allocation sized from an unchecked
//! header field. Deterministic (fixed-seed PRNG) so a failure reproduces.

use std::panic::{catch_unwind, AssertUnwindSafe};

use psoxide_settings::library::{
    disc_from_cue_pieces, disc_from_cue_str, load_disc_from_ccd, parse_sbi,
};

const SECTOR: usize = psx_iso::SECTOR_BYTES;

/// xorshift64*: tiny, dependency-free, deterministic.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

/// Touch what the CD-ROM model reads from a mounted disc, at edge LBAs.
fn exercise(disc: &psx_iso::Disc) {
    let end = disc.leadout_lba();
    for lba in [
        0,
        1,
        149,
        150,
        end.saturating_sub(1),
        end,
        u32::MAX - 1,
        u32::MAX,
    ] {
        let _ = disc.read_sector_raw(lba);
        let _ = disc.read_sector_user(lba);
        let _ = disc.read_cdda_sector(lba);
        let _ = disc.track_position_for_lba(lba);
    }
    let _ = disc.sector_count();
}

/// Run `f`, turning a panic into a test failure that names the input.
fn no_panic<T>(what: &str, input: &dyn std::fmt::Debug, f: impl FnOnce() -> T) {
    if catch_unwind(AssertUnwindSafe(f)).is_err() {
        panic!("{what} panicked on input: {input:?}");
    }
}

fn msf(rng: &mut Rng) -> String {
    match rng.below(6) {
        0 => format!(
            "{}:{}:{}",
            rng.next() as u32,
            rng.next() as u32,
            rng.next() as u32
        ),
        1 => "99:59:74".to_string(),
        2 => "::".to_string(),
        3 => format!("{:02}:{:02}", rng.below(100), rng.below(60)),
        _ => format!(
            "{:02}:{:02}:{:02}",
            rng.below(100),
            rng.below(60),
            rng.below(75)
        ),
    }
}

fn random_cue(rng: &mut Rng) -> String {
    let files = ["a.bin", "\"b c.bin\"", "\"unterminated", "a.bin BINARY", ""];
    let modes = ["MODE2/2352", "AUDIO", "MODE1/2352", "", "audio"];
    let mut out = String::new();
    for _ in 0..rng.below(12) {
        let line = match rng.below(6) {
            0 => format!("FILE {} BINARY", rng.pick(&files)),
            1 => format!("  TRACK {} {}", rng.next() as u8, rng.pick(&modes)),
            2 => format!("    INDEX 00 {}", msf(rng)),
            3 => format!("    INDEX 01 {}", msf(rng)),
            4 => format!("    PREGAP {}", msf(rng)),
            _ => {
                let len = rng.below(20);
                String::from_utf8_lossy(&rng.bytes(len)).into_owned()
            }
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// A file body a CUE might point at: usually whole sectors, sometimes not,
/// sometimes starting with a sync pattern so the pregap detector runs.
fn random_bin(rng: &mut Rng) -> Vec<u8> {
    let sectors = rng.below(6);
    let extra = if rng.below(4) == 0 {
        rng.below(SECTOR)
    } else {
        0
    };
    let mut bytes = vec![0u8; sectors * SECTOR + extra];
    if bytes.len() >= 16 && rng.below(2) == 0 {
        bytes[1..11].fill(0xFF);
        bytes[12] = rng.next() as u8;
        bytes[13] = rng.next() as u8;
        bytes[14] = rng.next() as u8;
    }
    bytes
}

#[test]
fn cue_sheets_never_panic() {
    let mut rng = Rng(0x5eed_c0e5);
    for _ in 0..20_000 {
        let cue = random_cue(&mut rng);
        let bin = random_bin(&mut rng);
        no_panic("disc_from_cue_str", &cue, || {
            if let Ok(disc) = disc_from_cue_str(&cue, &mut |_| Ok(bin.clone())) {
                exercise(&disc);
            }
        });
        let piece_sizes: Vec<Vec<u8>> = (0..4).map(|_| random_bin(&mut rng)).collect();
        no_panic("disc_from_cue_pieces", &cue, || {
            if let Ok(disc) =
                disc_from_cue_pieces(&cue, |n| Ok(piece_sizes[n as usize % 4].clone()))
            {
                exercise(&disc);
            }
        });
    }
}

#[test]
fn cue_msf_fields_saturate_instead_of_overflowing() {
    // 4294967295 minutes overflowed `m * 60 * 75` (a panic in debug builds,
    // a wrapped nonsense LBA in release).
    let cue = "FILE a.bin BINARY\nTRACK 01 MODE2/2352\nPREGAP 4294967295:00:00\n\
               INDEX 01 00:00:00\nTRACK 02 AUDIO\nPREGAP 4294967295:59:74\nINDEX 01 00:00:01\n";
    let bin = vec![0u8; 4 * SECTOR];
    no_panic("disc_from_cue_str", &cue, || {
        if let Ok(disc) = disc_from_cue_str(cue, &mut |_| Ok(bin.clone())) {
            exercise(&disc);
        }
    });
}

fn random_ccd(rng: &mut Rng) -> String {
    let keys = ["Point", "Control", "PLBA", "INDEX 0", "INDEX 1", "junk"];
    let mut out = String::new();
    for _ in 0..rng.below(16) {
        match rng.below(4) {
            0 => out.push_str(&format!("[Entry {}]\n", rng.below(8))),
            1 => out.push_str(&format!("[TRACK {}]\n", rng.below(256))),
            _ => {
                let value = match rng.below(4) {
                    0 => format!("0x{:x}", rng.next() as u32),
                    1 => format!("{}", rng.next() as i32),
                    2 => format!("{}", rng.below(8)),
                    _ => "0xa2".to_string(),
                };
                out.push_str(&format!("{}={}\n", rng.pick(&keys), value));
            }
        }
    }
    out
}

#[test]
fn ccd_sheets_never_panic() {
    let dir = tempfile::TempDir::new().unwrap();
    let ccd = dir.path().join("disc.ccd");
    let img = dir.path().join("disc.img");
    let mut rng = Rng(0xcc_d15c);
    for _ in 0..3_000 {
        let sheet = random_ccd(&mut rng);
        std::fs::write(&ccd, &sheet).unwrap();
        std::fs::write(&img, random_bin(&mut rng)).unwrap();
        no_panic("load_disc_from_ccd", &sheet, || {
            if let Ok(disc) = load_disc_from_ccd(&ccd) {
                exercise(&disc);
            }
        });
    }
}

#[test]
fn sbi_files_never_panic() {
    let mut rng = Rng(0x5b1);
    for _ in 0..50_000 {
        let mut bytes = b"SBI\0".to_vec();
        if rng.below(8) == 0 {
            bytes.clear();
        }
        let len = rng.below(64);
        bytes.extend(rng.bytes(len));
        // Bias record types toward the valid ones so the parser gets deep.
        for i in (7..bytes.len()).step_by(4) {
            if rng.below(2) == 0 {
                bytes[i] = 1 + rng.below(3) as u8;
            }
        }
        no_panic("parse_sbi", &bytes, || {
            let _ = parse_sbi(&bytes);
        });
    }
}

/// ECM is only reachable through a `.ccd` whose `.img` is missing, so drive
/// the decoder through that path with random and truncated streams.
#[test]
fn ecm_containers_never_panic() {
    let dir = tempfile::TempDir::new().unwrap();
    let ccd = dir.path().join("disc.ccd");
    std::fs::write(&ccd, "[Entry 0]\nPoint=0x01\nControl=0x04\nPLBA=0\n").unwrap();
    let ecm = dir.path().join("disc.img.ecm");
    let mut rng = Rng(0xec_0ec);
    for _ in 0..5_000 {
        let mut stream = b"ECM\0".to_vec();
        for _ in 0..rng.below(6) {
            // A record header: type bits, count bits, continuation bytes.
            let continuation = rng.below(6);
            let mut header = rng.next() as u8;
            for _ in 0..continuation {
                stream.push(header | 0x80);
                header = rng.next() as u8;
            }
            stream.push(header & 0x7F);
            let len = rng.below(3000);
            stream.extend(rng.bytes(len));
        }
        if rng.below(3) == 0 {
            stream.extend([0xFC, 0xFF, 0xFF, 0xFF, 0x3F]);
            let len = rng.below(6);
            stream.extend(rng.bytes(len));
        }
        std::fs::write(&ecm, &stream).unwrap();
        no_panic("ecm decode", &stream.len(), || {
            let _ = load_disc_from_ccd(&ccd);
        });
    }
}

/// The loader hands each file's buffer to its first track instead of
/// copying; every track must still hold exactly its slice of its file.
#[test]
fn cue_tracks_hold_exactly_their_file_extents() {
    let mut rng = Rng(0x7ac5);
    let a: Vec<u8> = rng.bytes(10 * SECTOR);
    let b: Vec<u8> = rng.bytes(4 * SECTOR);
    let cue = "FILE \"a.bin\" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:00\n\
               TRACK 02 AUDIO\n    INDEX 00 00:00:05\n    INDEX 01 00:00:06\n\
               TRACK 03 AUDIO\n    INDEX 01 00:00:08\n\
               FILE \"b.bin\" BINARY\n  TRACK 04 AUDIO\n    INDEX 01 00:00:00\n";
    let disc = disc_from_cue_str(cue, &mut |path| {
        Ok(if path.ends_with("a.bin") {
            a.clone()
        } else {
            b.clone()
        })
    })
    .unwrap();
    let expect: [(u8, &[u8]); 4] = [
        (1, &a[..5 * SECTOR]),
        (2, &a[5 * SECTOR..8 * SECTOR]),
        (3, &a[8 * SECTOR..]),
        (4, &b[..]),
    ];
    for (number, bytes) in expect {
        let source = &disc.track(number).unwrap().source;
        let mut held = vec![0u8; source.len() as usize];
        assert!(source.read_at(0, &mut held));
        assert_eq!(held, bytes, "track {number}");
    }
}
