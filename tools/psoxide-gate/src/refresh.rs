//! Recompose verified capture data without starting an emulator.
use crate::exec::{Capture, Group, JourneyResult};
use crate::img::{Img, Rect};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    capture: Vec<Record>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Record {
    game: String,
    title: String,
    checkpoint: String,
    tick: u64,
    frame: Option<u64>,
    bin: PathBuf,
    bin_sha256: String,
    report: PathBuf,
    report_sha256: String,
    cpu: PathBuf,
    cpu_sha256: String,
    hw1: PathBuf,
    hw1_sha256: String,
    hw3: PathBuf,
    hw3_sha256: String,
    #[serde(default)]
    strict: Vec<Rect>,
    receipt: Option<PathBuf>,
    notes: Option<String>,
}
fn verified(path: &Path, expected: &str) -> Result<(), String> {
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("full SHA256 required for {}", path.display()));
    }
    let mut f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hash = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    let actual = format!("{:x}", hash.finalize());
    if actual != expected.to_ascii_lowercase() {
        return Err(format!(
            "SHA256 mismatch for {}: expected {expected}, found {actual}",
            path.display()
        ));
    }
    Ok(())
}
fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn run(manifest: &Path, out: &Path) -> Result<(), String> {
    if out.exists() {
        return Err(format!(
            "refresh output must be new to preserve evidence: {}",
            out.display()
        ));
    }
    let text = std::fs::read_to_string(manifest).map_err(|e| e.to_string())?;
    let m: Manifest = toml::from_str(&text).map_err(|e| format!("{}: {e}", manifest.display()))?;
    if m.capture.is_empty() {
        return Err("no captures in manifest".into());
    }
    let mut results: BTreeMap<String, JourneyResult> = BTreeMap::new();
    let mut verified_files = BTreeMap::<PathBuf, String>::new();
    for r in m.capture {
        if !safe_name(&r.game) || !safe_name(&r.checkpoint) {
            return Err(
                "game/checkpoint names must contain only letters, digits, '-' or '_'".into(),
            );
        }
        for (path, hash) in [
            (&r.bin, &r.bin_sha256),
            (&r.report, &r.report_sha256),
            (&r.cpu, &r.cpu_sha256),
            (&r.hw1, &r.hw1_sha256),
            (&r.hw3, &r.hw3_sha256),
        ] {
            if !path.is_absolute() {
                return Err(format!(
                    "absolute capture/identity path required: {}",
                    path.display()
                ));
            }
            if let Some(previous) = verified_files.get(path) {
                if previous != hash {
                    return Err(format!("conflicting identity for {}", path.display()));
                }
            } else {
                verified(path, hash)?;
                verified_files.insert(path.clone(), hash.clone());
            }
        }
        let source = std::fs::read_to_string(&r.report).map_err(|e| e.to_string())?;
        if !source.contains(&r.bin_sha256[..12]) {
            return Err(format!(
                "source report is not bound to BIN {}",
                r.bin_sha256
            ));
        }
        // Existing reports expose tick identity, never a video-frame counter.
        let header = format!("<h3>{} ", r.checkpoint);
        let section = source
            .split(&header)
            .nth(1)
            .and_then(|s| s.split("</section>").next())
            .ok_or_else(|| format!("checkpoint {} absent from source report", r.checkpoint))?;
        let recorded_tick = section.split("tick ").nth(1).map(|s| {
            s.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
        });
        if recorded_tick.as_deref() != Some(r.tick.to_string().as_str()) {
            return Err(format!(
                "checkpoint {} tick {} not bound to source report",
                r.checkpoint, r.tick
            ));
        }
        for (path, suffix) in [
            (&r.cpu, ".new.png"),
            (&r.hw1, ".hw1.png"),
            (&r.hw3, ".hw3.png"),
        ] {
            let name = format!("{}{}", r.checkpoint, suffix);
            if path.file_name().and_then(|s| s.to_str()) != Some(&name) {
                return Err(format!(
                    "{} is not the {} capture",
                    path.display(),
                    r.checkpoint
                ));
            }
            let captured = std::fs::canonicalize(path).map_err(|e| e.to_string())?;
            let bound = section
                .split("href=\"")
                .skip(1)
                .filter_map(|s| s.split('"').next())
                .map(|s| s.replace("&amp;", "&").replace("&quot;", "\""))
                .any(|href| {
                    std::fs::canonicalize(r.report.parent().unwrap().join(href))
                        .ok()
                        .as_ref()
                        == Some(&captured)
                });
            if !bound {
                return Err(format!(
                    "{} is not linked by source checkpoint {}",
                    path.display(),
                    r.checkpoint
                ));
            }
        }
        if r.frame.is_some() {
            return Err("legacy report has no video-frame identity; frame must be omitted".into());
        }
        let cpu = Img::load_png(&r.cpu)?;
        let hw1 = Img::load_png(&r.hw1)?;
        let hw3 = Img::load_png(&r.hw3)?;
        if (hw1.w, hw1.h) != (cpu.w, cpu.h) || (hw3.w, hw3.h) != (cpu.w * 3, cpu.h * 3) {
            return Err(format!(
                "{}:{} renderer dimensions do not match CPU1/HW1/HW3",
                r.game, r.checkpoint
            ));
        }
        let entry=results.entry(r.game.clone()).or_insert_with(||JourneyResult {
            name:r.game.clone(),title:r.title.clone(),disc:r.bin.clone(),disc_id:r.bin_sha256.clone(),
            groups:Vec::new(),abort:None,ticks:0,emu_secs:0.0,wall:std::time::Duration::ZERO,
            hw_adapter:None,notes:vec!["Report refresh from hash-verified raw captures. Acceptance verdicts remain in the original report; this refresh makes no new gameplay or tolerance acceptance claim.".into()],cards:Vec::new(),
        });
        if entry.disc_id != r.bin_sha256 {
            return Err(format!("{} combines different BIN identities", r.game));
        }
        if entry.groups.iter().any(|g| g.name == r.checkpoint) {
            return Err(format!("duplicate checkpoint {}:{}", r.game, r.checkpoint));
        }
        entry.ticks = entry.ticks.max(r.tick);
        entry.notes.push(format!("{}: source report {} SHA256 {}; raw CPU1 {} SHA256 {}; raw HW1 {} SHA256 {}; raw HW3 {} SHA256 {}",r.checkpoint,r.report.display(),r.report_sha256,r.cpu.display(),r.cpu_sha256,r.hw1.display(),r.hw1_sha256,r.hw3.display(),r.hw3_sha256));
        if let Some(receipt) = r.receipt {
            entry
                .notes
                .push(format!("Artifact binding receipt: {}", receipt.display()));
        }
        if let Some(notes) = r.notes {
            entry.notes.push(notes);
        }
        entry.groups.push(Group {
            name: r.checkpoint,
            label: Some("verified capture refresh; original acceptance retained separately".into()),
            tick: r.tick,
            capture: Some(Capture {
                frame: r.frame,
                strict: r.strict,
                cpu,
                golden: None,
                golden_error: None,
                hw: vec![(1, hw1), (3, hw3)],
                hw_skip: None,
            }),
            checks: Vec::new(),
            wants_golden: false,
        });
    }
    let results: Vec<_> = results.into_values().collect();
    let index =
        crate::report::write_report(out, &results, &[], "PSoXide installed capture review")?;
    std::fs::write(out.join("capture-manifest.toml"), text).map_err(|e| e.to_string())?;
    let mut inventory =
        String::from("game\tBIN_SHA256\tcheckpoint\ttick\tframe\tHW1_exact\tHW3_exact\n");
    for r in &results {
        for g in &r.groups {
            let c = g.capture.as_ref().unwrap();
            use std::fmt::Write as _;
            let a = c.hw[0].1.diff(&c.cpu, 0, 1, &[], None).differing;
            let b = c.hw[1].1.diff(&c.cpu.enlarge(3), 0, 1, &[], None).differing;
            let _ = writeln!(
                inventory,
                "{}\t{}\t{}\t{}\tunrecorded\t{a}\t{b}",
                r.name, r.disc_id, g.name, g.tick
            );
        }
    }
    std::fs::write(out.join("exact-diff-inventory.tsv"), inventory).map_err(|e| e.to_string())?;
    println!("report: {}", index.display());
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BeforeAfter {
    before: Side,
    after: Side,
    out: PathBuf,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Side {
    image: PathBuf,
    sha256: String,
    build: String,
    checkpoint: String,
    mode: String,
    scale: u32,
    tick: u64,
}
/// Matched-mode source pixels in one 3x display row, with embedded labels.
pub fn before_after(manifest: &Path) -> Result<(), String> {
    let m: BeforeAfter =
        toml::from_str(&std::fs::read_to_string(manifest).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    if m.out.exists() {
        return Err("before/after output already exists".into());
    }
    verified(&m.before.image, &m.before.sha256)?;
    verified(&m.after.image, &m.after.sha256)?;
    if m.before.mode != m.after.mode
        || m.before.scale != m.after.scale
        || m.before.checkpoint != m.after.checkpoint
    {
        return Err("before/after requires matching mode, scale and checkpoint".into());
    }
    let a = Img::load_png(&m.before.image)?;
    let b = Img::load_png(&m.after.image)?;
    if (a.w, a.h) != (b.w, b.h) {
        return Err("before/after dimensions differ".into());
    }
    let count = a.diff(&b, 0, 1, &[], None).differing;
    let label = |side: &Side, which: &str, display: u32| {
        let mut lines =
            crate::contact::label_lines(&side.mode, side.scale, &side.checkpoint, side.tick, None);
        lines.push(format!("{which} BUILD={} DISPLAY={display}X", side.build));
        lines.push(if count == 0 {
            "NO VISIBLE CHANGE: 0 DIFFERING PIXELS".into()
        } else {
            format!("{count} EXACT DIFFERING PIXELS")
        });
        lines
    };
    let display = 3;
    if m.before.scale == 0 || display % m.before.scale != 0 {
        return Err("source scale must divide the requested 3x presentation scale".into());
    }
    let enlargement = display / m.before.scale;
    let a = crate::contact::labeled_with_font(
        &a.enlarge(enlargement),
        &label(&m.before, "BEFORE", display),
        2,
    );
    let b = crate::contact::labeled_with_font(
        &b.enlarge(enlargement),
        &label(&m.after, "AFTER", display),
        2,
    );
    let w = a.w + b.w + 8;
    let h = a.h.max(b.h);
    let mut page = Img::from_rgba(w, h, vec![24; (w * h * 4) as usize]);
    for (img, x) in [(&a, 0), (&b, a.w + 8)] {
        for row in 0..img.h {
            let src = (row * img.w * 4) as usize;
            let dst = ((row * w + x) * 4) as usize;
            page.rgba[dst..dst + (img.w * 4) as usize]
                .copy_from_slice(&img.rgba[src..src + (img.w * 4) as usize]);
        }
    }
    page.save_png(&m.out)?;
    println!(
        "{}: {count} exact differing pixels{}",
        m.out.display(),
        if count == 0 {
            "; no visible change"
        } else {
            ""
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(dir: &Path) -> Record {
        let bin = dir.join("game.bin");
        std::fs::write(&bin, b"bound disc").unwrap();
        let hash = |p: &Path| format!("{:x}", Sha256::digest(std::fs::read(p).unwrap()));
        let cpu = dir.join("menu.new.png");
        let hw1 = dir.join("menu.hw1.png");
        let hw3 = dir.join("menu.hw3.png");
        let image = Img::from_rgba(2, 1, vec![10; 8]);
        image.save_png(&cpu).unwrap();
        image.save_png(&hw1).unwrap();
        image.enlarge(3).save_png(&hw3).unwrap();
        let report = dir.join("index.html");
        std::fs::write(&report,format!("{}<section><h3>menu <small>tick 12</small></h3><a href=\"menu.new.png\"></a><a href=\"menu.hw1.png\"></a><a href=\"menu.hw3.png\"></a></section>",hash(&bin))).unwrap();
        Record {
            game: "sample".into(),
            title: "Sample".into(),
            checkpoint: "menu".into(),
            tick: 12,
            frame: None,
            bin_sha256: hash(&bin),
            bin,
            report_sha256: hash(&report),
            report,
            cpu_sha256: hash(&cpu),
            cpu,
            hw1_sha256: hash(&hw1),
            hw1,
            hw3_sha256: hash(&hw3),
            hw3,
            strict: vec![Rect {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
            }],
            receipt: None,
            notes: None,
        }
    }
    fn invoke(dir: &Path, records: Vec<Record>) -> Result<(), String> {
        let manifest = dir.join("capture.toml");
        std::fs::write(
            &manifest,
            toml::to_string(&Manifest { capture: records }).unwrap(),
        )
        .unwrap();
        run(&manifest, &dir.join("output"))
    }
    #[test]
    fn refresh_binds_hashes_ticks_checkpoint_paths_and_dimensions() {
        let cases = [
            "hash",
            "tick",
            "wrong_checkpoint_path",
            "unlinked_capture",
            "dimensions",
            "duplicate",
        ];
        for case in cases {
            let dir = tempfile::tempdir().unwrap();
            let mut r = fixture(dir.path());
            let mut duplicate = None;
            match case {
                "hash" => r.cpu_sha256 = "0".repeat(64),
                "tick" => r.tick = 13,
                "wrong_checkpoint_path" => {
                    let wrong = dir.path().join("different.new.png");
                    std::fs::copy(&r.cpu, &wrong).unwrap();
                    r.cpu = wrong;
                }
                "unlinked_capture" => {
                    let sub = dir.path().join("unlinked");
                    std::fs::create_dir(&sub).unwrap();
                    let wrong = sub.join("menu.new.png");
                    std::fs::copy(&r.cpu, &wrong).unwrap();
                    r.cpu = wrong;
                }
                "dimensions" => {
                    Img::from_rgba(1, 1, vec![10; 4]).save_png(&r.hw3).unwrap();
                    r.hw3_sha256 = format!("{:x}", Sha256::digest(std::fs::read(&r.hw3).unwrap()));
                }
                "duplicate" => duplicate = Some(fixture(dir.path())),
                _ => unreachable!(),
            }
            let mut records = vec![r];
            if let Some(second) = duplicate {
                records.push(second);
            }
            assert!(invoke(dir.path(), records).is_err(), "{case} accepted");
            assert!(
                !dir.path().join("output").exists(),
                "{case} wrote output before validation"
            );
        }
    }
    #[test]
    fn refresh_retains_exact_counts_and_paired_strict_crops() {
        let dir = tempfile::tempdir().unwrap();
        let r = fixture(dir.path());
        invoke(dir.path(), vec![r]).unwrap();
        let root = dir.path().join("output/sample");
        let coverage = std::fs::read_to_string(root.join("menu.crop-coverage.tsv")).unwrap();
        assert!(coverage.contains("declared-text-hud\t0\t0\t3\t3\t0"));
        assert!(coverage.contains("covered=0 omitted=0 exact=0"));
        assert!(root.join("menu.crop0001.pair.png").exists());
        assert!(!root.join("menu.cpu3.png").exists());
        let html = std::fs::read_to_string(dir.path().join("output/index.html")).unwrap();
        assert!(!html.contains(">PASS</span>"));
        assert!(html.contains("execution timings not recorded"));
        assert!(!html.contains("0.0 s wall"));
    }
}
