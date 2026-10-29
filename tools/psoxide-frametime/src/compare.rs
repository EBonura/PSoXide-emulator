//! Two runs of the same route (before and after a change) on one page.
//!
//! Loads shift everything in time, so the runs are lined up by journey step: each step of the
//! second run is stretched or squeezed onto the same stretch of x axis the first run gave it.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use serde_json::json;

use crate::config::Config;
use crate::log::{Frame, Run};
use crate::page::{bucket_s, buckets, esc, load_spans, round1, scene_spans, shell, y_max};
use crate::stats::{summarise, Summary};

/// Where each journey step sits on the time axis: step to (start, end) in seconds.
fn spans(run: &Run) -> HashMap<u64, (f64, f64)> {
    let mut out = HashMap::new();
    for (i, (step, start)) in run.step_starts.iter().enumerate() {
        let end = run.step_starts.get(i + 1).map_or(run.total_s, |n| n.1);
        out.insert(*step, (*start, end));
    }
    out
}

/// Frames as `(x, ms)` with `x` on the first run's axis; frames of steps only one run has are
/// dropped, and so is the loading.
fn aligned(
    run: &Run,
    mine: &HashMap<u64, (f64, f64)>,
    axis: &HashMap<u64, (f64, f64)>,
) -> Vec<(f64, f64)> {
    run.play_frames()
        .filter_map(|f| {
            let (m0, m1) = *mine.get(&f.step)?;
            let (a0, a1) = *axis.get(&f.step)?;
            let frac = if m1 > m0 {
                ((f.t_s - m0) / (m1 - m0)).clamp(0.0, 1.0)
            } else {
                0.0
            };
            Some((a0 + frac * (a1 - a0), f.ms()))
        })
        .collect()
}

fn scene_summary(run: &Run, scene: &str) -> Option<Summary> {
    let frames: Vec<&Frame> = run.play_frames().filter(|f| f.scene == scene).collect();
    let loads: Vec<_> = run.loads.iter().filter(|l| l.to == scene).collect();
    summarise(
        scene,
        &frames,
        loads.len(),
        loads.iter().map(|l| l.seconds).sum(),
    )
}

/// Colour class for a change where `higher_is_better` says which way is good.
fn delta_class(delta: f64, flat: f64, higher_is_better: bool) -> &'static str {
    if delta.abs() <= flat {
        "d-flat"
    } else if (delta > 0.0) == higher_is_better {
        "d-good"
    } else {
        "d-bad"
    }
}

fn cell3(
    before: Option<f64>,
    after: Option<f64>,
    digits: usize,
    flat: f64,
    higher_is_better: bool,
    suffix: &str,
) -> String {
    match (before, after) {
        (Some(b), Some(a)) => {
            let d = a - b;
            format!(
                "<td class=\"sep\">{b:.digits$}{suffix}</td><td>{a:.digits$}{suffix}</td><td class=\"{}\">{d:+.digits$}</td>",
                delta_class(d, flat, higher_is_better)
            )
        }
        _ => "<td class=\"sep\">n/a</td><td>n/a</td><td></td>".to_string(),
    }
}

/// Write `out/data/compare.csv` and `out/index.html`.
pub(crate) fn write(
    before: &Run,
    after: &Run,
    cfg: &Config,
    names: (&str, &str),
    out: &Path,
) -> Result<String, String> {
    let data_dir = out.join("data");
    fs::create_dir_all(&data_dir).map_err(|e| format!("create {}: {e}", data_dir.display()))?;
    let (sa, sb) = (spans(before), spans(after));
    let common = sa.keys().filter(|k| sb.contains_key(k)).count();
    if common == 0 {
        return Err("the two logs share no journey steps: they are not the same route".into());
    }
    let mut warnings = Vec::new();
    if sa.len() != sb.len() {
        warnings.push(format!(
            "the runs have {} and {} journey steps; only the {common} steps both have are lined up",
            sa.len(),
            sb.len()
        ));
    }
    let mut scenes = before.scenes.clone();
    for s in &after.scenes {
        if !scenes.contains(s) {
            scenes.push(s.clone());
        }
    }
    let mut rows: Vec<(String, Option<Summary>, Option<Summary>)> = scenes
        .iter()
        .map(|s| (s.clone(), scene_summary(before, s), scene_summary(after, s)))
        .collect();
    let route = |r: &Run| {
        let f: Vec<&Frame> = r.play_frames().collect();
        summarise(
            "Whole route",
            &f,
            r.loads.len(),
            r.loads.iter().map(|l| l.seconds).sum(),
        )
    };
    let (ra, rb) = (route(before), route(after));
    rows.push(("Whole route".into(), ra.clone(), rb.clone()));

    let mut csv = String::from("scene,frames_before,frames_after,mean_fps_before,mean_fps_after,p1_low_before,p1_low_after,over2_before,over2_after,over3_before,over3_after,seconds_before,seconds_after,load_s_before,load_s_after\n");
    for (name, a, b) in &rows {
        let g = |s: &Option<Summary>, f: fn(&Summary) -> f64| {
            s.as_ref().map_or(String::new(), |x| format!("{:.2}", f(x)))
        };
        let _ = writeln!(
            csv,
            "{name},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            a.as_ref().map_or(0, |x| x.frames),
            b.as_ref().map_or(0, |x| x.frames),
            g(a, |x| x.mean_fps),
            g(b, |x| x.mean_fps),
            g(a, |x| x.p1_low_fps),
            g(b, |x| x.p1_low_fps),
            g(a, |x| x.pct_over_2),
            g(b, |x| x.pct_over_2),
            g(a, |x| x.pct_over_3),
            g(b, |x| x.pct_over_3),
            g(a, |x| x.seconds),
            g(b, |x| x.seconds),
            g(a, |x| x.load_seconds),
            g(b, |x| x.load_seconds),
        );
    }
    fs::write(data_dir.join("compare.csv"), csv).map_err(|e| format!("write compare.csv: {e}"))?;

    let mut table =
        String::from("<div class=\"tablewrap\"><table><thead><tr><th rowspan=\"2\">Scene</th>");
    for h in [
        "Mean fps",
        "1% low fps",
        "Over 2 vblanks (%)",
        "Over 3 vblanks (%)",
        "Play time (s)",
    ] {
        let _ = write!(
            table,
            "<th colspan=\"3\" class=\"sep\" style=\"text-align:center\">{h}</th>"
        );
    }
    table.push_str("</tr><tr>");
    for _ in 0..5 {
        let _ = write!(
            table,
            "<th class=\"sep\">{}</th><th>{}</th><th>change</th>",
            esc(names.0),
            esc(names.1)
        );
    }
    table.push_str("</tr></thead><tbody>\n");
    for (name, a, b) in &rows {
        let all = name == "Whole route";
        let pick = |f: fn(&Summary) -> f64| (a.as_ref().map(f), b.as_ref().map(f));
        let (m, l, o2, o3, t) = (
            pick(|x| x.mean_fps),
            pick(|x| x.p1_low_fps),
            pick(|x| x.pct_over_2),
            pick(|x| x.pct_over_3),
            pick(|x| x.seconds),
        );
        let _ = writeln!(
            table,
            "<tr{}><td>{}</td>{}{}{}{}{}</tr>",
            if all { " class=\"all\"" } else { "" },
            esc(name),
            cell3(m.0, m.1, 2, 0.05, true, ""),
            cell3(l.0, l.1, 2, 0.05, true, ""),
            cell3(o2.0, o2.1, 1, 0.5, false, ""),
            cell3(o3.0, o3.1, 1, 0.5, false, ""),
            cell3(t.0, t.1, 1, 0.5, false, ""),
        );
    }
    table.push_str("</tbody></table></div>");

    let tile = |label: &str,
                a: Option<f64>,
                b: Option<f64>,
                unit: &str,
                digits: usize,
                hib: bool| match (a, b) {
        (Some(a), Some(b)) => {
            let rel = if a != 0.0 { (b - a) / a * 100.0 } else { 0.0 };
            format!(
                "<div class=\"tile\"><div class=\"v\" style=\"font-size:21px\">{a:.digits$} to {b:.digits$}{unit}</div><div class=\"k\">{label} <span style=\"font-weight:600;color:var({})\">({rel:+.1}%)</span></div></div>",
                match delta_class(rel, 0.5, hib) {
                    "d-good" => "--good",
                    "d-bad" => "--bad",
                    _ => "--muted",
                }
            )
        }
        _ => String::new(),
    };
    // Tile text colours use the same tokens as the table.
    let tiles = format!(
        "{}{}{}{}",
        tile(
            "mean fps",
            ra.as_ref().map(|s| s.mean_fps),
            rb.as_ref().map(|s| s.mean_fps),
            "",
            2,
            true
        ),
        tile(
            "1% low fps",
            ra.as_ref().map(|s| s.p1_low_fps),
            rb.as_ref().map(|s| s.p1_low_fps),
            "",
            2,
            true
        ),
        tile(
            "frames slower than 30 fps",
            ra.as_ref().map(|s| s.pct_over_2),
            rb.as_ref().map(|s| s.pct_over_2),
            "%",
            1,
            false
        ),
        tile(
            "frames slower than 20 fps",
            ra.as_ref().map(|s| s.pct_over_3),
            rb.as_ref().map(|s| s.pct_over_3),
            "%",
            1,
            false
        ),
    );

    let total = before.total_s;
    let bucket = bucket_s(total);
    let pa = aligned(before, &sa, &sa);
    let pb = aligned(after, &sb, &sa);
    let data = json!({
        "total_s": round1(total), "ymax": y_max(&[before, after]), "vb": before.vblank_ms, "bucket": bucket,
        "band": false, "aligned": true,
        "series": [
            {"name": names.0, "color": "--before", "width": 1.6, "points": buckets(&pa, total, bucket)},
            {"name": names.1, "color": "--line", "width": 1.3, "points": buckets(&pb, total, bucket)},
        ],
        "loads": load_spans(before), "maps": scene_spans(before), "marks": [],
    });
    let title = format!("{} Frame Time", cfg.title);
    let mut notes = String::new();
    for w in &warnings {
        let _ = write!(notes, "<p class=\"caption note-flag\">{}</p>", esc(w));
    }
    let body = format!(
        "<h1>{} ({} against {})</h1>\n<p class=\"lede\">The same route run twice on the emulator: {} ({} play frames, {} loads) and {} ({} play frames, {} loads). Changes are {} minus {}.</p>\n{notes}<div class=\"tiles\">{tiles}</div>\n\
<h2>Frame time along the route</h2>\n<div class=\"card\">\n<svg id=\"chart\" role=\"img\" aria-label=\"Mean frame time in milliseconds along the route for both runs, lined up by journey step.\"></svg>\n<div id=\"tip\"></div>\n<div class=\"legend\">\
<span><i class=\"sw\" style=\"border-color:var(--before)\"></i>{} (mean, {bucket:.0} s buckets)</span>\
<span><i class=\"sw\" style=\"border-color:var(--line)\"></i>{}</span>\
<span><i class=\"sw dash\" style=\"border-color:var(--ref30)\"></i>30 fps (2 vblanks)</span>\
<span><i class=\"sw dash\" style=\"border-color:var(--ref20)\"></i>20 fps (3 vblanks)</span>\
<span><i class=\"sw load\"></i>loading pause of the {} run</span></div>\n</div>\n\
<p class=\"caption\">Lower is better. The x axis is route position: every journey step of the {} run is stretched or squeezed onto the stretch of axis the {} run gave it, so a scene sits at the same place in both lines even when loads shifted the clock. Loading frames are left out of both lines.</p>\n\
<h2>Per scene</h2>\n{table}\n\
<p class=\"caption\">Change is {} minus {}. Green is better, red is worse, grey is within noise (0.05 fps, 0.5 points). Mean fps is frames divided by time spent on them. 1% low is 1000 divided by the 99th percentile frame time. Loading frames are not in any column.</p>\n\
<h2>How this was measured</h2>\n<div class=\"method\"><p>Both logs come from <code>psoxide-gate run --tick-log</code> on the same journey; a frame is a tick in which the display buffer flipped and its time is the cycles since the previous flip. Scenes come from the game's own scene word and loads follow the rules of the single-route page. The runs are deterministic, so a difference here is a difference in the build or the emulator, not run-to-run noise.</p></div>",
        esc(&title), esc(names.0), esc(names.1),
        esc(names.0), ra.as_ref().map_or(0, |s| s.frames), before.loads.len(),
        esc(names.1), rb.as_ref().map_or(0, |s| s.frames), after.loads.len(),
        esc(names.1), esc(names.0),
        esc(names.0), esc(names.1), esc(names.0),
        esc(names.1), esc(names.0),
        esc(names.1), esc(names.0),
    );
    let html = shell(
        &format!("{title} Compare"),
        &format!("{} frame time, {} against {}.", cfg.title, names.0, names.1),
        &body,
        &data,
    );
    fs::write(out.join("index.html"), html).map_err(|e| format!("write index.html: {e}"))?;
    Ok(rows
        .iter()
        .find(|r| r.0 == "Whole route")
        .map(|(_, a, b)| match (a, b) {
            (Some(a), Some(b)) => format!(
                "whole route: mean {:.2} -> {:.2} fps, 1% low {:.2} -> {:.2}, over 3 vblanks {:.1}% -> {:.1}%",
                a.mean_fps, b.mean_fps, a.p1_low_fps, b.p1_low_fps, a.pct_over_3, b.pct_over_3
            ),
            _ => "whole route: no frames".into(),
        })
        .unwrap_or_default())
}
