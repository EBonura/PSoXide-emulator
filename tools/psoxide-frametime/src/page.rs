//! Shared HTML pieces: the page shell, text escaping and the chart data.

use serde_json::{json, Value};

use crate::log::Run;

const TEMPLATE: &str = include_str!("template.html");

/// Escape text for HTML.
pub(crate) fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `m:ss` (or `h:mm:ss`) of game time.
pub(crate) fn clock(s: f64) -> String {
    let t = s.max(0.0) as u64;
    if t >= 3600 {
        format!("{}:{:02}:{:02}", t / 3600, t % 3600 / 60, t % 60)
    } else {
        format!("{}:{:02}", t / 60, t % 60)
    }
}

/// Fill the shell with a body and the chart data.
pub(crate) fn shell(title: &str, desc: &str, body: &str, data: &Value) -> String {
    // `</` inside the JSON would end the script element.
    let json = data.to_string().replace("</", "<\\/");
    TEMPLATE
        .replace("__TITLE__", &esc(title))
        .replace("__DESC__", &esc(desc))
        .replace("__BODY__", body)
        .replace("__DATA__", &json)
}

/// Mean and worst frame time per bucket of `bucket` seconds, as `[mean_ms, worst_ms]` or null.
pub(crate) fn buckets(points: &[(f64, f64)], total_s: f64, bucket: f64) -> Vec<Value> {
    let nb = (total_s / bucket).ceil() as usize + 1;
    let mut cnt = vec![0u32; nb];
    let mut sum = vec![0f64; nb];
    let mut mx = vec![0f64; nb];
    for (x, ms) in points {
        let b = ((x / bucket) as usize).min(nb - 1);
        cnt[b] += 1;
        sum[b] += ms;
        mx[b] = mx[b].max(*ms);
    }
    (0..nb)
        .map(|b| {
            if cnt[b] == 0 {
                Value::Null
            } else {
                json!([
                    (sum[b] / f64::from(cnt[b]) * 10.0).round() / 10.0,
                    mx[b].round()
                ])
            }
        })
        .collect()
}

/// Runs of one scene for the strip above the chart: from the load into it (or its first frame) to
/// its last frame. A scene the route returns to gets a run each time.
pub(crate) fn scene_spans(run: &Run) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut cur: Option<(String, f64, f64)> = None;
    let flush = |c: Option<(String, f64, f64)>, out: &mut Vec<Value>| {
        if let Some((name, t0, t1)) = c {
            out.push(json!({"map": name, "t0": round1(t0), "t1": round1(t1)}));
        }
    };
    for f in &run.frames {
        match &mut cur {
            Some((name, _, t1)) if *name == f.scene => *t1 = f.t_s,
            _ => {
                flush(cur.take(), &mut out);
                let start = run
                    .loads
                    .iter()
                    .find(|l| l.to == f.scene && l.start_s <= f.t_s && f.t_s - l.start_s < 30.0)
                    .map_or(f.t_s, |l| l.start_s);
                cur = Some((f.scene.clone(), start, f.t_s));
            }
        }
    }
    flush(cur.take(), &mut out);
    out
}

pub(crate) fn load_spans(run: &Run) -> Vec<Value> {
    run.loads
        .iter()
        .map(
            |l| json!({"from": l.from, "to": l.to, "t": round1(l.start_s), "s": round1(l.seconds)}),
        )
        .collect()
}

pub(crate) fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// Top of the chart in steps of 50 ms: the slowest frame, unless it is more than twice the 99.9th percentile.
pub(crate) fn y_max(runs: &[&Run]) -> f64 {
    let mut ms: Vec<f64> = runs
        .iter()
        .flat_map(|r| r.play_frames().map(|f| f.ms()))
        .collect();
    ms.sort_by(f64::total_cmp);
    let p = ms
        .get(((ms.len() as f64) * 0.999) as usize)
        .copied()
        .unwrap_or(100.0);
    let worst = ms.last().copied().unwrap_or(p);
    let top = if worst <= p * 2.0 { worst } else { p * 1.1 };
    ((top / 50.0).ceil() * 50.0).max(150.0)
}

/// Bucket width that keeps the chart under about 1,500 columns.
pub(crate) fn bucket_s(total_s: f64) -> f64 {
    (total_s / 1500.0).ceil().max(1.0)
}
