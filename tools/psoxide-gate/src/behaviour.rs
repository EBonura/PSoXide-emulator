//! Behaviour versions: which emulated behaviour a run depended on, so a result can be carried
//! across a gate change that left those areas alone.
//!
//! [`AREAS`] holds one version per behaviour area. The commit that changes an area's emulated
//! behaviour bumps its number here. `behaviour.lock` (beside `Cargo.toml`) records, per area, the
//! version and a digest of the area's source files ([`SOURCES`]); a test fails when the sources
//! of an area changed and the lock was not renewed, so a change cannot slip past the table:
//! either bump the area (its behaviour changed) or renew the lock alone (it did not, say why in
//! the commit), with `PSOXIDE_BEHAVIOUR_RELOCK=1 cargo test -p psoxide-gate behaviour`.
//!
//! After every journey the gate prints `AREAS <journey> <model> <area>=<version> ...` for the
//! areas the run exercised and `DIGEST <journey> <model> <hex>` for what it did (see
//! [`RunDigest`]); both go into the receipt and are printed again on a receipt hit.
//!
//! Version 1 of every area is the behaviour of emulator f280c6e711d9.

use sha2::{Digest, Sha256};

/// The behaviour version of every area, in print order. `rules` and `journey` are added by
/// [`table`]: the first is a digest of the conformance rule set, the second
/// [`JOURNEY_LANGUAGE`].
pub const AREAS: [(&str, u32); 15] = [
    ("cpu", 1),
    ("gte", 1),
    ("muldiv", 1),
    // 2: a Stop takes 620 ms to complete and a read issued during its spin-down 264 ms more
    // after the spin-up (records 0x314, 0x315: v1.28 and the v2.24 console run, median of two).
    ("cd", 2),
    // 2: channel-4 (RAM to SPU) DMA moves a halfword every 32 clocks at every COMMON_DELAY
    // (silicon record 0x65), and an abort mid-transfer keeps only the blocks moved so far.
    // 3: a halfword written to the data port while SPUCNT holds Manual Write lands nowhere
    // (silicon record 0xA7: none of eight arrive on both v2.18 consoles).
    ("spu", 3),
    ("pad", 1),
    ("mdec", 1),
    // 2: GP1(03h) display off presents black (display_rgba8, display_hash, the gate's frames).
    // 3: textured rectangles pay the texture-cache cliff triangles pay (silicon record 0x9E0).
    ("gpu", 3),
    ("dma", 1),
    ("timers", 1),
    ("irq", 1),
    ("card", 1),
    ("mister", 1),
    ("strict", 1),
    // 2: span-drawn polygon triangles cover host samples at scales above 1.
    ("hw", 2),
];

/// Version of the journey language: what the gate makes of a journey file (steps, conditions,
/// comparisons). Bumped when the same file can give another verdict.
pub const JOURNEY_LANGUAGE: u32 = 1;

/// Source files (path prefixes from the repository root) whose changes can change each area's
/// behaviour. Files named `tests.rs` or ending `_tests.rs` are left out.
pub const SOURCES: [(&str, &[&str]); 16] = [
    (
        "cpu",
        &[
            "emu/crates/emulator-core/src/cpu.rs",
            "emu/crates/emulator-core/src/cpu/",
            "emu/crates/emulator-core/src/bus.rs",
            "emu/crates/emulator-core/src/bus/",
            "emu/crates/emulator-core/src/ram.rs",
            "emu/crates/emulator-core/src/scheduler.rs",
            "emu/crates/emulator-core/src/hle_",
            "emu/crates/emulator-core/src/fastboot.rs",
            "emu/crates/emulator-core/src/entry_state.rs",
            "emu/crates/emulator-core/src/limits.rs",
        ],
    ),
    ("gte", &["sdk/crates/psx-gte-core/"]),
    ("muldiv", &["emu/crates/emulator-core/src/cpu.rs"]),
    (
        "cd",
        &[
            "emu/crates/emulator-core/src/cdrom.rs",
            "emu/crates/emulator-core/src/cdrom/",
            "emu/crates/emulator-core/src/system_cnf.rs",
            "crates/psx-iso/",
        ],
    ),
    (
        "spu",
        &[
            "emu/crates/emulator-core/src/spu.rs",
            "emu/crates/emulator-core/src/spu/",
            "sdk/crates/psx-spu/",
        ],
    ),
    (
        "pad",
        &[
            "emu/crates/emulator-core/src/pad.rs",
            "emu/crates/emulator-core/src/sio.rs",
            "emu/crates/emulator-core/src/sio1.rs",
            "emu/crates/emulator-core/src/input_tape.rs",
        ],
    ),
    ("mdec", &["emu/crates/emulator-core/src/mdec.rs"]),
    (
        "gpu",
        &[
            "emu/crates/emulator-core/src/gpu.rs",
            "emu/crates/emulator-core/src/gpu/",
            "emu/crates/emulator-core/src/vram.rs",
        ],
    ),
    ("dma", &["emu/crates/emulator-core/src/dma.rs"]),
    ("timers", &["emu/crates/emulator-core/src/timers.rs"]),
    ("irq", &["emu/crates/emulator-core/src/irq.rs"]),
    ("card", &["emu/crates/emulator-core/src/pad.rs"]),
    (
        "mister",
        &["emu/crates/emulator-core/src/hardware_target.rs"],
    ),
    (
        "strict",
        &[
            "emu/crates/emulator-core/src/hardware_target.rs",
            "emu/crates/emulator-core/src/bus/exp2_strict.rs",
        ],
    ),
    ("hw", &["emu/crates/psx-gpu-render/"]),
    (
        "journey",
        &[
            "tools/psoxide-gate/src/journey.rs",
            "tools/psoxide-gate/src/expr.rs",
            "tools/psoxide-gate/src/exec.rs",
            "tools/psoxide-gate/src/machine.rs",
            "tools/psoxide-gate/src/compare.rs",
            "tools/psoxide-gate/src/img.rs",
        ],
    ),
];

/// The conformance rules as compiled in: their source, so any change to what a rule flags
/// changes the digest.
const RULES_SOURCE: &str = include_str!("../../../emu/crates/emulator-core/src/conformance.rs");

/// First 16 hex digits of the SHA-256 of the conformance rule set.
pub fn rules_digest() -> String {
    hex(&Sha256::digest(RULES_SOURCE.as_bytes()))[..16].to_string()
}

/// Every area with its version as printed: the numbered areas, then `rules=<hex16>` and
/// `journey=<n>`.
pub fn table() -> Vec<(&'static str, String)> {
    let mut t: Vec<(&'static str, String)> =
        AREAS.iter().map(|&(a, v)| (a, v.to_string())).collect();
    t.push(("rules", rules_digest()));
    t.push(("journey", JOURNEY_LANGUAGE.to_string()));
    t
}

/// What a finished run touched, read off the machine after it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Touched {
    pub cd: bool,
    pub spu: bool,
    pub mdec: bool,
    pub card: bool,
    pub mister: bool,
    pub strict: bool,
    pub hw: bool,
}

impl Touched {
    /// Read the devices of a finished run. `hw`: the run captured hardware-renderer frames.
    pub fn of(bus: &emulator_core::Bus, hw: bool) -> Touched {
        use emulator_core::hardware_target::{CdAckModel, Implementation, MdecRequestModel};
        let target = emulator_core::HardwareTarget::from_env();
        let card_touched = |port: &emulator_core::pad::PortDevice| {
            port.memcard()
                .is_some_and(|c| c.command_histogram().iter().any(|&n| n > 0))
        };
        Touched {
            cd: bus.cdrom.commands_dispatched() > 0,
            spu: bus.spu.register_writes() > 0,
            mdec: bus.mdec.commands_seen() > 0,
            card: card_touched(bus.sio0().port1()) || card_touched(bus.sio0().port2()),
            mister: target.implementation == Implementation::MisterFpga
                || target.gpu_irq != emulator_core::HardwareTarget::real().gpu_irq,
            strict: target.cd_ack == CdAckModel::Strict
                || target.mdec_req != MdecRequestModel::Shown
                || std::env::var("PSOXIDE_STRICT_EXP2").is_ok_and(|v| v == "1"),
            hw,
        }
    }

    /// The exercised areas with their versions, in table order. cpu, gte, muldiv, gpu, dma,
    /// timers, irq, pad, rules and journey are always in; the others when touched.
    pub fn areas(self) -> Vec<String> {
        table()
            .into_iter()
            .filter(|(area, _)| match *area {
                "cd" => self.cd,
                "spu" => self.spu,
                "mdec" => self.mdec,
                "card" => self.card,
                "mister" => self.mister,
                "strict" => self.strict,
                "hw" => self.hw,
                _ => true,
            })
            .map(|(area, version)| format!("{area}={version}"))
            .collect()
    }
}

/// The run digest: SHA-256 over every checkpoint's name, RAM hash and VRAM hash in order, then
/// the final tick count. Two runs that are the same run print the same digest.
#[derive(Clone, Default)]
pub struct RunDigest {
    hash: Sha256,
}

impl RunDigest {
    /// Add a checkpoint.
    pub fn checkpoint(&mut self, name: &str, ram: &[u8], vram: &[u16]) {
        let mut v = Sha256::new();
        for w in vram {
            v.update(w.to_le_bytes());
        }
        self.hash.update(name.as_bytes());
        self.hash.update([0]);
        self.hash.update(Sha256::digest(ram));
        self.hash.update(v.finalize());
    }

    /// The digest after the run's last tick, 32 hex digits.
    pub fn finish(self, ticks: u64) -> String {
        let mut hash = self.hash;
        hash.update(ticks.to_le_bytes());
        hex(&hash.finalize())[..32].to_string()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `AREAS <journey> <model> ...` and `DIGEST <journey> <model> <hex>`.
pub fn lines(journey: &str, model: &str, areas: &[String], digest: &str) -> [String; 2] {
    [
        format!("AREAS {journey} {model} {}", areas.join(" ")),
        format!("DIGEST {journey} {model} {digest}"),
    ]
}

/// `SIGNALS <journey> <model> gp0-1f=N gp0-1f-written=N cd-ack-broken=N mdec-commands=N
/// golden-on=yes|no`: everything the hardware models treat differently, as this run met it
/// (docs/conformance.md, "Model equivalence"). A model that differs from this one only in knobs whose
/// trigger reads 0 here would have run this very run. `None` while any count is unknown (no
/// boot, or a receipt from a gate that did not keep one).
pub fn signals_line(r: &crate::exec::JourneyResult, model: &str) -> Option<String> {
    let executed = r.gp0_irq_commands?;
    let written = r.facts.gp0_irq_writes?;
    let cd_ack = r.cd_ack_hits()?;
    let mdec = r.facts.mdec_command_writes?;
    let golden_on = if r.facts.golden_on { "yes" } else { "no" };
    Some(format!(
        "SIGNALS {} {model} gp0-1f={executed} gp0-1f-written={written} cd-ack-broken={cd_ack} \
         mdec-commands={mdec} golden-on={golden_on}",
        r.name
    ))
}

/// The model a run emulates, as the lines name it: the `--hardware` spec, `real` when unset.
pub fn model() -> String {
    std::env::var(emulator_core::hardware_target::ENV_VAR)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "real".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn files_under(root: &Path, prefix: &str) -> Vec<PathBuf> {
        let full = root.join(prefix);
        let mut out = Vec::new();
        if prefix.ends_with('/') {
            let mut stack = vec![full];
            while let Some(dir) = stack.pop() {
                for e in std::fs::read_dir(&dir).expect("area source dir") {
                    let p = e.expect("dir entry").path();
                    if p.is_dir() {
                        stack.push(p);
                    } else {
                        out.push(p);
                    }
                }
            }
        } else {
            let (dir, stem) = (full.parent().expect("parent"), full.file_name().unwrap());
            for e in std::fs::read_dir(dir).expect("area source dir") {
                let p = e.expect("dir entry").path();
                if p.is_file()
                    && p.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(&*stem.to_string_lossy())
                {
                    out.push(p);
                }
            }
        }
        out.retain(|p| {
            let n = p.file_name().unwrap().to_string_lossy();
            n != "tests.rs" && !n.ends_with("_tests.rs") && !n.starts_with('.')
        });
        out.sort();
        assert!(!out.is_empty(), "no source files for {prefix}");
        out
    }

    /// Digest of an area's source files (relative path and bytes of each).
    fn sources_digest(root: &Path, prefixes: &[&str]) -> String {
        let mut h = Sha256::new();
        for prefix in prefixes {
            for p in files_under(root, prefix) {
                let rel = p.strip_prefix(root).unwrap_or(&p);
                h.update(rel.to_string_lossy().as_bytes());
                h.update([0]);
                h.update(std::fs::read(&p).expect("read source"));
            }
        }
        hex(&h.finalize())
    }

    fn version_of(area: &str) -> String {
        table()
            .into_iter()
            .find(|(a, _)| *a == area)
            .map(|(_, v)| v)
            .expect("area in table")
    }

    /// The lock matches the sources and the versions: a source change needs a version bump
    /// and a renewed lock, or a renewed lock alone when behaviour did not change.
    #[test]
    fn behaviour_lock_follows_the_area_sources() {
        let root = repo_root();
        let lock_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("behaviour.lock");
        let now: BTreeMap<String, (String, String)> = SOURCES
            .iter()
            .map(|(area, prefixes)| {
                (
                    area.to_string(),
                    (version_of(area), sources_digest(&root, prefixes)),
                )
            })
            .collect();
        if std::env::var("PSOXIDE_BEHAVIOUR_RELOCK").is_ok_and(|v| v == "1") {
            let mut text = String::from(
                "# Behaviour lock: each area's version and the digest of its sources\n\
                 # (tools/psoxide-gate/src/behaviour.rs). Renew with\n\
                 # PSOXIDE_BEHAVIOUR_RELOCK=1 cargo test -p psoxide-gate behaviour\n",
            );
            for (area, (version, digest)) in &now {
                text.push_str(&format!("{area} = \"{version} {digest}\"\n"));
            }
            std::fs::write(&lock_path, text).expect("write behaviour.lock");
            return;
        }
        let text = std::fs::read_to_string(&lock_path).expect("behaviour.lock");
        let locked: BTreeMap<String, String> = toml::from_str(&text).expect("behaviour.lock");
        let mut problems = Vec::new();
        for (area, (version, digest)) in &now {
            let Some((lv, ld)) = locked.get(area).and_then(|s| s.split_once(' ')) else {
                problems.push(format!("{area}: not in the lock"));
                continue;
            };
            match (lv == version, ld == digest) {
                (true, true) => {}
                (true, false) => problems.push(format!(
                    "{area}: its sources changed and its version ({version}) did not: bump it in \
                     AREAS if behaviour changed, then renew the lock either way"
                )),
                (false, _) => problems.push(format!(
                    "{area}: version {version} in AREAS, {lv} in the lock: renew the lock"
                )),
            }
        }
        assert!(
            problems.is_empty(),
            "behaviour.lock is stale (renew with PSOXIDE_BEHAVIOUR_RELOCK=1 cargo test -p \
             psoxide-gate behaviour):\n{}",
            problems.join("\n")
        );
    }

    #[test]
    fn every_numbered_area_has_sources_and_the_table_prints_them_all() {
        for (area, _) in AREAS {
            assert!(
                SOURCES.iter().any(|(a, _)| *a == area),
                "{area} has no sources"
            );
        }
        let names: Vec<&str> = table().into_iter().map(|(a, _)| a).collect();
        assert_eq!(names.len(), AREAS.len() + 2);
        assert_eq!(&names[names.len() - 2..], ["rules", "journey"]);
        assert_eq!(rules_digest().len(), 16);
    }

    /// Always-on areas are listed for an idle run; the device areas only when touched.
    #[test]
    fn exercised_areas_follow_what_was_touched() {
        let idle = Touched::default().areas();
        let names: Vec<&str> = idle.iter().map(|a| a.split('=').next().unwrap()).collect();
        assert_eq!(
            names,
            ["cpu", "gte", "muldiv", "pad", "gpu", "dma", "timers", "irq", "rules", "journey"]
        );
        let busy = Touched {
            cd: true,
            mdec: true,
            hw: true,
            ..Touched::default()
        }
        .areas();
        assert!(busy.contains(&"cd=2".to_string()));
        assert!(busy.contains(&"mdec=1".to_string()));
        assert!(busy.contains(&"hw=2".to_string()));
        assert!(!busy.iter().any(|a| a.starts_with("spu=")));
    }

    #[test]
    fn run_digest_is_the_same_for_the_same_run_only() {
        let ram = vec![7u8; 64];
        let vram = vec![9u16; 32];
        let run = |name: &str, ticks: u64, ram: &[u8]| {
            let mut d = RunDigest::default();
            d.checkpoint(name, ram, &vram);
            d.finish(ticks)
        };
        let a = run("menu", 100, &ram);
        assert_eq!(a, run("menu", 100, &ram));
        assert_eq!(a.len(), 32);
        assert_ne!(a, run("menu", 101, &ram));
        assert_ne!(a, run("title", 100, &ram));
        let mut other = ram.clone();
        other[3] = 8;
        assert_ne!(a, run("menu", 100, &other));
    }
}
