//! Measures how well each cover-lookup route finds a games folder.
//!
//! `cargo run --release -p psoxide-settings --example cover_probe -- <games folder> <redump.dat>`
//!
//! Reads the folder (never writes), lists one game per cue/bin, reads each
//! disc's boot serial, hashes its data track and looks both up in the Redump
//! catalogue, then asks the cover collections (with `curl --head`) whether
//! each route's picture exists. Prints one row per game and the hit rate of
//! every route.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use psoxide_settings::game_group::{group_games, FoundFile};
use psoxide_settings::library::{cue_referenced_files, disc_boot_serial};
use psoxide_settings::redump::{crc_of_track, data_track, RedumpDat};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn exists(url: &str) -> bool {
    Command::new("curl")
        .args(["-sfI", "--max-time", "20", "-o", "/dev/null", url])
        .status()
        .is_ok_and(|s| s.success())
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn libretro(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| if "&*/:`<>?\\|\"".contains(c) { '_' } else { c })
        .collect();
    format!(
        "https://raw.githubusercontent.com/libretro-thumbnails/Sony_-_PlayStation/master/Named_Boxarts/{}.png",
        encode(&clean)
    )
}

fn xlenore(serial: &str) -> String {
    format!("https://raw.githubusercontent.com/xlenore/psx-covers/main/covers/default/{serial}.jpg")
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (root, dat_path) = (PathBuf::from(&args[1]), PathBuf::from(&args[2]));
    let dat = RedumpDat::parse(&std::fs::read_to_string(dat_path).expect("dat"));
    let mut files = Vec::new();
    walk(&root, &mut files);
    let found: Vec<FoundFile> = files
        .iter()
        .filter(|p| {
            matches!(
                p.extension()
                    .and_then(|e| e.to_str())
                    .map(str::to_lowercase)
                    .as_deref(),
                Some("cue" | "bin" | "iso" | "ccd")
            )
        })
        .map(|p| {
            let path = p.to_string_lossy().replace('\\', "/");
            if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("cue")) {
                let refs = cue_referenced_files(p)
                    .unwrap_or_default()
                    .iter()
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
                    .collect();
                FoundFile::sheet(path, refs)
            } else {
                FoundFile::plain(path)
            }
        })
        .collect();
    let games = group_games(&found);
    println!("{} files, {} games", found.len(), games.len());
    let mut tally: HashMap<&str, (usize, usize)> = HashMap::new();
    let mut hit = |route: &'static str, ok: bool| {
        let e = tally.entry(route).or_default();
        e.1 += 1;
        e.0 += usize::from(ok);
    };
    for g in &games {
        let entry = Path::new(&g.entry);
        let stem = psoxide_settings::game_group::stem_of(&g.entry);
        let stem = psoxide_settings::game_group::strip_track(stem).to_string();
        let serial = disc_boot_serial(entry);
        let t0 = std::time::Instant::now();
        let hashed = data_track(entry).and_then(|t| crc_of_track(&t));
        let secs = t0.elapsed().as_secs_f32();
        let by_hash = hashed.and_then(|(s, c)| dat.by_track(s, c));
        let by_serial = serial.as_deref().and_then(|s| dat.by_serial(s));
        let x_serial = serial.as_deref().is_some_and(|s| exists(&xlenore(s)));
        let l_stem = exists(&libretro(&stem));
        let l_dat_hash = by_hash.is_some_and(|d| exists(&libretro(&d.name)));
        let l_dat_serial = by_serial.is_some_and(|d| exists(&libretro(&d.name)));
        let x_dat = by_hash.is_some_and(|d| exists(&xlenore(&d.serial)));
        let exact_name = dat.by_name(&stem).is_some();
        hit("serial read from SYSTEM.CNF", serial.is_some());
        hit("DAT knows the serial", by_serial.is_some());
        hit("hash found in DAT", by_hash.is_some());
        hit("xlenore by serial", x_serial);
        hit("libretro by file name", l_stem);
        hit("libretro by DAT name (serial)", l_dat_serial);
        hit("libretro by DAT name (hash)", l_dat_hash);
        hit("xlenore by DAT serial (hash)", x_dat);
        let any = x_serial || l_dat_serial || l_dat_hash || x_dat || l_stem;
        hit("ANY route (chain)", any);
        let before = x_serial || l_stem;
        hit("BEFORE this change (serial, then file name)", before);
        println!(
            "{:<58} serial={:<11} hash={:<5} {:>5.1}s  x:{} lname:{} lserial:{} lhash:{} xhash:{} exactname:{}  -> {}",
            truncate(&g.title, 58),
            serial.as_deref().unwrap_or("-"),
            by_hash.map_or("miss", |_| "hit"),
            secs,
            u8::from(x_serial),
            u8::from(l_stem),
            u8::from(l_dat_serial),
            u8::from(l_dat_hash),
            u8::from(x_dat),
            u8::from(exact_name),
            if any { "ART" } else { "none" }
        );
    }
    println!();
    let mut rows: Vec<_> = tally.into_iter().collect();
    rows.sort();
    for (route, (n, of)) in rows {
        println!("{route:<48} {n:>3}/{of:<3}");
    }
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}
