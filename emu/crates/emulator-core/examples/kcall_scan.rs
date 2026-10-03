//! Static scan of a disc or executable for BIOS calls.
//!
//! Lists every A0h/B0h/C0h function call site, every SYSCALL and every
//! kernel patch routine found in the code, for the census in
//! `compat/games.toml`. It reads the owner's own discs and prints facts
//! only (function numbers, file names, offsets).
//!
//! ```bash
//! cargo run -p emulator-core --release --example kcall_scan -- \
//!     [--toml] <disc.cue | disc.ccd | disc.bin | program.exe>...
//! ```
//!
//! A disc is walked through its ISO9660 tree and every file is scanned:
//! executables with their header's load address, other files (overlays)
//! as raw words at file offsets. Images the disc loader cannot open, such
//! as `.img.ecm`, are not supported.
//!
//! What counts as a call site (psx-spx "BIOS Function Summary": call 00A0h,
//! 00B0h or 00C0h with the function number in R9): a register loaded with
//! A0h, B0h or C0h by `addiu`/`ori` from `$zero`, then `jr` or `jalr` on
//! that register within the next four instructions, with `$t1` (R9) set to a
//! constant within the three instructions before the load or anywhere up to
//! the jump's delay slot. A SYSCALL counts when a constant was put in `$a0` (R4), the
//! function number, within the three instructions before it. The routine
//! after a B(56h)/B(57h) call is decoded with
//! [`emulator_core::hle_patch::identify`]; when the call sits in a small
//! wrapper function that jumps on with `jr`, the routines after each `jal`
//! to that wrapper in the same executable are decoded instead.

#[path = "support/disc.rs"]
mod disc_support;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use emulator_core::hle_patch::{identify, Got, WINDOW};
use psx_iso::Disc;

const EXE_MAGIC: &[u8] = b"PS-X EXE";
const SECTOR: usize = 2048;
/// Files larger than this are media, not code.
const MAX_FILE: u32 = 1 << 30;

#[derive(Default)]
struct Findings {
    functions: BTreeSet<(u8, u32)>,
    syscalls: BTreeSet<u32>,
    patches: Vec<String>,
    /// Every B(56h)/B(57h) site: where the routine starts and what it is.
    patch_sites: Vec<String>,
    /// Files with at least one finding.
    files: Vec<String>,
}

fn main() {
    let mut toml = false;
    let mut inputs = Vec::new();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--toml" => toml = true,
            "-h" | "--help" => {
                eprintln!("usage: kcall_scan [--toml] <disc.cue|.ccd|.bin|program.exe>...");
                return;
            }
            _ => inputs.push(PathBuf::from(arg)),
        }
    }
    if inputs.is_empty() {
        eprintln!("usage: kcall_scan [--toml] <disc.cue|.ccd|.bin|program.exe>...");
        std::process::exit(2);
    }
    for input in inputs {
        match scan_input(&input) {
            Ok(found) => report(&input, &found, toml),
            Err(error) => eprintln!("{}: {error}", input.display()),
        }
    }
}

fn scan_input(path: &Path) -> Result<Findings, String> {
    let mut found = Findings::default();
    let is_exe = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"));
    if is_exe {
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        scan_file(&path.display().to_string(), &bytes, &mut found);
        return Ok(found);
    }
    if is_ecm(path) {
        return Err("ECM images are not supported; decode the image first".into());
    }
    let disc = disc_support::load_disc_path(path)?;
    let mut files = Vec::new();
    walk(&disc, &mut files)?;
    for (name, lba, size) in files {
        if size > MAX_FILE {
            continue;
        }
        if let Some(bytes) = read_extent(&disc, lba, size) {
            scan_file(&name, &bytes, &mut found);
        }
    }
    Ok(found)
}

/// Whether `path` is, or names, an ECM container: a `.ecm` file, a cue
/// sheet with an `.ecm` FILE, or a clone-CD sheet without its `.img`.
fn is_ecm(path: &Path) -> bool {
    let ext = |p: &Path| {
        p.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
    };
    match ext(path).as_deref() {
        Some("ecm") => true,
        Some("cue") => std::fs::read_to_string(path).is_ok_and(|sheet| {
            sheet.lines().any(|line| {
                line.trim_start().starts_with("FILE") && line.to_ascii_lowercase().contains(".ecm")
            })
        }),
        Some("ccd") => !path.with_extension("img").exists(),
        _ => false,
    }
}

/// Every file on the disc as `(path, first sector, size)`.
fn walk(disc: &Disc, out: &mut Vec<(String, u32, u32)>) -> Result<(), String> {
    let pvd = disc
        .read_sector_user(16)
        .ok_or("no primary volume descriptor")?;
    if pvd[0] != 1 || &pvd[1..6] != b"CD001" {
        return Err("not an ISO9660 disc".into());
    }
    let root = &pvd[156..156 + 34];
    let mut pending = vec![(String::new(), le32(root, 2), le32(root, 10))];
    let mut visited = BTreeSet::new();
    while let Some((prefix, lba, size)) = pending.pop() {
        if !visited.insert(lba) {
            continue;
        }
        let Some(bytes) = read_extent(disc, lba, size) else {
            continue;
        };
        for sector in bytes.chunks(SECTOR) {
            let mut at = 0;
            while at < sector.len() {
                let len = usize::from(sector[at]);
                if len < 34 || at + len > sector.len() {
                    break;
                }
                let record = &sector[at..at + len];
                at += len;
                let name_len = usize::from(record[32]);
                let Some(raw) = record.get(33..33 + name_len) else {
                    continue;
                };
                if raw == [0] || raw == [1] {
                    continue;
                }
                let name = String::from_utf8_lossy(raw);
                let name = name.split(';').next().unwrap_or_default();
                let path = format!("{prefix}\\{name}");
                if record[25] & 2 != 0 {
                    pending.push((path, le32(record, 2), le32(record, 10)));
                } else {
                    out.push((path, le32(record, 2), le32(record, 10)));
                }
            }
        }
    }
    out.sort();
    Ok(())
}

fn read_extent(disc: &Disc, lba: u32, size: u32) -> Option<Vec<u8>> {
    let sectors = (size as usize).div_ceil(SECTOR);
    let mut bytes = Vec::with_capacity(sectors * SECTOR);
    for k in 0..sectors {
        bytes.extend_from_slice(&disc.read_sector_user(lba + k as u32)?);
    }
    bytes.truncate(size as usize);
    Some(bytes)
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Scan one file's words. An executable's payload starts after its 800h
/// header and loads at the header's destination; anything else is scanned
/// from offset 0 with no address.
fn scan_file(name: &str, bytes: &[u8], found: &mut Findings) {
    let (payload, base) = if bytes.starts_with(EXE_MAGIC) && bytes.len() > 0x800 {
        (&bytes[0x800..], Some(le32(bytes, 0x18)))
    } else {
        (bytes, None)
    };
    let words: Vec<u32> = payload
        .chunks_exact(4)
        .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
        .collect();
    let hits = scan_words(&words, base, found, |i| match base {
        Some(base) => format!("{name} {:08x}", base.wrapping_add(4 * i as u32)),
        None => format!("{name} +{:x}", 4 * i),
    });
    if hits > 0 {
        found.files.push(format!("{name} ({hits} sites)"));
    }
}

fn op(w: u32) -> u32 {
    w >> 26
}
fn rs(w: u32) -> u32 {
    (w >> 21) & 31
}
fn rt(w: u32) -> u32 {
    (w >> 16) & 31
}

/// `addiu`/`ori reg, $zero, imm`: the constant put in `reg`.
fn constant_into(w: u32, reg: u32) -> Option<u32> {
    ((op(w) == 0x09 || op(w) == 0x0D) && rs(w) == 0 && rt(w) == reg).then_some(w & 0xFFFF)
}

fn is_jump_through(w: u32, reg: u32) -> bool {
    op(w) == 0 && matches!(w & 0x3F, 0x08 | 0x09) && rs(w) == reg
}

/// One BIOS call site.
struct Site {
    /// First instruction of the sequence (the table load or the `$t1` set).
    start: usize,
    /// The `jr`/`jalr`.
    jump: usize,
    table: u8,
    func: Option<u32>,
}

/// Call sites in `words`.
fn call_sites(words: &[u32]) -> Vec<Site> {
    let mut sites = Vec::new();
    for (i, &w) in words.iter().enumerate() {
        if !((op(w) == 0x09 || op(w) == 0x0D) && rs(w) == 0) {
            continue;
        }
        let table = match w & 0xFFFF {
            0xA0 => 0,
            0xB0 => 1,
            0xC0 => 2,
            _ => continue,
        };
        let reg = rt(w);
        if reg == 0 {
            continue;
        }
        let Some(jump) =
            (i + 1..(i + 5).min(words.len())).find(|&j| is_jump_through(words[j], reg))
        else {
            continue;
        };
        let lo = i.saturating_sub(3);
        let hi = (jump + 2).min(words.len());
        let t1 = (lo..hi)
            .rev()
            .find(|&k| constant_into(words[k], 9).is_some());
        sites.push(Site {
            start: t1.map_or(i, |k| k.min(i)),
            jump,
            table,
            func: t1.and_then(|k| constant_into(words[k], 9)),
        });
    }
    sites
}

/// Where kernel patch routines start for one GetC0Table/GetB0Table site:
/// after the call's delay slot when the site calls (`jalr`), or after every
/// `jal` to it when the site is a small function that jumps on (`jr`), in
/// which case the routine follows each of that function's callers. The
/// callers can only be found in an executable, whose load address is known.
fn routine_starts(words: &[u32], site: &Site, base: Option<u32>) -> Vec<usize> {
    if words[site.jump] & 0x3F == 0x09 {
        return vec![(site.jump + 2).min(words.len())];
    }
    let Some(base) = base else {
        return Vec::new();
    };
    let target = base.wrapping_add(4 * site.start as u32);
    words
        .iter()
        .enumerate()
        .filter(|&(_, &w)| op(w) == 0x03 && (w & 0x03FF_FFFF) << 2 == target & 0x0FFF_FFFF)
        .map(|(k, _)| (k + 2).min(words.len()))
        .collect()
}

/// Record what `words` hold; returns the number of call sites and SYSCALLs.
fn scan_words(
    words: &[u32],
    base: Option<u32>,
    found: &mut Findings,
    place: impl Fn(usize) -> String,
) -> usize {
    let sites = call_sites(words);
    let mut hits = sites.len();
    for site in &sites {
        let Some(func) = site.func else { continue };
        found.functions.insert((site.table, func));
        let Some(got) = (site.table == 1)
            .then(|| Got::from_function(func))
            .flatten()
        else {
            continue;
        };
        let starts = routine_starts(words, site, base);
        if starts.is_empty() {
            found.patch_sites.push(format!(
                "B({func:02X}h) wrapper at {} has no caller in this file",
                place(site.start)
            ));
        }
        for after in starts {
            let window = &words[after..(after + WINDOW).min(words.len())];
            let routines = identify(got, window);
            let names: Vec<&str> = routines.iter().map(|r| r.name()).collect();
            found.patch_sites.push(format!(
                "B({func:02X}h) routine at {}: {}",
                place(after),
                if names.is_empty() {
                    "unrecognised".to_string()
                } else {
                    names.join(", ")
                }
            ));
            if routines.is_empty() {
                found
                    .patches
                    .push(format!("unrecognised at {}", place(after)));
            }
            for routine in routines {
                let name = routine.name().to_string();
                if !found.patches.contains(&name) {
                    found.patches.push(name);
                }
            }
        }
    }
    for (i, &w) in words.iter().enumerate() {
        if op(w) == 0 && w & 0x3F == 0x0C && w >> 6 & 0xF_FFFF == 0 {
            let lo = i.saturating_sub(3);
            // A bare SYSCALL word is also the number 12 in data; only one
            // with a constant function number counts.
            if let Some(number) = (lo..i).rev().find_map(|k| constant_into(words[k], 4)) {
                found.syscalls.insert(number);
                hits += 1;
            }
        }
    }
    hits
}

fn label(table: u8, func: u32) -> String {
    format!("{}({func:02X}h)", ['A', 'B', 'C'][usize::from(table)])
}

fn report(input: &Path, found: &Findings, toml: bool) {
    let functions: Vec<String> = found.functions.iter().map(|&(t, f)| label(t, f)).collect();
    let syscalls: Vec<String> = found
        .syscalls
        .iter()
        .map(|n| format!("SYS({n:02X}h)"))
        .collect();
    if toml {
        let quote = |items: &[String]| {
            items
                .iter()
                .map(|s| format!("\"{s}\""))
                .collect::<Vec<_>>()
                .join(", ")
        };
        println!("# {}", input.display());
        println!("kernel_functions_static = [{}]", quote(&functions));
        println!("syscalls_static = [{}]", quote(&syscalls));
        println!("kernel_patches_static = [{}]", quote(&found.patches));
        return;
    }
    println!("{}", input.display());
    for file in &found.files {
        println!("  file      {file}");
    }
    println!("  functions {}", functions.join(" "));
    println!("  syscalls  {}", syscalls.join(" "));
    for site in &found.patch_sites {
        println!("  patch     {site}");
    }
}
