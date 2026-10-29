//! Numbers derived from a measured run: per-scene summaries, worst stretches, and a
//! least-squares fit of frame time on the counters.

use crate::log::{Frame, Run};

/// The figures for one scene, or for the whole route.
#[derive(Clone, Debug)]
pub(crate) struct Summary {
    pub name: String,
    pub frames: usize,
    pub seconds: f64,
    pub mean_fps: f64,
    pub p1_low_fps: f64,
    pub median_ms: f64,
    pub worst_ms: f64,
    pub worst_t_s: f64,
    pub pct_over_2: f64,
    pub pct_over_3: f64,
    pub loads: usize,
    pub load_seconds: f64,
}

/// Summarise frames (loads already excluded) plus the loads that belong to the scene.
pub(crate) fn summarise(
    name: &str,
    frames: &[&Frame],
    loads: usize,
    load_seconds: f64,
) -> Option<Summary> {
    if frames.is_empty() {
        return None;
    }
    let n = frames.len();
    let ms: Vec<f64> = frames.iter().map(|f| f.ms()).collect();
    let seconds = ms.iter().sum::<f64>() / 1000.0;
    let mut sorted = ms.clone();
    sorted.sort_by(f64::total_cmp);
    let p99 = sorted[((n as f64 * 0.99).ceil() as usize).clamp(1, n) - 1];
    let (worst_i, worst_ms) =
        ms.iter()
            .copied()
            .enumerate()
            .fold((0, 0.0), |a, (i, v)| if v > a.1 { (i, v) } else { a });
    let over = |k: u64| frames.iter().filter(|f| f.vblanks > k).count() as f64 * 100.0 / n as f64;
    Some(Summary {
        name: name.to_string(),
        frames: n,
        seconds,
        mean_fps: n as f64 / seconds,
        p1_low_fps: 1000.0 / p99,
        median_ms: sorted[n / 2],
        worst_ms,
        worst_t_s: frames[worst_i].t_s,
        pct_over_2: over(2),
        pct_over_3: over(3),
        loads,
        load_seconds: load_seconds + 0.0,
    })
}

impl Run {
    /// One summary per scene in route order, then the whole route last.
    pub(crate) fn summaries(&self) -> Vec<Summary> {
        let mut out = Vec::new();
        for s in &self.scenes {
            let frames: Vec<&Frame> = self.play_frames().filter(|f| &f.scene == s).collect();
            let loads: Vec<_> = self.loads.iter().filter(|l| &l.to == s).collect();
            if let Some(sum) = summarise(
                s,
                &frames,
                loads.len(),
                loads.iter().map(|l| l.seconds).sum::<f64>() + 0.0,
            ) {
                out.push(sum);
            }
        }
        let all: Vec<&Frame> = self.play_frames().collect();
        if let Some(sum) = summarise(
            "Whole route",
            &all,
            self.loads.len(),
            self.loads.iter().map(|l| l.seconds).sum(),
        ) {
            out.push(sum);
        }
        out
    }
}

/// A slow stretch of one scene.
pub(crate) struct Window {
    pub scene: String,
    pub fps: f64,
    pub t0: f64,
    pub t1: f64,
    pub first: usize,
    pub worst_ms: f64,
    pub mean_ms: f64,
    /// Mean of each counter over the stretch.
    pub counters: Vec<f64>,
    /// Mean frame time the fit predicts from those counters.
    pub predicted_ms: Option<f64>,
}

impl Run {
    /// Windows of about 10 s of play inside one scene (at least 8 s), slowest first.
    pub(crate) fn windows(&self, fit: Option<&Fit>) -> Vec<Window> {
        let idx: Vec<usize> = (0..self.frames.len())
            .filter(|i| !self.frames[*i].load)
            .collect();
        let mut out = Vec::new();
        let mut k = 0;
        while k < idx.len() {
            let a = idx[k];
            let mut b = k;
            let mut t = 0.0f64;
            while b < idx.len() && t < 10.0 && self.frames[idx[b]].scene == self.frames[a].scene {
                t += self.frames[idx[b]].cycles as f64 / crate::log::BUS_HZ;
                b += 1;
            }
            if t >= 8.0 && b > k {
                let last = idx[b - 1];
                let sel: Vec<&Frame> = idx[k..b].iter().map(|i| &self.frames[*i]).collect();
                let nn = sel.len() as f64;
                let counters: Vec<f64> = (0..self.counter_labels.len())
                    .map(|c| sel.iter().map(|f| f64::from(f.counters[c])).sum::<f64>() / nn)
                    .collect();
                let mean_ms = t * 1000.0 / nn;
                out.push(Window {
                    scene: self.frames[a].scene.clone(),
                    fps: nn / t,
                    t0: self.frames[a].t_s,
                    t1: self.frames[last].t_s,
                    first: a,
                    worst_ms: sel.iter().map(|f| f.ms()).fold(0.0, f64::max),
                    mean_ms,
                    predicted_ms: fit.map(|f| f.predict(&counters)),
                    counters,
                });
            }
            k = b.max(k + 1);
        }
        out.sort_by(|a, b| a.fps.total_cmp(&b.fps));
        out
    }

    /// The slowest play frames.
    pub(crate) fn worst_frames(&self, n: usize) -> Vec<&Frame> {
        let mut v: Vec<&Frame> = self.play_frames().collect();
        v.sort_by_key(|f| std::cmp::Reverse(f.cycles));
        v.truncate(n);
        v
    }
}

/// Frame time (ms) = intercept + sum(coef * counter), least squares over the play frames.
pub(crate) struct Fit {
    intercept: f64,
    coefs: Vec<f64>,
    /// Share of the variance in frame time the counters account for (0 to 1).
    pub r2: f64,
}

impl Fit {
    pub(crate) fn predict(&self, counters: &[f64]) -> f64 {
        self.intercept
            + self
                .coefs
                .iter()
                .zip(counters)
                .map(|(c, v)| c * v)
                .sum::<f64>()
    }

    /// Fit over the run's play frames; `None` with no counters or a singular system.
    pub(crate) fn of(run: &Run) -> Option<Fit> {
        let k = run.counter_labels.len();
        let rows: Vec<&Frame> = run.play_frames().collect();
        if k == 0 || rows.len() < k + 10 {
            return None;
        }
        let d = k + 1;
        let mut a = vec![vec![0.0f64; d + 1]; d];
        let x = |f: &Frame, i: usize| {
            if i == 0 {
                1.0
            } else {
                f64::from(f.counters[i - 1])
            }
        };
        for f in &rows {
            let y = f.ms();
            let xs: Vec<f64> = (0..d).map(|i| x(f, i)).collect();
            for (i, row) in a.iter_mut().enumerate() {
                for (cell, xj) in row.iter_mut().zip(&xs) {
                    *cell += xs[i] * xj;
                }
                row[d] += xs[i] * y;
            }
        }
        for (i, row) in a.iter_mut().enumerate() {
            // Tiny ridge so constant counters do not make the system singular.
            row[i] += 1e-9 * row[i].abs().max(1.0);
        }
        let beta = solve(a)?;
        let mean = rows.iter().map(|f| f.ms()).sum::<f64>() / rows.len() as f64;
        let (mut ss_res, mut ss_tot) = (0.0, 0.0);
        for f in &rows {
            let pred: f64 = (0..d).map(|i| beta[i] * x(f, i)).sum();
            ss_res += (f.ms() - pred).powi(2);
            ss_tot += (f.ms() - mean).powi(2);
        }
        Some(Fit {
            intercept: beta[0],
            coefs: beta[1..].to_vec(),
            r2: if ss_tot > 0.0 {
                1.0 - ss_res / ss_tot
            } else {
                0.0
            },
        })
    }
}

/// Solve the augmented system `m` (n rows of n+1) by Gaussian elimination with partial pivoting.
fn solve(mut m: Vec<Vec<f64>>) -> Option<Vec<f64>> {
    let d = m.len();
    for c in 0..d {
        let p = (c..d).max_by(|x, y| m[*x][c].abs().total_cmp(&m[*y][c].abs()))?;
        if m[p][c].abs() < 1e-12 {
            return None;
        }
        m.swap(c, p);
        let pivot = m[c].clone();
        for (r, row) in m.iter_mut().enumerate() {
            if r != c {
                let f = row[c] / pivot[c];
                for (cell, pv) in row.iter_mut().zip(&pivot).skip(c) {
                    *cell -= f * pv;
                }
            }
        }
    }
    Some((0..d).map(|i| m[i][d] / m[i][i]).collect())
}
