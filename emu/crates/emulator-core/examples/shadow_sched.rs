// SPDX-License-Identifier: GPL-2.0-or-later
//! Lane ps1-sched: offline analysis of a block table written by
//! `PSOXIDE_SHADOW_SCHED=FILE`.
//!
//! usage: shadow_sched BLOCKS CENSUS RANGES [--restarts N] [--threads N]
//!
//! Prints the report to stdout. See `emulator-core/src/shadow_sched.rs`.

use emulator_core::shadow_sched::{analyse_block, load, Block, BlockResult, Totals, Variant};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

fn hex(s: &str) -> u32 {
    u32::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or(0)
}

#[derive(Default)]
struct Census {
    kv: HashMap<String, u64>,
    /// (start, charged cycles, name)
    funcs: Vec<(u32, u64, String)>,
}

fn read_census(path: &str) -> Census {
    let mut c = Census::default();
    let text = std::fs::read_to_string(path).expect("census file");
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.first() == Some(&"func") && f.len() >= 9 {
            c.funcs
                .push((hex(f[1]), f[f.len() - 5].parse().unwrap_or(0), f[3].to_string()));
        } else if f.len() == 2 {
            if let Ok(v) = f[1].parse::<u64>() {
                c.kv.insert(f[0].to_string(), v);
            }
        }
    }
    c
}

fn read_ranges(path: &str) -> Vec<(u32, u32, String)> {
    let text = std::fs::read_to_string(path).expect("ranges file");
    let mut v: Vec<(u32, u32, String)> = text
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() >= 3).then(|| (hex(f[0]), hex(f[1]), f[2..].join(" ")))
        })
        .collect();
    v.sort();
    v
}

fn func_of(ranges: &[(u32, u32, String)], pc: u32) -> Option<usize> {
    let i = ranges.partition_point(|r| r.0 <= pc);
    if i == 0 {
        return None;
    }
    (pc < ranges[i - 1].1).then_some(i - 1)
}

fn run(blocks: &[Block], v: Variant, restarts: u32, threads: usize) -> Vec<BlockResult> {
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<Option<BlockResult>>> = Mutex::new(vec![None; blocks.len()]);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| loop {
                let at = next.fetch_add(32, Ordering::Relaxed);
                if at >= blocks.len() {
                    break;
                }
                let end = (at + 32).min(blocks.len());
                let part: Vec<BlockResult> = blocks[at..end]
                    .iter()
                    .map(|b| analyse_block(b, v, restarts))
                    .collect();
                let mut g = out.lock().unwrap();
                for (i, r) in part.into_iter().enumerate() {
                    g[at + i] = Some(r);
                }
            });
        }
    });
    out.into_inner()
        .unwrap()
        .into_iter()
        .map(|r| r.expect("block result"))
        .collect()
}

fn pct(a: u64, b: u64) -> f64 {
    if b == 0 {
        0.0
    } else {
        100.0 * a as f64 / b as f64
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: shadow_sched BLOCKS CENSUS RANGES [--restarts N] [--threads N]");
        std::process::exit(2);
    }
    let mut restarts = 24u32;
    let validate = args.iter().any(|a| a == "--validate");
    let frame_ceiling: Option<f64> = args
        .iter()
        .position(|a| a == "--frame-ceiling")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok());
    let mut threads = 4usize;
    let mut i = 4;
    while i + 1 < args.len() {
        match args[i].as_str() {
            "--restarts" => restarts = args[i + 1].parse().unwrap_or(24),
            "--threads" => threads = args[i + 1].parse().unwrap_or(4),
            _ => {}
        }
        i += 2;
    }
    let blocks = load(std::path::Path::new(&args[1])).expect("block table");
    let census = read_census(&args[2]);
    let ranges = read_ranges(&args[3]);
    if args.iter().any(|a| a == "--selftest") {
        // The checker must notice an unconstrained shuffle.
        let (mut n, mut caught) = (0u32, 0u32);
        let mut s = 0x2545_f491_4f6c_dd1du64;
        for b in blocks.iter().filter(|b| b.items.len() > 8).take(2000) {
            let m = b.items.len() - 2;
            let mut order: Vec<u16> = (0..m as u16).collect();
            for i in (1..m).rev() {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                order.swap(i, (s % (i as u64 + 1)) as usize);
            }
            n += 1;
            if emulator_core::shadow_sched::validate(b, &order, 8).is_err() {
                caught += 1;
            }
        }
        println!("selftest: {caught} of {n} random shuffles flagged as different");
    }
    let get = |k: &str| census.kv.get(k).copied().unwrap_or(0);
    let work = get("work_cycles");
    let window = get("window_cycles");

    let dyn_instrs: u64 = blocks.iter().map(|b| b.count * b.items.len() as u64).sum();
    let dyn_opened: u64 = blocks
        .iter()
        .map(|b| b.count * b.items.iter().filter(|x| x.2 & 8 != 0).count() as u64)
        .sum();
    println!("blocks distinct {}", blocks.len());
    println!(
        "instructions in blocks {} (census instrs {}), shadows opened in blocks {} (census {})",
        dyn_instrs,
        get("instrs"),
        dyn_opened,
        get("shadows_opened")
    );
    println!("work_cycles {work} window_cycles {window}");
    let execs: u64 = blocks.iter().map(|b| b.count).sum();
    println!(
        "block executions {execs}, mean length {:.2} instructions",
        dyn_instrs as f64 / execs.max(1) as f64
    );

    let cross_hidden: Option<u64> = std::fs::read_to_string(format!("{}.info", args[1]))
        .ok()
        .and_then(|t| {
            t.lines()
                .find_map(|l| l.strip_prefix("cross_hidden ").and_then(|v| v.trim().parse().ok()))
        });
    let variants: [(&str, Variant); 4] = [
        ("P physical registers, store-ordered memory", Variant::default()),
        (
            "R free register renaming (true dependencies only)",
            Variant {
                rename: true,
                ..Variant::default()
            },
        ),
        (
            "F physical, GTE and mult/div instructions are barriers (stall-neutral)",
            Variant {
                fence: true,
                ..Variant::default()
            },
        ),
        (
            "D physical, same-base disjoint-offset accesses do not alias",
            Variant {
                disambig: true,
                ..Variant::default()
            },
        ),
    ];
    let ceiling_slots = (4 * get("shadows_opened")).saturating_sub(get("hidden_instrs"));
    println!(
        "ceiling in slots: 4 x shadows opened - hidden today = {ceiling_slots} ({:.2}% of work cycles, {:.2}% of window cycles)",
        pct(ceiling_slots, work),
        pct(ceiling_slots, window)
    );
    let mut all: Vec<(&str, Vec<BlockResult>)> = Vec::new();
    for (name, v) in variants {
        let t0 = std::time::Instant::now();
        let res = run(&blocks, v, restarts, threads);
        let mut tot = Totals::default();
        for (b, r) in blocks.iter().zip(&res) {
            tot.add(r, b.count);
        }
        let saving = tot.best - tot.orig.min(tot.best);
        println!("\n== variant {name} ({:.1} s)", t0.elapsed().as_secs_f64());
        println!(
            "block-local hidden, original order {} (census hidden_instrs {}; gap census - local = {})",
            tot.orig,
            get("hidden_instrs"),
            get("hidden_instrs") as i64 - tot.orig as i64
        );
        println!("block-local hidden, best order {}", tot.best);
        if let Some(cross) = cross_hidden {
            println!(
                "decomposition: census {} = block-local {} + cross-block {} - I-cache-miss loss {} (loss derived by identity)",
                get("hidden_instrs"),
                tot.orig,
                cross,
                (tot.orig + cross) as i64 - get("hidden_instrs") as i64
            );
        }
        println!(
            "saving {} cycles = {:.2}% of work cycles, {:.2}% of window cycles, {:.1}% of the ceiling slots",
            saving,
            pct(saving, work),
            pct(saving, window),
            pct(saving, ceiling_slots)
        );
        if let Some(fc) = frame_ceiling {
            println!(
                "scaled to the shadowmax frame-cycle ceiling of {fc:.1}%: reachable = {:.2}% of frame cycles",
                fc * saving as f64 / ceiling_slots.max(1) as f64
            );
        }
        println!(
            "saving from blocks that got the exact search {} ({:.1}% of the saving)",
            tot.saving_exact,
            pct(tot.saving_exact, saving)
        );
        println!(
            "pinned blocks (load-delay hazard or too long) {} distinct, {} executions ({:.2}% of block executions)",
            tot.pinned_blocks,
            tot.pinned_execs,
            pct(tot.pinned_execs, blocks.iter().map(|b| b.count).sum())
        );
        let (mut hb, mut hn, mut hw, mut hmiss) = (0u64, 0u64, 0u64, 0u64);
        for (b, r) in blocks.iter().zip(&res) {
            if let Some(h) = r.heuristic {
                hb += 1;
                if h < r.best {
                    hn += 1;
                    hw += u64::from(r.best - h) * b.count;
                }
                hmiss += u64::from(r.best.saturating_sub(r.orig)) * b.count;
            }
        }
        println!(
            "heuristic vs exact on {hb} sampled exact blocks: heuristic trailed on {hn}, weighted miss {hw} cycles of {hmiss} exact saving"
        );
        let heur_blocks = blocks
            .iter()
            .zip(&res)
            .filter(|(_, r)| !r.exact && r.pinned == emulator_core::shadow_sched::Pinned::No)
            .count();
        println!("blocks that got the heuristic search {heur_blocks}");
        let u = &tot.unused;
        println!(
            "unused slots in the best order: total {} | another load ended it: none left {} / left but not ready {} / ready but unused {} | a store ended it: none left {} / not ready {} / ready but unused {} | other bus op {} | dependent read {} | block edge {}",
            u.total(), u.load_none_left, u.load_not_ready, u.load_ready_unused, u.store_none_left, u.store_not_ready, u.store_ready_unused, u.other_bus, u.dependent, u.block_edge
        );
        println!(
            "shadows opened {} -> slots {}; hidden best {}; unused + hidden = {}",
            tot.openers,
            4 * tot.openers,
            tot.best,
            u.total() + tot.best
        );
        if validate && !v.rename {
            let mut bad = 0u64;
            let mut checked = 0u64;
            for (b, r) in blocks.iter().zip(&res) {
                if r.order.is_empty() {
                    continue;
                }
                checked += 1;
                if let Err(e) = emulator_core::shadow_sched::validate(b, &r.order, 8) {
                    bad += 1;
                    if bad <= 5 {
                        println!("VALIDATE FAIL {:#010x} len {}: {e}", b.items[0].0, b.items.len());
                        if std::env::var_os("SCHED_DEBUG").is_some() {
                            for (i, it) in b.items.iter().enumerate() {
                                println!("  {i:3} {:08x} {:08x} c{}", it.0, it.1, it.2);
                            }
                            println!("  order {:?}", r.order);
                            let pos = |k: usize| r.order.iter().position(|&q| q as usize == k);
                            for i in 0..b.items.len() {
                                for j in i + 1..b.items.len() {
                                    let (Some(pi), Some(pj)) = (pos(i), pos(j)) else { continue };
                                    let mem = |w: u32| matches!(w >> 26, 0x20..=0x2e | 0x32 | 0x3a);
                                    if pj < pi && mem(b.items[i].1) && mem(b.items[j].1) {
                                        println!("  inverted mem pair {i} {:08x} / {j} {:08x}", b.items[i].1, b.items[j].1);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            println!("semantic check: {checked} reordered blocks run on 8 random states each, {bad} differ");
        }
        all.push((name, res));
    }

    // Per function, concentration and top blocks for variant P and R.
    for (vi, label) in [(0usize, "P"), (1, "R")] {
        let res = &all[vi].1;
        let mut per_func: HashMap<Option<usize>, (u64, u64)> = HashMap::new();
        let mut gains: Vec<(u64, usize)> = Vec::new();
        for (bi, (b, r)) in blocks.iter().zip(res).enumerate() {
            let g = u64::from(r.best.saturating_sub(r.orig)) * b.count;
            let f = func_of(&ranges, b.items[0].0);
            let e = per_func.entry(f).or_default();
            e.0 += g;
            e.1 += u64::from(r.orig) * b.count;
            if g > 0 {
                gains.push((g, bi));
            }
        }
        gains.sort_by(|a, b| b.0.cmp(&a.0));
        let total: u64 = gains.iter().map(|g| g.0).sum();
        println!("\n== variant {label}: concentration (blocks with a gain: {})", gains.len());
        for k in [10usize, 50, 200] {
            let s: u64 = gains.iter().take(k).map(|g| g.0).sum();
            println!("top {k} blocks: {:.1}% of the saving", pct(s, total));
        }
        let mut funcs: Vec<(Option<usize>, (u64, u64))> = per_func.into_iter().collect();
        funcs.sort_by(|a, b| (b.1).0.cmp(&(a.1).0));
        println!("== variant {label}: top 15 functions by saving (cycles; % of work cycles; function's own share of work cycles)");
        let charged: HashMap<u32, u64> = census.funcs.iter().map(|f| (f.0, f.1)).collect();
        for (f, (g, hid)) in funcs.iter().take(15) {
            let (name, start) = match f {
                Some(i) => (ranges[*i].2.clone(), ranges[*i].0),
                None => ("(no range)".to_string(), 0),
            };
            let c = charged.get(&start).copied().unwrap_or(0);
            println!(
                "{name} saving {g} ({:.3}% of work) share {:.2}% of work cycles, hidden today {hid}",
                pct(*g, work),
                pct(c, work)
            );
        }
        println!("== variant {label}: top 10 blocks");
        for (g, bi) in gains.iter().take(10) {
            let b = &blocks[*bi];
            let r = &res[*bi];
            let f = func_of(&ranges, b.items[0].0)
                .map(|i| ranges[i].2.clone())
                .unwrap_or_default();
            println!(
                "{:#010x} {f} len {} runs {} hidden {} -> {} gain {} exact {}",
                b.items[0].0,
                b.items.len(),
                b.count,
                r.orig,
                r.best,
                g,
                r.exact
            );
        }
    }
}
