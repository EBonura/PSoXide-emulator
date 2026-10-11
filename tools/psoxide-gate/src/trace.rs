//! Opt-in diagnostic evidence. Packet IDs never refer to a drained GPU log.
use std::path::Path;

use emulator_core::gpu::{Gpu, GpuCmdLogEntry};
use sha2::{Digest, Sha256};

use crate::machine::Machine;

pub fn capture_checkpoint(selected: &[String], name: &str, failed: bool) -> bool {
    failed || selected.iter().any(|s| s == name)
}

const LIMIT: usize = 4 * 1024 * 1024;
const UNKNOWN: u64 = u64::MAX;

pub struct Trace {
    packets: std::collections::VecDeque<(u64, Vec<u8>)>,
    bytes: usize,
    owners: Vec<u64>,
    truncated: bool,
    epoch: u32,
    saved: usize,
}

fn packet_bytes(tick: u64, epoch: u32, entries: &[GpuCmdLogEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in entries {
        // Tick and local index identify this immutable packet snapshot. Pending
        // uploads/polylines may appear again next tick with additional words.
        out.extend_from_slice(&tick.to_le_bytes());
        out.extend_from_slice(&epoch.to_le_bytes());
        out.extend_from_slice(&e.index.to_le_bytes());
        out.extend_from_slice(&(e.fifo.len() as u32).to_le_bytes());
        for word in e.fifo.as_slice() {
            out.extend_from_slice(&word.to_le_bytes());
        }
    }
    out
}

impl Trace {
    pub fn new() -> Self {
        Self {
            packets: Default::default(),
            bytes: 0,
            owners: vec![UNKNOWN; 1024 * 512],
            truncated: false,
            epoch: 0,
            saved: 0,
        }
    }

    fn retained_owner(&self, owner: u64) -> u64 {
        let begin = self.packets.front().map(|p| p.0).unwrap_or(u64::MAX);
        if owner != UNKNOWN && owner >> 32 >= begin {
            owner
        } else {
            UNKNOWN
        }
    }

    pub fn reboot(&mut self) {
        self.epoch += 1;
        self.owners.fill(UNKNOWN);
    }

    pub fn record(&mut self, tick: u64, gpu: &Gpu) {
        let bytes = packet_bytes(tick, self.epoch, &gpu.cmd_log);
        let fits = bytes.len() <= LIMIT && tick < u64::from(u32::MAX);
        while self.bytes + bytes.len() > LIMIT && !self.packets.is_empty() {
            let (_, old) = self.packets.pop_front().unwrap();
            self.bytes -= old.len();
            self.truncated = true;
        }
        if fits && !bytes.is_empty() {
            self.bytes += bytes.len();
            self.packets.push_back((tick, bytes));
        } else if !fits {
            self.truncated = true;
        }
        if let Some(stamps) = gpu.pixel_owner_stamps() {
            for (owner, &local) in self.owners.iter_mut().zip(stamps) {
                if local != u32::MAX {
                    *owner = if fits && gpu.cmd_log.get(local as usize).is_some() {
                        (tick << 32) | u64::from(local)
                    } else {
                        UNKNOWN
                    };
                }
            }
        }
    }

    pub fn save(
        &mut self,
        root: &Path,
        index: usize,
        name: &str,
        m: &Machine,
        disc: &Path,
        repo: &Path,
        journey: &Path,
        symbols: &crate::symbols::Symbols,
        state: &[String],
        contract: &crate::journey::Journey,
    ) -> Result<String, String> {
        if self.saved >= 16 {
            return Err("trace checkpoint cap (16) reached".into());
        }
        self.saved += 1;
        let dir = root.join(format!("checkpoint-{index:04}"));
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let begin = self.packets.front().map(|p| p.0).unwrap_or(m.ticks);
        let mut packets = b"PXGP0V1\0".to_vec();
        for (_, bytes) in &self.packets {
            packets.extend_from_slice(bytes);
        }
        std::fs::write(dir.join("packets.bin"), &packets).map_err(|e| e.to_string())?;
        let mut owners = Vec::with_capacity(self.owners.len() * 8);
        for &owner in &self.owners {
            let retained = self.retained_owner(owner);
            owners.extend_from_slice(&retained.to_le_bytes());
        }
        let ram = m.bus.ram();
        let mut vram = Vec::with_capacity(1024 * 512 * 2);
        for word in m.bus.gpu.vram.words() {
            vram.extend_from_slice(&word.to_le_bytes());
        }
        for (filename, bytes) in [
            ("owners.bin", owners.as_slice()),
            ("ram.bin", ram),
            ("vram.bin", vram.as_slice()),
        ] {
            std::fs::write(dir.join(filename), bytes).map_err(|e| e.to_string())?;
        }
        let display = m.bus.gpu.display_area();
        let source = std::process::Command::new("git")
            .args(["-C"])
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .unwrap_or_else(|| "unavailable".into());
        let receipt = format!("schema = 1\ncheckpoint = {name:?}\ntick = {}\nepoch = {}\ntruncated = {}\npacket_bytes = {}\npacket_sha256 = {:?}\nram_sha256 = {:?}\nvram_sha256 = {:?}\nowners_sha256 = {:?}\ndisc_id_abbreviated = {:?}\nsource_head = {source:?}\npc = {}\ngprs = {:?}\ndisplay_origin = [{}, {}]\n",
            m.ticks, self.epoch, self.truncated, packets.len(), hash(&packets),
            hash(ram), hash(&vram), hash(&owners), crate::exec::disc_id(disc),
            m.cpu.pc(), m.cpu.gprs(), display.x, display.y);
        let dirty = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["status", "--porcelain", "--untracked-files=all"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| !o.stdout.is_empty());
        let tool = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut bindings = format!("state_requests_truncated = {}\npacket_begin_tick = {begin}\npacket_end_tick = {}\nsource_dirty_status = {:?}\njourney_sha256 = {:?}\ntool_sha256 = {:?}\ndisc_file_sha256 = {:?}\n", state.len() > 32, m.ticks,
            dirty.map(|b| b.to_string()).unwrap_or_else(|| "unknown".into()),
            file_hash(journey)?, file_hash(&tool)?, file_hash(disc)?);
        if disc
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("cue"))
        {
            let cue = std::fs::read_to_string(disc).map_err(|e| e.to_string())?;
            for line in cue.lines().filter_map(|l| l.trim().strip_prefix("FILE ")) {
                let filename = if let Some(q) = line.strip_prefix('"') {
                    q.split_once('"').ok_or("invalid CUE FILE")?.0
                } else {
                    line.split_whitespace().next().ok_or("invalid CUE FILE")?
                };
                let path = disc.parent().ok_or("disc parent missing")?.join(filename);
                bindings.push_str(&format!(
                    "\n[[disc_tracks]]\nfile = {filename:?}\nsha256 = {:?}\n",
                    file_hash(&path)?
                ));
            }
        }
        for rel in contract.symbols.list() {
            let path = repo.join(rel);
            bindings.push_str(&format!(
                "\n[[symbol_sources]]\npath = {:?}\nsha256_or_error = {:?}\n",
                path.display().to_string(),
                file_hash(&path).unwrap_or_else(|e| format!("unavailable: {e}"))
            ));
        }
        for request in [
            "SIM_NOW:4",
            "DEFERRED_OVERLAYS:128",
            "LIVE_CHECKPOINT:256",
            "SHOW_VIEWMODEL:1",
        ]
        .iter()
        .map(|s| s.to_string())
        .chain(state.iter().take(32).cloned())
        {
            bindings.push_str(&format!("\n[[state]]\nrequest = {request:?}\n"));
            let result = read_state(&request, symbols, m.bus.ram());
            match result {
                Ok((address, bytes)) => bindings.push_str(&format!(
                    "address = {address}\nbytes = {bytes:?}\nsha256 = {:?}\n",
                    hash(&bytes)
                )),
                Err(error) => bindings.push_str(&format!("unavailable = {error:?}\n")),
            }
        }

        let receipt = receipt + &bindings;
        std::fs::write(dir.join("receipt.toml"), receipt).map_err(|e| e.to_string())?;
        Ok(format!(
            "diagnostic trace {} (truncated={}; CPU owners only, not HW coverage)",
            dir.display(),
            self.truncated
        ))
    }
}

fn read_state(
    request: &str,
    symbols: &crate::symbols::Symbols,
    ram: &[u8],
) -> Result<(u32, Vec<u8>), String> {
    let (name, len) = request
        .rsplit_once(':')
        .ok_or("state must be SYMBOL:BYTES")?;
    let len: usize = len.parse().map_err(|_| "invalid state length")?;
    if len == 0 || len > 256 {
        return Err("state length must be 1..=256".into());
    }
    let address = symbols.resolve(name)?;
    let physical = address & 0x1fff_ffff;
    if physical >= 0x0020_0000 {
        return Err("state must be resident main RAM".into());
    }
    let start = physical as usize;
    let end = start.checked_add(len).ok_or("state range overflow")?;
    let bytes = ram.get(start..end).ok_or("state range outside RAM")?;
    Ok((address, bytes.to_vec()))
}

fn file_hash(path: &Path) -> Result<String, String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Resolve native VRAM coordinates, never claim hardware coverage from CPU owners.
pub fn inspect(checkpoint: &Path, x: u16, y: u16) -> Result<String, String> {
    if x >= 1024 || y >= 512 {
        return Err("VRAM coordinate out of bounds".into());
    }
    let text = std::fs::read_to_string(checkpoint.join("receipt.toml"))
        .map_err(|e| e.to_string())?;
    let receipt: toml::Value = toml::from_str(&text).map_err(|e| e.to_string())?;
    let owners = std::fs::read(checkpoint.join("owners.bin")).map_err(|e| e.to_string())?;
    if owners.len() != 1024 * 512 * 8
        || receipt.get("owners_sha256").and_then(|v| v.as_str()) != Some(hash(&owners).as_str())
    {
        return Err("owner evidence hash/length mismatch".into());
    }
    let owner = u64::from_le_bytes(
        owners[(usize::from(y) * 1024 + usize::from(x)) * 8..][..8]
            .try_into()
            .unwrap(),
    );
    if owner == UNKNOWN {
        return Err("CPU owner unknown; no primitive qualification".into());
    }
    let tick = owner >> 32;
    let local = owner as u32;
    let stream = std::fs::read(checkpoint.join("packets.bin")).map_err(|e| e.to_string())?;
    let length = receipt
        .get("packet_bytes")
        .and_then(|v| v.as_integer())
        .and_then(|n| usize::try_from(n).ok())
        .ok_or("invalid prefix length")?;
    let prefix = stream.get(..length).ok_or("short packet prefix")?;
    if receipt.get("packet_sha256").and_then(|v| v.as_str()) != Some(hash(prefix).as_str())
        || prefix.get(..8) != Some(b"PXGP0V1\0")
    {
        return Err("packet evidence hash/header mismatch".into());
    }
    let mut pos = 8;
    while pos < prefix.len() {
        let header = prefix.get(pos..pos + 20).ok_or("short packet header")?;
        let t = u64::from_le_bytes(header[..8].try_into().unwrap());
        let epoch = u32::from_le_bytes(header[8..12].try_into().unwrap());
        let index = u32::from_le_bytes(header[12..16].try_into().unwrap());
        let count = u32::from_le_bytes(header[16..20].try_into().unwrap()) as usize;
        pos += 20;
        let bytes = count.checked_mul(4).ok_or("packet size overflow")?;
        let end = pos.checked_add(bytes).ok_or("packet range overflow")?;
        let words = prefix.get(pos..end).ok_or("short packet payload")?;
        if t == tick && index == local {
            let fifo = words
                .chunks_exact(4)
                .map(|w| format!("{:08x}", u32::from_le_bytes(w.try_into().unwrap())))
                .collect::<Vec<_>>()
                .join(" ");
            return Ok(format!("CPU owner epoch={epoch} tick={tick} packet={local} GP0=[{fifo}]; HW ownership is unverified"));
        }
        pos = end;
    }
    Err("owner packet missing from retained prefix".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_selection_preserves_failures_and_requires_explicit_trace_cli() {
        use clap::Parser;
        assert!(!capture_checkpoint(&[], "baseline", false));
        assert!(capture_checkpoint(&[], "baseline", true));
        assert!(capture_checkpoint(&["baseline".into()], "baseline", false));
        assert!(!capture_checkpoint(&["other".into()], "baseline", false));
        assert!(crate::Cli::try_parse_from([
            "gate",
            "run",
            "journey.toml",
            "--trace-checkpoint",
            "baseline"
        ])
        .is_err());
        let parsed = crate::Cli::try_parse_from([
            "gate",
            "run",
            "journey.toml",
            "--trace-failures",
            "--trace-checkpoint",
            "baseline",
        ])
        .unwrap();
        let crate::Cmd::Run(args) = parsed.command else {
            panic!("wrong command")
        };
        assert!(args.trace_failures);
        assert_eq!(args.trace_checkpoint, vec!["baseline"]);
        let parsed = crate::Cli::try_parse_from(["gate", "run", "journey.toml"]).unwrap();
        let crate::Cmd::Run(args) = parsed.command else {
            panic!("wrong command")
        };
        assert!(!args.trace_failures);
        assert!(args.trace_checkpoint.is_empty());
    }

    #[test]
    fn pixel_inspection_binds_exact_packet_and_rejects_corruption() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let mut gpu = Gpu::new();
        gpu.enable_pixel_tracer();
        let fifo = [0x600000f8, 0x000a000a, 0x00020002];
        for word in fifo {
            gpu.write32(0x1f801810, word);
        }
        let mut stream = b"PXGP0V1\0".to_vec();
        stream.extend_from_slice(&packet_bytes(7, 0, &gpu.cmd_log));
        let mut owners = vec![255u8; 1024 * 512 * 8];
        owners[(10 * 1024 + 10) * 8..][..8].copy_from_slice(&(7u64 << 32).to_le_bytes());
        std::fs::write(dir.join("packets.bin"), &stream).unwrap();
        std::fs::write(dir.join("owners.bin"), &owners).unwrap();
        std::fs::write(
            dir.join("receipt.toml"),
            format!(
                "packet_bytes = {}\npacket_sha256 = {:?}\nowners_sha256 = {:?}\n",
                stream.len(),
                hash(&stream),
                hash(&owners)
            ),
        )
        .unwrap();
        let report = inspect(dir, 10, 10).unwrap();
        assert!(report.contains("600000f8 000a000a 00020002"));
        assert!(report.contains("HW ownership is unverified"));
        assert!(inspect(dir, 0, 0).is_err());
        assert!(inspect(dir, 1024, 0).is_err());
        let last = stream.len() - 1;
        stream[last] ^= 1;
        std::fs::write(dir.join("packets.bin"), stream).unwrap();
        assert!(inspect(dir, 10, 10).unwrap_err().contains("hash/header"));
    }

    #[test]
    fn empty_ticks_do_not_allocate_window_entries() {
        let mut archive = Trace::new();
        let gpu = Gpu::new();
        for tick in 0..100_000 {
            archive.record(tick, &gpu);
        }
        assert!(archive.packets.is_empty());
        assert_eq!(archive.bytes, 0);
        assert!(!archive.truncated);
    }

    #[test]
    fn named_state_fails_closed_for_missing_or_nonram_addresses() {
        let mut symbols = crate::symbols::Symbols::default();
        symbols.insert("POSE", 0x80000004);
        symbols.insert("IO", 0x1f801810);
        let ram = vec![7u8; 32];
        assert_eq!(
            read_state("POSE:4", &symbols, &ram).unwrap(),
            (0x80000004, vec![7; 4])
        );
        assert!(read_state("MISSING:4", &symbols, &ram).is_err());
        assert!(read_state("POSE:257", &symbols, &ram).is_err());
        assert!(read_state("IO:4", &symbols, &ram).is_err());
        assert!(read_state("POSE:32", &symbols, &ram).is_err());
    }

    #[test]
    fn window_eviction_and_reboot_never_reuse_owner_ids() {
        let mut archive = Trace::new();
        let mut gpu = Gpu::new();
        gpu.enable_pixel_tracer();
        for word in [0x600000f8, 0x000a000a, 0x00020002] {
            gpu.write32(0x1f801810, word);
        }
        archive.record(1, &gpu);
        assert_eq!(
            archive.retained_owner(archive.owners[10 * 1024 + 10]),
            1u64 << 32
        );
        gpu.drain_completed_cmd_log();
        gpu.clear_pixel_owner_stamps();
        gpu.cmd_log.push(GpuCmdLogEntry {
            index: 0,
            opcode: 0xa0,
            fifo: emulator_core::gpu::GpuCmdFifo::from_slice(&vec![0x12345678; LIMIT / 4 - 5]),
        });
        archive.record(2, &gpu);
        assert!(archive.truncated);
        assert!(archive.bytes <= LIMIT);
        assert_eq!(
            archive.retained_owner(archive.owners[10 * 1024 + 10]),
            UNKNOWN
        );
        archive.reboot();
        assert_eq!(archive.epoch, 1);
        assert!(archive.owners.iter().all(|&id| id == UNKNOWN));
        gpu.enable_pixel_tracer();
        for word in [0x600000f8, 0x000a000a, 0x00020002] {
            gpu.write32(0x1f801810, word);
        }
        archive.record(3, &gpu);
        assert_eq!(archive.owners[10 * 1024 + 10], 3u64 << 32);
        assert_ne!(archive.owners[10 * 1024 + 10], 1u64 << 32);
    }

    #[test]
    fn pending_upload_snapshots_keep_exact_payload_and_unknown_owners() {
        let mut gpu = Gpu::new();
        gpu.enable_pixel_tracer();
        let setup = [0xa0000000, 0x00200020, 0x00010004];
        for word in setup {
            gpu.write32(0x1f801810, word);
        }
        gpu.write32(0x1f801810, 0x12345678);
        let first = packet_bytes(10, 0, &gpu.cmd_log);
        assert_eq!(
            &first[20..],
            [setup[0], setup[1], setup[2], 0x12345678]
                .iter()
                .flat_map(|w| w.to_le_bytes())
                .collect::<Vec<_>>()
        );
        assert!(gpu.drain_completed_cmd_log().is_empty());
        gpu.clear_pixel_owner_stamps();
        gpu.write32(0x1f801810, 0x90abcdef);
        let last = gpu.drain_completed_cmd_log();
        assert_eq!(
            last[0].fifo.as_slice(),
            &[setup[0], setup[1], setup[2], 0x12345678, 0x90abcdef]
        );
    }

    #[test]
    fn packet_bytes_and_owner_survive_drain_without_changing_pixels() {
        let mut traced = Gpu::new();
        let mut plain = Gpu::new();
        traced.enable_pixel_tracer();
        plain.enable_cmd_log();
        let mut archive = Trace::new();
        let packet = [0x600000f8, 0x000a000a, 0x00020002];
        for gpu in [&mut traced, &mut plain] {
            for word in packet {
                assert!(gpu.write32(0x1f801810, word));
            }
        }
        archive.record(1, &traced);
        let encoded = packet_bytes(1, 0, &traced.cmd_log);
        assert_eq!(
            &encoded[20..],
            packet
                .iter()
                .flat_map(|w| w.to_le_bytes())
                .collect::<Vec<_>>()
        );
        assert_eq!(traced.vram.words(), plain.vram.words());
        assert_eq!(archive.owners[10 * 1024 + 10], 1u64 << 32);
        let drained = traced.drain_completed_cmd_log();
        assert_eq!(drained[0].fifo.as_slice(), &packet);
        traced.clear_pixel_owner_stamps();
        let second = [0x6000f800, 0x00140014, 0x00020002];
        for word in second {
            traced.write32(0x1f801810, word);
            plain.write32(0x1f801810, word);
        }
        archive.record(2, &traced);
        assert_eq!(archive.owners[10 * 1024 + 10], 1u64 << 32);
        assert_eq!(archive.owners[20 * 1024 + 20], 2u64 << 32);
        assert_eq!(traced.vram.words(), plain.vram.words());
        // Strict comparison is bit-for-bit identical with tracing enabled.
        let changed = traced
            .vram
            .words()
            .iter()
            .zip(plain.vram.words())
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(changed, 0);
        let image = |gpu: &Gpu| {
            let mut rgba = Vec::new();
            for &w in gpu.vram.words() {
                rgba.extend_from_slice(&[
                    ((w & 31) * 8) as u8,
                    (((w >> 5) & 31) * 8) as u8,
                    (((w >> 10) & 31) * 8) as u8,
                    255,
                ]);
            }
            crate::img::Img::from_rgba(1024, 512, rgba)
        };
        let a = image(&traced);
        let b = image(&plain);
        let strict = crate::img::Tolerance::EXACT;
        let same = a.diff(&b, 0, 1, &[], None);
        assert!(same.within(strict));
        let golden = image(&Gpu::new());
        let trace_result = a.diff(&golden, 0, 1, &[], None);
        let plain_result = b.diff(&golden, 0, 1, &[], None);
        assert!(plain_result.differing > 0);
        assert!(!plain_result.within(strict));
        assert_eq!(trace_result.differing, plain_result.differing);
        assert_eq!(trace_result.within(strict), plain_result.within(strict));
    }
}
