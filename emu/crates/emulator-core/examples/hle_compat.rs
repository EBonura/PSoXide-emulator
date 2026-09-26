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
//!     [--shots <dir> [--shot-every N]]
//! ```
//!
//! `--hash-log <dir>` writes the display hash of every frame to
//! `<dir>/<id>.hle.txt`, one
//! `frame hash` line per VBlank, to find where two runs part.
//!
//! `--inputs compat/inputs.toml` gives games their own run length and pad
//! schedule (the one that reaches gameplay); the others use `--frames` and
//! `--pad-pulses`/`--input-tape`.
//!
//! `--save-dir <dir> --save-at N[,M...]` writes the HLE run's state at
//! those frames as `<dir>/<id>.<N>.state`; `--load-state <file>` starts the
//! HLE run from one instead of booting (frames and pulses stay counted from
//! the EXE entry). For finding an input schedule without replaying the boot
//! each time; judge the final schedule from a cold run.
//!
//! `--shots` writes each game's final display as `<id>.ppm`, and with
//! `--shot-every N` also every N frames as `<id>.<frame>.ppm`. That is game
//! imagery: keep the directory local.
//!
//! Every run uses the same fixed setup so results compare across builds:
//! a digital pad on port 1 (buttons from `--input-tape`, one sample per
//! VBlank, released after the tape ends), a formatted empty memory card on
//! port 1, and no frame limiter. Discs that are not present are listed as
//! missing; images the disc loader cannot read (for example `.img.ecm`) are
//! skipped.
//!
//! PSoXide has no BIOS path: every run is the HLE kernel. What a real
//! kernel did with the same input, recorded before BIOS support was
//! removed, is kept as data in `compat/reference/` and `compat/reference.toml`.

#[path = "support/args.rs"]
mod args_support;
#[path = "support/disc.rs"]
mod disc_support;
#[path = "support/pad.rs"]
mod pad_support;

use std::path::{Path, PathBuf};
use std::time::Instant;

use emulator_core::system_cnf::{load_disc_boot, read_file};
use emulator_core::{
    fast_boot_disc, read_tape, Bus, ButtonState, Cpu, EmulatorState, EmulatorStateRef, PadSample,
};
use psoxide_settings::savestate::SaveStateV1;
use psx_iso::Disc;
use sha2::{Digest, Sha256};

const CPU_HZ: f64 = 33_868_800.0;
/// Frames between display-hash samples for `distinct_display_hashes`.
const HASH_EVERY: u64 = 60;
/// Instruction budget per requested frame before a run is declared stuck.
const STEPS_PER_FRAME_CAP: u64 = 1_000_000;

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
    /// `git rev-parse --short HEAD` of the emulator tree the runner was
    /// started from, with `-dirty` for uncommitted tracked changes.
    emulator_commit: Option<String>,
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
    /// Sectors the drive delivered that the game never collected.
    cd_sectors_dropped: u64,
    /// MDEC macroblocks decoded (nonzero means FMV or MDEC images played).
    mdec_macroblocks: u64,
    /// LibCrypt sectors read from a `.sbi` next to the disc (`None`: no
    /// `.sbi` found).
    sbi_sectors: Option<usize>,
    /// This game's own schedule from `--inputs`, when it has one.
    pad_pulses: Option<String>,
    /// Shift-JIS codes the game asked B(51h) Krom2RawAdd for (HLE run).
    font_requests: Vec<String>,
    /// GetlocP queries that landed on a sector listed in the `.sbi`
    /// (counted even with `--no-sbi`, which leaves the list unapplied), and
    /// the frame of the first one.
    libcrypt_getlocp: Option<(usize, Option<u64>)>,
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
    let mut no_sbi = false;
    let mut shots: Option<PathBuf> = None;
    let mut shot_every = 0u64;
    let mut hash_log: Option<PathBuf> = None;
    let mut states = States::default();
    let mut pulses: Option<String> = None;
    let mut inputs: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--list" => list = args_support::take_path(&mut args, "--list"),
            "--games-dir" => dirs.push(args_support::take_path(&mut args, "--games-dir")),
            "--frames" => frames = args_support::take_u64(&mut args, "--frames"),
            "--only" => only.push(args_support::take_string(&mut args, "--only")),
            "--input-tape" => tape_path = Some(args_support::take_path(&mut args, "--input-tape")),
            "--pad-pulses" => pulses = Some(args_support::take_string(&mut args, "--pad-pulses")),
            "--inputs" => inputs = Some(args_support::take_path(&mut args, "--inputs")),
            "--json" => json = Some(args_support::take_path(&mut args, "--json")),
            "--strict" => strict = true,
            "--no-sbi" => no_sbi = true,
            "--shots" => shots = Some(args_support::take_path(&mut args, "--shots")),
            "--shot-every" => shot_every = args_support::take_u64(&mut args, "--shot-every"),
            "--hash-log" => hash_log = Some(args_support::take_path(&mut args, "--hash-log")),
            "--load-state" => {
                states.load = Some(args_support::take_path(&mut args, "--load-state"))
            }
            "--save-dir" => states.dir = Some(args_support::take_path(&mut args, "--save-dir")),
            "--save-at" => {
                let text = args_support::take_string(&mut args, "--save-at");
                states.save_at = text
                    .split(',')
                    .map(|f| f.trim().parse().expect("--save-at takes frame numbers"))
                    .collect();
            }
            other => panic!("unknown argument {other}; see the header of hle_compat.rs"),
        }
    }
    assert!(!dirs.is_empty(), "pass at least one --games-dir");
    // A scripted pulse list (mask@vblank+frames, the frontend's
    // --pad-pulses format) becomes a one-sample-per-VBlank tape.
    let tape = match (&tape_path, &pulses) {
        (Some(path), _) => Some(read_tape(path).unwrap_or_else(|e| panic!("{e}"))),
        (None, Some(text)) => Some(pulse_tape(text, frames)),
        (None, None) => None,
    };
    let inputs = inputs.map(|path| load_inputs(&path)).unwrap_or_default();

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
        // A per-game schedule from --inputs replaces the common one.
        let (frames, tape) = match inputs.get(&game.id) {
            Some((game_frames, text)) => {
                result.pad_pulses = Some(text.clone());
                (*game_frames, Some(pulse_tape(text, *game_frames)))
            }
            None => (frames, tape.clone()),
        };
        match found {
            None => result.status = "missing".into(),
            Some(Found::Unreadable(path, error)) => {
                result.status = "skipped".into();
                result.detail = Some(format!("{}: {error}", file_name(&path)));
            }
            Some(Found::Disc(path)) => match disc_support::load_disc_path(&path) {
                Ok(disc) => {
                    // LibCrypt discs need their .sbi; record whether one was
                    // found so a protected game without it is not mistaken
                    // for an emulator failure.
                    let sbi = match psoxide_settings::library::load_sbi_for(&path) {
                        Ok(lbas) => lbas,
                        Err(error) => {
                            eprintln!("[hle-compat] ignoring {error}");
                            None
                        }
                    };
                    result.sbi_sectors = sbi.as_ref().map(Vec::len);
                    eprintln!("[hle-compat] {} <- {}", game.id, file_name(&path));
                    let shot = shots
                        .as_ref()
                        .map(|dir| dir.join(format!("{}.ppm", game.id)));
                    let mut sbi = sbi.unwrap_or_default();
                    sbi.sort_unstable();
                    let applied = if no_sbi { Vec::new() } else { sbi.clone() };
                    let hashes = |kind: &str| {
                        hash_log
                            .as_ref()
                            .map(|dir| dir.join(format!("{}.{kind}.txt", game.id)))
                    };
                    run_hle(
                        &mut result,
                        disc,
                        (&applied, &sbi),
                        hashes("hle"),
                        frames,
                        tape.as_deref(),
                        strict,
                        shot,
                        shot_every,
                        &states,
                    );
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
            schema: "psoxide-hle-compat/3",
            emulator_commit: emulator_commit(),
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

/// A scripted pulse list (mask@vblank+frames, the frontend's --pad-pulses
/// format) as a one-sample-per-VBlank tape.
fn pulse_tape(text: &str, frames: u64) -> Vec<PadSample> {
    let pulses = pad_support::parse_pad_pulses(text).unwrap_or_else(|e| panic!("{e}"));
    (0..frames)
        .map(|vb| PadSample::from_buttons(pad_support::effective_mask(0, &pulses, vb)))
        .collect()
}

/// `--inputs`: per-game `frames` and `pulses` (compat/inputs.toml).
fn load_inputs(path: &Path) -> std::collections::BTreeMap<String, (u64, String)> {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let table: toml::Table = text
        .parse()
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    table
        .iter()
        .filter_map(|(id, entry)| {
            let frames = entry.get("frames")?.as_integer()?;
            let pulses = entry.get("pulses")?.as_str()?;
            Some((
                id.clone(),
                (frames as u64, pulses.split_whitespace().collect()),
            ))
        })
        .collect()
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
    // The `.sbi` sectors applied to the drive, and all of them listed.
    (sbi, sbi_listed): (&[u32], &[u32]),
    hash_log: Option<PathBuf>,
    frames: u64,
    tape: Option<&[PadSample]>,
    strict: bool,
    shot: Option<PathBuf>,
    shot_every: u64,
    states: &States,
) {
    let mut bus = Bus::new_without_bios();
    bus.set_hle_strict(strict);
    let mut cpu = Cpu::new();
    if let Err(error) = fast_boot_disc(&mut bus, &mut cpu, &disc) {
        result.status = "boot_failed".into();
        result.detail = Some(format!("{error:?}"));
        return;
    }
    result.status = "ran".into();
    result.reached_entry = true;
    bus.cdrom.insert_disc(Some(disc));
    bus.cdrom.set_bad_subq_sectors(sbi.to_vec());
    bus.attach_digital_pad_port1();
    bus.attach_memcard_port1(Vec::new());
    let mut start_frame = 0;
    if let Some(path) = &states.load {
        let loaded = SaveStateV1::<EmulatorState>::read_from(path)
            .unwrap_or_else(|e| panic!("--load-state {}: {e}", path.display()));
        start_frame = loaded
            .header
            .game_id
            .rsplit(':')
            .next()
            .and_then(|f| f.parse().ok())
            .expect("--load-state: not an hle_compat state (game id <id>:<frame>)");
        let mut payload = loaded.payload;
        payload.bus.restore_excluded_from(&mut bus);
        payload.bus.set_hle_strict(strict);
        payload.bus.cdrom.set_bad_subq_sectors(sbi.to_vec());
        // Save states leave the memory card out; hle_compat keeps its
        // in-memory card next to the state (absent: never written).
        let card = std::fs::read(path.with_extension("mcd")).unwrap_or_default();
        payload.bus.attach_memcard_port1(card);
        cpu = payload.cpu;
        bus = payload.bus;
        eprintln!(
            "[hle-compat] loaded {} at frame {start_frame}",
            path.display()
        );
    }
    let cd_log = cd_log_cap();
    if let Some(cap) = cd_log {
        bus.cdrom.enable_command_log(cap);
        bus.cdrom.enable_response_log(cap);
    }
    let entry_cycle = bus.cycles();
    let saves = states
        .dir
        .as_ref()
        .map(|dir| (states.save_at.as_slice(), dir.join(&result.id)));

    let mut watch = Watch {
        lbas: sbi_listed,
        seen: 0,
        first: None,
    };
    let start = Instant::now();
    let periodic = periodic_shots(shot.as_deref(), shot_every);
    let (stop, last_vblank, steps, hashes) = run_frames(
        &mut cpu,
        &mut bus,
        (start_frame, frames),
        tape,
        periodic.as_ref(),
        hash_log,
        saves,
        &mut watch,
    );
    let first_libcrypt_frame = watch.first;
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
    result.cd_sectors_dropped = bus.cdrom.dropped_sectors();
    result.mdec_macroblocks = bus.mdec.macroblocks_decoded();
    if !sbi_listed.is_empty() {
        let hits = bus
            .cdrom
            .getlocp_lbas()
            .iter()
            .filter(|lba| sbi_listed.binary_search(lba).is_ok())
            .count();
        result.libcrypt_getlocp = Some((hits, first_libcrypt_frame));
    }
    result.font_requests = bus
        .hle_font_requests()
        .iter()
        .map(|code| format!("{code:04X}"))
        .collect();
    result.first_unimplemented = bus.hle_bios_first_unimplemented().map(ToString::to_string);
    result.kernel_patches = bus
        .hle_bios_patches()
        .iter()
        .map(|(name, _)| name.clone())
        .collect();
    if let Some(path) = shot {
        write_ppm(&bus, &path);
    }
    if cd_log.is_some() {
        print_cd_log(&bus, "cd", entry_cycle);
    }
    if let Some(dir) = &states.dir {
        // Main RAM at the end of the run, for disassembling a stall.
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(dir.join(format!("{}.ram", result.id)), bus.ram());
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
    (start_frame, frames): (u64, u64),
    tape: Option<&[PadSample]>,
    periodic: Option<&(u64, PathBuf)>,
    hash_log: Option<PathBuf>,
    saves: Option<(&[u64], PathBuf)>,
    watch: &mut Watch<'_>,
) -> (String, u64, u64, std::collections::BTreeSet<u64>) {
    apply_sample(bus, tape, start_frame);
    let mut frame_hashes = hash_log.as_ref().map(|_| String::new());
    let cap = frames
        .saturating_mul(STEPS_PER_FRAME_CAP)
        .saturating_add(10_000_000);
    let mut hashes = std::collections::BTreeSet::new();
    let base_vblank = bus.irq().raise_counts()[0].wrapping_sub(start_frame);
    let mut last_vblank = start_frame;
    // Latest port-1 card image for save states (the snapshot only
    // returns bytes written since the previous one).
    let mut card: Option<Vec<u8>> = None;
    let mut steps = 0u64;
    let stop = loop {
        // Run to the next VBlank (or the step cap) in one call; the checks
        // below only act on a VBlank, and the SPU catch-up they did after
        // every instruction the next instruction does first anyway.
        let (ran, result) = cpu.run(bus, cap.saturating_sub(steps).max(1), u64::MAX, |bus| {
            bus.irq().raise_counts()[0].wrapping_sub(base_vblank) != last_vblank
        });
        steps += ran;
        if let Err(error) = result {
            bus.run_spu_to_current_cycle();
            // With --strict this is usually the first unimplemented call;
            // COP0 still holds the last exception (an unresolved one ends
            // in A(40h)).
            let c = cpu.cop0();
            break format!(
                "cpu_error: {error} (cause={:#010x} epc={:#010x} badvaddr={:#010x})",
                c[13], c[14], c[8]
            );
        }
        bus.run_spu_to_current_cycle();
        bus.spu.discard_audio();
        let vblank = bus.irq().raise_counts()[0] - base_vblank;
        if vblank != last_vblank {
            last_vblank = vblank;
            watch.check(bus, vblank);
            apply_sample(bus, tape, vblank);
            if vblank.is_multiple_of(HASH_EVERY) {
                hashes.insert(bus.gpu.display_hash().0);
            }
            if let Some(log) = frame_hashes.as_mut() {
                log.push_str(&format!("{vblank} {:016x}\n", bus.gpu.display_hash().0));
            }
            if let Some((every, base)) = periodic {
                if vblank.is_multiple_of(*every) && vblank < frames {
                    let name = format!("{}.{vblank}.ppm", base.display());
                    write_ppm(bus, Path::new(&name));
                }
            }
            if let Some((at, base)) = saves.as_ref() {
                if at.contains(&vblank) {
                    if let Some(bytes) = bus.memcard_port1_snapshot() {
                        card = Some(bytes);
                    }
                    save_state(cpu, bus, card.as_deref(), base, vblank);
                }
            }
            if status_every() != 0 && vblank.is_multiple_of(status_every()) {
                eprintln!(
                    "[status] frame {vblank} pc={:08x} ra={:08x} cd_cmds={} cd_last={:02x} getlocp={:?} cd_pops={} mdec_mb={} hash={:016x}",
                    cpu.pc(),
                    cpu.gpr(31),
                    bus.cdrom.commands_dispatched(),
                    bus.cdrom.last_command(),
                    bus.cdrom.getlocp_lbas().last(),
                    bus.cdrom.data_fifo_pops(),
                    bus.mdec.macroblocks_decoded(),
                    bus.gpu.display_hash().0
                );
            }
            if vblank >= frames {
                break "frames".to_string();
            }
        }
        if steps >= cap {
            break "step_cap".to_string();
        }
    };
    if let (Some(path), Some(log)) = (hash_log, frame_hashes) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, log);
    }
    (stop, last_vblank, steps, hashes)
}

/// `PSOXIDE_COMPAT_CDLOG=N`: the first N CD commands and responses of the
/// HLE run, interleaved by cycle, on stderr (runs of one command folded).
fn cd_log_cap() -> Option<usize> {
    std::env::var("PSOXIDE_COMPAT_CDLOG")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
}

/// Times are seconds from the EXE entry (`entry_cycle`).
fn print_cd_log(bus: &Bus, tag: &str, entry_cycle: u64) {
    let mut lines: Vec<(u64, String)> = Vec::new();
    for c in bus.cdrom.command_log() {
        lines.push((
            c.cycle,
            format!(
                "cmd {:02x} {:02x?}",
                c.command,
                &c.params[..c.param_len as usize]
            ),
        ));
    }
    for r in bus.cdrom.response_log() {
        lines.push((
            r.cycle,
            format!("  resp {:?} {:02x?}", r.irq, &r.bytes[..r.len as usize]),
        ));
    }
    lines.sort_by_key(|(cycle, _)| *cycle);
    let mut last = String::new();
    let mut repeat = 0;
    for (cycle, text) in lines {
        if text == last {
            repeat += 1;
            continue;
        }
        if repeat > 0 {
            eprintln!("[{tag}]   (x{repeat} more)");
        }
        repeat = 0;
        eprintln!(
            "[{tag}] {:.4}s {text}",
            cycle.saturating_sub(entry_cycle) as f64 / CPU_HZ
        );
        last = text;
    }
}

/// `PSOXIDE_COMPAT_STATUS=N`: one status line on stderr every N frames
/// (PC, CD and MDEC progress) for telling a stall from a slow screen.
fn status_every() -> u64 {
    static EVERY: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *EVERY.get_or_init(|| {
        std::env::var("PSOXIDE_COMPAT_STATUS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    })
}

/// Finds the first frame a GetlocP lands on a `.sbi` sector.
struct Watch<'a> {
    lbas: &'a [u32],
    seen: usize,
    first: Option<u64>,
}

impl Watch<'_> {
    fn check(&mut self, bus: &Bus, frame: u64) {
        if self.lbas.is_empty() || self.first.is_some() {
            return;
        }
        let log = bus.cdrom.getlocp_lbas();
        if log[self.seen..]
            .iter()
            .any(|lba| self.lbas.binary_search(lba).is_ok())
        {
            self.first = Some(frame);
        }
        self.seen = log.len();
    }
}

/// Save states for input exploration (`--save-dir`/`--save-at`, then
/// `--load-state`): the HLE run only, frames counted from the EXE entry
/// either way so pulse schedules stay absolute.
#[derive(Default)]
struct States {
    load: Option<PathBuf>,
    dir: Option<PathBuf>,
    save_at: Vec<u64>,
}

/// Write `<base>.<frame>.state`; the game id records `<id>:<frame>`.
fn save_state(cpu: &Cpu, bus: &Bus, card: Option<&[u8]>, base: &Path, frame: u64) {
    let id = base
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let state = SaveStateV1::new(
        EmulatorStateRef { cpu, bus },
        format!("{id}:{frame}"),
        bus.cycles(),
    );
    let path = PathBuf::from(format!("{}.{frame}.state", base.display()));
    if let Some(card) = card {
        let _ = std::fs::write(path.with_extension("mcd"), card);
    }
    match state.write_to(&path) {
        Ok(()) => eprintln!("[hle-compat] saved {}", path.display()),
        Err(error) => eprintln!("[hle-compat] save {}: {error}", path.display()),
    }
}

/// `--shot-every`: the interval and the final shot's path without `.ppm`.
fn periodic_shots(shot: Option<&Path>, every: u64) -> Option<(u64, PathBuf)> {
    let path = shot?;
    (every > 0).then(|| (every, path.with_extension("")))
}

fn apply_sample(bus: &mut Bus, tape: Option<&[PadSample]>, frame: u64) {
    let Some(tape) = tape else {
        return;
    };
    let sample = tape.get(frame as usize).copied().unwrap_or_default();
    bus.set_port1_buttons(ButtonState::from_bits(sample.buttons));
    bus.set_port1_sticks(sample.right_x, sample.right_y, sample.left_x, sample.left_y);
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
        if r.status == "ran" {
            println!(
                "    mdec macroblocks {}, cd sectors dropped {}",
                r.mdec_macroblocks, r.cd_sectors_dropped
            );
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

/// The emulator commit the runner was built from, read from the source
/// tree next to this example (dev tool: needs `git` on PATH).
fn emulator_commit() -> Option<String> {
    let dir = env!("CARGO_MANIFEST_DIR");
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let sha = git(&["rev-parse", "--short=9", "HEAD"])?;
    let dirty =
        git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|out| !out.is_empty());
    Some(if dirty { format!("{sha}-dirty") } else { sha })
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
