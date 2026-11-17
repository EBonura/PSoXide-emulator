//! Result receipts: a passed journey is not played again while nothing that can change its
//! verdict has changed.
//!
//! A receipt is a small TOML file named after the journey and the first 16 hex digits of a key.
//! The key is the SHA-256 of every input the verdict depends on, listed in clear inside the
//! receipt: the disc image (cue, every track file, `.sbi`), the journey file, each golden frame,
//! symbol file, tape and card fixture the journey names, the gate binary itself (which holds the
//! emulator core, so any change to either changes it), the hardware target, the entry state, every
//! flag that alters behaviour and every `PSOXIDE_`, `PSX_` and `WGPU_` environment variable.
//!
//! Only a PASS is written and only a PASS is honoured. A journey whose inputs cannot all be
//! listed (a disc format the gate cannot enumerate, an unreadable input) is refused, never guessed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::exec::{digest, JourneyResult};
use crate::journey::Journey;

/// Environment variables whose names start with one of these are part of every key. Every
/// variable the emulator, the renderer and the gate read has such a prefix; a test fails when
/// a new read does not.
const ENV_PREFIXES: [&str; 3] = ["PSOXIDE_", "PSX_", "WGPU_"];

/// Where the verdict of a journey came from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum State {
    /// Receipts are not in use.
    #[default]
    Off,
    /// A receipt with this key said PASS and the journey was not played.
    Hit,
    /// Played, because no receipt matched.
    Ran,
    /// Played again on request and the receipt held.
    Verified,
    /// Played again on request and the fresh result differs from the receipt.
    Mismatch,
    /// Played, and no receipt can be kept for it; the reason.
    Refused(String),
}

/// One named input of a key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Input {
    pub name: String,
    pub value: String,
}

/// What a receipt is looked up by.
#[derive(Clone, Debug)]
pub struct Key {
    pub hex: String,
    pub inputs: Vec<Input>,
}

impl Key {
    /// The first 16 hex digits, for file names and messages.
    pub fn prefix(&self) -> &str {
        &self.hex[..16]
    }
}

/// A kept PASS.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub key: String,
    pub journey: String,
    pub verdict: String,
    pub ticks: u64,
    pub emu_secs: f64,
    pub wall_secs: f64,
    /// GP0(1Fh) commands the guest issued.
    pub gp0_1f: u64,
    /// CD acknowledge writes that broke a conformance rule (docs/conformance.md).
    #[serde(default)]
    pub cd_ack: u64,
    pub disc_id: String,
    /// UTC, `YYYY-MM-DDTHH:MM:SSZ`.
    pub date: String,
    pub unix_secs: u64,
    #[serde(default)]
    pub input: Vec<Input>,
}

impl Receipt {
    /// The receipt for a finished run, or `None` unless it passed and counted its GP0(1Fh)
    /// commands: a failure, an abort or an uncounted run is never kept.
    pub fn of(result: &JourneyResult, key: &Key) -> Option<Receipt> {
        let gp0_1f = result.gp0_irq_commands?;
        let cd_ack = result.cd_ack_hits()?;
        if !result.passed() || result.abort.is_some() {
            return None;
        }
        let unix_secs = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        Some(Receipt {
            key: key.hex.clone(),
            journey: result.name.clone(),
            verdict: "PASS".into(),
            ticks: result.ticks,
            emu_secs: result.emu_secs,
            wall_secs: result.wall.as_secs_f64(),
            gp0_1f,
            cd_ack,
            disc_id: result.disc_id.clone(),
            date: utc_date_time(unix_secs),
            unix_secs,
            input: key.inputs.clone(),
        })
    }

    /// The one loud line a hit prints.
    pub fn hit_line(&self) -> String {
        format!(
            "RECEIPT HIT {} {} (passed {}, {:.1} s saved)",
            self.journey,
            &self.key[..16],
            self.date.split('T').next().unwrap_or(&self.date),
            self.wall_secs
        )
    }

    /// What differs between this receipt and a fresh result, or `None` when they agree.
    pub fn differs_from(&self, fresh: &JourneyResult) -> Option<String> {
        let verdict = if fresh.passed() { "PASS" } else { "FAIL" };
        let mut diffs = Vec::new();
        if verdict != self.verdict {
            diffs.push(format!("verdict {} then, {verdict} now", self.verdict));
        }
        if fresh.ticks != self.ticks {
            diffs.push(format!("ticks {} then, {} now", self.ticks, fresh.ticks));
        }
        if fresh.gp0_irq_commands != Some(self.gp0_1f) {
            diffs.push(format!(
                "GP0(1Fh) {} then, {} now",
                self.gp0_1f,
                fresh
                    .gp0_irq_commands
                    .map_or("uncounted".to_string(), |n| n.to_string())
            ));
        }
        if fresh.cd_ack_hits() != Some(self.cd_ack) {
            diffs.push(format!(
                "CD acknowledge conformance hits {} then, {} now",
                self.cd_ack,
                fresh
                    .cd_ack_hits()
                    .map_or("uncounted".to_string(), |n| n.to_string())
            ));
        }
        (!diffs.is_empty()).then(|| diffs.join("; "))
    }
}

/// A directory of receipts.
#[derive(Clone, Debug)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: &Path) -> Store {
        Store {
            dir: dir.to_path_buf(),
        }
    }

    fn path(&self, journey: &str, key: &Key) -> PathBuf {
        self.dir
            .join(format!("{journey}-{}.receipt.toml", key.prefix()))
    }

    /// The PASS receipt with exactly this key, if there is one. A file that does not parse, names
    /// another key or says anything but PASS is not a receipt.
    pub fn find(&self, journey: &str, key: &Key) -> Option<Receipt> {
        let path = self.path(journey, key);
        let text = std::fs::read_to_string(&path).ok()?;
        match toml::from_str::<Receipt>(&text) {
            Ok(r) if r.key == key.hex && r.journey == journey && r.verdict == "PASS" => Some(r),
            Ok(_) => None,
            Err(e) => {
                eprintln!("[gate] ignoring unreadable receipt {}: {e}", path.display());
                None
            }
        }
    }

    /// Keep a receipt, atomically (journeys finish on two threads). Refuses anything but a PASS.
    pub fn write(&self, receipt: &Receipt) -> Result<PathBuf, String> {
        if receipt.verdict != "PASS" {
            return Err(format!("not keeping a {} receipt", receipt.verdict));
        }
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| format!("mkdir {}: {e}", self.dir.display()))?;
        let path = self.dir.join(format!(
            "{}-{}.receipt.toml",
            receipt.journey,
            &receipt.key[..16]
        ));
        let text = toml::to_string(receipt).map_err(|e| format!("receipt: {e}"))?;
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, text).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("rename {}: {e}", path.display()))?;
        Ok(path)
    }
}

/// Everything a key is built from besides the process (see `compute_key`).
pub struct Context<'a> {
    pub journey: &'a Journey,
    pub journey_path: &'a Path,
    pub repo_root: &'a Path,
    pub disc: &'a Path,
    pub golden_dir: &'a Path,
    /// Every flag and setting that alters behaviour, as name and value.
    pub flags: &'a [(String, String)],
}

/// The key for a journey about to run in this process, or why no receipt can cover it.
pub fn compute_key(c: &Context<'_>) -> Result<Key, String> {
    let env: Vec<(String, String)> = std::env::vars_os()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    key_from(c, &env, &build_id()?)
}

/// `compute_key` with the environment and the build identity given.
pub fn key_from(c: &Context<'_>, env: &[(String, String)], build: &str) -> Result<Key, String> {
    let mut inputs = vec![
        Input {
            name: "build".into(),
            value: build.into(),
        },
        Input {
            name: "host".into(),
            value: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        },
    ];
    for (name, value) in c.flags {
        inputs.push(Input {
            name: format!("flag {name}"),
            value: value.clone(),
        });
    }
    let mut vars: Vec<&(String, String)> = env
        .iter()
        .filter(|(k, _)| ENV_PREFIXES.iter().any(|p| k.starts_with(p)))
        .collect();
    vars.sort();
    for (name, value) in vars {
        inputs.push(Input {
            name: format!("env {name}"),
            value: value.clone(),
        });
    }
    let mut file = |name: String, path: &Path, must_exist: bool| -> Result<(), String> {
        let value = match file_sha256(path) {
            Ok(sha) => sha,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !must_exist => "absent".into(),
            Err(e) => return Err(format!("cannot read {} for the key: {e}", path.display())),
        };
        inputs.push(Input { name, value });
        Ok(())
    };
    file("journey".into(), c.journey_path, true)?;
    for (name, path) in disc_files(c.disc)? {
        // The `.sbi` next to a cue is read only when it exists.
        let must_exist = name != "disc sbi";
        file(name, &path, must_exist)?;
    }
    for rel in c.journey.symbols.list() {
        file(format!("symbols {rel}"), &c.repo_root.join(rel), false)?;
    }
    if let Some(f) = &c.journey.card_fixture {
        file(
            format!("card fixture {}", f.path),
            &c.repo_root.join(&f.path),
            true,
        )?;
    }
    let mut tapes: Vec<&str> = c
        .journey
        .all_steps()
        .iter()
        .filter_map(|s| s.tape.as_deref())
        .collect();
    tapes.sort_unstable();
    tapes.dedup();
    for rel in tapes {
        file(format!("tape {rel}"), &c.repo_root.join(rel), true)?;
    }
    let mut goldens: Vec<&str> = c
        .journey
        .all_steps()
        .iter()
        .filter_map(|s| s.checkpoint.as_deref())
        .collect();
    goldens.sort_unstable();
    goldens.dedup();
    for name in goldens {
        file(
            format!("golden {name}"),
            &c.golden_dir.join(format!("{name}.png")),
            false,
        )?;
    }
    let mut text = String::new();
    for i in &inputs {
        text.push_str(&format!("{}={}\n", i.name, i.value));
    }
    let hex = format!("{:x}", Sha256::digest(text.as_bytes()));
    Ok(Key { hex, inputs })
}

/// Every file the emulator reads to mount `disc`, by role.
fn disc_files(disc: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let ext = disc
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "cue" => {
            let mut files = vec![("disc cue".to_string(), disc.to_path_buf())];
            for (i, track) in psoxide_settings::library::cue_referenced_files(disc)?
                .into_iter()
                .enumerate()
            {
                files.push((format!("disc file {}", i + 1), track));
            }
            files.push((
                "disc sbi".into(),
                psoxide_settings::library::sbi_path_for(disc),
            ));
            Ok(files)
        }
        "bin" | "iso" => Ok(vec![("disc image".into(), disc.to_path_buf())]),
        "ccd" => {
            let img = disc.with_extension("img");
            let mut ecm = img.clone().into_os_string();
            ecm.push(".ecm");
            let image = if img.exists() {
                img
            } else {
                PathBuf::from(ecm)
            };
            Ok(vec![
                ("disc ccd".into(), disc.to_path_buf()),
                ("disc image".into(), image),
            ])
        }
        other => Err(format!(
            "disc extension .{other} is not one the gate can list the files of"
        )),
    }
}

type FileKey = (PathBuf, u64, u128);

/// SHA-256 of a file, remembered for the run by path, size and modification time: `run all`
/// hashes one multi-hundred-megabyte disc once, not once per journey.
fn file_sha256(path: &Path) -> std::io::Result<String> {
    static MEMO: OnceLock<Mutex<HashMap<FileKey, String>>> = OnceLock::new();
    let meta = std::fs::metadata(path)?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    let memo_key = (path.to_path_buf(), meta.len(), mtime);
    let memo = MEMO.get_or_init(Default::default);
    if let Some(hit) = memo.lock().expect("memo").get(&memo_key) {
        return Ok(hit.clone());
    }
    let sha = hex(&digest::sha256_of(&mut std::fs::File::open(path)?)?);
    memo.lock().expect("memo").insert(memo_key, sha.clone());
    Ok(sha)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Identity of the running gate: the hash of its own executable. The emulator core, the JIT and
/// the renderer are linked into it, so a change to any of them is a different build.
fn build_id() -> Result<String, String> {
    static ID: OnceLock<Result<String, String>> = OnceLock::new();
    ID.get_or_init(|| {
        let exe = std::env::current_exe().map_err(|e| format!("gate executable: {e}"))?;
        let sha = file_sha256(&exe).map_err(|e| format!("hash {}: {e}", exe.display()))?;
        Ok(format!(
            "psoxide-gate {} sha256 {sha}",
            env!("CARGO_PKG_VERSION")
        ))
    })
    .clone()
}

/// Whether the second hardware model can take the first one's result: it passed, the guest
/// never issued GP0(1Fh) and never wrote a CD acknowledge that breaks a conformance rule, the
/// only things the models treat differently (`cd-ack=strict` changes nothing else).
///
/// A result with a golden compare skipped by `golden_on` stands for no other model: the picture
/// was never judged here, so the second model must look at it itself.
pub fn model_equivalent(first: &JourneyResult) -> bool {
    first.passed()
        && first.gp0_irq_commands == Some(0)
        && first.cd_ack_hits() == Some(0)
        && first.count(crate::exec::Status::GoldenSkipped) == 0
}

/// `YYYY-MM-DDTHH:MM:SSZ` for a Unix time (proleptic Gregorian, UTC).
fn utc_date_time(unix_secs: u64) -> String {
    let (days, rem) = (unix_secs / 86_400, unix_secs % 86_400);
    // Civil-from-days (Howard Hinnant), shifted to start in March.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Check, Group, Status};
    use std::time::Duration;

    /// A repo with a disc, a journey that names a golden, a symbol file and a tape.
    struct Fixture {
        dir: tempfile::TempDir,
        journey: Journey,
    }

    const JOURNEY: &str = "name = 'fx'\ndisc = 'disc.cue'\nsymbols = 'game.sym'\n\
        [[step]]\ntape = 'run.tape'\nwait = 1\n[[step]]\ncheckpoint = 'menu'\n";

    impl Fixture {
        fn new() -> Fixture {
            let dir = tempfile::tempdir().unwrap();
            let w = |name: &str, bytes: &[u8]| {
                let path = dir.path().join(name);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, bytes).unwrap();
            };
            w(
                "disc.cue",
                b"FILE \"disc.bin\" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:00\n",
            );
            w("disc.bin", b"disc bytes");
            w("tests/journey.toml", JOURNEY.as_bytes());
            w("game.sym", b"symbols");
            w("run.tape", b"tape");
            w("tests/golden/fx/menu.png", b"golden");
            let journey = toml::from_str(JOURNEY).unwrap();
            Fixture { dir, journey }
        }

        fn write(&self, name: &str, bytes: &[u8]) {
            std::fs::write(self.dir.path().join(name), bytes).unwrap();
        }

        fn key_with(
            &self,
            flags: &[(String, String)],
            env: &[(String, String)],
            build: &str,
        ) -> Result<Key, String> {
            let root = self.dir.path();
            key_from(
                &Context {
                    journey: &self.journey,
                    journey_path: &root.join("tests/journey.toml"),
                    repo_root: root,
                    disc: &root.join("disc.cue"),
                    golden_dir: &root.join("tests/golden/fx"),
                    flags,
                },
                env,
                build,
            )
        }

        fn key(&self) -> String {
            self.key_with(&[], &[], "build-a").unwrap().hex
        }
    }

    #[test]
    fn the_key_changes_when_any_single_input_changes() {
        let fx = Fixture::new();
        let base = fx.key();
        assert_eq!(base, fx.key(), "the key is stable");
        let flag = |v: &str| vec![("hardware".to_string(), v.to_string())];
        let env = |v: &str| vec![("PSOXIDE_CHECK_GPU_LL_FIFO".to_string(), v.to_string())];
        let mut seen = vec![base.clone()];
        let mut distinct = |key: String| {
            assert!(!seen.contains(&key), "an input did not change the key");
            seen.push(key);
        };
        distinct(fx.key_with(&[], &[], "build-b").unwrap().hex);
        distinct(fx.key_with(&flag("real"), &[], "build-a").unwrap().hex);
        distinct(fx.key_with(&flag("mister"), &[], "build-a").unwrap().hex);
        distinct(fx.key_with(&[], &env("1"), "build-a").unwrap().hex);
        distinct(fx.key_with(&[], &env("0"), "build-a").unwrap().hex);
        for (file, bytes) in [
            ("disc.bin", &b"other disc"[..]),
            (
                "disc.cue",
                b"FILE \"disc.bin\" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:01\n",
            ),
            ("disc.sbi", b"SBI\0"),
            ("tests/journey.toml", b"name = 'fx'\ndisc = 'disc.cue'\n"),
            ("game.sym", b"other symbols"),
            ("run.tape", b"other tape"),
            ("tests/golden/fx/menu.png", b"other golden"),
        ] {
            fx.write(file, bytes);
            distinct(fx.key());
        }
    }

    #[test]
    fn variables_outside_the_prefixes_do_not_change_the_key() {
        let fx = Fixture::new();
        let noise = vec![("EDITOR".to_string(), "vi".to_string())];
        assert_eq!(fx.key(), fx.key_with(&[], &noise, "build-a").unwrap().hex);
    }

    #[test]
    fn an_unknown_disc_is_refused_and_a_card_journey_is_keyed() {
        let mut fx = Fixture::new();
        fx.journey = toml::from_str(
            "name = 'fx'\n[[step]]\nlabel = 'cut'\n[step.card]\naction = 'snapshot'\n",
        )
        .unwrap();
        assert!(fx.key_with(&[], &[], "b").is_ok());
        let root = fx.dir.path();
        let flags: [(String, String); 0] = [];
        let err = key_from(
            &Context {
                journey: &fx.journey,
                journey_path: &root.join("tests/journey.toml"),
                repo_root: root,
                disc: &root.join("disc.mdf"),
                golden_dir: root,
                flags: &flags,
            },
            &[],
            "b",
        )
        .unwrap_err();
        assert!(err.contains("mdf"), "{err}");
    }

    fn result(passed: bool, gp0: Option<u64>) -> JourneyResult {
        JourneyResult {
            name: "fx".into(),
            title: "fx".into(),
            disc: PathBuf::from("disc.cue"),
            disc_id: "0123456789ab".into(),
            groups: vec![Group {
                name: "menu".into(),
                label: None,
                tick: 7,
                capture: None,
                checks: vec![Check {
                    name: "c".into(),
                    status: if passed { Status::Pass } else { Status::Fail },
                    detail: String::new(),
                }],
                wants_golden: false,
            }],
            abort: None,
            ticks: 7,
            emu_secs: 7.0 / 60.0,
            wall: Duration::from_secs(12),
            hw_adapter: None,
            notes: Vec::new(),
            cards: Vec::new(),
            gp0_irq_commands: gp0,
            conformance: Some(Vec::new()),
            cd_acknowledges: None,
            mdec_commands: None,
            receipt: State::Off,
        }
    }

    #[test]
    fn a_failure_an_abort_or_an_uncounted_run_is_never_cached() {
        let fx = Fixture::new();
        let key = fx.key_with(&[], &[], "b").unwrap();
        assert!(Receipt::of(&result(false, Some(0)), &key).is_none());
        let mut aborted = result(true, Some(0));
        aborted.abort = Some("guest fault".into());
        assert!(Receipt::of(&aborted, &key).is_none());
        assert!(Receipt::of(&result(true, None), &key).is_none());
        let store = Store::new(&fx.dir.path().join("receipts"));
        let mut forged = Receipt::of(&result(true, Some(0)), &key).unwrap();
        forged.verdict = "FAIL".into();
        assert!(store.write(&forged).is_err());
        // A FAIL file dropped into the store by hand is not honoured either.
        std::fs::create_dir_all(fx.dir.path().join("receipts")).unwrap();
        std::fs::write(
            fx.dir
                .path()
                .join(format!("receipts/fx-{}.receipt.toml", key.prefix())),
            toml::to_string(&forged).unwrap(),
        )
        .unwrap();
        assert!(store.find("fx", &key).is_none());
    }

    #[test]
    fn a_kept_pass_is_found_by_its_key_only_and_reported_as_a_hit() {
        let fx = Fixture::new();
        let store = Store::new(&fx.dir.path().join("receipts"));
        let key = fx.key_with(&[], &[], "b").unwrap();
        assert!(store.find("fx", &key).is_none());
        let kept = Receipt::of(&result(true, Some(3)), &key).unwrap();
        store.write(&kept).unwrap();
        let found = store.find("fx", &key).expect("receipt");
        assert_eq!(
            (found.ticks, found.gp0_1f, found.input.len()),
            (7, 3, key.inputs.len())
        );
        let line = found.hit_line();
        assert!(
            line.starts_with(&format!("RECEIPT HIT fx {} (passed 20", key.prefix())),
            "{line}"
        );
        assert!(line.ends_with(", 12.0 s saved)"), "{line}");
        let other = fx.key_with(&[], &[], "c").unwrap();
        assert!(store.find("fx", &other).is_none());
    }

    #[test]
    fn a_fresh_result_that_differs_from_the_receipt_is_named() {
        let fx = Fixture::new();
        let key = fx.key_with(&[], &[], "b").unwrap();
        let kept = Receipt::of(&result(true, Some(0)), &key).unwrap();
        assert_eq!(kept.differs_from(&result(true, Some(0))), None);
        let mut later = result(true, Some(2));
        later.ticks = 9;
        let said = kept.differs_from(&later).unwrap();
        assert!(
            said.contains("ticks 7 then, 9 now") && said.contains("GP0(1Fh) 0 then, 2 now"),
            "{said}"
        );
        assert!(kept
            .differs_from(&result(false, Some(0)))
            .unwrap()
            .contains("verdict PASS then, FAIL now"));
    }

    #[test]
    fn the_second_model_waits_for_a_pass_with_no_gp0_1f() {
        assert!(model_equivalent(&result(true, Some(0))));
        assert!(!model_equivalent(&result(true, Some(1))));
        assert!(!model_equivalent(&result(true, None)));
        assert!(!model_equivalent(&result(false, Some(0))));
    }

    #[test]
    fn the_second_model_waits_for_a_pass_with_no_broken_cd_acknowledge() {
        use emulator_core::conformance::Rule;
        let hit = |rule| crate::exec::ConformanceHit {
            rule,
            hits: 3,
            first_pc: Some(0x8001_0000),
            symbol: None,
        };
        let mut r = result(true, Some(0));
        r.conformance = Some(vec![hit(Rule::CdAckPartial)]);
        assert!(!model_equivalent(&r));
        r.conformance = Some(vec![hit(Rule::CdAckWithParamReset)]);
        assert!(!model_equivalent(&r));
        r.conformance = None;
        assert!(!model_equivalent(&r));
        // The receipt keeps the count and a fresh run that differs is caught.
        let key = Key {
            hex: "0".repeat(64),
            inputs: Vec::new(),
        };
        r.conformance = Some(Vec::new());
        let kept = Receipt::of(&r, &key).expect("a clean pass is kept");
        assert_eq!(kept.cd_ack, 0);
        r.conformance = Some(vec![hit(Rule::CdAckPartial)]);
        assert!(kept
            .differs_from(&r)
            .is_some_and(|d| d.contains("CD acknowledge")));
    }

    #[test]
    fn dates_are_utc() {
        assert_eq!(utc_date_time(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_date_time(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc_date_time(1_794_700_800 + 3_661), "2026-11-15T01:01:01Z");
    }

    /// Every environment variable the emulator, the renderer and the gate read must start with a
    /// prefix the key includes, or a verdict could depend on a variable the key cannot see.
    #[test]
    fn every_environment_read_in_the_gate_and_the_core_has_a_keyed_prefix() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut files = Vec::new();
        for dir in [
            "emu/crates",
            "crates",
            "sdk/crates",
            "tools/psoxide-gate/src",
        ] {
            rust_files(&root.join(dir), &mut files);
        }
        assert!(files.len() > 100, "found only {} sources", files.len());
        let mut unkeyed = Vec::new();
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap_or_default();
            for pat in ["env::var(\"", "env::var_os(\"", "env_flag!(\""] {
                for part in text.split(pat).skip(1) {
                    let name = part.split('"').next().unwrap_or("");
                    let known = ENV_PREFIXES.iter().any(|p| name.starts_with(p));
                    if !known && name != "HOME" && name != "CARGO_MANIFEST_DIR" {
                        unkeyed.push(format!("{name} in {}", file.display()));
                    }
                }
            }
        }
        assert!(
            unkeyed.is_empty(),
            "add these to ENV_PREFIXES or the key: {unkeyed:?}"
        );
    }

    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // The frontend, tests, examples and benches are not linked into the gate.
                let skipped = ["target", "frontend", "tests", "examples", "benches"];
                if path
                    .file_name()
                    .is_some_and(|n| !skipped.iter().any(|s| n == *s))
                {
                    rust_files(&path, out);
                }
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
}
