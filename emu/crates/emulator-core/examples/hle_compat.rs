//! HLE BIOS compatibility runner for commercial discs.
//!
//! Reads the fact list in `compat/games.toml`, finds the matching discs in
//! the given directories by hash (boot executable sha256, or the data
//! file's sha256 for images the loader cannot open), boots each one through
//! the HLE path with no BIOS for a fixed number of frames, and reports:
//! whether the boot reached the executable entry, the first unimplemented
//! BIOS call, the stubbed calls it relied on, frames run, emulation speed as
//! a multiple of real time (no limiter), and the final display hash.
//!
//! ```bash
//! cargo run -p emulator-core --release --example hle_compat -- \
//!     --games-dir "/path/to/your/discs" --frames 1800 [--only crash] \
//!     [--input-tape run.pxtape | --pad-pulses 0x0008@600+8,...] \
//!     [--strict] [--json report.json] \
//!     [--shots <dir>]
//! ```
//!
//! `--shots` writes each game's final display as `<id>.ppm`. That is game
//! imagery: keep the directory local.
//!
//! Every run uses the same fixed setup so results compare across builds:
//! a digital pad on port 1 (buttons from `--input-tape`, one sample per
//! VBlank, released after the tape ends), a formatted empty memory card on
//! port 1, and no frame limiter. Discs that are not present are listed as
//! missing; images the disc loader cannot read (for example `.img.ecm`) are
//! skipped.
//!
//! Dev-only parity mode: with `--parity` and `PSOXIDE_PARITY_BIOS=<path>`
//! set, each disc is also cold-booted through that real BIOS to its EXE
//! entry, and the entry state is diffed against the HLE boot. The BIOS is
//! read only from that variable and never leaves this process; this mode
//! is for private verification with your own BIOS dump and is not reachable
//! from the frontend.
//!
//! Dev-only reference mode: `--reference` (also reading
//! `PSOXIDE_PARITY_BIOS`) runs each disc a second time through that real
//! BIOS with the same input and memory card, counting frames from the EXE
//! entry so both runs line up, and reports its display hash next to the
//! HLE one (with `--shots`, as `<id>.bios.ppm`). It shows how far the game
//! gets with a real kernel, which is what the HLE run is judged against.

#[path = "support/args.rs"]
mod args_support;
#[path = "support/disc.rs"]
mod disc_support;
#[path = "support/pad.rs"]
mod pad_support;

use std::path::{Path, PathBuf};
use std::time::Instant;

use emulator_core::system_cnf::{load_disc_boot, read_file};
use emulator_core::{fast_boot_disc_with_hle, read_tape, Bus, ButtonState, Cpu, PadSample};
use psx_iso::Disc;
use sha2::{Digest, Sha256};

const CPU_HZ: f64 = 33_868_800.0;
/// Frames between display-hash samples for `distinct_display_hashes`.
const HASH_EVERY: u64 = 60;
/// Instruction budget per requested frame before a run is declared stuck.
const STEPS_PER_FRAME_CAP: u64 = 1_000_000;
/// Instruction budget for a real-BIOS cold boot to reach the EXE entry.
const PARITY_BOOT_STEP_CAP: u64 = 800_000_000;

struct Game {
    id: String,
    title: String,
    disc_sha256: String,
    ecm_sha256: String,
    boot_exe_sha256: String,
}

enum Found {
    Disc(PathBuf),
    Unreadable(PathBuf, String),
}

#[derive(serde::Serialize)]
struct Report {
    schema: &'static str,
    frames: u64,
    input_tape: Option<String>,
    frame_limiter: bool,
    results: Vec<GameResult>,
}

#[derive(serde::Serialize, Default)]
struct GameResult {
    id: String,
    title: String,
    /// `ran`, `missing`, `skipped` or `boot_failed`.
    status: String,
    detail: Option<String>,
    reached_entry: bool,
    first_unimplemented: Option<String>,
    unimplemented: Vec<String>,
    stubbed: Vec<String>,
    kernel_patches: Vec<String>,
    stop_reason: Option<String>,
    frames: u64,
    instructions: u64,
    cycles: u64,
    emulated_seconds: f64,
    host_seconds: f64,
    speed_x_realtime: f64,
    display_hash: Option<String>,
    distinct_display_hashes: usize,
    parity: Option<Vec<ParityField>>,
    /// Real-BIOS run (dev-only `--reference`).
    reference: Option<Reference>,
}

#[derive(serde::Serialize)]
struct Reference {
    stop_reason: String,
    frames: u64,
    display_hash: String,
    distinct_display_hashes: usize,
}

#[derive(serde::Serialize)]
struct ParityField {
    field: &'static str,
    real_bios: String,
    hle: String,
    matches: bool,
}

fn main() {
    let mut list = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../compat/games.toml"
    ));
    let mut dirs = Vec::new();
    let mut frames = 1800u64;
    let mut only = Vec::new();
    let mut tape_path = None;
    let mut json = None;
    let mut strict = false;
    let mut parity = false;
    let mut reference = false;
    let mut shots: Option<PathBuf> = None;
    let mut pulses: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--list" => list = args_support::take_path(&mut args, "--list"),
            "--games-dir" => dirs.push(args_support::take_path(&mut args, "--games-dir")),
            "--frames" => frames = args_support::take_u64(&mut args, "--frames"),
            "--only" => only.push(args_support::take_string(&mut args, "--only")),
            "--input-tape" => tape_path = Some(args_support::take_path(&mut args, "--input-tape")),
            "--pad-pulses" => pulses = Some(args_support::take_string(&mut args, "--pad-pulses")),
            "--json" => json = Some(args_support::take_path(&mut args, "--json")),
            "--strict" => strict = true,
            "--parity" => parity = true,
            "--reference" => reference = true,
            "--shots" => shots = Some(args_support::take_path(&mut args, "--shots")),
            other => panic!("unknown argument {other}; see the header of hle_compat.rs"),
        }
    }
    assert!(!dirs.is_empty(), "pass at least one --games-dir");
    let parity_bios = (parity || reference).then(|| {
        let path = std::env::var_os("PSOXIDE_PARITY_BIOS")
            .expect("--parity/--reference need PSOXIDE_PARITY_BIOS=<path to your BIOS dump>");
        std::fs::read(&path).expect("PSOXIDE_PARITY_BIOS readable")
    });
    // A scripted pulse list (mask@vblank+frames, the frontend's
    // --pad-pulses format) becomes a one-sample-per-VBlank tape.
    let tape = match (&tape_path, &pulses) {
        (Some(path), _) => Some(read_tape(path).unwrap_or_else(|e| panic!("{e}"))),
        (None, Some(text)) => {
            let pulses = pad_support::parse_pad_pulses(text).unwrap_or_else(|e| panic!("{e}"));
            Some(
                (0..frames)
                    .map(|vb| PadSample::from_buttons(pad_support::effective_mask(0, &pulses, vb)))
                    .collect(),
            )
        }
        (None, None) => None,
    };

    let games: Vec<Game> = load_list(&list)
        .into_iter()
        .filter(|game| only.is_empty() || only.contains(&game.id))
        .collect();
    let found = find_discs(&games, &dirs);

    let mut results = Vec::new();
    for (game, found) in games.iter().zip(found) {
        let mut result = GameResult {
            id: game.id.clone(),
            title: game.title.clone(),
            ..GameResult::default()
        };
        match found {
            None => result.status = "missing".into(),
            Some(Found::Unreadable(path, error)) => {
                result.status = "skipped".into();
                result.detail = Some(format!("{}: {error}", file_name(&path)));
            }
            Some(Found::Disc(path)) => match disc_support::load_disc_path(&path) {
                Ok(disc) => {
                    eprintln!("[hle-compat] {} <- {}", game.id, file_name(&path));
                    let shot = shots
                        .as_ref()
                        .map(|dir| dir.join(format!("{}.ppm", game.id)));
                    run_hle(
                        &mut result,
                        disc.clone(),
                        frames,
                        tape.as_deref(),
                        strict,
                        shot,
                    );
                    if let (true, Some(bios)) = (reference, parity_bios.as_ref()) {
                        let shot = shots
                            .as_ref()
                            .map(|dir| dir.join(format!("{}.bios.ppm", game.id)));
                        result.reference =
                            run_reference(bios, disc.clone(), frames, tape.as_deref(), shot);
                    }
                    if let (true, Some(bios)) = (parity, parity_bios.as_ref()) {
                        result.parity = parity_diff(bios, &disc);
                        if result.parity.is_none() {
                            eprintln!("[hle-compat] parity unavailable for {}", game.id);
                        }
                    }
                }
                Err(error) => {
                    result.status = "skipped".into();
                    result.detail = Some(error);
                }
            },
        }
        results.push(result);
    }

    print_table(&results);
    if let Some(path) = json {
        let report = Report {
            schema: "psoxide-hle-compat/1",
            frames,
            input_tape: tape_path.as_deref().map(file_name).or(pulses),
            frame_limiter: false,
            results,
        };
        let text = serde_json::to_string_pretty(&report).expect("serialize report");
        std::fs::write(&path, text + "\n").expect("write --json");
        eprintln!("[hle-compat] wrote {}", path.display());
    }
}

fn load_list(path: &Path) -> Vec<Game> {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let table: toml::Table = text
        .parse()
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let field = |entry: &toml::Value, key: &str| {
        entry
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    table
        .get("game")
        .and_then(|v| v.as_array())
        .map(|entries| {
            entries
                .iter()
                .map(|entry| Game {
                    id: field(entry, "id"),
                    title: field(entry, "title"),
                    disc_sha256: field(entry, "disc_sha256"),
                    ecm_sha256: field(entry, "ecm_sha256"),
                    boot_exe_sha256: field(entry, "boot_exe_sha256"),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Match every disc sheet under `dirs` against the list. A loadable disc is
/// identified by its boot executable's sha256; one that fails to load is
/// identified by its data file's sha256 (`disc_sha256`, or `ecm_sha256` for
/// an ECM container) so it can be reported as skipped rather than missing.
fn find_discs(games: &[Game], dirs: &[PathBuf]) -> Vec<Option<Found>> {
    let mut found: Vec<Option<Found>> = games.iter().map(|_| None).collect();
    for dir in dirs {
        let sheets = disc_support::discover_cue_files(dir).unwrap_or_else(|e| panic!("{e}"));
        for sheet in sheets {
            if found.iter().all(Option::is_some) {
                return found;
            }
            match disc_support::load_disc_path(&sheet) {
                Ok(disc) => {
                    let Some(hash) = boot_exe_sha256(&disc) else {
                        continue;
                    };
                    if let Some(i) = games.iter().position(|g| g.boot_exe_sha256 == hash) {
                        found[i].get_or_insert(Found::Disc(sheet));
                    }
                }
                Err(error) => {
                    let Some(hash) = first_data_file(&sheet).and_then(|p| file_sha256(&p)) else {
                        continue;
                    };
                    let known = |g: &Game| g.disc_sha256 == hash || g.ecm_sha256 == hash;
                    if let Some(i) = games.iter().position(known) {
                        found[i].get_or_insert(Found::Unreadable(sheet, error));
                    }
                }
            }
        }
    }
    found
}

fn boot_exe_sha256(disc: &Disc) -> Option<String> {
    let boot = load_disc_boot(disc).ok()?;
    let bytes = read_file(disc, &boot.cnf.boot_path).ok()?;
    Some(hex(&Sha256::digest(&bytes)))
}

/// The data file to hash: a cue sheet's first `FILE`, or the clone-CD image
/// (plain or ECM-packed) next to a `.ccd`.
fn first_data_file(sheet: &Path) -> Option<PathBuf> {
    let ext = sheet.extension()?.to_str()?.to_ascii_lowercase();
    if ext == "ccd" {
        let img = sheet.with_extension("img");
        let ecm = sheet.with_extension("img.ecm");
        return [img, ecm].into_iter().find(|p| p.exists());
    }
    let text = std::fs::read_to_string(sheet).ok()?;
    let line = text
        .lines()
        .find(|l| l.trim_start().to_ascii_uppercase().starts_with("FILE"))?;
    let name = line.split('"').nth(1)?;
    Some(sheet.parent()?.join(name))
}

fn file_sha256(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(hex(&hasher.finalize()))
}

fn run_hle(
    result: &mut GameResult,
    disc: Disc,
    frames: u64,
    tape: Option<&[PadSample]>,
    strict: bool,
    shot: Option<PathBuf>,
) {
    let mut bus = Bus::new_without_bios();
    bus.set_hle_strict(strict);
    let mut cpu = Cpu::new();
    if let Err(error) = fast_boot_disc_with_hle(&mut bus, &mut cpu, &disc, true) {
        result.status = "boot_failed".into();
        result.detail = Some(format!("{error:?}"));
        return;
    }
    result.status = "ran".into();
    result.reached_entry = true;
    bus.cdrom.insert_disc(Some(disc));
    bus.attach_digital_pad_port1();
    bus.attach_memcard_port1(Vec::new());

    let start = Instant::now();
    let (stop, last_vblank, steps, hashes) = run_frames(&mut cpu, &mut bus, frames, tape);
    let host = start.elapsed().as_secs_f64();

    result.stop_reason = Some(stop);
    result.frames = last_vblank;
    result.instructions = steps;
    result.cycles = bus.cycles();
    result.emulated_seconds = bus.cycles() as f64 / CPU_HZ;
    result.host_seconds = host;
    result.speed_x_realtime = if host > 0.0 {
        result.emulated_seconds / host
    } else {
        0.0
    };
    result.display_hash = Some(format!("0x{:016x}", bus.gpu.display_hash().0));
    result.distinct_display_hashes = hashes.len();
    result.first_unimplemented = bus.hle_bios_first_unimplemented().map(ToString::to_string);
    result.kernel_patches = bus
        .hle_bios_patches()
        .iter()
        .map(|(name, _)| name.clone())
        .collect();
    if let Some(path) = shot {
        write_ppm(&bus, &path);
    }
    for record in bus.hle_bios_records() {
        let line = format!(
            "{}({:02X}h) {} x{}",
            record.table.letter(),
            record.func,
            record.name,
            record.count
        );
        match record.outcome {
            emulator_core::hle_bios::Outcome::Unimplemented => result.unimplemented.push(line),
            _ => result.stubbed.push(line),
        }
    }
}

/// Run `frames` VBlanks from the current state (counted from here),
/// feeding the tape one sample per VBlank. Returns the stop reason, frames
/// run, instructions, and the display hashes sampled every
/// [`HASH_EVERY`] frames.
fn run_frames(
    cpu: &mut Cpu,
    bus: &mut Bus,
    frames: u64,
    tape: Option<&[PadSample]>,
) -> (String, u64, u64, std::collections::BTreeSet<u64>) {
    apply_sample(bus, tape, 0);
    let cap = frames
        .saturating_mul(STEPS_PER_FRAME_CAP)
        .saturating_add(10_000_000);
    let mut hashes = std::collections::BTreeSet::new();
    let base_vblank = bus.irq().raise_counts()[0];
    let mut last_vblank = 0u64;
    let mut steps = 0u64;
    let stop = loop {
        if let Err(error) = cpu.step(bus) {
            break format!("cpu_error: {error}");
        }
        steps += 1;
        bus.run_spu_to_current_cycle();
        if bus.spu.audio_queue_len() != 0 {
            let _ = bus.spu.drain_audio();
        }
        let vblank = bus.irq().raise_counts()[0] - base_vblank;
        if vblank != last_vblank {
            last_vblank = vblank;
            apply_sample(bus, tape, vblank);
            if vblank.is_multiple_of(HASH_EVERY) {
                hashes.insert(bus.gpu.display_hash().0);
            }
            if vblank >= frames {
                break "frames".to_string();
            }
        }
        if steps >= cap {
            break "step_cap".to_string();
        }
    };
    (stop, last_vblank, steps, hashes)
}

/// Dev-only: cold-boot `disc` through the real BIOS to the EXE entry, then
/// run the same frames and input as the HLE run.
fn run_reference(
    bios: &[u8],
    disc: Disc,
    frames: u64,
    tape: Option<&[PadSample]>,
    shot: Option<PathBuf>,
) -> Option<Reference> {
    let entry = load_disc_boot(&disc).ok()?.exe.initial_pc & 0x1FFF_FFFF;
    let mut bus = Bus::new(bios.to_vec()).ok()?;
    let mut cpu = Cpu::new();
    bus.cdrom.insert_disc(Some(disc));
    bus.attach_digital_pad_port1();
    bus.attach_memcard_port1(Vec::new());
    let mut reached = false;
    for _ in 0..PARITY_BOOT_STEP_CAP {
        if cpu.pc() & 0x1FFF_FFFF == entry {
            reached = true;
            break;
        }
        cpu.step(&mut bus).ok()?;
        bus.run_spu_to_current_cycle();
        if bus.spu.audio_queue_len() != 0 {
            let _ = bus.spu.drain_audio();
        }
    }
    if !reached {
        eprintln!("[hle-compat] reference: real BIOS did not reach the EXE entry");
        return None;
    }
    let (stop, frames_run, _, hashes) = run_frames(&mut cpu, &mut bus, frames, tape);
    if let Some(path) = shot {
        write_ppm(&bus, &path);
    }
    Some(Reference {
        stop_reason: stop,
        frames: frames_run,
        display_hash: format!("0x{:016x}", bus.gpu.display_hash().0),
        distinct_display_hashes: hashes.len(),
    })
}

fn apply_sample(bus: &mut Bus, tape: Option<&[PadSample]>, frame: u64) {
    let Some(tape) = tape else {
        return;
    };
    let sample = tape.get(frame as usize).copied().unwrap_or_default();
    bus.set_port1_buttons(ButtonState::from_bits(sample.buttons));
    bus.set_port1_sticks(sample.right_x, sample.right_y, sample.left_x, sample.left_y);
}

/// Entry-state fields compared in parity mode, with the mask applied
/// before comparing (GPUSTAT keeps only the display-mode and draw bits).
fn entry_state(cpu: &Cpu, bus: &mut Bus) -> Vec<(&'static str, u32, u32)> {
    let spu = |bus: &Bus, addr: u32| u32::from(bus.spu.read16(addr));
    let ram32 = |bus: &Bus, addr: u32| {
        (0..4).fold(0u32, |acc, k| {
            acc | u32::from(bus.try_read8(addr + k).unwrap_or(0)) << (8 * k)
        })
    };
    vec![
        ("pc", cpu.pc(), u32::MAX),
        ("sp", cpu.gpr(29), u32::MAX),
        ("fp", cpu.gpr(30), u32::MAX),
        ("gp", cpu.gpr(28), u32::MAX),
        ("a0", cpu.gpr(4), u32::MAX),
        ("a1", cpu.gpr(5), u32::MAX),
        ("sr", cpu.cop0()[12], u32::MAX),
        ("i_mask", bus.read32(0x1F80_1074) & 0xFFFF, u32::MAX),
        ("dpcr", bus.debug_dma_read32(0x1F80_10F0), u32::MAX),
        ("dicr", bus.debug_dma_read32(0x1F80_10F4), u32::MAX),
        ("gpustat", bus.read32(0x1F80_1814), 0x00FF_0600),
        ("spucnt", spu(bus, 0x1F80_1DAA), u32::MAX),
        ("main_vol_l", spu(bus, 0x1F80_1D80), u32::MAX),
        ("main_vol_r", spu(bus, 0x1F80_1D82), u32::MAX),
        ("reverb_vol_l", spu(bus, 0x1F80_1D84), u32::MAX),
        ("reverb_vol_r", spu(bus, 0x1F80_1D86), u32::MAX),
        ("eon_lo", spu(bus, 0x1F80_1D98), u32::MAX),
        ("eon_hi", spu(bus, 0x1F80_1D9A), u32::MAX),
        ("reverb_base", spu(bus, 0x1F80_1DA2), u32::MAX),
        ("transfer_ctrl", spu(bus, 0x1F80_1DAC), u32::MAX),
        ("cd_vol_l", spu(bus, 0x1F80_1DB0), u32::MAX),
        ("cd_vol_r", spu(bus, 0x1F80_1DB2), u32::MAX),
        ("ext_vol_l", spu(bus, 0x1F80_1DB4), u32::MAX),
        ("ram_size_mb", ram32(bus, 0x60), u32::MAX),
    ]
}

fn parity_diff(bios: &[u8], disc: &Disc) -> Option<Vec<ParityField>> {
    let entry = load_disc_boot(disc).ok()?.exe.initial_pc & 0x1FFF_FFFF;

    let mut bus = Bus::new(bios.to_vec()).ok()?;
    let mut cpu = Cpu::new();
    bus.cdrom.insert_disc(Some(disc.clone()));
    bus.attach_digital_pad_port1();
    bus.attach_memcard_port1(Vec::new());
    let mut reached = false;
    for _ in 0..PARITY_BOOT_STEP_CAP {
        if cpu.pc() & 0x1FFF_FFFF == entry {
            reached = true;
            break;
        }
        cpu.step(&mut bus).ok()?;
        bus.run_spu_to_current_cycle();
        if bus.spu.audio_queue_len() != 0 {
            let _ = bus.spu.drain_audio();
        }
    }
    if !reached {
        eprintln!("[hle-compat] parity: real BIOS did not reach the EXE entry");
        return None;
    }
    let real = entry_state(&cpu, &mut bus);

    let mut bus = Bus::new_without_bios();
    let mut cpu = Cpu::new();
    fast_boot_disc_with_hle(&mut bus, &mut cpu, disc, true).ok()?;
    let hle = entry_state(&cpu, &mut bus);

    Some(
        real.into_iter()
            .zip(hle)
            .map(|((field, real, mask), (_, hle, _))| ParityField {
                field,
                real_bios: format!("0x{real:08x}"),
                hle: format!("0x{hle:08x}"),
                matches: real & mask == hle & mask,
            })
            .collect(),
    )
}

fn print_table(results: &[GameResult]) {
    println!(
        "{:<12} {:<11} {:<5} {:>6} {:>7} {:<18} {:>5} first unimplemented",
        "id", "status", "entry", "frames", "x-real", "display", "hashes"
    );
    for r in results {
        println!(
            "{:<12} {:<11} {:<5} {:>6} {:>7.2} {:<18} {:>5} {}",
            r.id,
            r.status,
            if r.reached_entry { "yes" } else { "no" },
            r.frames,
            r.speed_x_realtime,
            r.display_hash.as_deref().unwrap_or("-"),
            r.distinct_display_hashes,
            r.first_unimplemented
                .as_deref()
                .or(r.detail.as_deref())
                .unwrap_or("-"),
        );
        if let Some(reference) = &r.reference {
            println!(
                "    real BIOS: {} frames, display {}, {} hashes ({})",
                reference.frames,
                reference.display_hash,
                reference.distinct_display_hashes,
                reference.stop_reason
            );
        }
        if let Some(parity) = &r.parity {
            let matching = parity.iter().filter(|f| f.matches).count();
            println!("    parity: {matching}/{} entry fields match", parity.len());
            for f in parity.iter().filter(|f| !f.matches) {
                println!(
                    "    parity {:<14} real={} hle={}",
                    f.field, f.real_bios, f.hle
                );
            }
        }
    }
}

fn write_ppm(bus: &Bus, path: &Path) {
    let (rgba, w, h) = bus.gpu.display_rgba8();
    if w == 0 || h == 0 {
        return;
    }
    let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
    for px in rgba.chunks_exact(4) {
        ppm.extend_from_slice(&px[..3]);
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, ppm);
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
