//! The HTML contact sheet: golden | new | diff heat-map per checkpoint, the
//! hardware renderer frames beside them, and pass/fail per check.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::exec::{Group, JourneyResult, Status};
use crate::img::Img;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
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

/// Write PNGs for every capture into `dir` and return the file stem prefix
/// used in the page.
fn write_images(dir: &Path, group: &Group) -> Result<Vec<(String, String, String)>, String> {
    // (caption, relative file, css class)
    let Some(cap) = &group.capture else { return Ok(Vec::new()) };
    let mut shots = Vec::new();
    let stem = &group.name;
    let mut put = |caption: String, file: String, img: &Img, class: &str| -> Result<(), String> {
        img.save_png(&dir.join(&file))?;
        shots.push((caption, file, class.to_string()));
        Ok(())
    };
    if let Some(g) = &cap.golden {
        put("golden".into(), format!("{stem}.golden.png"), g, "")?;
    }
    put("new (cpu 1x)".into(), format!("{stem}.new.png"), &cap.cpu, "")?;
    if let Some(g) = &cap.golden {
        if (g.w, g.h) == (cap.cpu.w, cap.cpu.h) {
            put("golden vs new".into(), format!("{stem}.diff.png"), &cap.cpu.heatmap(g, 0, 0), "heat")?;
        }
    }
    for (scale, frame) in &cap.hw {
        put(format!("hw {scale}x"), format!("{stem}.hw{scale}.png"), frame, "")?;
        let base = if *scale == 1 { cap.cpu.clone() } else { cap.cpu.enlarge(*scale) };
        if (base.w, base.h) == (frame.w, frame.h) {
            put(
                format!("hw {scale}x vs cpu"),
                format!("{stem}.hw{scale}.diff.png"),
                &frame.heatmap(&base, if *scale == 1 { cap.hw1_channel } else { cap.hwn_channel }, if *scale == 1 { 0 } else { *scale }),
                "heat",
            )?;
        }
    }
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
        badge(worst),
        group.tick,
        group.label.as_deref().map(|l| format!(" &middot; {}", esc(l))).unwrap_or_default()
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
figure{margin:0}figure img{display:block;width:320px;height:auto;image-rendering:pixelated;border:1px solid var(--line);background:#000}
figcaption{font-size:12px;color:var(--mut);margin-top:3px}
.detail{color:var(--mut);font-family:ui-monospace,Menlo,monospace;font-size:12px}
.note{color:var(--mut);font-size:13px}
a{color:inherit}
"#;

/// Write the report under `out`. Returns the index page path.
pub fn write_report(out: &Path, results: &[JourneyResult], title: &str) -> Result<PathBuf, String> {
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
        "<p class=\"note\">{} journeys, {} failed. Goldens are the CPU rasterizer at 1x; hardware frames are checked against it.</p>",
        results.len(),
        failed
    );
    html.push_str("<table class=\"summary\"><tr><th>journey</th><th></th><th>checks</th><th>fail</th><th>ticks</th><th>wall</th><th>disc</th></tr>");
    for r in results {
        let _ = write!(
            html,
            "<tr><td><a href=\"#{n}\">{n}</a></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{:.1} s</td><td>{}</td></tr>",
            badge(if r.passed() { Status::Pass } else { Status::Fail }),
            r.checks().count(),
            r.failures(),
            r.ticks,
            r.wall.as_secs_f64(),
            esc(&r.disc_id),
            n = esc(&r.name)
        );
    }
    html.push_str("</table>");
    for r in results {
        let _ = write!(
            html,
            "<h2 id=\"{n}\">{t} {}</h2><p class=\"note\">{d}<br>{} ticks ({:.0} s emulated), {:.1} s wall{}</p>",
            badge(if r.passed() { Status::Pass } else { Status::Fail }),
            r.ticks,
            r.emu_secs,
            r.wall.as_secs_f64(),
            r.hw_adapter.as_deref().map(|a| format!(", hardware renderer on {}", esc(a))).unwrap_or_default(),
            n = esc(&r.name),
            t = esc(&r.title),
            d = esc(&format!("{} (sha256 {})", r.disc.display(), r.disc_id)),
        );
        if let Some(a) = &r.abort {
            let _ = write!(html, "<p><span class=\"badge fail\">ABORTED</span> {}</p>", esc(a));
        }
        for n in &r.notes {
            let _ = write!(html, "<p class=\"note\">{}</p>", esc(n));
        }
        let dir = out.join(&r.name);
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
