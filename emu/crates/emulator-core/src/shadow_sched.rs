// SPDX-License-Identifier: GPL-2.0-or-later
//! Load-shadow scheduling study (lane ps1-sched): how much of the load-shadow
//! ceiling a legal reordering of instructions inside straight-line blocks can
//! reach. Opt-in through `PSOXIDE_SHADOW_SCHED=FILE`; inert otherwise.
//!
//! The collector (`Collector`) runs inside the census hook and records every
//! distinct straight-line block of the window with its execution count. The
//! analysis half (`analyse`) is offline: `examples/shadow_sched.rs` reads the
//! dump, builds each block's dependency graph, and searches for the order
//! that hides the most instructions under the emulator's own load-shadow
//! rule (`Cpu::hides_in_load_shadow`), applied block-locally.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;

/// One executed instruction: PC, word, class byte (access class in the low
/// three bits as in `shadow_census::Access`, bit 3 set when the instruction
/// opened a load shadow).
pub type Item = (u32, u32, u8);

/// Bit of the class byte: the instruction opened a load shadow.
pub const OPENED: u8 = 8;

/// A distinct block with how often it ran.
#[derive(Clone, Debug)]
pub struct Block {
    /// Executions in the window.
    pub count: u64,
    /// Destination register of a load-delayed write issued by the instruction
    /// executed right before the block (0 if none).
    pub entry_delayed: u8,
    /// The instructions.
    pub items: Vec<Item>,
}

/// Collects blocks while the census is live.
#[derive(Debug, Default)]
pub struct Collector {
    cur: Vec<Item>,
    entry: u8,
    last_delayed: u8,
    last_pc: u32,
    after_branch: bool,
    closed: bool,
    blocks: HashMap<u128, Block>,
    /// Instructions recorded.
    pub instrs: u64,
    /// Block counter (the block being collected).
    pub serial: u64,
    /// Hidden instructions whose shadow was opened in an earlier block.
    pub cross_hidden: u64,
}

fn is_branch(w: u32) -> bool {
    match w >> 26 {
        1..=7 => true,
        0 => matches!(w & 0x3f, 8 | 9),
        _ => false,
    }
}

/// Destination GPR of a load-delayed write (loads, MFC0, MFC2, CFC2), else 0.
pub fn delayed_dest(w: u32) -> u8 {
    let rt = ((w >> 16) & 31) as u8;
    match w >> 26 {
        0x20..=0x26 => rt,
        0x10 | 0x12 if (w >> 21) & 31 == 0 => rt,
        0x12 if (w >> 21) & 31 == 2 => rt,
        _ => 0,
    }
}

fn key_of(entry: u8, items: &[Item]) -> u128 {
    let mut a: u64 = 0xcbf2_9ce4_8422_2325 ^ u64::from(entry);
    let mut b: u64 = 0x9e37_79b9_7f4a_7c15;
    for &(pc, w, c) in items {
        for v in [u64::from(pc), u64::from(w), u64::from(c)] {
            a = (a ^ v).wrapping_mul(0x0000_0100_0000_01b3);
            b = (b.rotate_left(23) ^ v).wrapping_mul(0xff51_afd7_ed55_8ccd);
            b ^= b >> 29;
        }
    }
    (u128::from(a) << 64) | u128::from(b)
}

impl Collector {
    fn flush_cur(&mut self) {
        self.closed = false;
        self.after_branch = false;
        if self.cur.is_empty() {
            return;
        }
        let key = key_of(self.entry, &self.cur);
        match self.blocks.get_mut(&key) {
            Some(b) => b.count += 1,
            None => {
                self.blocks.insert(
                    key,
                    Block {
                        count: 1,
                        entry_delayed: self.entry,
                        items: self.cur.clone(),
                    },
                );
            }
        }
        self.cur.clear();
        self.serial += 1;
    }

    /// An instruction is about to run (counted, outside wait ranges).
    #[inline]
    pub fn push(&mut self, pc: u32, word: u32, class: u8) {
        if self.closed || (!self.cur.is_empty() && pc != self.last_pc.wrapping_add(4)) {
            self.flush_cur();
        }
        if self.cur.is_empty() {
            self.entry = self.last_delayed;
        }
        self.cur.push((pc, word, class));
        self.last_pc = pc;
        self.instrs += 1;
        self.last_delayed = delayed_dest(word);
        if self.after_branch {
            self.closed = true;
        } else if is_branch(word) {
            self.after_branch = true;
        }
    }

    /// The instruction just pushed opened a load shadow.
    #[inline]
    pub fn mark_open(&mut self) {
        if let Some(last) = self.cur.last_mut() {
            last.2 |= OPENED;
        }
    }

    /// An instruction inside a wait range was skipped: the block is cut.
    #[inline]
    pub fn on_wait(&mut self) {
        self.flush_cur();
        self.last_delayed = 0;
    }

    /// Write the block table.
    ///
    /// # Errors
    ///
    /// When the file cannot be written.
    pub fn dump(&mut self, path: &Path) -> std::io::Result<usize> {
        self.flush_cur();
        let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
        out.write_all(b"SCHB1\0\0\0")?;
        out.write_all(&(self.blocks.len() as u64).to_le_bytes())?;
        for b in self.blocks.values() {
            out.write_all(&b.count.to_le_bytes())?;
            out.write_all(&[b.entry_delayed, 0, 0, 0])?;
            out.write_all(&(b.items.len() as u32).to_le_bytes())?;
            for &(pc, w, c) in &b.items {
                out.write_all(&pc.to_le_bytes())?;
                out.write_all(&w.to_le_bytes())?;
                out.write_all(&[c])?;
            }
        }
        out.flush()?;
        let mut info = path.as_os_str().to_owned();
        info.push(".info");
        std::fs::write(
            info,
            format!("cross_hidden {}\ninstrs {}\n", self.cross_hidden, self.instrs),
        )?;
        Ok(self.blocks.len())
    }
}

/// Read a block table written by [`Collector::dump`].
///
/// # Errors
///
/// When the file is missing or malformed.
pub fn load(path: &Path) -> std::io::Result<Vec<Block>> {
    let mut data = Vec::new();
    std::fs::File::open(path)?.read_to_end(&mut data)?;
    let bad = || std::io::Error::new(std::io::ErrorKind::InvalidData, "bad block table");
    if data.len() < 16 || &data[..5] != b"SCHB1" {
        return Err(bad());
    }
    let n = u64::from_le_bytes(data[8..16].try_into().map_err(|_| bad())?) as usize;
    let mut at = 16;
    let mut out = Vec::with_capacity(n);
    let u32at = |d: &[u8], i: usize| u32::from_le_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]]);
    for _ in 0..n {
        if at + 16 > data.len() {
            return Err(bad());
        }
        let count = u64::from_le_bytes(data[at..at + 8].try_into().map_err(|_| bad())?);
        let entry_delayed = data[at + 8];
        let len = u32at(&data, at + 12) as usize;
        at += 16;
        if at + 9 * len > data.len() {
            return Err(bad());
        }
        let mut items = Vec::with_capacity(len);
        for _ in 0..len {
            items.push((u32at(&data, at), u32at(&data, at + 4), data[at + 8]));
            at += 9;
        }
        out.push(Block {
            count,
            entry_delayed,
            items,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Analysis
// ---------------------------------------------------------------------------

const HI: u64 = 1 << 32;
const LO: u64 = 1 << 33;
const GTE: u64 = 1 << 34;

/// What the scheduler may assume.
#[derive(Clone, Copy, Debug, Default)]
pub struct Variant {
    /// Free register renaming: only true (read-after-write) register
    /// dependencies, plus the last writer of each register stays last; the
    /// shadow rule ends only at a real reader of the loaded register.
    pub rename: bool,
    /// GTE and multiply/divide instructions are barriers (stall-neutral).
    pub fence: bool,
    /// Same-base accesses at disjoint offsets do not alias.
    pub disambig: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mem {
    No,
    Load,
    Store,
}

#[derive(Clone, Copy, Debug)]
struct Insn {
    word: u32,
    reads: u64,
    writes: u64,
    barrier: bool,
    trap: bool,
    fenceable: bool,
    mem: Mem,
    /// Spad or I/O load (kept in order against stores and against each other).
    strong: bool,
    io: bool,
    base: u8,
    lo: i32,
    hi: i32,
    delayed: u8,
    opens: bool,
    bus: bool,
    is_load: bool,
    branch: bool,
}

fn gpr(r: u32) -> u64 {
    if r == 0 {
        0
    } else {
        1u64 << r
    }
}

fn decode(word: u32, class: u8) -> Insn {
    let op = word >> 26;
    let rs = (word >> 21) & 31;
    let rt = (word >> 16) & 31;
    let rd = (word >> 11) & 31;
    let funct = word & 63;
    let mut i = Insn {
        word,
        reads: 0,
        writes: 0,
        barrier: false,
        trap: false,
        fenceable: false,
        mem: Mem::No,
        strong: false,
        io: false,
        base: 0,
        lo: 0,
        hi: 0,
        delayed: delayed_dest(word),
        opens: class & OPENED != 0,
        bus: op >= 0x20,
        is_load: false,
        branch: is_branch(word),
    };
    let acc = class & 7;
    match op {
        0 => match funct {
            0 | 2 | 3 => {
                i.reads = gpr(rt);
                i.writes = gpr(rd);
            }
            4 | 6 | 7 | 0x21 | 0x23 | 0x24..=0x27 | 0x2a | 0x2b => {
                i.reads = gpr(rs) | gpr(rt);
                i.writes = gpr(rd);
            }
            0x20 | 0x22 => {
                i.reads = gpr(rs) | gpr(rt);
                i.writes = gpr(rd);
                i.trap = true;
            }
            8 => i.reads = gpr(rs),
            9 => {
                i.reads = gpr(rs);
                i.writes = gpr(rd);
            }
            0x10 => {
                i.reads = HI;
                i.writes = gpr(rd);
                i.fenceable = true;
            }
            0x12 => {
                i.reads = LO;
                i.writes = gpr(rd);
                i.fenceable = true;
            }
            0x11 => {
                i.reads = gpr(rs);
                i.writes = HI;
                i.fenceable = true;
            }
            0x13 => {
                i.reads = gpr(rs);
                i.writes = LO;
                i.fenceable = true;
            }
            0x18..=0x1b => {
                i.reads = gpr(rs) | gpr(rt);
                i.writes = HI | LO;
                i.fenceable = true;
            }
            _ => i.barrier = true,
        },
        1 => match rt {
            0 | 1 => i.reads = gpr(rs),
            0x10 | 0x11 => {
                i.reads = gpr(rs);
                i.writes = gpr(31);
            }
            _ => i.barrier = true,
        },
        2 => {}
        3 => i.writes = gpr(31),
        4 | 5 => i.reads = gpr(rs) | gpr(rt),
        6 | 7 => i.reads = gpr(rs),
        8 => {
            i.reads = gpr(rs);
            i.writes = gpr(rt);
            i.trap = true;
        }
        9..=0xe => {
            i.reads = gpr(rs);
            i.writes = gpr(rt);
        }
        0xf => i.writes = gpr(rt),
        0x10 => {
            i.barrier = true;
            if rs == 0 {
                i.writes = gpr(rt);
            }
        }
        0x12 => {
            i.fenceable = true;
            i.reads |= GTE;
            i.writes |= GTE;
            if word & (1 << 25) == 0 {
                match rs {
                    0 | 2 => i.writes |= gpr(rt),
                    4 | 6 => i.reads |= gpr(rt),
                    _ => i.barrier = true,
                }
            }
        }
        0x20..=0x26 => {
            i.reads = gpr(rs);
            if op == 0x22 || op == 0x26 {
                i.reads |= gpr(rt);
            }
            i.writes = gpr(rt);
            i.mem = Mem::Load;
            i.is_load = true;
        }
        0x28 | 0x29 | 0x2a | 0x2b | 0x2e => {
            i.reads = gpr(rs) | gpr(rt);
            i.mem = Mem::Store;
        }
        0x32 => {
            i.reads = gpr(rs) | GTE;
            i.writes = GTE;
            i.mem = Mem::Load;
            i.is_load = true;
            i.fenceable = true;
        }
        0x3a => {
            i.reads = gpr(rs) | GTE;
            i.writes = GTE;
            i.mem = Mem::Store;
            i.fenceable = true;
        }
        _ => i.barrier = true,
    }
    if i.mem != Mem::No {
        i.base = rs as u8;
        let off = word as u16 as i16 as i32;
        let (lo, hi) = match op {
            0x20 | 0x24 | 0x28 => (off, off + 1),
            0x21 | 0x25 | 0x29 => (off, off + 2),
            0x23 | 0x2b | 0x32 | 0x3a => (off, off + 4),
            _ => (off - 3, off + 4),
        };
        i.lo = lo;
        i.hi = hi;
        i.strong = i.mem == Mem::Load && matches!(acc, 3 | 5);
        i.io = matches!(acc, 3..=6);
    }
    i
}

/// A block ready for scheduling.
struct Prep {
    ins: Vec<Insn>,
    /// Instructions free to move: `0..m`; the rest is the fixed tail.
    m: usize,
    preds: Vec<Vec<u16>>,
    succs: Vec<Vec<u16>>,
    /// Longest path to the end of the movable region.
    height: Vec<u32>,
    entry_delayed: u8,
}

fn conflict(
    a: &Insn,
    b: &Insn,
    ia: usize,
    ib: usize,
    ins: &[Insn],
    v: Variant,
    last_writer_of: &[usize; 64],
) -> bool {
    if a.barrier || b.barrier {
        return true;
    }
    if v.fence && (a.fenceable || b.fenceable) {
        return true;
    }
    // Registers.
    let raw = a.writes & b.reads != 0;
    if raw {
        return true;
    }
    if v.rename {
        // Only the final definition of a register is pinned: nothing that reads or
        // writes the old value may follow it.
        let mut w = (a.reads | a.writes) & b.writes & !(HI | LO | GTE);
        while w != 0 {
            let r = w.trailing_zeros() as usize;
            w &= w - 1;
            if last_writer_of[r] == ib {
                return true;
            }
        }
        // HI, LO and GTE state keep their order (no renaming).
        let special = HI | LO | GTE;
        if (a.writes | a.reads) & b.writes & special != 0 || a.writes & b.reads & special != 0 {
            return true;
        }
    } else if a.reads & b.writes != 0 || a.writes & b.writes != 0 {
        return true;
    }
    // Memory.
    if a.mem != Mem::No && b.mem != Mem::No {
        let one_store = a.mem == Mem::Store || b.mem == Mem::Store;
        if one_store || (a.strong && b.strong) {
            let mut alias = true;
            if v.disambig && one_store && a.base == b.base {
                let base_written = (ia..ib).any(|k| ins[k].writes & gpr(u32::from(a.base)) != 0);
                if !base_written && (a.hi <= b.lo || b.hi <= a.lo) {
                    alias = false;
                }
            }
            if alias {
                return true;
            }
        }
    }
    // Instructions that may trap keep their order against stores and I/O.
    let a_sens = a.mem == Mem::Store || a.io || a.trap;
    let b_sens = b.mem == Mem::Store || b.io || b.trap;
    if (a.trap && b_sens) || (b.trap && a_sens) {
        return true;
    }
    false
}

/// Why a block is not reordered at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pinned {
    /// Reordered normally.
    No,
    /// A read of a load-delay slot value (hazard code), or an lwl/lwr pair.
    LoadDelay,
    /// Longer than the supported size.
    TooLong,
}

fn prepare(block: &Block, v: Variant) -> (Prep, Pinned) {
    let ins: Vec<Insn> = block
        .items
        .iter()
        .map(|&(_, w, c)| decode(w, c))
        .collect();
    let n = ins.len();
    // Fixed tail: branch + delay slot, a trailing branch, or a pinned last instruction.
    let tail = if n >= 2 && ins[n - 2].branch {
        2
    } else {
        1
    };
    let m = n.saturating_sub(tail);
    let mut pinned = Pinned::No;
    if n > 4000 {
        pinned = Pinned::TooLong;
    }
    // The original order must be free of load-delay reads.
    let mut prev = block.entry_delayed;
    for x in &ins {
        if prev != 0 && x.reads & gpr(u32::from(prev)) != 0 {
            pinned = Pinned::LoadDelay;
        }
        prev = x.delayed;
    }
    let mut last_writer_of = [usize::MAX; 64];
    for (k, x) in ins.iter().enumerate() {
        let mut w = x.writes;
        while w != 0 {
            let r = w.trailing_zeros() as usize;
            w &= w - 1;
            if r < 64 {
                last_writer_of[r] = k;
            }
        }
    }
    let mut preds = vec![Vec::new(); m];
    let mut succs = vec![Vec::new(); m];
    if pinned == Pinned::No {
        for j in 0..m {
            for i in 0..j {
                if conflict(&ins[i], &ins[j], i, j, &ins, v, &last_writer_of) {
                    preds[j].push(i as u16);
                    succs[i].push(j as u16);
                }
            }
        }
    }
    let mut height = vec![1u32; m];
    for i in (0..m).rev() {
        for &s in &succs[i] {
            height[i] = height[i].max(height[s as usize] + 1);
        }
    }
    (
        Prep {
            ins,
            m,
            preds,
            succs,
            height,
            entry_delayed: block.entry_delayed,
        },
        pinned,
    )
}

/// Does `x` end a shadow opened by a load into `reg`?
#[inline]
fn reads_loaded(x: &Insn, reg: u8, sem: bool) -> bool {
    if reg == 0 {
        return false;
    }
    if sem {
        x.reads & gpr(u32::from(reg)) != 0
    } else {
        let rs = ((x.word >> 21) & 31) as u8;
        let rt = ((x.word >> 16) & 31) as u8;
        rs == reg || rt == reg
    }
}

/// Block-local cost model state.
#[derive(Clone, Copy)]
struct Shadow {
    reg: u8,
    pos: u8,
    open: bool,
}

#[inline]
fn step(sh: &mut Shadow, x: &Insn, sem: bool) -> bool {
    let mut hidden = false;
    if sh.open {
        sh.pos += 1;
        if x.bus || reads_loaded(x, sh.reg, sem) || sh.pos > 6 {
            sh.open = false;
        } else if sh.pos > 2 {
            hidden = true;
        }
    }
    if x.opens {
        *sh = Shadow {
            reg: ((x.word >> 16) & 31) as u8,
            pos: 0,
            open: true,
        };
    }
    hidden
}

/// Hidden instructions of the sequence `order` followed by the fixed tail;
/// `None` when it breaks the load-delay rule.
fn eval(p: &Prep, order: &[u16], sem: bool) -> Option<u32> {
    let mut sh = Shadow {
        reg: 0,
        pos: 0,
        open: false,
    };
    let mut prev = p.entry_delayed;
    let mut hidden = 0;
    let tail = p.ins.len() - p.m;
    let seq = order
        .iter()
        .map(|&k| &p.ins[k as usize])
        .chain(p.ins[p.m..].iter().take(tail));
    for x in seq {
        if prev != 0 && x.reads & gpr(u32::from(prev)) != 0 {
            return None;
        }
        prev = x.delayed;
        hidden += u32::from(step(&mut sh, x, sem));
    }
    Some(hidden)
}

/// How the unused slots of a shadow end, in the best order.
#[derive(Clone, Copy, Debug, Default)]
pub struct Unused {
    /// Another load ended the shadow; no movable instruction that could hide was left in the block.
    pub load_none_left: u64,
    /// Same, hiders were left but none was ready (each waits on something not yet issued).
    pub load_not_ready: u64,
    /// Same, a ready hider existed but the best order spent it elsewhere.
    pub load_ready_unused: u64,
    /// A store ended the shadow: none left.
    pub store_none_left: u64,
    /// A store ended it: hiders left, none ready.
    pub store_not_ready: u64,
    /// A store ended it: a ready hider existed.
    pub store_ready_unused: u64,
    /// Ended by another bus opcode.
    pub other_bus: u64,
    /// Ended by a read of the loaded register (dependent use).
    pub dependent: u64,
    /// Shadow still open at the block's end.
    pub block_edge: u64,
}

impl Unused {
    fn add(&mut self, o: &Unused, w: u64) {
        self.load_none_left += o.load_none_left * w;
        self.load_not_ready += o.load_not_ready * w;
        self.load_ready_unused += o.load_ready_unused * w;
        self.store_none_left += o.store_none_left * w;
        self.store_not_ready += o.store_not_ready * w;
        self.store_ready_unused += o.store_ready_unused * w;
        self.other_bus += o.other_bus * w;
        self.dependent += o.dependent * w;
        self.block_edge += o.block_edge * w;
    }
    /// Sum of all categories.
    pub fn total(&self) -> u64 {
        self.load_none_left
            + self.load_not_ready
            + self.load_ready_unused
            + self.store_none_left
            + self.store_not_ready
            + self.store_ready_unused
            + self.other_bus
            + self.dependent
            + self.block_edge
    }
}

fn classify(p: &Prep, order: &[u16], sem: bool) -> Unused {
    let mut u = Unused::default();
    let m = p.m;
    let tail = p.ins.len() - m;
    let mut pos_of = vec![usize::MAX; m];
    for (i, &k) in order.iter().enumerate() {
        pos_of[k as usize] = i;
    }
    let seq: Vec<(usize, &Insn)> = order
        .iter()
        .map(|&k| (k as usize, &p.ins[k as usize]))
        .chain((m..m + tail).map(|k| (k, &p.ins[k])))
        .collect();
    let mut sh = Shadow {
        reg: 0,
        pos: 0,
        open: false,
    };
    let mut hid_in = 0u64;
    for (idx, &(_, x)) in seq.iter().enumerate() {
        if sh.open {
            let dep = reads_loaded(x, sh.reg, sem);
            let free = 4u64.saturating_sub(hid_in);
            if free > 0 && (x.bus || dep) {
                if x.bus {
                    let mut left = 0;
                    let mut ready = 0;
                    if idx < m {
                        for &(k, y) in &seq[idx + 1..m] {
                            if y.bus || reads_loaded(y, sh.reg, sem) {
                                continue;
                            }
                            left += 1;
                            if p.preds[k].iter().all(|&q| pos_of[q as usize] < idx) {
                                ready += 1;
                            }
                        }
                    }
                    let kind = if left == 0 {
                        0
                    } else if ready == 0 {
                        1
                    } else {
                        2
                    };
                    match (x.mem, kind) {
                        (Mem::Load, 0) => u.load_none_left += free,
                        (Mem::Load, 1) => u.load_not_ready += free,
                        (Mem::Load, _) => u.load_ready_unused += free,
                        (Mem::Store, 0) => u.store_none_left += free,
                        (Mem::Store, 1) => u.store_not_ready += free,
                        (Mem::Store, _) => u.store_ready_unused += free,
                        (Mem::No, _) => u.other_bus += free,
                    }
                } else {
                    u.dependent += free;
                }
            }
        }
        let h = step(&mut sh, x, sem);
        if h {
            hid_in += 1;
        }
        if x.opens {
            hid_in = 0;
        }
    }
    if sh.open {
        u.block_edge += 4u64.saturating_sub(hid_in);
    }
    u
}

fn rng_next(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

/// One randomized list schedule; `None` on a dead end.
fn list_schedule(p: &Prep, seed: &mut u64, sem: bool, noise: u64) -> Option<Vec<u16>> {
    let m = p.m;
    let mut remaining: Vec<usize> = p.preds.iter().map(Vec::len).collect();
    let mut ready: Vec<u16> = (0..m as u16).filter(|&k| remaining[k as usize] == 0).collect();
    let mut order = Vec::with_capacity(m);
    let mut sh = Shadow {
        reg: 0,
        pos: 0,
        open: false,
    };
    let mut prev = p.entry_delayed;
    let rand_pri: Vec<u64> = (0..m).map(|_| rng_next(seed) % (noise + 1)).collect();
    while order.len() < m {
        let mut best: Option<(u64, usize)> = None;
        for (ri, &k) in ready.iter().enumerate() {
            let x = &p.ins[k as usize];
            if prev != 0 && x.reads & gpr(u32::from(prev)) != 0 {
                continue;
            }
            let mut score = u64::from(p.height[k as usize]) * 4 + rand_pri[k as usize];
            if sh.open {
                let next_pos = sh.pos + 1;
                let ends = x.bus || reads_loaded(x, sh.reg, sem) || next_pos > 6;
                if !ends {
                    // A hider (or a paid filler): strongly preferred while a shadow is open.
                    score += 1000;
                    if next_pos > 2 {
                        score += 500;
                    }
                }
            } else if x.opens {
                score += 200;
            }
            if best.is_none_or(|(s, _)| score > s) {
                best = Some((score, ri));
            }
        }
        let (_, ri) = best?;
        let k = ready.swap_remove(ri);
        let x = &p.ins[k as usize];
        prev = x.delayed;
        step(&mut sh, x, sem);
        order.push(k);
        for &s in &p.succs[k as usize] {
            remaining[s as usize] -= 1;
            if remaining[s as usize] == 0 {
                ready.push(s);
            }
        }
    }
    Some(order)
}

fn hill_climb(p: &Prep, order: &mut Vec<u16>, sem: bool, best: &mut u32) {
    let m = p.m;
    let passes = if m > 150 { 2 } else { 8 };
    let mut pos_of = vec![0usize; m];
    for _ in 0..passes {
        let mut improved = false;
        for i in 0..m {
            let k = order[i];
            let mut cand = order.clone();
            cand.remove(i);
            for (idx, &q) in cand.iter().enumerate() {
                pos_of[q as usize] = idx;
            }
            // Insert positions q in lo..=hi keep every dependency.
            let lo = p.preds[k as usize]
                .iter()
                .map(|&q| pos_of[q as usize] + 1)
                .max()
                .unwrap_or(0);
            let hi = p.succs[k as usize]
                .iter()
                .map(|&q| pos_of[q as usize])
                .min()
                .unwrap_or(m - 1);
            let mut local_best = *best;
            let mut local = None;
            for q in lo..=hi {
                if q == i {
                    continue;
                }
                let mut t = cand.clone();
                t.insert(q, k);
                if let Some(h) = eval(p, &t, sem) {
                    if h > local_best {
                        local_best = h;
                        local = Some(t);
                    }
                }
            }
            if let Some(t) = local {
                *order = t;
                *best = local_best;
                improved = true;
            }
        }
        if !improved {
            break;
        }
    }
}

/// Result for one block under one variant.
#[derive(Clone, Debug)]
pub struct BlockResult {
    /// Hidden instructions per execution, original order (the emulator's rule).
    pub orig: u32,
    /// Hidden instructions per execution, best order found.
    pub best: u32,
    /// Found by the exact search.
    pub exact: bool,
    /// When both searches ran: what the heuristic alone reached.
    pub heuristic: Option<u32>,
    /// Why the block was not reordered.
    pub pinned: Pinned,
    /// Unused slots per execution in the best order.
    pub unused: Unused,
    /// Instructions that open a shadow in the block.
    pub openers: u32,
    /// The best order found for the movable instructions (empty: the original).
    pub order: Vec<u16>,
}

fn limit(name: &str, default: usize) -> usize {
    static CACHE: std::sync::OnceLock<(usize, usize, usize)> = std::sync::OnceLock::new();
    let (a, b, c) = *CACHE.get_or_init(|| {
        let get = |n: &str, d: usize| {
            std::env::var(n)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        (
            get("SCHED_EXACT_MAX", 30),
            get("SCHED_EXACT_STATES", 3_000_000),
            get("SCHED_BEAM", 8000),
        )
    });
    let _ = default;
    match name {
        "max" => a,
        "states" => b,
        _ => c,
    }
}

struct Exact<'a> {
    p: &'a Prep,
    pmask: Vec<u32>,
    memo: HashMap<u64, i16>,
    sem: bool,
    over: bool,
}

impl Exact<'_> {
    fn key(set: u32, prev: u8, sh: Shadow) -> u64 {
        let (reg, pos) = if sh.open {
            (u64::from(sh.reg), u64::from(sh.pos) + 1)
        } else {
            (0, 0)
        };
        u64::from(set) | u64::from(prev) << 32 | reg << 37 | pos << 42
    }

    fn solve(&mut self, set: u32, prev: u8, sh: Shadow) -> i16 {
        let m = self.p.m;
        if set.count_ones() as usize == m {
            // Run the fixed tail from this state.
            let mut sh = sh;
            let mut prev = prev;
            let mut hidden = 0i16;
            for x in &self.p.ins[m..] {
                if prev != 0 && x.reads & gpr(u32::from(prev)) != 0 {
                    return -1;
                }
                prev = x.delayed;
                hidden += i16::from(step(&mut sh, x, self.sem));
            }
            return hidden;
        }
        let key = Self::key(set, prev, sh);
        if let Some(&v) = self.memo.get(&key) {
            return v;
        }
        if self.memo.len() > limit("states", 0) {
            self.over = true;
            return -1;
        }
        let mut best = -1i16;
        for k in 0..m {
            if set & (1 << k) != 0 || self.pmask[k] & !set != 0 {
                continue;
            }
            let x = &self.p.ins[k];
            if prev != 0 && x.reads & gpr(u32::from(prev)) != 0 {
                continue;
            }
            let mut sh2 = sh;
            let h = i16::from(step(&mut sh2, x, self.sem));
            let r = self.solve(set | 1 << k, x.delayed, sh2);
            if self.over {
                return -1;
            }
            if r >= 0 {
                best = best.max(r + h);
            }
        }
        self.memo.insert(key, best);
        best
    }

    /// Walk the memo to an order that reaches `target`.
    fn rebuild(&mut self, target: i16) -> Vec<u16> {
        let m = self.p.m;
        let mut set = 0u32;
        let mut prev = self.p.entry_delayed;
        let mut sh = Shadow {
            reg: 0,
            pos: 0,
            open: false,
        };
        let mut left = target;
        let mut order = Vec::with_capacity(m);
        while order.len() < m {
            let mut chosen = None;
            for k in 0..m {
                if set & (1 << k) != 0 || self.pmask[k] & !set != 0 {
                    continue;
                }
                let x = self.p.ins[k];
                if prev != 0 && x.reads & gpr(u32::from(prev)) != 0 {
                    continue;
                }
                let mut sh2 = sh;
                let h = i16::from(step(&mut sh2, &x, self.sem));
                let r = self.solve(set | 1 << k, x.delayed, sh2);
                if r >= 0 && r + h == left {
                    chosen = Some((k, h, sh2, x.delayed));
                    break;
                }
            }
            let Some((k, h, sh2, d)) = chosen else {
                break;
            };
            order.push(k as u16);
            set |= 1 << k;
            left -= h;
            sh = sh2;
            prev = d;
        }
        order
    }
}

fn exact_search(p: &Prep, sem: bool) -> Option<(u32, Vec<u16>)> {
    let mut pmask = vec![0u32; p.m];
    for (j, ps) in p.preds.iter().enumerate() {
        for &i in ps {
            pmask[j] |= 1 << i;
        }
    }
    let mut ex = Exact {
        p,
        pmask,
        memo: HashMap::new(),
        sem,
        over: false,
    };
    let sh = Shadow {
        reg: 0,
        pos: 0,
        open: false,
    };
    let r = ex.solve(0, p.entry_delayed, sh);
    if ex.over || r < 0 {
        return None;
    }
    let order = ex.rebuild(r);
    if order.len() != p.m {
        return None;
    }
    Some((r as u32, order))
}


/// Beam search over (scheduled set, previous load-delay register, shadow
/// state): the width best states per level survive. `None` when the block
/// has more than 256 movable instructions.
fn beam_search(p: &Prep, sem: bool, width: usize) -> Option<(u32, Vec<u16>)> {
    let m = p.m;
    if m > 256 {
        return None;
    }
    #[derive(Clone, Copy)]
    struct St {
        set: [u64; 4],
        prev: u8,
        sh: Shadow,
        hidden: u32,
        hiders_left: u32,
        parent: u32,
        pick: u16,
    }
    let mut pm = vec![[0u64; 4]; m];
    for (j, ps) in p.preds.iter().enumerate() {
        for &i in ps {
            pm[j][(i / 64) as usize] |= 1 << (i % 64);
        }
    }
    let hiders = p.ins[..m].iter().filter(|x| !x.bus).count() as u32;
    let start = St {
        set: [0; 4],
        prev: p.entry_delayed,
        sh: Shadow {
            reg: 0,
            pos: 0,
            open: false,
        },
        hidden: 0,
        hiders_left: hiders,
        parent: 0,
        pick: 0,
    };
    let mut levels: Vec<Vec<St>> = vec![vec![start]];
    for _ in 0..m {
        let cur = levels.last()?;
        let mut next: HashMap<(u64, u64, u64, u64, u8, u8, u8), usize> = HashMap::new();
        let mut out: Vec<St> = Vec::new();
        for (si, st) in cur.iter().enumerate() {
            for k in 0..m {
                if st.set[k / 64] >> (k % 64) & 1 != 0 {
                    continue;
                }
                let q = &pm[k];
                if (0..4).any(|w| q[w] & !st.set[w] != 0) {
                    continue;
                }
                let x = &p.ins[k];
                if st.prev != 0 && x.reads & gpr(u32::from(st.prev)) != 0 {
                    continue;
                }
                let mut sh = st.sh;
                let h = u32::from(step(&mut sh, x, sem));
                let mut set = st.set;
                set[k / 64] |= 1 << (k % 64);
                let (r, ps) = if sh.open { (sh.reg, sh.pos + 1) } else { (0, 0) };
                let key = (set[0], set[1], set[2], set[3], x.delayed, r, ps);
                let ns = St {
                    set,
                    prev: x.delayed,
                    sh,
                    hidden: st.hidden + h,
                    hiders_left: st.hiders_left - u32::from(!x.bus),
                    parent: si as u32,
                    pick: k as u16,
                };
                match next.get(&key) {
                    Some(&i) => {
                        if out[i].hidden < ns.hidden {
                            out[i] = ns;
                        }
                    }
                    None => {
                        next.insert(key, out.len());
                        out.push(ns);
                    }
                }
            }
        }
        if out.is_empty() {
            return None;
        }
        let score = |s: &St| -> u32 {
            let bonus = if s.sh.open {
                (6 - u32::from(s.sh.pos)).min(s.hiders_left)
            } else {
                0
            };
            s.hidden * 16 + bonus
        };
        out.sort_by_key(|s| std::cmp::Reverse(score(s)));
        out.truncate(width);
        levels.push(out);
    }
    // Finish with the fixed tail.
    let last = levels.last()?;
    let mut best: Option<(u32, usize)> = None;
    for (i, st) in last.iter().enumerate() {
        let mut sh = st.sh;
        let mut prev = st.prev;
        let mut hid = st.hidden;
        let mut ok = true;
        for x in &p.ins[m..] {
            if prev != 0 && x.reads & gpr(u32::from(prev)) != 0 {
                ok = false;
                break;
            }
            prev = x.delayed;
            hid += u32::from(step(&mut sh, x, sem));
        }
        if ok && best.is_none_or(|(b, _)| hid > b) {
            best = Some((hid, i));
        }
    }
    let (hid, mut at) = best?;
    let mut order = vec![0u16; m];
    for lv in (1..=m).rev() {
        let st = &levels[lv][at];
        order[lv - 1] = st.pick;
        at = st.parent as usize;
    }
    Some((hid, order))
}

fn heuristic(p: &Prep, sem: bool, restarts: u32, salt: u64) -> (u32, Vec<u16>) {
    let orig_order: Vec<u16> = (0..p.m as u16).collect();
    let mut h_best = eval(p, &orig_order, sem).unwrap_or(0);
    let mut h_order = orig_order;
    let mut seed = 0x1234_5678_9abc_def1u64 ^ salt;
    let k = if p.m > 150 { restarts / 4 } else { restarts };
    for r in 0..k.max(1) {
        let noise = [0u64, 2, 8, 30][(r % 4) as usize];
        if let Some(mut o) = list_schedule(p, &mut seed, sem, noise) {
            if let Some(mut h) = eval(p, &o, sem) {
                hill_climb(p, &mut o, sem, &mut h);
                if h > h_best {
                    h_best = h;
                    h_order = o;
                }
            }
        }
    }
    // Beam search over schedule states.
    if let Some((h, o)) = beam_search(p, sem, limit("beam", 0)) {
        if h > h_best && eval(p, &o, sem) == Some(h) {
            h_best = h;
            h_order = o;
        }
    }
    // Climb from the best order found (including the original).
    let mut o = h_order.clone();
    let mut h = h_best;
    hill_climb(p, &mut o, sem, &mut h);
    if h > h_best {
        h_best = h;
        h_order = o;
    }
    (h_best, h_order)
}

/// Analyse one block.
pub fn analyse_block(block: &Block, v: Variant, restarts: u32) -> BlockResult {
    let (p, pinned) = prepare(block, v);
    let orig_order: Vec<u16> = (0..p.m as u16).collect();
    let orig = eval(&p, &orig_order, false).unwrap_or(0);
    let openers = p.ins.iter().filter(|x| x.opens).count() as u32;
    let sem = v.rename;
    let mut res = BlockResult {
        orig,
        best: orig,
        exact: false,
        heuristic: None,
        pinned,
        unused: Unused::default(),
        openers,
        order: Vec::new(),
    };
    if pinned != Pinned::No || openers == 0 || p.m < 2 {
        res.unused = classify(&p, &orig_order, false);
        res.exact = true;
        return res;
    }
    // Upper bound on hidden instructions: four per shadow, and the
    // instructions that can hide at all.
    let hiders = p.ins.iter().filter(|x| !x.bus).count() as u32;
    let ub = (4 * openers).min(hiders);
    let sem_orig = eval(&p, &orig_order, sem).unwrap_or(0);
    if sem_orig.max(orig) >= ub {
        res.best = sem_orig.max(orig);
        res.exact = true;
        res.unused = classify(&p, &orig_order, sem);
        return res;
    }
    let salt = u64::from(block.items[0].0) << 7 ^ block.items.len() as u64;
    if p.m <= limit("max", 0).min(31) {
        if let Some((e, order)) = exact_search(&p, sem) {
            res.exact = true;
            res.best = e.max(orig);
            if e >= orig {
                res.unused = classify(&p, &order, sem);
                res.order = order;
            } else {
                res.unused = classify(&p, &orig_order, false);
            }
            // The heuristic's gap to the exact value, measured on a quarter of the blocks.
            if salt % 4 == 0 {
                res.heuristic = Some(heuristic(&p, sem, restarts, salt).0.max(orig));
            }
            return res;
        }
    }
    let (h, order) = heuristic(&p, sem, restarts, salt);
    res.best = h.max(orig);
    if h >= orig {
        res.order = order.clone();
    }
    res.unused = if h >= orig {
        classify(&p, &order, sem)
    } else {
        classify(&p, &orig_order, false)
    };
    res
}

// ---------------------------------------------------------------------------
// Semantic check: run the original and the reordered block on random states
// ---------------------------------------------------------------------------

fn mix(a: u32, b: u32) -> u32 {
    let mut x = u64::from(a) << 32 | u64::from(b);
    x = x.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    x ^= x >> 29;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    (x ^ (x >> 32)) as u32
}

#[derive(Clone, PartialEq, Debug)]
struct Machine {
    regs: [u32; 32],
    hi: u32,
    lo: u32,
    g: u32,
    mem: HashMap<u32, u32>,
    pending: Option<(u8, u32)>,
}

impl Machine {
    fn word(&self, a: u32) -> u32 {
        let k = a & 0xffff_fffc;
        self.mem.get(&k).copied().unwrap_or_else(|| mix(k, 0x5eed))
    }

    fn run(&mut self, w: u32, pc: u32) {
        let op = w >> 26;
        let rs = ((w >> 21) & 31) as usize;
        let rt = ((w >> 16) & 31) as usize;
        let rd = ((w >> 11) & 31) as usize;
        let sh = (w >> 6) & 31;
        let imm = w as u16 as i16 as i32 as u32;
        let immz = w & 0xffff;
        let committing = self.pending.take();
        let (a, b) = (self.regs[rs], self.regs[rt]);
        let mut write: Option<(usize, u32)> = None;
        let mut load: Option<(usize, u32)> = None;
        let addr = a.wrapping_add(imm);
        match op {
            0 => {
                let v = match w & 63 {
                    0 => Some(b << sh),
                    2 => Some(b >> sh),
                    3 => Some(((b as i32) >> sh) as u32),
                    4 => Some(b << (a & 31)),
                    6 => Some(b >> (a & 31)),
                    7 => Some(((b as i32) >> (a & 31)) as u32),
                    9 => Some(pc.wrapping_add(8)),
                    0x10 => Some(self.hi),
                    0x12 => Some(self.lo),
                    0x20 | 0x21 => Some(a.wrapping_add(b)),
                    0x22 | 0x23 => Some(a.wrapping_sub(b)),
                    0x24 => Some(a & b),
                    0x25 => Some(a | b),
                    0x26 => Some(a ^ b),
                    0x27 => Some(!(a | b)),
                    0x2a => Some(u32::from((a as i32) < (b as i32))),
                    0x2b => Some(u32::from(a < b)),
                    _ => None,
                };
                match w & 63 {
                    0x11 => self.hi = a,
                    0x13 => self.lo = a,
                    0x18 => {
                        let p = i64::from(a as i32) * i64::from(b as i32);
                        self.hi = (p >> 32) as u32;
                        self.lo = p as u32;
                    }
                    0x19 => {
                        let p = u64::from(a) * u64::from(b);
                        self.hi = (p >> 32) as u32;
                        self.lo = p as u32;
                    }
                    0x1a => {
                        if b == 0 {
                            self.lo = if (a as i32) < 0 { 1 } else { u32::MAX };
                            self.hi = a;
                        } else {
                            self.lo = (a as i32).wrapping_div(b as i32) as u32;
                            self.hi = (a as i32).wrapping_rem(b as i32) as u32;
                        }
                    }
                    0x1b => {
                        if b == 0 {
                            self.lo = u32::MAX;
                            self.hi = a;
                        } else {
                            self.lo = a / b;
                            self.hi = a % b;
                        }
                    }
                    0xc | 0xd => self.g = mix(self.g, w),
                    _ => {}
                }
                if let Some(v) = v {
                    write = Some((rd, v));
                }
            }
            1 => {
                if rt >= 0x10 {
                    write = Some((31, pc.wrapping_add(8)));
                }
            }
            3 => write = Some((31, pc.wrapping_add(8))),
            8 | 9 => write = Some((rt, a.wrapping_add(imm))),
            0xa => write = Some((rt, u32::from((a as i32) < (imm as i32)))),
            0xb => write = Some((rt, u32::from(a < imm))),
            0xc => write = Some((rt, a & immz)),
            0xd => write = Some((rt, a | immz)),
            0xe => write = Some((rt, a ^ immz)),
            0xf => write = Some((rt, immz << 16)),
            0x10 => {
                if rs == 0 {
                    load = Some((rt, mix(self.g, w)));
                } else {
                    self.g = mix(self.g, w);
                }
            }
            0x12 => {
                if w & (1 << 25) != 0 {
                    self.g = mix(self.g, w);
                } else {
                    match rs {
                        0 | 2 => load = Some((rt, mix(self.g, w))),
                        4 | 6 => self.g = mix(self.g ^ b, w),
                        _ => self.g = mix(self.g, w),
                    }
                }
            }
            0x20 => {
                let v = (self.word(addr) >> (8 * (addr & 3))) & 0xff;
                load = Some((rt, v as u8 as i8 as i32 as u32));
            }
            0x24 => {
                let v = (self.word(addr) >> (8 * (addr & 3))) & 0xff;
                load = Some((rt, v));
            }
            0x21 => {
                let v = (self.word(addr) >> (8 * (addr & 2))) & 0xffff;
                load = Some((rt, v as u16 as i16 as i32 as u32));
            }
            0x25 => {
                let v = (self.word(addr) >> (8 * (addr & 2))) & 0xffff;
                load = Some((rt, v));
            }
            0x22 | 0x26 => load = Some((rt, mix(mix(b, self.word(addr)), addr & 3 | op << 2))),
            0x23 => load = Some((rt, self.word(addr))),
            0x28 => {
                let k = addr & 0xffff_fffc;
                let old = self.word(addr);
                let sh = 8 * (addr & 3);
                self.mem.insert(k, old & !(0xff << sh) | (b & 0xff) << sh);
            }
            0x29 => {
                let k = addr & 0xffff_fffc;
                let old = self.word(addr);
                let sh = 8 * (addr & 2);
                self.mem.insert(k, old & !(0xffff << sh) | (b & 0xffff) << sh);
            }
            0x2a | 0x2e => {
                let k = addr & 0xffff_fffc;
                let old = self.word(addr);
                self.mem.insert(k, mix(mix(old, b), addr & 3 | op << 2));
            }
            0x2b => {
                self.mem.insert(addr & 0xffff_fffc, b);
            }
            0x32 => {
                self.g = mix(self.g, self.word(addr));
            }
            0x3a => {
                let k = addr & 0xffff_fffc;
                self.mem.insert(k, mix(self.g, w));
            }
            _ => self.g = mix(self.g, w),
        }
        let mut squashed = None;
        if let Some((r, v)) = write {
            if r != 0 {
                self.regs[r] = v;
                squashed = Some(r);
            }
        }
        if let Some((r, v)) = committing {
            if r != 0 && squashed != Some(r as usize) {
                self.regs[r as usize] = v;
            }
        }
        if let Some((r, v)) = load {
            if r != 0 {
                self.pending = Some((r as u8, v));
            }
        }
    }
}

/// Run `block` in its original order and with `order` for the movable part on
/// `trials` random initial states; `Err` names the first difference.
///
/// # Errors
///
/// When the two orders end in different machine states.
pub fn validate(block: &Block, order: &[u16], trials: u32) -> Result<(), String> {
    let n = block.items.len();
    let tail = if n >= 2 && is_branch(block.items[n - 2].1) { 2 } else { 1 };
    let m = n.saturating_sub(tail);
    if order.len() != m {
        return Err(format!("order length {} != movable {m}", order.len()));
    }
    let mut seen = vec![false; m];
    for &k in order {
        if seen[k as usize] {
            return Err("duplicate index".into());
        }
        seen[k as usize] = true;
    }
    let mut seed = 0x9e37_79b9_7f4a_7c15u64 ^ u64::from(block.items[0].0);
    for t in 0..trials {
        let mut r = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut regs = [0u32; 32];
        for v in regs.iter_mut().skip(1) {
            let x = r();
            *v = if x & 3 == 0 {
                [0x1000u32, 0x1004, 0x1010, 0x1100][(x >> 8) as usize & 3]
            } else {
                ((x >> 16) as u32) & !3
            };
        }
        let start = Machine {
            regs,
            hi: r() as u32,
            lo: r() as u32,
            g: r() as u32,
            mem: HashMap::new(),
            pending: (block.entry_delayed != 0).then(|| (block.entry_delayed, r() as u32)),
        };
        let start_sp = start.regs[29];
        let mut a = start.clone();
        for &(pc, w, _) in &block.items {
            a.run(w, pc);
        }
        let mut b = start;
        for &k in order {
            let (pc, w, _) = block.items[k as usize];
            b.run(w, pc);
        }
        for &(pc, w, _) in &block.items[m..] {
            b.run(w, pc);
        }
        if a != b {
            let regs: Vec<usize> = (0..32).filter(|&i| a.regs[i] != b.regs[i]).collect();
            let mut mem: Vec<u32> = a
                .mem
                .keys()
                .chain(b.mem.keys())
                .copied()
                .filter(|k| a.mem.get(k) != b.mem.get(k))
                .collect();
            mem.sort_unstable();
            mem.dedup();
            let vals: Vec<(u32, Option<u32>, Option<u32>)> = mem
                .iter()
                .map(|k| (*k, a.mem.get(k).copied(), b.mem.get(k).copied()))
                .collect();
            return Err(format!(
                "trial {t}: sp {:x} differs in regs {regs:?}, hi/lo/g {}, pending {}, mem words {vals:x?}",
                start_sp,
                a.hi != b.hi || a.lo != b.lo || a.g != b.g,
                a.pending != b.pending
            ));
        }
    }
    Ok(())
}

/// Totals over a set of blocks.
#[derive(Clone, Debug, Default)]
pub struct Totals {
    /// Weighted hidden instructions, original order.
    pub orig: u64,
    /// Weighted hidden instructions, best order.
    pub best: u64,
    /// Weighted saving found by the exact search.
    pub saving_exact: u64,
    /// Weighted saving of blocks that were pinned (always 0).
    pub pinned_blocks: u64,
    /// Weighted executions of pinned blocks.
    pub pinned_execs: u64,
    /// Weighted unused slots by reason.
    pub unused: Unused,
    /// Weighted shadows opened.
    pub openers: u64,
    /// Distinct blocks analysed.
    pub blocks: u64,
}

impl Totals {
    /// Fold one block result in.
    pub fn add(&mut self, r: &BlockResult, count: u64) {
        self.orig += u64::from(r.orig) * count;
        self.best += u64::from(r.best) * count;
        if r.exact {
            self.saving_exact += u64::from(r.best - r.orig.min(r.best)) * count;
        }
        if r.pinned != Pinned::No {
            self.pinned_blocks += 1;
            self.pinned_execs += count;
        }
        self.unused.add(&r.unused, count);
        self.openers += u64::from(r.openers) * count;
        self.blocks += 1;
    }
    /// Merge another total.
    pub fn merge(&mut self, o: &Totals) {
        self.orig += o.orig;
        self.best += o.best;
        self.saving_exact += o.saving_exact;
        self.pinned_blocks += o.pinned_blocks;
        self.pinned_execs += o.pinned_execs;
        self.unused.add(&o.unused, 1);
        self.openers += o.openers;
        self.blocks += o.blocks;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lw(rt: u32, rs: u32, off: u32) -> u32 {
        0x23 << 26 | rs << 21 | rt << 16 | off
    }
    fn addiu(rt: u32, rs: u32, imm: u32) -> u32 {
        9 << 26 | rs << 21 | rt << 16 | imm
    }
    fn addu(rd: u32, rs: u32, rt: u32) -> u32 {
        rs << 21 | rt << 16 | rd << 11 | 0x21
    }
    fn sw(rt: u32, rs: u32, off: u32) -> u32 {
        0x2b << 26 | rs << 21 | rt << 16 | off
    }
    fn jr(rs: u32) -> u32 {
        rs << 21 | 8
    }

    fn block(words: &[(u32, u8)]) -> Block {
        Block {
            count: 1,
            entry_delayed: 0,
            items: words
                .iter()
                .enumerate()
                .map(|(i, &(w, c))| (0x8001_0000 + 4 * i as u32, w, c))
                .collect(),
        }
    }

    #[test]
    fn fills_the_shadow() {
        let ram_load = 1 | OPENED;
        let b = block(&[
            (lw(8, 4, 0), ram_load),
            (addiu(10, 10, 1), 0),
            (addu(9, 8, 8), 0),
            (addiu(11, 11, 1), 0),
            (addiu(12, 12, 1), 0),
            (addiu(13, 13, 1), 0),
            (addiu(14, 14, 1), 0),
            (addiu(15, 15, 1), 0),
            (jr(31), 0),
            (0, 0),
        ]);
        let r = analyse_block(&b, Variant::default(), 8);
        assert_eq!(r.orig, 0);
        assert_eq!(r.best, 4);
        assert!(r.exact);
    }

    #[test]
    fn store_blocks_a_load_across_it() {
        let ram_load = 1 | OPENED;
        // The load cannot move above the store to the same base: no gain possible.
        let b = block(&[
            (sw(5, 4, 0), 2),
            (lw(8, 4, 0), ram_load),
            (addu(9, 8, 8), 0),
            (addiu(10, 10, 1), 0),
            (addiu(11, 11, 1), 0),
            (jr(31), 0),
            (0, 0),
        ]);
        let r = analyse_block(&b, Variant::default(), 8);
        assert!(r.best >= r.orig);
    }

    #[test]
    fn collector_cuts_at_delay_slots() {
        let mut c = Collector::default();
        let pcs = [0x8001_0000u32, 0x8001_0004, 0x8001_0008, 0x8002_0000];
        c.push(pcs[0], addiu(8, 8, 1), 0);
        c.push(pcs[1], jr(31), 0);
        c.push(pcs[2], 0, 0);
        c.push(pcs[3], addiu(9, 9, 1), 0);
        c.flush_cur();
        assert_eq!(c.blocks.len(), 2);
    }
}
