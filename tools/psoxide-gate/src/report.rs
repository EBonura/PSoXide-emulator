//! CPU1 reference, HW1 and HW3 contact evidence with exact diffs and crop coverage.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::exec::{Group, JourneyResult, Status};
use crate::img::Img;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn badge(s: Status) -> String {
    let class = match s {
        Status::Pass => "pass",
        Status::Fail => "fail",
        Status::XFail => "xfail",
        Status::XPass => "xpass",
        Status::Skip => "skip",
    };
    format!("<span class=\"badge {class}\">{}</span>", s.label())
}

/// Preserve raw captures and add exact matrix and exhaustive paired crops.
fn write_images(dir: &Path, group: &Group) -> Result<Vec<(String, String, String)>, String> {
    use crate::contact::{crop, differing_regions, exact_overlay, label_lines, matrix, paired};
    use crate::img::Rect;
    let Some(cap) = &group.capture else {
        return Ok(Vec::new());
    };
    let mut shots = Vec::new();
    let stem = &group.name;
    let mut put = |caption: String, file: String, img: &Img, class: &str| -> Result<(), String> {
        img.save_png(&dir.join(&file))?;
        shots.push((caption, file, class.to_string()));
        Ok(())
    };
    put(
        "CPU1 reference | HW1 | HW3 at native sizes; exact diffs below".into(),
        format!("{stem}.matrix.png"),
        &matrix(cap, stem, group.tick),
        "matrix",
    )?;
    put(
        "raw CPU1 reference".into(),
        format!("{stem}.new.png"),
        &cap.cpu,
        "raw",
    )?;
    if let Some(g) = &cap.golden {
        put(
            "raw historical golden".into(),
            format!("{stem}.golden.png"),
            g,
            "raw",
        )?;
        if (g.w, g.h) == (cap.cpu.w, cap.cpu.h) {
            put(
                "CPU1 vs historical golden: exact difference".into(),
                format!("{stem}.diff.png"),
                &exact_overlay(&cap.cpu, g),
                "raw",
            )?;
        }
    }
    let mut inventory = String::from("mode\texact_differing_pixels\tcompared_pixels\tstatus\n");
    let mut coverage = String::from("crop\tpurpose\tx\ty\tw\th\texact_differing_pixels\n");
    for (scale, frame) in &cap.hw {
        put(
            format!("raw HW{scale}"),
            format!("{stem}.hw{scale}.png"),
            frame,
            "raw",
        )?;
        let reference = cap.cpu.enlarge(*scale);
        if (frame.w, frame.h) != (reference.w, reference.h) {
            let _ = writeln!(
                inventory,
                "HW{scale}\tunknown\tunknown\tsize mismatch {}x{} vs {}x{}",
                frame.w, frame.h, reference.w, reference.h
            );
            continue;
        }
        let stats = frame.diff(&reference, 0, 1, &[], None);
        let _ = writeln!(
            inventory,
            "HW{scale}\t{}\t{}\t{}",
            stats.differing,
            stats.compared,
            if stats.differing == 0 {
                "identical"
            } else {
                "nonzero"
            }
        );
        put(
            format!(
                "HW{scale} vs CPU1: {} exact differing pixels",
                stats.differing
            ),
            format!("{stem}.hw{scale}.diff.png"),
            &exact_overlay(frame, &reference),
            "raw",
        )?;
        if *scale != 3 {
            continue;
        }
        // Full-frame pair makes undeclared text/HUD coverage visible, never implicit.
        // Journey strict regions provide the focused text/HUD pairs as well.
        let full = Rect {
            x: 0,
            y: 0,
            w: frame.w,
            h: frame.h,
        };
        let mut crops = vec![("text-hud-full-frame", full)];
        for r in &cap.strict {
            crops.push((
                "declared-text-hud",
                Rect {
                    x: r.x * 3,
                    y: r.y * 3,
                    w: r.w * 3,
                    h: r.h * 3,
                },
            ));
        }
        let differences = differing_regions(frame, &reference);
        crops.extend(differences.iter().map(|r| ("difference-tile", *r)));
        let mut covered = 0;
        for (i, (purpose, r)) in crops.iter().enumerate() {
            let hw = crop(frame, *r)?;
            let cpu = crop(&reference, *r)?;
            let count = hw.diff(&cpu, 0, 1, &[], None).differing;
            if *purpose == "difference-tile" {
                covered += count;
            }
            let id = format!("{stem}.crop{i:04}");
            let _ = writeln!(
                coverage,
                "{id}\t{purpose}\t{}\t{}\t{}\t{}\t{count}",
                r.x, r.y, r.w, r.h
            );
            let labels = |mode| {
                let mut labels = label_lines(mode, 3, stem, group.tick, cap.frame);
                labels.push(format!("CROP {i} AT {},{} {}X{} 1:1", r.x, r.y, r.w, r.h));
                labels
            };
            put(format!("crop {i} {purpose}: HW3 | CPU1 reference; ({},{} {}x{}); {count} exact differing pixels", r.x, r.y, r.w, r.h),
                format!("{id}.pair.png"), &paired(&hw, &labels("HW3"), &cpu, &labels("CPU1 REFERENCE"), 1), "crop")?;
        }
        if covered != stats.differing {
            return Err("difference crop coverage mismatch".into());
        }
        let _ = writeln!(
            coverage,
            "TOTAL\tdifference-tiles={} covered={} omitted=0 exact={}",
            differences.len(),
            covered,
            stats.differing
        );
    }
    std::fs::write(dir.join(format!("{stem}.exact-diffs.tsv")), inventory)
        .map_err(|e| e.to_string())?;
    std::fs::write(dir.join(format!("{stem}.crop-coverage.tsv")), coverage)
        .map_err(|e| e.to_string())?;
    Ok(shots)
}

fn group_html(out: &mut String, dir_rel: &str, group: &Group, shots: &[(String, String, String)]) {
    let worst = group
        .checks
        .iter()
        .map(|c| c.status)
        .fold(Status::Pass, |a, b| match (a, b) {
            (Status::Fail, _) | (_, Status::Fail) => Status::Fail,
            (Status::XPass, _) | (_, Status::XPass) => Status::XPass,
            (Status::XFail, _) | (_, Status::XFail) => Status::XFail,
            _ => Status::Pass,
        });
    let _ = write!(
        out,
        "<section class=\"cp\"><h3>{} {} <small>tick {}{}</small></h3>",
        esc(&group.name),
        badge(if group.checks.is_empty() {
            Status::Skip
        } else {
            worst
        }),
        group.tick,
        group
            .label
            .as_deref()
            .map(|l| format!(" &middot; {}", esc(l)))
            .unwrap_or_default()
    );
    if !shots.is_empty() {
        out.push_str("<div class=\"shots\">");
        for (caption, file, class) in shots {
            let _ = write!(
                out,
                "<figure class=\"{class}\"><a href=\"{dir_rel}/{f}\"><img loading=\"lazy\" src=\"{dir_rel}/{f}\"></a><figcaption>{c}</figcaption></figure>",
                f = esc(file),
                c = esc(caption)
            );
        }
        out.push_str("</div>");
    }
    if group.capture.is_some() {
        let _ = write!(out, "<p class=\"note\"><a href=\"{dir_rel}/{}.exact-diffs.tsv\">Exact pixel inventory</a> | <a href=\"{dir_rel}/{}.crop-coverage.tsv\">Crop coverage (all differing tiles, omitted=0)</a>. Magenta means any nonzero channel difference; unchanged reference pixels are dimmed. Acceptance checks below use their stated tolerances independently. Crops show source pixels at 1:1.</p>", esc(&group.name), esc(&group.name));
    }
    if group.capture.as_ref().is_some_and(|c| c.strict.is_empty()) {
        out.push_str("<p class=\"note\">MISSING FOCUSED TEXT/HUD REGIONS: no journey rectangles supplied. The full-frame 1:1 pair preserves all pixels; focused text/HUD review remains incomplete.</p>");
    }
    out.push_str("<table class=\"checks\">");

    for c in &group.checks {
        let _ = write!(
            out,
            "<tr><td>{}</td><td>{}</td><td class=\"detail\">{}</td></tr>",
            badge(c.status),
            esc(&c.name),
            esc(&c.detail)
        );
    }
    out.push_str("</table></section>");
}

const CSS: &str = r#"
:root{--bg:#111418;--fg:#d8dde3;--mut:#8a94a0;--card:#1a1f26;--line:#2a313a;--pass:#3fb950;--fail:#f85149;--xf:#d29922;--skip:#6e7681}
@media (prefers-color-scheme: light){:root{--bg:#f6f8fa;--fg:#1f2328;--mut:#59636e;--card:#fff;--line:#d1d9e0}}
body{margin:0;padding:24px;background:var(--bg);color:var(--fg);font:14px/1.45 -apple-system,system-ui,sans-serif}
h1{margin:0 0 4px;font-size:22px}h2{margin:32px 0 6px;font-size:18px}h3{margin:0 0 8px;font-size:15px}small{color:var(--mut);font-weight:400}
table{border-collapse:collapse}td,th{padding:3px 10px 3px 0;text-align:left;vertical-align:top}
.summary td,.summary th{border-bottom:1px solid var(--line);padding:5px 14px 5px 0}
.badge{display:inline-block;min-width:42px;text-align:center;padding:1px 7px;border-radius:9px;font-size:11px;font-weight:700;color:#fff}
.pass{background:var(--pass)}.fail{background:var(--fail)}.xfail,.xpass{background:var(--xf)}.skip{background:var(--skip)}
.cp{background:var(--card);border:1px solid var(--line);border-radius:8px;padding:12px 14px;margin:12px 0}
.shots{display:flex;flex-wrap:wrap;gap:12px;margin-bottom:10px}
figure{margin:0}figure img{display:block;max-width:100%;height:auto;image-rendering:pixelated;border:1px solid var(--line);background:#000}.raw img{width:320px}.matrix{width:100%;overflow:auto}.matrix img,.crop img{max-width:none}.crop{overflow:auto}
figcaption{font-size:12px;color:var(--mut);margin-top:3px}
.detail{color:var(--mut);font-family:ui-monospace,Menlo,monospace;font-size:12px}
.note{color:var(--mut);font-size:13px}
a{color:inherit}
"#;

/// Write the report under `out`. Returns the index page path.
pub fn write_report(
    out: &Path,
    results: &[JourneyResult],
    missing: &[(String, String)],
    title: &str,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(out).map_err(|e| format!("mkdir {}: {e}", out.display()))?;
    let mut html = String::new();
    let _ = write!(
        html,
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{}</title><style>{CSS}</style></head><body><h1>{}</h1>",
        esc(title),
        esc(title)
    );
    let failed = results.iter().filter(|r| !r.passed()).count();
    let _ = write!(
        html,
        "<p class=\"note\">{} journeys, {} failed, {} missing. Goldens are the CPU rasterizer at 1x; hardware frames are checked against it.</p>",
        results.len(),
        failed,
        missing.len()
    );
    html.push_str("<table class=\"summary\"><tr><th>journey</th><th></th><th>checks</th><th>fail</th><th>ticks</th><th>wall</th><th>disc</th></tr>");
    for r in results {
        let _ = write!(
            html,
            "<tr><td><a href=\"#{n}\">{n}</a></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            badge(if r.checks().count() == 0 { Status::Skip } else if r.passed() { Status::Pass } else { Status::Fail }),
            r.checks().count(),
            r.failures(),
            r.ticks,
            if r.checks().count() == 0 { "unrecorded".into() } else { format!("{:.1} s", r.wall.as_secs_f64()) },
            esc(&r.disc_id),
            n = esc(&r.name)
        );
    }
    for (name, why) in missing {
        let _ = write!(html, "<tr><td>{}</td><td><span class=\"badge fail\">MISSING</span></td><td colspan=\"5\">{}</td></tr>", esc(name), esc(why));
    }
    html.push_str("</table>");
    for r in results {
        let timing = if r.checks().count() == 0 {
            "Capture refresh: execution timings not recorded here; see the original receipt.".into()
        } else {
            format!(
                "{} ticks ({:.0} s emulated), {:.1} s wall",
                r.ticks,
                r.emu_secs,
                r.wall.as_secs_f64()
            )
        };
        let _ = write!(
            html,
            "<h2 id=\"{n}\">{t} {}</h2><p class=\"note\">{d}<br>{timing}{}</p>",
            badge(if r.checks().count() == 0 {
                Status::Skip
            } else if r.passed() {
                Status::Pass
            } else {
                Status::Fail
            }),
            r.hw_adapter
                .as_deref()
                .map(|a| format!(", hardware renderer on {}", esc(a)))
                .unwrap_or_default(),
            n = esc(&r.name),
            t = esc(&r.title),
            d = esc(&format!(
                "{} (sha256 prefix {})",
                r.disc.display(),
                r.disc_id
            )),
        );
        if let Some(a) = &r.abort {
            let _ = write!(
                html,
                "<p><span class=\"badge fail\">ABORTED</span> {}</p>",
                esc(a)
            );
        }
        for n in &r.notes {
            let _ = write!(html, "<p class=\"note\">{}</p>", esc(n));
        }
        let dir = out.join(&r.name);
        for card in &r.cards {
            let card_dir = dir.join("cards");
            std::fs::create_dir_all(&card_dir).map_err(|e| e.to_string())?;
            let persisted = std::fs::read(card_dir.join(&card.name)).map_err(|e| e.to_string())?;
            if persisted != card.bytes {
                return Err("card receipt changed since capture".into());
            }
            let _ = write!(
                html,
                "<p class=\"note\"><a href=\"{}/cards/{}\">{}</a>: {}</p>",
                esc(&r.name),
                esc(&card.name),
                esc(&card.name),
                esc(&card.detail)
            );
        }
        for g in &r.groups {
            let shots = write_images(&dir, g)?;
            group_html(&mut html, &r.name, g, &shots);
        }
    }
    html.push_str("</body></html>");
    let index = out.join("index.html");
    std::fs::write(&index, html).map_err(|e| format!("write {}: {e}", index.display()))?;
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Capture, Group};
    use std::time::Duration;

    #[test]
    fn contact_sheet_has_three_modes_and_missing_routes() {
        let dir = tempfile::tempdir().unwrap();
        let result = JourneyResult {
            name: "sample".into(),
            title: "Sample".into(),
            disc: PathBuf::from("sample.cue"),
            disc_id: "ba7816bf8f01".into(),
            groups: vec![Group {
                name: "menu".into(),
                label: None,
                tick: 1,
                capture: Some(Capture {
                    frame: None,
                    strict: Vec::new(),
                    cpu: Img::from_rgba(1, 1, vec![10, 20, 30, 255]),
                    golden: None,
                    golden_error: None,
                    hw: Vec::new(),
                    hw_skip: None,
                }),
                checks: Vec::new(),
                wants_golden: false,
            }],
            abort: None,
            ticks: 1,
            emu_secs: 1.0 / 60.0,
            wall: Duration::ZERO,
            hw_adapter: None,
            notes: Vec::new(),
            cards: Vec::new(),
        };
        let index = write_report(
            dir.path(),
            &[result],
            &[("absent".into(), "no journey".into())],
            "Gate",
        )
        .unwrap();
        let html = std::fs::read_to_string(index).unwrap();
        assert!(html.contains("CPU1 reference | HW1 | HW3"));
        assert!(html.contains("MISSING"));
        assert!(!dir.path().join("sample/menu.cpu3.png").exists());
        assert!(dir.path().join("sample/menu.matrix.png").exists());
    }
}
