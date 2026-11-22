//! The hardware-test rows the texture cost laws are fitted to, replayed
//! through the GPU as the hardware-tests program sends them: v2.1 records
//! 0x200 to 0x223 (texture state), v2.4 0x8E0 to 0x92A (UV windows, 480i)
//! and v2.26 0xAC0 to 0xAEE (texture page refill). Each row is drawn five
//! times and the median list cost taken, as the program's sampler does.

use super::*;

/// Console medians, Hardware Tests v2.26 run 8017 (2026-11-22), and the
/// emulator's own (twin run, PSoXide-emulator 59af7e74).
#[rustfmt::skip]
const CONSOLE: &[(u16, u64)] = &[
    (0x200, 2268), (0x201, 2425), (0x202, 2462), (0x203, 2268), (0x204, 2307), (0x205, 2427),
    (0x206, 6433), (0x207, 2329), (0x208, 2292), (0x209, 2922), (0x20A, 2292), (0x20B, 2521),
    (0x20C, 5857), (0x20D, 10367), (0x20E, 6346), (0x20F, 5827), (0x210, 6110), (0x211, 12707),
    (0x212, 10714), (0x213, 8104), (0x214, 6565), (0x215, 17595), (0x216, 6558), (0x217, 10647),
    (0x218, 9934), (0x219, 16819), (0x21A, 10444), (0x21B, 9827), (0x21C, 10666),
    (0x21D, 21631), (0x21E, 15039), (0x21F, 12703), (0x220, 11198), (0x221, 31771),
    (0x222, 11280), (0x223, 18521), (0xAC0, 9928), (0xAC2, 11120), (0xAC4, 11158),
    (0xAC6, 10917), (0xAC8, 9913), (0xACA, 14697), (0xACC, 14677), (0xACE, 14699),
    (0xAD0, 9833), (0xAD2, 19851), (0xAD4, 19841), (0xAD6, 19851), (0xAD8, 9924),
    (0xADA, 12461), (0xADC, 12433), (0xADE, 12435), (0xAE0, 9930), (0xAE2, 19587),
    (0xAE4, 19527), (0xAE6, 19623), (0xAE8, 11193), (0xAEA, 29668), (0xAEC, 29680),
    (0xAEE, 29712), (0x8E0, 5412), (0x8E2, 5386), (0x8E4, 5393), (0x8E6, 5400), (0x8E8, 14973),
    (0x8EA, 24119), (0x8EC, 5376), (0x8EE, 5379), (0x8F0, 5391), (0x8F2, 12025),
    (0x8F4, 26554), (0x8F6, 44089), (0x8F8, 5378), (0x8FA, 5413), (0x8FC, 5412),
    (0x8FE, 21956), (0x900, 45170), (0x902, 45161), (0x904, 5397), (0x906, 5388),
    (0x908, 5380), (0x90A, 5380), (0x90C, 5396), (0x90E, 5386), (0x910, 5387), (0x912, 5377),
    (0x914, 5396), (0x916, 5392), (0x918, 5388), (0x91A, 5860), (0x91C, 5403), (0x91E, 5383),
    (0x920, 5408), (0x922, 5406), (0x924, 5406), (0x926, 5387), (0x928, 5397),
    (0x92A, 16353),
];

#[rustfmt::skip]
const EMULATOR: &[(u16, u64)] = &[
    (0x200, 2182), (0x201, 2312), (0x202, 2585), (0x203, 2182), (0x204, 2221), (0x205, 2416),
    (0x206, 6550), (0x207, 2221), (0x208, 2247), (0x209, 2598), (0x20A, 2247), (0x20B, 2247),
    (0x20C, 5575), (0x20D, 9280), (0x20E, 5965), (0x20F, 5575), (0x210, 6069), (0x211, 11815),
    (0x212, 10385), (0x213, 6069), (0x214, 6173), (0x215, 16560), (0x216, 6173), (0x217, 6173),
    (0x218, 9956), (0x219, 17769), (0x21A, 10359), (0x21B, 9956), (0x21C, 10944),
    (0x21D, 23047), (0x21E, 15273), (0x21F, 10944), (0x220, 11165), (0x221, 33031),
    (0x222, 11165), (0x223, 11165), (0xAC0, 9956), (0xAC2, 17951), (0xAC4, 9956),
    (0xAC6, 17951), (0xAC8, 9956), (0xACA, 17951), (0xACC, 9956), (0xACE, 17951),
    (0xAD0, 16001), (0xAD2, 23996), (0xAD4, 16001), (0xAD6, 23996), (0xAD8, 10944),
    (0xADA, 23346), (0xADC, 10944), (0xADE, 23346), (0xAE0, 10944), (0xAE2, 23346),
    (0xAE4, 10944), (0xAE6, 23346), (0xAE8, 30275), (0xAEA, 42677), (0xAEC, 30275),
    (0xAEE, 42677), (0x8E0, 5575), (0x8E2, 5575), (0x8E4, 5575), (0x8E6, 5575), (0x8E8, 15390),
    (0x8EA, 25140), (0x8EC, 5575), (0x8EE, 5575), (0x8F0, 5575), (0x8F2, 12088),
    (0x8F4, 25218), (0x8F6, 44705), (0x8F8, 5575), (0x8FA, 5575), (0x8FC, 5575),
    (0x8FE, 25218), (0x900, 44861), (0x902, 45303), (0x904, 5575), (0x906, 5575),
    (0x908, 5575), (0x90A, 5575), (0x90C, 5575), (0x90E, 5575), (0x910, 5575), (0x912, 5575),
    (0x914, 5575), (0x916, 5575), (0x918, 5575), (0x91A, 5575), (0x91C, 5575), (0x91E, 5575),
    (0x920, 5575), (0x922, 5575), (0x924, 5575), (0x926, 5575), (0x928, 5575),
    (0x92A, 5575),
];

fn xy(x: u32, y: u32) -> u32 {
    (y << 16) | x
}

/// A row: its record id, its display/environment words, its list.
struct Row {
    id: u16,
    group: &'static str,
    gp1: u32,
    env: Vec<u32>,
    list: Vec<Vec<u32>>,
}

fn open_env(e1: u32, e2: u32) -> Vec<u32> {
    vec![
        0xe100_0000 | e1,
        0xe200_0000 | e2,
        0xe300_0000,
        0xe400_0000 | 1023 | (511 << 10),
        0xe500_0000,
        0xe600_0000,
    ]
}

/// v2.1 records 0x200 to 0x223 (gpu_probes.rs `push_texture_state`).
fn rows_v21() -> Vec<Row> {
    let page_a = (768 / 64) | (1 << 4);
    let (clut_a, clut_b) = ((336 << 6) | 48, (337 << 6) | 48);
    let mut rows = Vec::new();
    let mut id = 0x200;
    for (w, h) in [(8u32, 8u32), (16, 32), (32, 32)] {
        for depth in [0u32, 1 << 7, 2 << 7] {
            for state in 0..4 {
                let mut list = Vec::new();
                for i in 0..16u32 {
                    let (x, y) = (640, 400 + (i & 7));
                    let odd = i & 1 == 1;
                    let page = if state == 1 && odd {
                        page_a + 1
                    } else {
                        page_a
                    };
                    let clut = if state == 2 && odd { clut_b } else { clut_a };
                    let (u0, v0) = if state == 3 {
                        ((i & 3) * 8, ((i >> 2) & 1) * 8)
                    } else {
                        (0, 0)
                    };
                    list.push(vec![
                        0x2480_8080,
                        xy(x, y),
                        (clut << 16) | (v0 << 8) | u0,
                        xy(x + w, y),
                        ((page | depth) << 16) | (v0 << 8) | (u0 + w),
                        xy(x, y + h),
                        ((v0 + h) << 8) | u0,
                    ]);
                }
                rows.push(Row {
                    id,
                    group: "v2.1",
                    gp1: 0x0800_0001,
                    env: open_env(page_a, 0),
                    list,
                });
                id += 1;
            }
        }
    }
    rows
}

fn uv_page_word(depth: u32) -> u32 {
    let (x, y) = match depth {
        0 => (640, 256),
        1 => (512, 256),
        _ => (768, 256),
    };
    (x / 64) | ((y / 256) << 4) | (depth << 7)
}

fn uv_clut(depth: u32) -> u32 {
    match depth {
        0 => (200 << 6) | (512 / 16),
        1 => (202 << 6) | (512 / 16),
        _ => 0,
    }
}

/// v2.4 records 0x8E0 to 0x92A (workload.rs `uv_windows`), in 640 x 480i.
fn rows_v24() -> Vec<Row> {
    let mut spec = Vec::new();
    for depth in 0..3u32 {
        for span in [8u32, 16, 32, 64, 128, 255] {
            spec.push((depth, span, 0u32, 0u32, 0u32));
        }
    }
    for depth in 0..3u32 {
        for (u, v) in [(8u32, 0u32), (0, 8), (100, 100), (223, 223)] {
            spec.push((depth, 32, u, v, 0));
        }
    }
    for depth in [0u32, 2] {
        for window in [1u32, 2, 4, 8] {
            spec.push((depth, 63, 0, 0, window));
        }
    }
    let mut id = 0x8e0;
    spec.into_iter()
        .map(|(depth, s, u0, v0, window)| {
            let mut list = Vec::new();
            for i in 0..16u32 {
                let (x, y) = (640, 100 + (i & 7));
                list.push(vec![
                    0x2480_8080,
                    xy(x, y),
                    (uv_clut(depth) << 16) | (v0 << 8) | u0,
                    xy(x + 32, y),
                    (uv_page_word(depth) << 16) | (v0 << 8) | (u0 + s),
                    xy(x, y + 32),
                    ((v0 + s) << 8) | u0,
                ]);
            }
            let mask = if window == 0 {
                0
            } else {
                let m = 32 - window;
                m | (m << 5)
            };
            let row = Row {
                id,
                group: "v2.4",
                gp1: 0x0800_0027,
                env: open_env(0, mask),
                list,
            };
            id += 2;
            row
        })
        .collect()
}

/// v2.26 records 0xAC0 to 0xAEE (workload.rs `texture_refill`).
fn rows_v226() -> Vec<Row> {
    let mut rows = Vec::new();
    let mut id = 0xac0;
    for depth in 0..2u32 {
        for span in [16u32, 32, 64] {
            for fresh in [false, true] {
                for alternate in [false, true] {
                    let page_a = uv_page_word(depth);
                    let page_b = page_a + if depth == 0 { 1 } else { 4 };
                    let across = 256 / span;
                    let mut list = Vec::new();
                    for i in 0..8u32 {
                        let (x, y) = (640, 100 + (i & 7));
                        let page = if alternate && i & 1 == 0 {
                            page_b
                        } else {
                            page_a
                        };
                        let (u0, v0) = if fresh {
                            ((i % across) * span, (i / across) * span)
                        } else {
                            (0, 0)
                        };
                        let (u1, v1) = (u0 + span - 1, v0 + span - 1);
                        list.push(vec![
                            0x2c80_8080,
                            xy(x, y),
                            (uv_clut(depth) << 16) | (v0 << 8) | u0,
                            xy(x + 32, y),
                            (page << 16) | (v0 << 8) | u1,
                            xy(x, y + 32),
                            (v1 << 8) | u0,
                            xy(x + 32, y + 32),
                            (v1 << 8) | u1,
                        ]);
                    }
                    rows.push(Row {
                        id,
                        group: "v2.26",
                        gp1: 0x0800_0001,
                        env: open_env(0, 0),
                        list,
                    });
                    id += 2;
                }
            }
        }
    }
    rows
}

fn all_rows() -> Vec<Row> {
    let mut rows = rows_v21();
    rows.extend(rows_v24());
    rows.extend(rows_v226());
    rows
}

/// The row's list cost, the median of five runs, under `law`.
fn measure(row: &Row, law: TextureLaw) -> u64 {
    let mut gpu = Gpu::new();
    gpu.texture_law = law;
    gpu.write32(GP1_ADDR, 0x0300_0000);
    gpu.write32(GP1_ADDR, row.gp1);
    let total = |gpu: &Gpu| gpu.gp0_timing_histogram().iter().sum::<u64>();
    let mut samples = [0u64; 5];
    for sample in &mut samples {
        for &word in &row.env {
            gpu.write32(GP0_ADDR, word);
        }
        let before = total(&gpu);
        for packet in &row.list {
            for &word in packet {
                gpu.write32(GP0_ADDR, word);
            }
        }
        *sample = total(&gpu) - before;
    }
    samples.sort_unstable();
    samples[2]
}

fn lookup(table: &[(u16, u64)], id: u16) -> u64 {
    table.iter().find(|(i, _)| *i == id).map_or(0, |(_, v)| *v)
}

/// Every row's console median and the two laws' costs (`old`, `new`), each
/// law's replay moved by its group's offset: the median of the twin run over
/// the default law's replay (the kick and timer reads the replay leaves out).
fn table() -> Vec<(u16, &'static str, i64, i64, i64)> {
    let rows = all_rows();
    let old: Vec<u64> = rows
        .iter()
        .map(|r| measure(r, TextureLaw::Pixels))
        .collect();
    let new: Vec<u64> = rows
        .iter()
        .map(|r| measure(r, TextureLaw::Texels))
        .collect();
    let mut by_group = std::collections::BTreeMap::<&str, Vec<i64>>::new();
    for (row, &replay) in rows.iter().zip(&old) {
        by_group
            .entry(row.group)
            .or_default()
            .push(lookup(EMULATOR, row.id) as i64 - replay as i64);
    }
    let offset: std::collections::BTreeMap<&str, i64> = by_group
        .into_iter()
        .map(|(group, mut v)| {
            v.sort_unstable();
            (group, v[v.len() / 2])
        })
        .collect();
    rows.iter()
        .zip(old.iter().zip(&new))
        .map(|(row, (&o, &n))| {
            let at = offset[row.group];
            (
                row.id,
                row.group,
                lookup(CONSOLE, row.id) as i64,
                o as i64 + at,
                n as i64 + at,
            )
        })
        .collect()
}

/// Summed absolute errors against the console, by group: `(old, new)`.
fn error_sums(table: &[(u16, &'static str, i64, i64, i64)]) -> Vec<(&'static str, i64, i64)> {
    let mut sums = std::collections::BTreeMap::<&str, (i64, i64)>::new();
    for &(_, group, console, old, new) in table {
        let entry = sums.entry(group).or_default();
        entry.0 += (old - console).abs();
        entry.1 += (new - console).abs();
    }
    sums.into_iter().map(|(g, (o, n))| (g, o, n)).collect()
}

/// `cargo test -p emulator-core --lib fit_table -- --ignored --nocapture`
/// prints every row: console, default law, texel law, and their errors.
#[test]
#[ignore]
fn fit_table() {
    let table = table();
    println!("id   group  console    old    new |  old-err  new-err");
    for &(id, group, console, old, new) in &table {
        println!(
            "{id:03X}  {group:6} {console:7} {old:6} {new:6} | {:8} {:8}",
            old - console,
            new - console
        );
    }
    for (group, old, new) in error_sums(&table) {
        println!("sum |error| {group}: old {old} new {new}");
    }
}

/// The texel law is closer to the console than the default law on every
/// group of rows; and on each v2.26 refill row the texture cost (over the
/// depth's span-16, same-texels, one-page row, which leaves out the per-pixel
/// read both laws share) is within a tenth of the console's, plus 200 clocks.
#[test]
fn texel_law_tracks_the_console_rows() {
    let table = table();
    for (group, old, new) in error_sums(&table) {
        assert!(new < old, "{group}: texel law error {new}, default {old}");
    }
    let base = |id: u16| {
        let first = if id < 0xad8 { 0xac0 } else { 0xad8 };
        table
            .iter()
            .find(|row| row.0 == first)
            .map(|row| (row.2, row.4))
    };
    for &(id, group, console, _, new) in &table {
        if group != "v2.26" {
            continue;
        }
        let (console_base, new_base) = base(id).expect("base row");
        let (console_cost, new_cost) = (console - console_base, new - new_base);
        assert!(
            (new_cost - console_cost).abs() <= console_cost.abs() / 10 + 200,
            "{id:03X}: texture cost {new_cost} against the console's {console_cost}"
        );
    }
}

/// `PSOXIDE_TEXTURE_LAW` names the law; anything but `texels` (or `cache`)
/// keeps the default.
#[test]
fn texture_law_spec_parses() {
    assert_eq!(TextureLaw::parse(None), TextureLaw::Pixels);
    assert_eq!(TextureLaw::parse(Some("")), TextureLaw::Pixels);
    assert_eq!(TextureLaw::parse(Some("pixels")), TextureLaw::Pixels);
    assert_eq!(TextureLaw::parse(Some("bogus")), TextureLaw::Pixels);
    assert_eq!(TextureLaw::parse(Some("texels")), TextureLaw::Texels);
    assert_eq!(TextureLaw::parse(Some("cache")), TextureLaw::Texels);
}
