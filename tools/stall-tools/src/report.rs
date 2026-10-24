//! Stall-budget report: where a window of a guest run spends its cycles.
//!
//! Inputs are the logs `frontend launch` writes for a replayed input tape:
//! `cycles.csv` (--cpu-cycle-profile-log), `route.csv`, and optionally
//! `class.csv` (--instruction-class-log), `pc.csv` (--pc-line-log with
//! --pc-log-words), `ramstall.csv`, `mmio.csv`, `ic.csv` (the matching
//! --*-stall-line-log flags) and `ram.bin` (--dump-ram). With the per-PC logs
//! and the RAM image the report finds wait loops (vblank spins, GPU and DMA
//! status polls) and removes them, so the shares describe work.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use crate::csv::{read_by_pc, Table};
use crate::mips;

pub struct Image(Vec<u8>);

impl Image {
    pub fn load(path: &Path) -> Option<Image> {
        std::fs::read(path).ok().map(Image)
    }

    pub fn word(&self, addr: u32) -> u32 {
        let o = (addr & 0x1f_ffff) as usize;
        self.0
            .get(o..o + 4)
            .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
}

/// Word addresses that belong to wait loops.
pub fn detect_waits(
    pc: &BTreeMap<u32, u64>,
    ram: &BTreeMap<u32, u64>,
    mmio: &BTreeMap<u32, u64>,
    ic: &BTreeMap<u32, u64>,
    word: &dyn Fn(u32) -> u32,
) -> BTreeSet<u32> {
    let total: u64 = pc.values().sum();
    let mmio_total: u64 = mmio.values().sum();
    let mut waits = BTreeSet::new();
    // Tight loops: 4 to 24 consecutive words retired the same number of times,
    // doing nothing but loads and ALU, worth at least 1% of all instructions.
    let addrs: Vec<u32> = pc.keys().copied().collect();
    let mut i = 0;
    while i < addrs.len() {
        let first = pc[&addrs[i]];
        let mut j = i;
        while j + 1 < addrs.len()
            && addrs[j + 1] == addrs[j] + 4
            && pc[&addrs[j + 1]].abs_diff(first) <= first / 100 + 2
        {
            j += 1;
        }
        let len = j - i + 1;
        if (4..=24).contains(&len) {
            let words: Vec<u32> = addrs[i..=j].iter().map(|&a| word(a)).collect();
            let sum: u64 = addrs[i..=j].iter().map(|a| pc[a]).sum();
            if mips::is_pure_poll(&words) && sum * 100 >= total {
                waits.extend(addrs[i..=j].iter().copied());
            }
        }
        i = j + 1;
    }
    // Status pollers: a run of at most 1,100 bytes (gaps up to 16 bytes bridged)
    // holding at least a tenth of all MMIO stall.
    let mut all: BTreeSet<u32> = pc.keys().copied().collect();
    all.extend(ram.keys().copied());
    all.extend(mmio.keys().copied());
    all.extend(ic.keys().copied());
    let all: Vec<u32> = all.into_iter().collect();
    let mut start = 0;
    while start < all.len() {
        let mut end = start;
        while end + 1 < all.len() && all[end + 1] - all[end] <= 16 {
            end += 1;
        }
        let (a, b) = (all[start], all[end]);
        let m: u64 = mmio.range(a..=b).map(|(_, v)| *v).sum();
        if b - a + 4 <= 1100 && mmio_total > 0 && m * 10 >= mmio_total {
            let mut x = a;
            while x <= b {
                waits.insert(x);
                x += 4;
            }
        }
        start = end + 1;
    }
    waits
}

#[derive(Debug, Default)]
pub struct Report {
    pub ticks: u64,
    pub flips: u64,
    pub total_cycles: u64,
    pub wait_cycles: u64,
    pub wait_words: usize,
    /// Percent of work cycles.
    pub issue: f64,
    pub nops: f64,
    pub ram_load_data: f64,
    pub ram_load_stack: f64,
    pub ram_store: f64,
    pub icache: f64,
    pub muldiv: f64,
    pub gte: f64,
    pub mmio: f64,
    pub other: f64,
    pub wait_share: f64,
}

pub fn build(dir: &Path, from_tick: u64) -> Result<Report, String> {
    let cycles = Table::read(&dir.join("cycles.csv"))?;
    let route = Table::read(&dir.join("route.csv"))?;
    let s = |n: &str| cycles.sum_from(n, from_tick);
    let total = s("profiled_cpu_cycles");
    if total == 0 {
        return Err("no cycles at or after the start tick".into());
    }
    let pc = read_by_pc(&dir.join("pc.csv"), "instructions");
    let ram = read_by_pc(&dir.join("ramstall.csv"), "ram_load_stall_cycles");
    let mmio = read_by_pc(&dir.join("mmio.csv"), "mmio_stall_cycles");
    let ic = read_by_pc(&dir.join("ic.csv"), "icache_refill_stall_cycles");
    let waits = match Image::load(&dir.join("ram.bin")) {
        Some(image) if !pc.is_empty() => detect_waits(&pc, &ram, &mmio, &ic, &|a| image.word(a)),
        _ => BTreeSet::new(),
    };
    let in_waits = |m: &BTreeMap<u32, u64>| -> u64 {
        m.iter()
            .filter(|(k, _)| waits.contains(k))
            .map(|(_, v)| *v)
            .sum()
    };
    let (wi, wr, wm, wc) = (
        in_waits(&pc),
        in_waits(&ram),
        in_waits(&mmio),
        in_waits(&ic),
    );
    let wait_cycles = wi + wr + wm + wc;
    let work = (total - wait_cycles.min(total)).max(1) as f64;
    let pct = |v: u64| 100.0 * v as f64 / work;
    let nops = Table::read(&dir.join("class.csv")).map_or(0, |t| t.sum_from("nops", from_tick));
    let ram_load = s("ram_load_stall_cycles").saturating_sub(wr);
    let stack = s("stack_ram_load_stall_cycles");
    let flips = route
        .rows
        .iter()
        .filter(|r| route.num(r, "route_tick") >= from_tick)
        .map(|r| route.num(r, "display_start_changed"))
        .sum();
    Ok(Report {
        ticks: cycles
            .rows
            .iter()
            .filter(|r| cycles.num(r, "route_tick") >= from_tick)
            .count() as u64,
        flips,
        total_cycles: total,
        wait_cycles,
        wait_words: waits.len(),
        issue: pct(s("issue_cycles").saturating_sub(wi)),
        nops: pct(nops),
        ram_load_data: pct(ram_load.saturating_sub(stack)),
        ram_load_stack: pct(stack),
        ram_store: pct(s("ram_store_stall_cycles")),
        icache: pct(s("icache_refill_stall_cycles").saturating_sub(wc)),
        muldiv: pct(s("muldiv_interlock_stall_cycles")),
        gte: pct(s("gte_busy_stall_cycles")),
        mmio: pct(s("mmio_stall_cycles").saturating_sub(wm)).max(0.0),
        other: pct(s("other_stall_cycles") + s("uncached_fetch_stall_cycles")),
        wait_share: 100.0 * wait_cycles as f64 / total as f64,
    })
}

impl Report {
    pub fn render(&self) -> String {
        let mut o = String::new();
        let fps = if self.ticks > 0 {
            59.94 * self.flips as f64 / self.ticks as f64
        } else {
            0.0
        };
        let _ = writeln!(
            o,
            "window: {} vblanks, {} presented frames ({fps:.1} fps), {} cycles",
            self.ticks, self.flips, self.total_cycles
        );
        let _ = writeln!(o, "wait loops: {:.1}% of all cycles ({} cycles, {} words); shares below are % of work cycles", self.wait_share, self.wait_cycles, self.wait_words);
        for (name, v) in [
            ("issue", self.issue),
            ("  of which nops", self.nops),
            ("ram load, data", self.ram_load_data),
            ("ram load, stack", self.ram_load_stack),
            ("ram store", self.ram_store),
            ("i-cache refill", self.icache),
            ("mul/div", self.muldiv),
            ("gte busy", self.gte),
            ("mmio", self.mmio),
            ("other", self.other),
        ] {
            let _ = writeln!(o, "{name:<18}{v:6.1}");
        }
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(items: &[(u32, u64)]) -> BTreeMap<u32, u64> {
        items.iter().copied().collect()
    }

    #[test]
    fn finds_a_vblank_spin_but_not_a_copy_loop() {
        // 0x1000..: lw, addiu, bne, nop spin retired 1000 times; 0x2000..: lw, sw, addiu, bne copy loop.
        let spin = [0x8fa8_0010u32, 0x2508_0001, 0x1500_fffd, 0];
        let copy = [0x8fa8_0010u32, 0xac88_0000, 0x2508_0001, 0x1500_fffc];
        let word = |a: u32| match a {
            0x1000..=0x100c => spin[((a - 0x1000) / 4) as usize],
            0x2000..=0x200c => copy[((a - 0x2000) / 4) as usize],
            _ => 0,
        };
        let mut pc = map(&[]);
        for k in 0..4 {
            pc.insert(0x1000 + 4 * k, 1000);
            pc.insert(0x2000 + 4 * k, 1000);
        }
        let waits = detect_waits(&pc, &map(&[]), &map(&[]), &map(&[]), &word);
        assert!(waits.contains(&0x1000) && waits.contains(&0x100c));
        assert!(!waits.contains(&0x2000));
    }

    #[test]
    fn finds_a_status_poller_by_its_mmio_stall() {
        let mut pc = map(&[]);
        for k in 0..8 {
            pc.insert(0x3000 + 4 * k, 10);
            pc.insert(0x9000 + 4 * k, 10);
        }
        let mmio = map(&[(0x3008, 900), (0x9008, 10)]);
        let waits = detect_waits(&pc, &map(&[]), &mmio, &map(&[]), &|_| 0);
        assert!(waits.contains(&0x3008) && !waits.contains(&0x9008));
    }
}
