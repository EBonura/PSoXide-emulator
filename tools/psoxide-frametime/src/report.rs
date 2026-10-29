//! One route: the CSVs and the page.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use serde_json::json;

use crate::config::Config;
use crate::log::{Frame, Run};
use crate::page::{buckets, clock, esc, load_spans, round1, scene_spans, shell};
use crate::stats::{Fit, Summary, Window};

/// Write `out/data/*.csv` and `out/index.html`.
pub(crate) fn write(
    run: &Run,
    cfg: &Config,
    notes: &[String],
    out: &Path,
) -> Result<Vec<Summary>, String> {
    let data = out.join("data");
    fs::create_dir_all(&data).map_err(|e| format!("create {}: {e}", data.display()))?;
    let summaries = run.summaries();
    let fit = Fit::of(run);
    let windows = run.windows(fit.as_ref());
    let put = |name: &str, text: String| {
        fs::write(data.join(name), text).map_err(|e| format!("write {name}: {e}"))
    };
    put("frametime.csv", frametime_csv(run))?;
    put("summary.csv", summary_csv(&summaries))?;
    put("loads.csv", loads_csv(run))?;
    put("windows.csv", windows_csv(run, &windows))?;
    put("worst_frames.csv", worst_csv(run))?;
    put("steps.csv", steps_csv(run))?;
    put("scene_counters.csv", counters_csv(run))?;
    let html = page(run, cfg, &summaries, &windows, fit.as_ref(), notes);
    fs::write(out.join("index.html"), html).map_err(|e| format!("write index.html: {e}"))?;
    Ok(summaries)
}

fn frametime_csv(run: &Run) -> String {
    let mut s = String::from(
        "frame,tick,t_s,scene,step,step_label,kind,vblanks,frame_ms,fps,cycles,instructions",
    );
    for l in run.counter_labels.iter().chain(&run.pos_labels) {
        let _ = write!(s, ",{}", l.replace(',', " "));
    }
    s.push('\n');
    for (n, f) in run.frames.iter().enumerate() {
        let ms = f.ms();
        let label = run
            .labels
            .get(&f.step)
            .map(|l| l.replace('"', "'"))
            .unwrap_or_default();
        let _ = write!(
            s,
            "{n},{},{:.3},{},{},\"{label}\",{},{},{ms:.2},{:.2},{},{}",
            f.tick,
            f.t_s,
            f.scene,
            f.step,
            if f.load { "load" } else { "play" },
            f.vblanks,
            1000.0 / ms,
            f.cycles,
            f.instrs
        );
        for v in f.counters.iter().chain(&f.pos) {
            let _ = write!(s, ",{v}");
        }
        s.push('\n');
    }
    s
}

fn summary_csv(rows: &[Summary]) -> String {
    let mut s = String::from("scene,frames,seconds,mean_fps,p1_low_fps,median_ms,worst_ms,worst_t_s,pct_over_2_vblanks,pct_over_3_vblanks,loads,load_seconds\n");
    for r in rows {
        let _ = writeln!(
            s,
            "{},{},{:.1},{:.2},{:.2},{:.1},{:.1},{:.1},{:.1},{:.1},{},{:.1}",
            r.name,
            r.frames,
            r.seconds,
            r.mean_fps,
            r.p1_low_fps,
            r.median_ms,
            r.worst_ms,
            r.worst_t_s,
            r.pct_over_2,
            r.pct_over_3,
            r.loads,
            r.load_seconds
        );
    }
    s
}

fn loads_csv(run: &Run) -> String {
    let mut s = String::from("from,to,start_tick,vblanks,seconds,start_t_s\n");
    for l in &run.loads {
        let _ = writeln!(
            s,
            "{},{},{},{},{:.1},{:.1}",
            l.from, l.to, l.start_tick, l.vblanks, l.seconds, l.start_s
        );
    }
    s
}

fn windows_csv(run: &Run, windows: &[Window]) -> String {
    let mut s = String::from("fps,scene,t0_s,t1_s,step,worst_ms,predicted_ms");
    for l in &run.counter_labels {
        let _ = write!(s, ",mean {}", l.replace(',', " "));
    }
    s.push('\n');
    for w in windows.iter().take(25) {
        let _ = write!(
            s,
            "{:.2},{},{:.1},{:.1},{},{:.0},{}",
            w.fps,
            w.scene,
            w.t0,
            w.t1,
            run.frames[w.first].step,
            w.worst_ms,
            w.predicted_ms.map_or(String::new(), |p| format!("{p:.0}"))
        );
        for c in &w.counters {
            let _ = write!(s, ",{c:.1}");
        }
        s.push('\n');
    }
    s
}

fn worst_csv(run: &Run) -> String {
    let mut s = String::from("t_s,scene,frame_ms,vblanks,step");
    for l in run.counter_labels.iter().chain(&run.pos_labels) {
        let _ = write!(s, ",{}", l.replace(',', " "));
    }
    s.push('\n');
    for f in run.worst_frames(15) {
        let _ = write!(
            s,
            "{:.1},{},{:.0},{},{}",
            f.t_s,
            f.scene,
            f.ms(),
            f.vblanks,
            f.step
        );
        for v in f.counters.iter().chain(&f.pos) {
            let _ = write!(s, ",{v}");
        }
        s.push('\n');
    }
    s
}

fn steps_csv(run: &Run) -> String {
    let mut by: std::collections::BTreeMap<u64, (usize, f64)> = std::collections::BTreeMap::new();
    for f in run.play_frames() {
        let e = by.entry(f.step).or_insert((0, 0.0));
        e.0 += 1;
        e.1 += f.ms() / 1000.0;
    }
    let mut s = String::from("step,label,frames,seconds,fps\n");
    for (k, (n, t)) in by {
        let label = run
            .labels
            .get(&k)
            .map(|l| l.replace('"', "'"))
            .unwrap_or_default();
        let _ = writeln!(s, "{k},\"{label}\",{n},{t:.1},{:.2}", n as f64 / t);
    }
    s
}

/// Mean of every counter per scene, play frames only.
fn scene_means(run: &Run, scene: &str) -> Vec<f64> {
    let sel: Vec<&Frame> = run.play_frames().filter(|f| f.scene == scene).collect();
    let n = sel.len().max(1) as f64;
    (0..run.counter_labels.len())
        .map(|c| sel.iter().map(|f| f64::from(f.counters[c])).sum::<f64>() / n)
        .collect()
}

fn counters_csv(run: &Run) -> String {
    let mut s = String::from("scene");
    for l in &run.counter_labels {
        let _ = write!(s, ",mean {}", l.replace(',', " "));
    }
    s.push('\n');
    for sc in &run.scenes {
        let _ = write!(s, "{sc}");
        for m in scene_means(run, sc) {
            let _ = write!(s, ",{m:.1}");
        }
        s.push('\n');
    }
    s
}

fn class_fps(v: f64) -> &'static str {
    if v >= 20.0 {
        "f-good"
    } else if v >= 15.0 {
        "f-warn"
    } else {
        "f-bad"
    }
}

fn table(run: &Run, rows: &[Summary]) -> String {
    let mut t = String::from("<div class=\"tablewrap\"><table><thead><tr><th>Scene</th><th>Frames</th><th>Play time (s)</th><th>Mean fps</th><th>1% low fps</th><th>Median (ms)</th><th>Worst (ms)</th><th>Over 2 vblanks</th><th>Over 3 vblanks</th><th>Loading (s)</th>");
    for l in &run.counter_labels {
        let _ = write!(t, "<th>{} (mean)</th>", esc(l));
    }
    if run.has_stages {
        t.push_str("<th>Update (ms, mean)</th><th>Render (ms, mean)</th>");
    }
    t.push_str("</tr></thead><tbody>\n");
    for r in rows {
        let all = r.name == "Whole route";
        let means = if all {
            route_means(run)
        } else {
            scene_means(run, &r.name)
        };
        let _ = write!(
            t,
            "<tr{}><td>{}</td><td>{}</td><td>{:.1}</td><td class=\"fps {}\">{:.2}<span class=\"bar\" style=\"width:{}px\"></span></td><td>{:.2}</td><td>{:.1}</td><td>{:.1}</td><td>{:.1}%</td><td>{:.1}%</td><td>{}</td>",
            if all { " class=\"all\"" } else { "" },
            esc(&r.name),
            r.frames,
            r.seconds,
            class_fps(r.mean_fps),
            r.mean_fps,
            (r.mean_fps / 30.0 * 56.0).round(),
            r.p1_low_fps,
            r.median_ms,
            r.worst_ms,
            r.pct_over_2,
            r.pct_over_3,
            if r.loads == 0 { "n/a".to_string() } else { format!("{:.1}", r.load_seconds) },
        );
        for m in means {
            let _ = write!(t, "<td>{m:.0}</td>");
        }
        if run.has_stages {
            let sel: Vec<&Frame> = run
                .play_frames()
                .filter(|f| all || f.scene == r.name)
                .collect();
            for k in 0..2 {
                let ms = sel
                    .iter()
                    .filter_map(|f| f.stages)
                    .map(|s| s[k] as f64 * 1000.0 / crate::log::BUS_HZ)
                    .sum::<f64>()
                    / sel.len().max(1) as f64;
                let _ = write!(t, "<td>{ms:.1}</td>");
            }
        }
        t.push_str("</tr>\n");
    }
    t.push_str("</tbody></table></div>");
    t
}

fn route_means(run: &Run) -> Vec<f64> {
    let sel: Vec<&Frame> = run.play_frames().collect();
    let n = sel.len().max(1) as f64;
    (0..run.counter_labels.len())
        .map(|c| sel.iter().map(|f| f64::from(f.counters[c])).sum::<f64>() / n)
        .collect()
}

/// Up to six slowest stretches that do not overlap, slowest first.
fn picks(windows: &[Window]) -> Vec<&Window> {
    let mut out: Vec<&Window> = Vec::new();
    for w in windows {
        if out.len() == 6 {
            break;
        }
        if out.iter().all(|o| w.t1 < o.t0 || w.t0 > o.t1) {
            out.push(w);
        }
    }
    out.sort_by(|a, b| a.t0.total_cmp(&b.t0));
    out
}

fn notes_html(run: &Run, cfg: &Config, picked: &[&Window], fit: Option<&Fit>) -> String {
    let route_mean = route_means(run);
    let mut s = String::new();
    for w in picked {
        let sc = scene_means(run, &w.scene);
        let _ = write!(
            s,
            "<li><span class=\"where\">{}, {} to {}</span><b>{:.1} fps over {:.0} s.</b> The worst frame is {:.0} ms ({} vblanks).",
            esc(&w.scene),
            clock(w.t0),
            clock(w.t1),
            w.fps,
            w.t1 - w.t0,
            w.worst_ms,
            (w.worst_ms / run.vblank_ms).round()
        );
        if !w.counters.is_empty() {
            let parts: Vec<String> = (0..w.counters.len())
                .map(|c| {
                    format!(
                        "{} {:.0} (scene mean {:.0}, route mean {:.0})",
                        esc(&run.counter_labels[c].to_lowercase()),
                        w.counters[c],
                        sc[c],
                        route_mean[c]
                    )
                })
                .collect();
            let _ = write!(s, " Counters over the stretch: {}.", parts.join("; "));
        }
        match w.predicted_ms {
            Some(p) if w.mean_ms > p * 1.25 => {
                let _ = write!(
                    s,
                    " <span class=\"note-flag\">Not explained by the counters.</span> A fit of frame time on them predicts {p:.0} ms per frame here; the stretch averages {:.0} ms. A render capture or guest profile of this stretch would be needed.",
                    w.mean_ms
                );
            }
            Some(p) => {
                let _ = write!(
                    s,
                    " The counter fit predicts {p:.0} ms per frame against {:.0} ms measured, so the counters account for this stretch.",
                    w.mean_ms
                );
            }
            None => s.push_str(" No counters are logged for this game, so no cause is offered."),
        }
        let _ = fit;
        s.push_str("</li>\n");
    }
    for n in &cfg.notes {
        let _ = writeln!(
            s,
            "<li><span class=\"where\">{}, {}</span>{}</li>",
            esc(&n.scene),
            esc(&n.at),
            esc(&n.text)
        );
    }
    s
}

fn page(
    run: &Run,
    cfg: &Config,
    rows: &[Summary],
    windows: &[Window],
    fit: Option<&Fit>,
    extra: &[String],
) -> String {
    let all = rows.last();
    let picked = picks(windows);
    let title = format!("{} Frame Time", cfg.title);
    let minutes = run.total_s / 60.0;
    let n_play = all.map_or(0, |a| a.frames);
    let lede = format!(
        "{} {:.0} minutes of game time, {} rendered frames, measured on the emulator from the screen buffer flips.",
        esc(&cfg.route),
        minutes,
        group(n_play)
    );
    let tiles = match all {
        Some(a) => {
            let share = |k: u64| {
                run.play_frames().filter(|f| f.vblanks <= k).count() as f64 * 100.0
                    / a.frames as f64
            };
            let (at_30, at_60) = (share(2), share(1));
            let ceiling = if at_60 >= 0.5 {
                format!("{at_30:.0}% run at 30 fps or better, {at_60:.0}% at 60 fps")
            } else {
                format!("{at_30:.0}% run at 30 fps or better")
            };
            let (lmin, lmax) = run.loads.iter().fold((f64::MAX, 0.0f64), |m, l| {
                (m.0.min(l.seconds), m.1.max(l.seconds))
            });
            format!(
                "<div class=\"tile\"><div class=\"v\">{:.1} fps</div><div class=\"k\">mean over the whole route, loading excluded</div></div>\
<div class=\"tile\"><div class=\"v\">{:.1} fps</div><div class=\"k\">1% low (99th percentile frame time)</div></div>\
<div class=\"tile\"><div class=\"v\">{:.0}%</div><div class=\"k\">of frames slower than 20 fps; {}</div></div>\
<div class=\"tile\"><div class=\"v\">{} {}</div><div class=\"k\">{}</div></div>",
                a.mean_fps,
                a.p1_low_fps,
                a.pct_over_3,
                ceiling,
                run.loads.len(),
                if run.loads.len() == 1 { "load" } else { "loads" },
                if run.loads.is_empty() {
                    "no loading pauses on this route".to_string()
                } else {
                    format!("{:.0} s in total, {:.1} to {:.1} s each, drawn as grey bands", a.load_seconds, lmin, lmax)
                }
            )
        }
        None => String::new(),
    };
    let r2 = fit.map_or(String::new(), |f| {
        format!(
            " A least-squares fit of frame time on the counters ({}) accounts for {:.0}% of the variance in frame time over the route; a stretch is flagged as not explained when its measured mean is more than 25% above what the fit predicts for its counters.",
            run.counter_labels.iter().map(|l| esc(&l.to_lowercase())).collect::<Vec<_>>().join(", "),
            f.r2 * 100.0
        )
    });
    let load_rule = match &cfg.scene.ready {
        Some(r) => format!(
            "A load starts when the scene word (<code>{}</code>) stops matching the resident scene and ends at the new scene's first simulation tick (<code>{}</code> changes).",
            esc(&cfg.scene.word),
            esc(r)
        ),
        None => format!(
            "A load is a frame interval of {} vblanks ({:.2} s) or more. The scene word is <code>{}</code>.",
            cfg.scene.load_vblanks,
            cfg.scene.load_vblanks as f64 * run.vblank_ms / 1000.0,
            esc(&cfg.scene.word)
        ),
    };
    let mut method = format!(
        "<p><b>Measuring.</b> The emulator ticks once per vblank ({:.2} ms). A frame is a tick in which the display buffer flipped; its frame time is the cycles since the previous flip, a whole number of vblanks. The journey is replayed by <code>psoxide-gate</code> with scripted pad input, so camera and movement are those of a route, not of a player looking around; time is emulated time. The per-frame log is <code>data/frametime.csv</code>.</p>\n<p><b>Loading.</b> {load_rule} Every frame whose interval touches a load is marked <code>load</code> and left out of every fps figure; the graph's x axis counts loading time too.</p>\n<p><b>Slow stretches.</b> The numbered marks are the slowest non-overlapping windows of about 10 s inside one scene.{r2}</p>\n",
        run.vblank_ms
    );
    for n in extra {
        let _ = writeln!(method, "<p>{}</p>", esc(n));
    }
    let band = true;
    let ymax = crate::page::y_max(&[run]);
    let bucket = crate::page::bucket_s(run.total_s);
    let pts: Vec<(f64, f64)> = run.play_frames().map(|f| (f.t_s, f.ms())).collect();
    let mut series = vec![
        json!({"name": "frame time", "color": "--line", "points": buckets(&pts, run.total_s, bucket)}),
    ];
    if run.has_stages {
        for (k, (name, color)) in [("update", "--upd"), ("render", "--rnd")]
            .into_iter()
            .enumerate()
        {
            let p: Vec<(f64, f64)> = run
                .play_frames()
                .filter_map(|f| {
                    f.stages
                        .map(|s| (f.t_s, s[k] as f64 * 1000.0 / crate::log::BUS_HZ))
                })
                .collect();
            series.push(json!({"name": name, "color": color, "width": 1.0, "points": buckets(&p, run.total_s, bucket)}));
        }
    }
    let marks: Vec<_> = picked
        .iter()
        .map(|w| json!({"t0": round1(w.t0), "t1": round1(w.t1)}))
        .collect();
    let data = json!({
        "total_s": round1(run.total_s), "ymax": ymax, "vb": run.vblank_ms, "bucket": bucket, "band": band,
        "aligned": false,
        "series": series,
        "loads": load_spans(run), "maps": scene_spans(run), "marks": marks,
    });
    let stage_legend = if run.has_stages {
        "<span><i class=\"sw\" style=\"border-color:var(--upd)\"></i>guest update stage</span><span><i class=\"sw\" style=\"border-color:var(--rnd)\"></i>guest render stage</span>"
    } else {
        ""
    };
    let clipped = if run.play_frames().any(|f| f.ms() > ymax) {
        format!(" Frames above {ymax:.0} ms are drawn at the top edge.")
    } else {
        String::new()
    };
    let body = format!(
        "<h1>{}</h1>\n<p class=\"lede\">{lede}</p>\n<div class=\"tiles\">{tiles}</div>\n\
<h2>Frame time along the route</h2>\n<div class=\"card\">\n<svg id=\"chart\" role=\"img\" aria-label=\"Frame time in milliseconds along the route, with scene boundaries, loading pauses, and 30 and 20 fps reference lines.\"></svg>\n<div id=\"tip\"></div>\n<div class=\"legend\">\
<span><i class=\"sw\" style=\"border-color:var(--line)\"></i>mean frame time, {bucket:.0} s buckets</span>\
<span><i class=\"sw band\"></i>mean to worst frame in the bucket</span>{stage_legend}\
<span><i class=\"sw dash\" style=\"border-color:var(--ref30)\"></i>30 fps (2 vblanks)</span>\
<span><i class=\"sw dash\" style=\"border-color:var(--ref20)\"></i>20 fps (3 vblanks)</span>\
<span><i class=\"sw load\"></i>loading pause (not in the numbers)</span></div>\n</div>\n\
<p class=\"caption\">Lower is better. The line sits on a ladder because the game flips the screen on vblank: a frame costs 1, 2, 3 or more vblanks of {:.1} ms. Numbered marks are the stretches described below.{clipped}</p>\n\
<h2>Per scene</h2>\n{}\n\
<p class=\"caption\">Mean fps is frames divided by time spent on them. 1% low is 1000 divided by the 99th percentile frame time. Over 2 vblanks means slower than 30 fps, over 3 vblanks slower than 20 fps. Loading pauses are counted in the last timing column only, never in the fps columns. Counter columns are per-frame means.</p>\n\
<h2>The worst stretches</h2>\n<ol class=\"notes\">\n{}</ol>\n\
<h2>How this was measured</h2>\n<div class=\"method\">\n{method}</div>",
        esc(&title),
        run.vblank_ms,
        table(run, rows),
        notes_html(run, cfg, &picked, fit),
    );
    shell(
        &title,
        &format!("Frame time of {} along its scripted route.", cfg.title),
        &body,
        &data,
    )
}

fn group(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}
