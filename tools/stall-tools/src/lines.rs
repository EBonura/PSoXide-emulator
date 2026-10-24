//! Hot-line ranking for the data-line log (`PSOXIDE_RAM_DATA_LOG`).
//!
//! The log has one row per 32-byte main-RAM line: `line_addr,stall_cycles,loads`.
//! `top` picks the hottest lines up to a byte budget (what an engine-owned
//! scratchpad plan would hold) and `window` finds the hottest contiguous run
//! of that size (what a scratchpad stack could hold). Both write range files
//! in the format `PSOXIDE_LIMIT_FAST_DATA` reads, and report the share of all
//! RAM-load stall cycles they cover.

pub struct Line {
    pub addr: u32,
    pub stall: u64,
}

pub fn parse(text: &str) -> Vec<Line> {
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let mut f = l.split(',');
            let addr = u32::from_str_radix(f.next()?.trim_start_matches("0x"), 16).ok()?;
            Some(Line {
                addr,
                stall: f.next()?.parse().ok()?,
            })
        })
        .collect()
}

fn ranges(mut addrs: Vec<u32>) -> Vec<(u32, u32)> {
    addrs.sort_unstable();
    let mut out: Vec<(u32, u32)> = Vec::new();
    for a in addrs {
        match out.last_mut() {
            Some(last) if last.1 == a => last.1 = a + 32,
            _ => out.push((a, a + 32)),
        }
    }
    out
}

/// Hottest `bytes` of lines: (ranges, share of all stall cycles in percent).
pub fn top(lines: &[Line], bytes: u32) -> (Vec<(u32, u32)>, f64) {
    let total: u64 = lines.iter().map(|l| l.stall).sum();
    let mut sorted: Vec<&Line> = lines.iter().collect();
    sorted.sort_by(|a, b| b.stall.cmp(&a.stall).then(a.addr.cmp(&b.addr)));
    let take = (bytes / 32) as usize;
    let chosen = &sorted[..take.min(sorted.len())];
    let covered: u64 = chosen.iter().map(|l| l.stall).sum();
    (
        ranges(chosen.iter().map(|l| l.addr).collect()),
        100.0 * covered as f64 / total.max(1) as f64,
    )
}

/// Hottest contiguous `bytes` window: ((start, end), share in percent).
pub fn window(lines: &[Line], bytes: u32) -> ((u32, u32), f64) {
    let total: u64 = lines.iter().map(|l| l.stall).sum();
    let mut sorted: Vec<&Line> = lines.iter().collect();
    sorted.sort_by_key(|l| l.addr);
    let (mut best, mut best_start, mut cur, mut j) = (0u64, 0u32, 0u64, 0usize);
    for i in 0..sorted.len() {
        cur += sorted[i].stall;
        while sorted[j].addr + bytes <= sorted[i].addr {
            cur -= sorted[j].stall;
            j += 1;
        }
        if cur > best {
            best = cur;
            best_start = sorted[j].addr;
        }
    }
    (
        (best_start, best_start + bytes),
        100.0 * best as f64 / total.max(1) as f64,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "line_addr,stall_cycles,loads\n0x00001000,50,8\n0x00001020,40,6\n0x00005000,45,7\n0x00001040,5,1\n";

    #[test]
    fn top_merges_adjacent_lines_and_reports_coverage() {
        let lines = parse(LOG);
        let (r, share) = top(&lines, 96);
        assert_eq!(r, vec![(0x1000, 0x1040), (0x5000, 0x5020)]);
        assert!((share - 100.0 * 135.0 / 140.0).abs() < 1e-9);
    }

    #[test]
    fn window_picks_the_best_contiguous_run() {
        let lines = parse(LOG);
        let ((s, e), share) = window(&lines, 64);
        assert_eq!((s, e), (0x1000, 0x1040));
        assert!((share - 100.0 * 90.0 / 140.0).abs() < 1e-9);
    }
}
