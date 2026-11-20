//! A machine saved and restored mid-run must go on exactly as the one that never stopped.
//!
//! The same disc runs from boot with pad inputs that are a pure function of the guest's pad
//! poll index. The reference runs straight through. At busy moments (a CD transfer queued, an
//! SPU key-on, a GPU DMA in flight, a pad transaction on the wire, or an event number picked
//! beforehand) its whole machine is serialized, restored into a fresh machine the way the
//! frontend and the journey bot restore one, and driven on. Both runs are compared after every
//! event (a pad poll or a video blank, so cuts land mid-frame) of a window past the cut: the
//! CPU, RAM, each peripheral and the whole serialized bus. A failure names the cut, how many
//! events later the machines parted and which components differ.
//!
//! The discs are the developer's library (`PSOXIDE_GAMES_DIR`, default `~/Downloads/ps1 games`);
//! a disc that is not there is skipped with a note. Developer probes for chasing a failure are
//! at the bottom (`--ignored`, `PSOXIDE_SS_DISC=<cue>`).
#![cfg(target_arch = "aarch64")]

#[path = "../../emulator-core/examples/support/disc.rs"]
mod disc_support;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use emulator_core::input_tape::PadSample;
use emulator_core::{fast_boot_disc, Bus, Cpu, EmulatorState, EmulatorStateRef};

fn games_dir() -> PathBuf {
    std::env::var_os("PSOXIDE_GAMES_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Downloads/ps1 games")))
        .unwrap_or_default()
}

/// A freshly booted machine with the disc mounted, a DualShock in analog mode and (optionally)
/// a memory card, as the gate and the journey bot boot one.
fn boot(cue: &Path, card: Option<&[u8]>) -> (Cpu, Bus) {
    let disc = disc_support::load_disc_path(cue).unwrap_or_else(|e| panic!("{e}"));
    let mut bus = Bus::new_without_bios();
    let mut cpu = Cpu::new();
    fast_boot_disc(&mut bus, &mut cpu, &disc).unwrap_or_else(|e| panic!("boot: {e:?}"));
    bus.cdrom.insert_disc(Some(disc));
    bus.attach_digital_pad_port1();
    let _ = bus.force_port1_analog_mode();
    if let Some(card) = card {
        bus.attach_memcard_port1(card.to_vec());
    }
    let _ = psoxide_jit::install_tier(&mut cpu);
    (cpu, bus)
}

const N: usize = 14;
const COMPONENTS: [&str; N] = [
    "cpu",
    "ram",
    "spu",
    "gpu",
    "cdrom",
    "mdec",
    "timers",
    "sio0",
    "irq",
    "dma",
    "sio1",
    "scheduler",
    "transfers",
    "bus-total",
];

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

fn ser<T: serde::Serialize>(value: &T) -> Vec<u8> {
    postcard::to_allocvec(value).expect("serialize")
}

/// Hash of each component of the machine; the last entries cover the bus's in-flight transfers
/// and the whole of the bus.
fn fingerprint(cpu: &Cpu, bus: &Bus) -> [u64; N] {
    [
        hash_bytes(&ser(cpu)),
        hash_bytes(bus.ram()),
        hash_bytes(&ser(&bus.spu)),
        hash_bytes(&ser(&bus.gpu)),
        hash_bytes(&ser(&bus.cdrom)),
        hash_bytes(&ser(&bus.mdec)),
        hash_bytes(&ser(bus.timers())),
        hash_bytes(&ser(bus.sio0())),
        hash_bytes(&ser(bus.irq())),
        hash_bytes(&ser(bus.debug_dma())),
        hash_bytes(&ser(bus.debug_sio1())),
        hash_bytes(&ser(&bus.scheduler)),
        hash_bytes(bus.debug_transfers().as_bytes()),
        hash_bytes(&ser(&EmulatorStateRef { cpu, bus })),
    ]
}

fn diff_names(a: &[u64; N], b: &[u64; N]) -> Vec<&'static str> {
    (0..N)
        .filter(|&i| a[i] != b[i])
        .map(|i| COMPONENTS[i])
        .collect()
}

/// One machine plus the harness's own view of time.
struct Run {
    cpu: Cpu,
    bus: Bus,
    polls: u64,
    event: u64,
    last_machine_polls: u64,
    last_machine_vblanks: u64,
}

impl Run {
    fn new(cpu: Cpu, bus: Bus) -> Self {
        let last_machine_polls = bus.port1_completed_polls();
        let last_machine_vblanks = bus.irq().raise_counts()[0];
        Self {
            cpu,
            bus,
            polls: last_machine_polls,
            event: 0,
            last_machine_polls,
            last_machine_vblanks,
        }
    }

    /// Run to the next pad poll or video blank, then the SPU catch-up the gate does.
    /// Returns whether a poll completed.
    fn next_event(&mut self) -> bool {
        let (p0, v0) = (self.last_machine_polls, self.last_machine_vblanks);
        loop {
            let (_, result) = self.cpu.run(&mut self.bus, 1 << 20, u64::MAX, |bus| {
                bus.port1_completed_polls() != p0 || bus.irq().raise_counts()[0] != v0
            });
            result.expect("guest fault");
            if self.bus.port1_completed_polls() != p0 || self.bus.irq().raise_counts()[0] != v0 {
                break;
            }
        }
        self.bus.run_spu_to_current_cycle();
        let _ = self.bus.spu.drain_audio();
        let (p1, v1) = (
            self.bus.port1_completed_polls(),
            self.bus.irq().raise_counts()[0],
        );
        self.last_machine_polls = p1;
        self.last_machine_vblanks = v1;
        self.polls += p1 - p0;
        self.event += 1;
        p1 != p0
    }

    /// Run one event and feed the pad for the poll that completed, if one did.
    fn step(&mut self, script: &Script) {
        if self.next_event() {
            script(self.polls).apply_to_bus(&mut self.bus);
        }
    }
}

/// Input for the pad poll with index `poll`: a pure function of the index.
type Script = dyn Fn(u64) -> PadSample;

/// Presses buttons in bursts and sweeps the sticks, the whole disc long: menus open and things
/// start without anyone knowing the game.
fn mash(poll: u64) -> PadSample {
    let mut x = (poll / 6).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0xd1b5_4a32_d192_ed03;
    x ^= x >> 29;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 32;
    // Every button but select.
    const MASK: u16 = !0x0001;
    let buttons = if x & 3 == 0 {
        0
    } else {
        (x >> 8) as u16 & MASK & (x >> 24) as u16
    };
    let stick = |shift: u32| -> u8 {
        let v = ((x >> shift) & 0xff) as u8;
        if (x >> (shift + 8)) & 3 == 0 {
            v
        } else {
            0x80
        }
    };
    PadSample {
        buttons,
        right_x: stick(40),
        right_y: stick(44),
        left_x: stick(48),
        left_y: stick(52),
    }
}

/// A kind of busy moment worth cutting at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Busy {
    /// A CD command or sector delivery is queued.
    Cd,
    /// An SPU voice was keyed on since the previous event.
    SpuKeyOn,
    /// A GPU DMA (a linked-list walk or a block transfer) is in flight.
    Dma,
    /// A pad or memory card byte is on the wire.
    PadTransaction,
}

const KINDS: [Busy; 4] = [Busy::Cd, Busy::SpuKeyOn, Busy::Dma, Busy::PadTransaction];

fn key_ons(bus: &Bus) -> u64 {
    bus.spu
        .voice_debug_counts()
        .0
        .iter()
        .map(|&n| u64::from(n))
        .sum()
}

fn is_busy(bus: &Bus, kind: Busy, key_ons_before: u64) -> bool {
    match kind {
        Busy::Cd => bus.cdrom.pending_queue_len() > 0 || bus.cdrom.data_fifo_ready(),
        Busy::SpuKeyOn => key_ons(bus) > key_ons_before,
        Busy::Dma => bus.debug_transfers().contains("Some("),
        Busy::PadTransaction => bus.sio0().debug_transfer_busy() || bus.sio0().debug_awaiting_ack(),
    }
}

struct Window {
    label: String,
    cut_event: u64,
    state: Vec<u8>,
    polls: u64,
    reference: Vec<[u64; N]>,
    transfers: Vec<String>,
}

/// How a disc is probed: after `skip` events, take up to `per_kind` cuts of each kind of busy
/// moment (at least `spacing` events apart), plus the `fixed` events; each cut is followed by
/// `window` compared events. Stops after `max_events`.
struct Plan {
    skip: u64,
    per_kind: usize,
    spacing: u64,
    window: u64,
    max_events: u64,
    fixed: Vec<u64>,
}

/// Run the reference, keeping the state at each cut and the fingerprints of the events after it.
/// Then restore each state and compare. Returns a line for each cut taken.
fn check_disc(
    name: &str,
    cue: &Path,
    card: Option<&[u8]>,
    script: &Script,
    plan: &Plan,
) -> Result<Vec<String>, String> {
    let (cpu, bus) = boot(cue, card);
    let mut reference = Run::new(cpu, bus);
    let mut windows: Vec<Window> = Vec::new();
    let mut taken = [0usize; 4];
    let mut last_cut = [0u64; 4];
    let mut fixed = plan.fixed.clone();
    fixed.sort_unstable();
    while reference.event < plan.max_events {
        let before = key_ons(&reference.bus);
        reference.step(script);
        let ev = reference.event;
        if windows
            .iter()
            .any(|w| (w.reference.len() as u64) < plan.window)
        {
            let fp = fingerprint(&reference.cpu, &reference.bus);
            let transfers = reference.bus.debug_transfers();
            for w in windows
                .iter_mut()
                .filter(|w| (w.reference.len() as u64) < plan.window)
            {
                w.reference.push(fp);
                w.transfers.push(transfers.clone());
            }
        } else if fixed.is_empty() && taken.iter().all(|&n| n >= plan.per_kind) {
            break;
        }
        let mut label = None;
        if ev >= plan.skip {
            for (i, kind) in KINDS.iter().enumerate() {
                if taken[i] < plan.per_kind
                    && ev >= last_cut[i] + plan.spacing
                    && is_busy(&reference.bus, *kind, before)
                {
                    taken[i] += 1;
                    last_cut[i] = ev;
                    label = Some(format!("{kind:?}"));
                    break;
                }
            }
        }
        if fixed.first() == Some(&ev) {
            fixed.remove(0);
            label.get_or_insert_with(|| "event".to_string());
        }
        if let Some(label) = label {
            let state = ser(&EmulatorStateRef {
                cpu: &reference.cpu,
                bus: &reference.bus,
            });
            windows.push(Window {
                label,
                cut_event: ev,
                state,
                polls: reference.polls,
                reference: Vec::new(),
                transfers: Vec::new(),
            });
        }
    }

    let mut report = Vec::new();
    let mut cuts = Vec::new();
    for w in &windows {
        let loaded: EmulatorState = postcard::from_bytes(&w.state).map_err(|e| e.to_string())?;
        let (_, mut donor) = boot(cue, card);
        let mut payload = loaded;
        payload.bus.restore_excluded_from(&mut donor);
        let mut cpu = payload.cpu;
        let _ = psoxide_jit::install_tier(&mut cpu);
        let mut run = Run::new(cpu, payload.bus);
        run.polls = w.polls;
        let mut parted = None;
        for (i, want) in w.reference.iter().enumerate() {
            run.step(script);
            let got = fingerprint(&run.cpu, &run.bus);
            if got != *want {
                parted = Some((i, diff_names(want, &got)));
                break;
            }
        }
        let head = format!(
            "{name}: {} cut at event {} (poll {})",
            w.label, w.cut_event, w.polls
        );
        match parted {
            None => {
                let line = format!("{head}: identical for {} events", w.reference.len());
                eprintln!("{line}");
                cuts.push(line);
            }
            Some((i, names)) => {
                let line = format!(
                    "{head}: parted {} events later in {names:?} (transfers: reference {} restored {})",
                    i + 1,
                    w.transfers[i],
                    run.bus.debug_transfers()
                );
                eprintln!("{line}");
                report.push(line);
            }
        }
    }
    if report.is_empty() {
        Ok(cuts)
    } else {
        Err(report.join("\n"))
    }
}

/// The first file the cue sheet names that is not there, if any: a library can keep a cue sheet
/// after its image is gone.
fn missing_track(cue: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(cue).ok()?;
    let dir = cue.parent()?;
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("FILE"))
        .filter_map(|rest| {
            let start = rest.find('"')? + 1;
            let end = rest.rfind('"')?;
            (end > start).then(|| dir.join(&rest[start..end]))
        })
        .find(|track| !track.exists())
}

fn check(cue: &str, plan: &Plan) {
    let path = games_dir().join(cue);
    if !path.exists() {
        eprintln!("skipping {cue}: {} not found", path.display());
        return;
    }
    if let Some(track) = missing_track(&path) {
        eprintln!("skipping {cue}: its image {} is not there", track.display());
        return;
    }
    match check_disc(cue, &path, None, &mash, plan) {
        Ok(cuts) => assert!(!cuts.is_empty(), "{cue}: no cut was taken"),
        Err(report) => panic!("{report}"),
    }
}

fn plan() -> Plan {
    Plan {
        skip: 150,
        per_kind: 2,
        spacing: 400,
        window: 30,
        max_events: 6000,
        fixed: vec![1500, 3000],
    }
}

#[test]
fn quake_resumes_exactly() {
    check("Quake (quake-psx).cue", &plan());
}

#[test]
fn half_life_resumes_exactly() {
    check("Half-Life (hl-psx).cue", &plan());
}

#[test]
fn wipeout_resumes_exactly() {
    check("WipEout PSoXide.cue", &plan());
}

#[test]
fn crash_bandicoot_resumes_exactly() {
    check("Crash Bandicoot (USA)/Crash Bandicoot (USA).cue", &plan());
}

// ----- developer probes -----

fn env_cuts() -> Vec<u64> {
    std::env::var("PSOXIDE_SS_CUTS")
        .ok()
        .map(|s| s.split(',').filter_map(|v| v.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![600, 2000, 4000])
}

fn probe_inputs() -> Option<(PathBuf, Option<Vec<u8>>)> {
    let cue = std::env::var_os("PSOXIDE_SS_DISC").map(PathBuf::from)?;
    let card = std::env::var_os("PSOXIDE_SS_CARD").map(|p| std::fs::read(p).expect("card"));
    Some((cue, card))
}

/// Reference against restore-and-continue at the events in `PSOXIDE_SS_CUTS`.
#[test]
#[ignore = "developer probe; set PSOXIDE_SS_DISC"]
fn probe_disc() {
    let Some((cue, card)) = probe_inputs() else {
        return;
    };
    let window = std::env::var("PSOXIDE_SS_WINDOW")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let cuts = env_cuts();
    let plan = Plan {
        skip: u64::MAX,
        per_kind: 0,
        spacing: 0,
        window,
        max_events: cuts.iter().max().copied().unwrap_or(0) + window,
        fixed: cuts,
    };
    check_disc("probe", &cue, card.as_deref(), &mash, &plan).unwrap();
}

/// Single-steps the live machine and a restored copy from the first event of
/// `PSOXIDE_SS_CUTS` and reports the first instruction where they part.
#[test]
#[ignore = "developer probe; set PSOXIDE_SS_DISC"]
fn probe_step() {
    let Some((cue, card)) = probe_inputs() else {
        return;
    };
    let cut = env_cuts()[0];
    let (cpu, bus) = boot(&cue, card.as_deref());
    let mut live = Run::new(cpu, bus);
    while live.event < cut {
        live.step(&mash);
    }
    let state = ser(&EmulatorStateRef {
        cpu: &live.cpu,
        bus: &live.bus,
    });
    let mut payload: EmulatorState = postcard::from_bytes(&state).unwrap();
    let (_, mut donor) = boot(&cue, card.as_deref());
    payload.bus.restore_excluded_from(&mut donor);
    let mut rcpu = payload.cpu;
    let mut rbus = payload.bus;
    for n in 0..5_000_000u64 {
        live.cpu.step(&mut live.bus).unwrap();
        rcpu.step(&mut rbus).unwrap();
        let mut scalars = live.bus.debug_state_scalars();
        let mut restored = rbus.debug_state_scalars();
        // The two last entries hash whole memories.
        scalars.truncate(scalars.len() - 2);
        restored.truncate(restored.len() - 2);
        let parted: Vec<_> = scalars
            .iter()
            .zip(&restored)
            .filter(|(a, b)| a.1 != b.1)
            .map(|(a, b)| (a.0, a.1, b.1))
            .collect();
        let sio = |b: &Bus| {
            let s = b.sio0();
            format!(
                "stat={:x} ctrl={:x} pend={} latched={} busy={} awaiting={} xfer={:?} ack={:?} ackend={:?} rx={:?} tx={:?}",
                s.debug_stat(),
                s.debug_ctrl(),
                s.debug_pending_irq(),
                s.debug_irq_latched(),
                s.debug_transfer_busy(),
                s.debug_awaiting_ack(),
                s.debug_transfer_deadline(),
                s.debug_ack_deadline(),
                s.debug_ack_end_deadline(),
                s.debug_rx(),
                s.debug_queued_tx()
            )
        };
        if sio(&live.bus) != sio(&rbus) {
            panic!(
                "step {n}: sio0 live {} restored {}",
                sio(&live.bus),
                sio(&rbus)
            );
        }
        if !parted.is_empty()
            || live.bus.debug_transfers() != rbus.debug_transfers()
            || live.cpu.pc() != rcpu.pc()
        {
            panic!(
                "step {n}: live pc={:08x} restored pc={:08x}; scalars {parted:?}; transfers live {} restored {}",
                live.cpu.pc(),
                rcpu.pc(),
                live.bus.debug_transfers(),
                rbus.debug_transfers()
            );
        }
    }
    eprintln!("no difference in 5M steps");
}
