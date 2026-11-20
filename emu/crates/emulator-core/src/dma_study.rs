//! Lane ps1-dma: where a frame's CPU cycles go while the GPU list walk runs.
//!
//! Measurement only. `PSOXIDE_DMA_STUDY=FILE` turns the study on; it needs a profile range file
//! (`PSOXIDE_LIMIT_PROFILE`). `PSOXIDE_DMA_CLASSES=FILE` names code ranges that are waits (lines
//! `START END CLASS`, hex, CLASS one of `gpu`, `vblank`, `other`, `instr`); `PSOXIDE_DMA_POLLMAP=FILE`
//! names polled data addresses (lines `START END CLASS`, physical hex) so a spin loop on a RAM flag
//! can be told apart. It never changes a cycle.
//!
//! Every CPU instruction's charged cycles go into one cell of a table: the bus state at the start
//! of the instruction (idle, list walk moving, list walk stalled, GPU drawing with no DMA, GPU
//! block DMA, MDEC, CD, SPU, OTC/PIO DMA) against the CPU state. The CPU state comes from the code
//! ranges and from a spin-loop detector: a loop of at most 16 instructions with a taken backward
//! branch, no store, and the same load address every iteration, from the second iteration on, is a
//! spin; its class follows what it polls. Frames are cut at display flips (a GP1(05h) that changes
//! the display start); a frame of `LOAD_VBLANKS` vblanks or more is a load and is left out of the
//! gameplay figures.

use crate::limits::RangeSet;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;

/// Bus states.
pub const STATES: usize = 9;
/// CPU classes (raw).
pub const CLASSES: usize = 10;
const STATE_NAMES: [&str; STATES] = [
    "no DMA, GPU idle",
    "list walk moving nodes",
    "list walk stalled (FIFO full)",
    "GPU drawing, no DMA",
    "GPU block DMA",
    "MDEC DMA",
    "CD DMA",
    "SPU DMA",
    "OTC/PIO DMA",
];
const CLASS_NAMES: [&str; CLASSES] = [
    "working",
    "sym wait GPU",
    "sym wait vblank",
    "sym other wait",
    "gate instr",
    "spin GPU/DMA reg",
    "spin timer/IRQ reg",
    "spin other MMIO",
    "spin RAM flag",
    "spin no load",
];
const SPIN_GPU: usize = 5;
const SPIN_TIMER: usize = 6;
const SPIN_MMIO: usize = 7;
const SPIN_RAM: usize = 8;
const SPIN_NONE: usize = 9;
/// Merged CPU states: working, waiting for the walk or the GPU, waiting for vblank, other wait,
/// gate instrumentation, spin on a RAM flag not mapped to anything.
const MERGED: usize = 5;
const MERGED_NAMES: [&str; MERGED] = [
    "working",
    "wait walk/GPU",
    "wait vblank",
    "other wait",
    "gate instr",
];
// A spin on a RAM address that nothing maps to a wait (HL's and OoT's are compute loops that
// re-read one value) stays working.
const MERGE_OF: [usize; CLASSES] = [0, 1, 2, 3, 4, 1, 2, 3, 0, 3];
const WORK: usize = 0;
const GPUWAIT: usize = 1;
const VBLANK: usize = 2;
const CLOCK: u64 = 33_868_800;
/// A frame this long (in vblanks) or longer is a load, not gameplay.
const LOAD_VBLANKS: u64 = 24;

fn gpu_active(state: usize) -> bool {
    (1..=4).contains(&state)
}

#[derive(Clone, Default, Debug)]
struct Frame {
    cells: [[u64; CLASSES]; STATES],
    cycles: u64,
    vblanks: u64,
}

impl Frame {
    fn merged(&self, group: usize) -> u64 {
        (0..CLASSES)
            .filter(|c| MERGE_OF[*c] == group)
            .map(|c| self.cells.iter().map(|row| row[c]).sum::<u64>())
            .sum()
    }
    fn state_sum(&self, state: usize) -> u64 {
        self.cells[state].iter().sum()
    }
    fn gpu_active(&self) -> u64 {
        (0..STATES)
            .filter(|s| gpu_active(*s))
            .map(|s| self.state_sum(s))
            .sum()
    }
    fn is_load(&self) -> bool {
        self.vblanks >= LOAD_VBLANKS
    }
}

#[derive(Clone, Default, Debug)]
struct Walk {
    framed: bool,
    frame: usize,
    start_off: u64,
    end_off: u64,
    moving: u64,
    stalled: u64,
    drain: u64,
    in_drain: bool,
    cpu: [u64; CLASSES],
}

#[derive(Clone, Default, Debug)]
struct LoopStat {
    iters: u64,
    cycles: u64,
    class: usize,
    addr: u32,
    episodes: u64,
    func: Option<usize>,
}

/// The study's counters.
#[derive(Default, Debug)]
pub struct DmaStudy {
    path: Option<PathBuf>,
    funcs: Vec<String>,
    classes: RangeSet,
    class_of: Vec<u8>,
    pollmap: RangeSet,
    pollmap_class: Vec<u8>,
    /// Set once the report has been written.
    pub written: bool,
    cells: [[u64; CLASSES]; STATES],
    changed_cycles: u64,
    total: u64,
    instrs: u64,
    frame: Frame,
    frame_open: bool,
    frame_vbl0: u64,
    frames: Vec<Frame>,
    last_start: Option<(u16, u16)>,
    walk: Option<Walk>,
    walks: Vec<Walk>,
    prev_f: Option<usize>,
    // spin detector
    iter_cells: [[u64; CLASSES]; STATES],
    iter_store0: u64,
    iter_load0: u64,
    prev_key: (u32, u32),
    prev_addr: u32,
    consec: u32,
    loops: HashMap<(u32, u32), LoopStat>,
    in_spin_of: Option<(u32, u32)>,
    // RAM accesses: [no walk moving, list walk moving]
    loads: [u64; 2],
    stores: [u64; 2],
    extra_load: u64,
    extra_store: u64,
    zero_wait_load: u64,
    zero_wait_store: u64,
    block_loads: u64,
    block_extra: u64,
    // Tax per merged CPU class: (accesses under a walk, extra clocks)
    class_tax: [(u64, u64); MERGED],
    // Per function (last slot = outside every function): loads, stores, extra load, extra store
    func_tax: Vec<[u64; 4]>,
    func_cls: Vec<[u64; CLASSES]>,
    polls: Vec<[u64; 2]>,
    kicks: Vec<u64>,
}

fn class_from_name(name: &str) -> u8 {
    match name {
        "gpu" => 1,
        "vblank" => 2,
        "other" => 3,
        "instr" => 4,
        _ => 0,
    }
}

impl DmaStudy {
    /// A study that writes `path`, with the function names of the profile set, the code class set
    /// and the polled-address class set.
    pub fn new(path: PathBuf, funcs: Vec<String>, classes: RangeSet, pollmap: RangeSet) -> Self {
        let n = funcs.len() + 1;
        let class_of = classes
            .ranges()
            .iter()
            .map(|r| class_from_name(&r.name))
            .collect();
        let pollmap_class = pollmap
            .ranges()
            .iter()
            .map(|r| match r.name.as_str() {
                "gpu" => SPIN_GPU as u8,
                "vblank" => SPIN_TIMER as u8,
                _ => SPIN_MMIO as u8,
            })
            .collect();
        Self {
            path: Some(path),
            funcs,
            classes,
            class_of,
            pollmap,
            pollmap_class,
            func_tax: vec![[0; 4]; n],
            func_cls: vec![[0; CLASSES]; n],
            polls: vec![[0; 2]; n],
            kicks: vec![0; n],
            ..Self::default()
        }
    }

    fn slot(&self, f: Option<usize>) -> usize {
        f.unwrap_or(self.funcs.len())
    }

    fn name(&self, slot: usize) -> &str {
        self.funcs.get(slot).map_or("(outside profile)", String::as_str)
    }

    fn class_at(&self, pc: u32) -> usize {
        self.classes
            .find(pc)
            .map_or(WORK, |i| self.class_of[i] as usize)
    }

    fn spin_class(&self, addr: u32, loads: u64) -> usize {
        if loads == 0 {
            return SPIN_NONE;
        }
        let phys = addr & 0x1fff_ffff;
        if let Some(i) = self.pollmap.find(phys) {
            return self.pollmap_class[i] as usize;
        }
        match phys {
            0x1f80_1810..=0x1f80_1817 | 0x1f80_10a0..=0x1f80_10af | 0x1f80_10f0..=0x1f80_10f7 => {
                SPIN_GPU
            }
            0x1f80_1070..=0x1f80_1077 | 0x1f80_1100..=0x1f80_112f => SPIN_TIMER,
            0x1f80_1000..=0x1f80_2fff => SPIN_MMIO,
            _ => SPIN_RAM,
        }
    }

    /// One CPU RAM access (`store` or load) while `moving` says a list walk is moving nodes.
    pub fn ram_access(&mut self, pc: u32, f: Option<usize>, store: bool, moving: bool, wait: u32) {
        let m = usize::from(moving);
        let wait = u64::from(wait);
        if store {
            self.stores[m] += 1;
        } else {
            self.loads[m] += 1;
        }
        if !moving {
            return;
        }
        let class = MERGE_OF[self.class_at(pc)];
        self.class_tax[class].0 += 1;
        self.class_tax[class].1 += wait;
        let slot = self.slot(f);
        if store {
            self.extra_store += wait;
            self.zero_wait_store += u64::from(wait == 0);
            self.func_tax[slot][1] += 1;
            self.func_tax[slot][3] += wait;
        } else {
            self.extra_load += wait;
            self.zero_wait_load += u64::from(wait == 0);
            self.func_tax[slot][0] += 1;
            self.func_tax[slot][2] += wait;
        }
    }

    /// A CPU load that paid the GPU block-DMA read wait.
    pub fn block_access(&mut self, wait: u32) {
        self.block_loads += 1;
        self.block_extra += u64::from(wait);
    }

    /// A poll read of GPUSTAT (0) or DMA channel 2's CHCR (1).
    pub fn poll(&mut self, f: Option<usize>, kind: usize) {
        let slot = self.slot(f);
        self.polls[slot][kind] += 1;
    }

    /// One instruction retired. `next_pc` is the PC after it; `stores`, `loads` and `last_load`
    /// are the running CPU store and load counts and the last load address.
    #[allow(clippy::too_many_arguments)]
    pub fn account(
        &mut self,
        pc: u32,
        next_pc: u32,
        f: Option<usize>,
        before: u8,
        after: u8,
        charged: u64,
        start: (u16, u16),
        vblanks: u64,
        stores: u64,
        loads: u64,
        last_load: u32,
    ) {
        let class = self.class_at(pc);
        let s = usize::from(before);
        self.cells[s][class] += charged;
        self.total += charged;
        self.instrs += 1;
        if before != after {
            self.changed_cycles += charged;
        }
        let slot = self.slot(f);
        self.func_cls[slot][class] += charged;
        let frame_before = self.frame.cycles;
        if self.frame_open {
            self.frame.cells[s][class] += charged;
            self.frame.cycles += charged;
        }
        self.iter_cells[s][class] += charged;
        self.walk_step(s, class, charged, frame_before);
        self.prev_f = f;

        // Spin detector: a taken backward jump of at most 16 instructions closes an iteration.
        if next_pc != pc.wrapping_add(4) && next_pc <= pc && pc - next_pc <= 60 {
            let key = (next_pc, pc);
            let no_store = stores == self.iter_store0;
            let iter_loads = loads - self.iter_load0;
            let addr = if iter_loads > 0 { last_load } else { 0 };
            if no_store && key == self.prev_key && addr == self.prev_addr {
                self.consec += 1;
            } else {
                self.consec = u32::from(no_store);
            }
            self.prev_key = key;
            self.prev_addr = addr;
            if self.consec >= 2 {
                let cls = self.spin_class(addr, iter_loads);
                for st in 0..STATES {
                    for c in 0..CLASSES {
                        let x = self.iter_cells[st][c];
                        if x == 0 || c == cls {
                            continue;
                        }
                        self.cells[st][c] -= x.min(self.cells[st][c]);
                        self.cells[st][cls] += x;
                        if self.frame_open {
                            self.frame.cells[st][c] -= x.min(self.frame.cells[st][c]);
                            self.frame.cells[st][cls] += x;
                        }
                        if let Some(w) = self.walk.as_mut() {
                            w.cpu[c] = w.cpu[c].saturating_sub(x);
                            w.cpu[cls] += x;
                        }
                    }
                }
                let total: u64 = self.iter_cells.iter().flatten().sum();
                let ent = self.loops.entry(key).or_default();
                ent.iters += 1;
                ent.cycles += total;
                ent.class = cls;
                ent.addr = addr;
                ent.func = f;
                if self.in_spin_of != Some(key) {
                    ent.episodes += 1;
                    self.in_spin_of = Some(key);
                }
            }
            self.iter_cells = [[0; CLASSES]; STATES];
            self.iter_store0 = stores;
            self.iter_load0 = loads;
        }

        if let Some(last) = self.last_start {
            if last != start {
                if self.frame_open {
                    let mut done = std::mem::take(&mut self.frame);
                    done.vblanks = vblanks - self.frame_vbl0;
                    self.frames.push(done);
                }
                self.frame_open = true;
                self.frame_vbl0 = vblanks;
                self.consec = 0;
                self.iter_cells = [[0; CLASSES]; STATES];
                self.iter_store0 = stores;
                self.iter_load0 = loads;
            }
        }
        self.last_start = Some(start);
    }

    fn finish_walk(&mut self) {
        if let Some(w) = self.walk.take() {
            self.walks.push(w);
        }
    }

    fn walk_step(&mut self, s: usize, class: usize, charged: u64, frame_before: u64) {
        let active = s == 1 || s == 2;
        if let Some(w) = self.walk.as_mut() {
            if active && w.in_drain {
                self.finish_walk();
            } else if !active && !w.in_drain {
                w.in_drain = true;
                w.end_off = frame_before;
            }
        }
        if active && self.walk.is_none() {
            let slot = self.slot(self.prev_f);
            self.kicks[slot] += 1;
            self.walk = Some(Walk {
                framed: self.frame_open,
                frame: self.frames.len(),
                start_off: frame_before,
                ..Walk::default()
            });
        }
        let mut finish = false;
        if let Some(w) = self.walk.as_mut() {
            w.cpu[class] += charged;
            if active {
                if s == 1 {
                    w.moving += charged;
                } else {
                    w.stalled += charged;
                }
            } else if s == 3 {
                w.drain += charged;
            } else {
                finish = true;
            }
        }
        if finish {
            self.finish_walk();
        }
    }

    fn table(
        o: &mut String,
        title: &str,
        cells: &[[u64; CLASSES]; STATES],
        total: u64,
        merged: bool,
    ) {
        let pct = |x: u64, d: u64| if d == 0 { 0.0 } else { 100.0 * x as f64 / d as f64 };
        let _ = writeln!(o, "\n{title}");
        let ncol = if merged { MERGED } else { CLASSES };
        let names: Vec<&str> = if merged {
            MERGED_NAMES.to_vec()
        } else {
            CLASS_NAMES.to_vec()
        };
        let val = |row: &[u64; CLASSES], col: usize| -> u64 {
            (0..CLASSES)
                .filter(|c| if merged { MERGE_OF[*c] == col } else { *c == col })
                .map(|c| row[c])
                .sum()
        };
        let _ = write!(o, "{:30}", "");
        for n in &names {
            let _ = write!(o, "{n:>26}");
        }
        let _ = writeln!(o, "{:>16}{:>9}", "row total", "% window");
        for (s, row) in cells.iter().enumerate() {
            let t: u64 = row.iter().sum();
            let _ = write!(o, "{:30}", STATE_NAMES[s]);
            for c in 0..ncol {
                let v = val(row, c);
                let _ = write!(o, "{:>16} {:>8.3}%", v, pct(v, total));
            }
            let _ = writeln!(o, "{t:>16}{:>8.3}%", pct(t, total));
        }
        let _ = write!(o, "{:30}", "column total");
        for c in 0..ncol {
            let t: u64 = cells.iter().map(|r| val(r, c)).sum();
            let _ = write!(o, "{:>16} {:>8.3}%", t, pct(t, total));
        }
        let _ = writeln!(o, "{total:>16}");
    }

    /// The text report.
    pub fn render(&mut self, window: u64, vblank_period: u64) -> String {
        self.finish_walk();
        let all_n = self.frames.len();
        let mut o = String::new();
        let pct = |x: u64, d: u64| if d == 0 { 0.0 } else { 100.0 * x as f64 / d as f64 };
        let _ = writeln!(o, "window_cycles {window}");
        let _ = writeln!(o, "attributed_cycles {}", self.total);
        let _ = writeln!(o, "unattributed_cycles {}", window as i64 - self.total as i64);
        let _ = writeln!(o, "instructions {}", self.instrs);
        let _ = writeln!(
            o,
            "cycles_of_instructions_whose_bus_state_changed {} ({:.3}% of window)",
            self.changed_cycles,
            pct(self.changed_cycles, self.total)
        );
        let _ = writeln!(o, "vblank_period {vblank_period}");
        let fcycles: u64 = self.frames.iter().map(|f| f.cycles).sum();
        let fvbl: u64 = self.frames.iter().map(|f| f.vblanks).sum();
        let loads: Vec<&Frame> = self.frames.iter().filter(|f| f.is_load()).collect();
        let _ = writeln!(
            o,
            "complete_frames {all_n} cycles {fcycles} vblanks {fvbl} fps_by_cycles {:.3} fps_by_vblanks {:.3}; load frames (>= {LOAD_VBLANKS} vblanks) {} with {} cycles",
            all_n as f64 * CLOCK as f64 / fcycles.max(1) as f64,
            all_n as f64 * CLOCK as f64 / (fvbl.max(1) * vblank_period) as f64,
            loads.len(),
            loads.iter().map(|f| f.cycles).sum::<u64>()
        );
        Self::table(
            &mut o,
            "== A. whole window: bus state (rows) x CPU state (columns), cycles and % of window (raw classes)",
            &self.cells,
            self.total,
            false,
        );
        Self::table(
            &mut o,
            "== A. whole window, merged CPU states",
            &self.cells,
            self.total,
            true,
        );

        // Gameplay frames only.
        let play: Vec<&Frame> = self.frames.iter().filter(|f| !f.is_load()).collect();
        let n = play.len();
        let mut gcells = [[0u64; CLASSES]; STATES];
        let mut gtotal = 0;
        for f in &play {
            gtotal += f.cycles;
            for s in 0..STATES {
                for c in 0..CLASSES {
                    gcells[s][c] += f.cells[s][c];
                }
            }
        }
        let gvbl: u64 = play.iter().map(|f| f.vblanks).sum();
        let _ = writeln!(
            o,
            "\ngameplay frames {n}: {gtotal} cycles, {gvbl} vblanks, fps by vblanks {:.3}",
            n as f64 * CLOCK as f64 / (gvbl.max(1) * vblank_period) as f64
        );
        Self::table(
            &mut o,
            "== A. gameplay frames only (load frames and the partial ends left out), raw classes",
            &gcells,
            gtotal,
            false,
        );
        Self::table(
            &mut o,
            "== A. gameplay frames only, merged CPU states",
            &gcells,
            gtotal,
            true,
        );

        let _ = writeln!(o, "\n== A per gameplay frame: mean / min / max cycles");
        let stat = |vals: Vec<u64>| -> (u64, u64, u64) {
            if vals.is_empty() {
                return (0, 0, 0);
            }
            let s: u64 = vals.iter().sum();
            (
                s / vals.len() as u64,
                *vals.iter().min().unwrap(),
                *vals.iter().max().unwrap(),
            )
        };
        let (m, lo, hi) = stat(play.iter().map(|f| f.cycles).collect());
        let _ = writeln!(o, "{:52} {m:>12} {lo:>12} {hi:>12}", "frame length");
        for s in 0..STATES {
            let (m, lo, hi) = stat(play.iter().map(|f| f.state_sum(s)).collect());
            if gcells[s].iter().sum::<u64>() > 0 {
                let _ = writeln!(
                    o,
                    "{:52} {m:>12} {lo:>12} {hi:>12}",
                    format!("bus: {}", STATE_NAMES[s])
                );
            }
        }
        for g in 0..MERGED {
            let (m, lo, hi) = stat(play.iter().map(|f| f.merged(g)).collect());
            let _ = writeln!(
                o,
                "{:52} {m:>12} {lo:>12} {hi:>12}",
                format!("cpu: {}", MERGED_NAMES[g])
            );
        }
        let cell = |f: &Frame, s: usize, g: usize| -> u64 {
            (0..CLASSES)
                .filter(|c| MERGE_OF[*c] == g)
                .map(|c| f.cells[s][c])
                .sum()
        };
        for s in 0..STATES {
            for g in 0..MERGED {
                let tot: u64 = play.iter().map(|f| cell(f, s, g)).sum();
                if tot == 0 {
                    continue;
                }
                let (m, lo, hi) = stat(play.iter().map(|f| cell(f, s, g)).collect());
                let _ = writeln!(
                    o,
                    "{:52} {m:>12} {lo:>12} {hi:>12}",
                    format!("cell {} | {}", STATE_NAMES[s], MERGED_NAMES[g])
                );
            }
        }
        let (m, lo, hi) = stat(play.iter().map(|f| f.gpu_active()).collect());
        let _ = writeln!(
            o,
            "{:52} {m:>12} {lo:>12} {hi:>12}",
            "GPU active (bus states 1 to 4)"
        );

        let _ = writeln!(o, "\n== C4 gameplay frame length in vblanks (flip to flip)");
        let mut hist = std::collections::BTreeMap::new();
        for f in &play {
            *hist.entry(f.vblanks).or_insert(0u64) += 1;
        }
        for (k, v) in &hist {
            let _ = writeln!(o, "{k} vblanks: {v} frames ({:.1}%)", pct(*v, n as u64));
        }
        let (m, lo, hi) = stat(play.iter().map(|f| f.merged(VBLANK)).collect());
        let _ = writeln!(
            o,
            "vblank-wait cycles per frame (symbol waits and spins on the timer/IRQ registers): mean {m} min {lo} max {hi} (mean {:.2}% of frame)",
            pct(m, gtotal / n.max(1) as u64)
        );
        // Busy time: the frame without the vblank wait and the gate's own instrumentation.
        let busy_of = |f: &Frame| f.cycles - f.merged(VBLANK) - f.merged(4);
        let (m, lo, hi) = stat(play.iter().map(|f| busy_of(f)).collect());
        let _ = writeln!(o, "busy time per frame (frame - vblank wait - gate instr): mean {m} min {lo} max {hi}");
        let mut bcells = [[0u64; CLASSES]; STATES];
        let mut btotal = 0;
        for f in &play {
            btotal += busy_of(f);
            for s in 0..STATES {
                for c in 0..CLASSES {
                    if MERGE_OF[c] != VBLANK && MERGE_OF[c] != 4 {
                        bcells[s][c] += f.cells[s][c];
                    }
                }
            }
        }
        Self::table(
            &mut o,
            "== A. gameplay frames, busy time only (vblank wait and gate instrumentation taken out), merged",
            &bcells,
            btotal,
            true,
        );
        let wait_g: u64 = play.iter().map(|f| f.merged(GPUWAIT)).sum();
        let gact: u64 = play.iter().map(|f| f.gpu_active()).sum();
        let _ = writeln!(
            o,
            "C3 arithmetic (upper bound, not a run): wait-on-walk/GPU cycles {wait_g} = {:.3}% of busy time {btotal}; if all of it became useful work the busy time per frame falls {:.3}% and the rate rises {:.3}% (fluid, no vblank rounding); GPU-active cycles are {:.2}% of busy time, so the floor is not reached",
            pct(wait_g, btotal),
            pct(wait_g, btotal),
            100.0 * (btotal as f64 / (btotal - wait_g).max(1) as f64 - 1.0),
            pct(gact, btotal)
        );

        let _ = writeln!(
            o,
            "\n== B. the tax (CPU main-RAM accesses; extra clocks = list-walk spacing wait over the idle-bus cost), whole window"
        );
        let _ = writeln!(
            o,
            "RAM loads: no walk moving {} / under a moving walk {}",
            self.loads[0], self.loads[1]
        );
        let _ = writeln!(
            o,
            "RAM stores: no walk moving {} / under a moving walk {}",
            self.stores[0], self.stores[1]
        );
        let _ = writeln!(
            o,
            "under a moving walk: loads {} paid {} extra clocks ({:.2} a load; {} paid nothing), stores {} paid {} ({:.2} a store; {} paid nothing)",
            self.loads[1],
            self.extra_load,
            self.extra_load as f64 / self.loads[1].max(1) as f64,
            self.zero_wait_load,
            self.stores[1],
            self.extra_store,
            self.extra_store as f64 / self.stores[1].max(1) as f64,
            self.zero_wait_store
        );
        let extra = self.extra_load + self.extra_store;
        let _ = writeln!(
            o,
            "extra clocks total {extra} = {:.3}% of window cycles, {:.0} per complete frame ({all_n} frames)",
            pct(extra, self.total),
            extra as f64 / all_n.max(1) as f64
        );
        let _ = writeln!(
            o,
            "GPU block DMA read wait: {} loads paid {} extra clocks",
            self.block_loads, self.block_extra
        );
        for (c, (acc, ex)) in self.class_tax.iter().enumerate() {
            let _ = writeln!(
                o,
                "tax paid by code in cpu state {}: {acc} accesses, {ex} extra clocks",
                MERGED_NAMES[c]
            );
        }
        let _ = writeln!(
            o,
            "top 15 functions by extra clocks (loads_under stores_under extra_load extra_store total):"
        );
        let mut order: Vec<usize> = (0..self.func_tax.len()).collect();
        order.sort_by_key(|i| std::cmp::Reverse(self.func_tax[*i][2] + self.func_tax[*i][3]));
        for i in order.iter().take(15) {
            let t = self.func_tax[*i];
            let _ = writeln!(
                o,
                "{} {} {} {} {} total {}",
                self.name(*i),
                t[0],
                t[1],
                t[2],
                t[3],
                t[2] + t[3]
            );
        }

        let _ = writeln!(
            o,
            "\n== spin loops found (head tail class polled-address iterations cycles episodes function), by cycles"
        );
        let mut lrows: Vec<(&(u32, u32), &LoopStat)> = self.loops.iter().collect();
        lrows.sort_by_key(|r| std::cmp::Reverse(r.1.cycles));
        for ((h, t), l) in lrows.iter().take(25) {
            let _ = writeln!(
                o,
                "{h:08x} {t:08x} {} addr {:08x} iters {} cycles {} episodes {} in {}",
                CLASS_NAMES[l.class],
                l.addr,
                l.iters,
                l.cycles,
                l.episodes,
                self.name(self.slot(l.func))
            );
        }

        let _ = writeln!(
            o,
            "\n== where the CPU is not working: top functions by non-working cycles (raw class: cycles)"
        );
        let mut rows: Vec<(u64, usize, usize)> = Vec::new();
        for (i, cls) in self.func_cls.iter().enumerate() {
            for c in 1..CLASSES {
                if cls[c] > 0 {
                    rows.push((cls[c], i, c));
                }
            }
        }
        rows.sort_by_key(|r| std::cmp::Reverse(r.0));
        for (cyc, i, c) in rows.iter().take(20) {
            let _ = writeln!(o, "{} [{}] {cyc}", self.name(*i), CLASS_NAMES[*c]);
        }
        let _ = writeln!(o, "\n== poll reads (GPUSTAT, DMA2 CHCR) by function");
        let mut prow: Vec<usize> = (0..self.polls.len()).collect();
        prow.sort_by_key(|i| std::cmp::Reverse(self.polls[*i][0] + self.polls[*i][1]));
        for i in prow.iter().take(12) {
            let p = self.polls[*i];
            if p[0] + p[1] == 0 {
                continue;
            }
            let _ = writeln!(o, "{} gpustat {} chcr2 {}", self.name(*i), p[0], p[1]);
        }

        let _ = writeln!(
            o,
            "\n== D. walks (trace): kicks by the function holding the CHCR store (whole window)"
        );
        let mut krow: Vec<usize> = (0..self.kicks.len()).collect();
        krow.sort_by_key(|i| std::cmp::Reverse(self.kicks[*i]));
        for i in krow.iter().take(8) {
            if self.kicks[*i] > 0 {
                let _ = writeln!(o, "{} {}", self.name(*i), self.kicks[*i]);
            }
        }
        let walks: Vec<&Walk> = self
            .walks
            .iter()
            .filter(|w| w.framed && w.frame < all_n && !self.frames[w.frame].is_load())
            .collect();
        let nw = walks.len();
        let _ = writeln!(
            o,
            "walks in the window {}, started in gameplay frames {nw}",
            self.walks.len()
        );
        let mut per_frame = vec![0u64; all_n];
        for w in &walks {
            per_frame[w.frame] += 1;
        }
        let mut wh = std::collections::BTreeMap::new();
        for (i, c) in per_frame.iter().enumerate() {
            if !self.frames[i].is_load() {
                *wh.entry(*c).or_insert(0u64) += 1;
            }
        }
        for (k, v) in &wh {
            let _ = writeln!(
                o,
                "{k} walks started in the frame: {v} frames ({:.1}%)",
                pct(*v, n as u64)
            );
        }
        let mean = |f: &dyn Fn(&Walk) -> u64| -> f64 {
            walks.iter().map(|w| f(w)).sum::<u64>() as f64 / nw.max(1) as f64
        };
        let _ = writeln!(
            o,
            "per walk mean cycles: moving {:.0} stalled {:.0} drain after walk {:.0}",
            mean(&|w| w.moving),
            mean(&|w| w.stalled),
            mean(&|w| w.drain)
        );
        for g in 0..MERGED {
            let _ = writeln!(
                o,
                "CPU in {} while a walk runs or drains: mean {:.0} cycles per walk",
                MERGED_NAMES[g],
                mean(&|w| (0..CLASSES)
                    .filter(|c| MERGE_OF[*c] == g)
                    .map(|c| w.cpu[c])
                    .sum())
            );
        }
        let _ = writeln!(
            o,
            "walk start offset in the frame (cycles from the flip): {}",
            Self::spread(walks.iter().map(|w| w.start_off).collect())
        );
        let _ = writeln!(
            o,
            "walk end offset (list read done): {}",
            Self::spread(walks.iter().map(|w| w.end_off).collect())
        );
        let _ = writeln!(
            o,
            "walk length moving+stalled: {}",
            Self::spread(walks.iter().map(|w| w.moving + w.stalled).collect())
        );
        let _ = writeln!(
            o,
            "walk moving part: {}",
            Self::spread(walks.iter().map(|w| w.moving).collect())
        );
        let _ = writeln!(
            o,
            "drain after walk: {}",
            Self::spread(walks.iter().map(|w| w.drain).collect())
        );
        let _ = writeln!(
            o,
            "gameplay frame length: {}",
            Self::spread(play.iter().map(|f| f.cycles).collect())
        );
        o
    }

    fn spread(mut v: Vec<u64>) -> String {
        if v.is_empty() {
            return "none".into();
        }
        v.sort_unstable();
        let p = |q: usize| v[(v.len() - 1) * q / 100];
        format!(
            "n {} min {} p10 {} p50 {} p90 {} max {} mean {}",
            v.len(),
            v[0],
            p(10),
            p(50),
            p(90),
            v[v.len() - 1],
            v.iter().sum::<u64>() / v.len() as u64
        )
    }

    /// Where to write the report.
    pub fn path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }
}
