// SPDX-License-Identifier: GPL-2.0-or-later
//! Limit-study oracles: opt-in switches that make one cost category free.
//!
//! Running the same gameplay route with one cost removed gives an upper
//! bound on what optimising that category can buy. Every switch is off
//! unless an environment variable names it, and with every switch off the
//! emulator runs exactly as before (no cycle is added or removed).
//!
//! `PSOXIDE_LIMIT_ORACLES` is a comma list of:
//!
//! - `icache`: every cached instruction fetch hits (no refill or streaming
//!   stalls). Uncached fetches (KSEG1, the BIOS ROM) keep their cost.
//! - `ram`: CPU data loads and stores to main RAM cost what the scratchpad
//!   costs (no wait states, refresh, write-buffer or load-shadow effects).
//! - `muldiv`: MULT/DIV results are ready at once (no HI/LO interlock).
//! - `gte`: GTE commands finish at once (no busy interlock).
//! - `gpu`: GPU commands take no drawing time, CPU GP0 stores never wait on
//!   drawing, and GPU DMA (linked list, block, OT clear) completes at once.
//! - `cd`: data reads seek for free and stream sectors 8x faster than double
//!   speed. Not literally instant: the guest's per-sector interrupt handler
//!   has to keep up. XA-audio reads (mode bit 6) keep their pace.
//! - `spram` (ablation, excluded from `all`): scratchpad loads cost what main-RAM
//!   loads cost, so the difference to a plain run is what the guest's current
//!   scratchpad tenants are worth.
//! - `mmio`: CPU reads of GPUSTAT, the DMA, timer and interrupt registers
//!   cost the one issue cycle only.
//!
//! `PSOXIDE_LIMIT_FREE=FILE` names code ranges whose computing takes no time
//! (issue, fetch, RAM, GTE and multiply/divide costs). A hardware access
//! (anything in the I/O area, GP0 stores included) still costs real time
//! from that access on, so a bounded wait or a GPU write inside a free range
//! sees the hardware progress. Interrupt handlers entered from inside the
//! range are charged normally. A free range that spins without touching
//! hardware would never see time pass, so after [`FREE_GUARD`] consecutive
//! free instructions the range is charged again until the CPU leaves it;
//! the trips are counted.
//!
//! `PSOXIDE_LIMIT_WAIT=FILE` names wait loops whose charged cycles are
//! counted (not changed), so a caller can split each frame into work and
//! waiting. `PSOXIDE_LIMIT_PROFILE=FILE` names functions whose charged (and
//! skipped) cycles are totalled per function. Range files hold one range per
//! line: `START END [NAME]` in hex, END exclusive; `#` starts a comment.
//!
//! `PSOXIDE_RAM_DATA_LOG=FILE` writes, per 32-byte main-RAM line, the CPU load
//! stall cycles and load count seen since activation (loads made inside
//! `PSOXIDE_LIMIT_WAIT` ranges are skipped, so a vblank spin does not top the
//! list). `PSOXIDE_LIMIT_FAST_DATA=FILE` names physical main-RAM ranges
//! (`START END [NAME]`) whose CPU loads cost what the scratchpad costs: the
//! "what if the engine kept these bytes in the scratchpad" bound. Both are
//! inert unless set, so a run without them is cycle-identical (checked on a
//! 900-poll Half-Life replay, every cycle-profile bucket of every vblank).
//! `stall-tools hot-lines` turns the log into a range file.
//!
//! `PSOXIDE_LIMIT_FROM_POLL=N` keeps every switch off until the guest has
//! completed N pad polls, so the run is identical to a plain one up to the
//! start of the measured window (loads and menus included).
//! `PSOXIDE_LIMIT_FROM_CYCLE=C` also waits for bus cycle C, for guests that
//! poll the pad during their loads.

use std::path::Path;

/// Cached instruction fetches always hit.
pub const ICACHE: u32 = 1 << 0;
/// Main-RAM data accesses cost like the scratchpad.
pub const RAM: u32 = 1 << 1;
/// Multiply/divide results are ready at once.
pub const MULDIV: u32 = 1 << 2;
/// GTE commands finish at once.
pub const GTE: u32 = 1 << 3;
/// GPU drawing and GPU DMA take no time.
pub const GPU: u32 = 1 << 4;
/// CD data reads seek for free and stream fast.
pub const CD: u32 = 1 << 5;
/// Status-register polling costs one cycle.
pub const MMIO: u32 = 1 << 6;
/// Ablation, not a limit: scratchpad loads cost what main-RAM loads cost.
/// Prices what the scratchpad tenants a guest already has are worth. Not part
/// of `all`.
pub const SPRAM: u32 = 1 << 7;
/// Load-shadow oracle (not part of `all`): nothing hides behind a load.
pub const SHADOWNONE: u32 = 1 << 8;
/// Load-shadow oracle (not part of `all`): reading the loaded register no longer ends a shadow.
pub const SHADOWDEP: u32 = 1 << 9;
/// Load-shadow oracle (not part of `all`): every main-RAM load refunds four issue cycles.
pub const SHADOWMAX: u32 = 1 << 10;
/// Internal: the shadow census is on (`PSOXIDE_SHADOW_CENSUS`). Not an oracle name.
pub const CENSUS: u32 = 1 << 11;
/// Any load-shadow study switch.
pub const SHADOW_STUDY: u32 = SHADOWNONE | SHADOWDEP | SHADOWMAX | CENSUS;

/// Consecutive free instructions after which a free range is charged again.
pub const FREE_GUARD: u64 = 1 << 24;

const NAMES: [(&str, u32); 11] = [
    ("icache", ICACHE),
    ("ram", RAM),
    ("muldiv", MULDIV),
    ("gte", GTE),
    ("gpu", GPU),
    ("cd", CD),
    ("mmio", MMIO),
    ("spram", SPRAM),
    ("shadownone", SHADOWNONE),
    ("shadowdep", SHADOWDEP),
    ("shadowmax", SHADOWMAX),
];

/// One named code range, `start..end`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeRange {
    /// First address in the range.
    pub start: u32,
    /// First address past the range.
    pub end: u32,
    /// Symbol name, or the start address when the file gave none.
    pub name: String,
}

/// Sorted, non-overlapping ranges with a binary-search lookup.
#[derive(Clone, Debug, Default)]
pub struct RangeSet {
    ranges: Vec<CodeRange>,
}

impl RangeSet {
    /// Build from ranges in any order; later overlapping ranges are dropped.
    pub fn new(mut ranges: Vec<CodeRange>) -> Self {
        ranges.retain(|range| range.end > range.start);
        ranges.sort_by_key(|range| range.start);
        let mut kept: Vec<CodeRange> = Vec::with_capacity(ranges.len());
        for range in ranges {
            if kept.last().is_some_and(|last| range.start < last.end) {
                continue;
            }
            kept.push(range);
        }
        Self { ranges: kept }
    }

    /// Parse a range file (see the module docs).
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut ranges = Vec::new();
        for (number, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut fields = line.split_whitespace();
            let hex = |field: Option<&str>| -> Result<u32, String> {
                let field = field.ok_or_else(|| format!("line {}: missing field", number + 1))?;
                u32::from_str_radix(field.trim_start_matches("0x"), 16)
                    .map_err(|error| format!("line {}: {field}: {error}", number + 1))
            };
            let start = hex(fields.next())?;
            let end = hex(fields.next())?;
            let name = fields
                .next()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{start:#010x}"));
            ranges.push(CodeRange { start, end, name });
        }
        Ok(Self::new(ranges))
    }

    fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        Self::parse(&text).map_err(|error| format!("{}: {error}", path.display()))
    }

    /// Index of the range holding `pc`, if any.
    #[inline]
    pub fn find(&self, pc: u32) -> Option<usize> {
        let index = self.ranges.partition_point(|range| range.start <= pc);
        let range = self.ranges.get(index.checked_sub(1)?)?;
        (pc < range.end).then_some(index - 1)
    }

    /// The ranges, sorted by start address.
    pub fn ranges(&self) -> &[CodeRange] {
        &self.ranges
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }
}

/// Oracle configuration and the counters the oracles keep.
///
/// Excluded from save states: a restored bus re-reads the environment.
#[derive(Debug, Default)]
pub struct LimitOracles {
    configured: u32,
    active: u32,
    pending: bool,
    from_poll: u64,
    from_cycle: u64,
    free: RangeSet,
    wait: RangeSet,
    profile: RangeSet,
    /// Lane stall-free: data addresses (physical, main RAM) whose loads cost like the scratchpad.
    fast_data: RangeSet,
    /// Lane stall-free: per 32-byte data line, (load stall cycles, loads), when `PSOXIDE_RAM_DATA_LOG` is set.
    data_log: Option<std::collections::HashMap<u32, (u64, u64)>>,
    track_pc: bool,
    /// Lane stall-free: PC of the instruction being executed (set while PCs are tracked).
    cur_pc: u32,
    /// The instruction being executed is free: the clock does not move.
    frozen: bool,
    free_run: u64,
    guard_escape: bool,
    /// Cycles removed by free ranges since activation.
    pub skipped_cycles: u64,
    /// Cycles charged inside wait ranges since activation.
    pub wait_cycles: u64,
    /// The part of `wait_cycles` that was stalls, when the CPU cycle
    /// profile is on: I-cache refills, RAM loads, RAM stores, MMIO, GTE and
    /// multiply/divide interlocks, in that order. Lets a caller take a stall
    /// category out of the work outside the wait loops only.
    pub wait_stalls: [u64; 6],
    /// Times a free range hit [`FREE_GUARD`].
    pub guard_trips: u64,
    /// Instructions retired inside free ranges since activation.
    pub free_instructions: u64,
    /// Hardware accesses from free ranges, charged in real time.
    pub thawed_accesses: u64,
    /// Per profile range: (charged cycles, skipped cycles, instructions).
    pub profile_totals: Vec<(u64, u64, u64)>,
    /// Bus cycle at activation, if active.
    pub activated_at_cycle: Option<u64>,
    /// Lane ps1-shadow: the shadow census, when `PSOXIDE_SHADOW_CENSUS` names a file.
    pub census: Option<Box<crate::shadow_census::ShadowCensus>>,
    /// Lane ps1-shadow: issue cycles still to refund (shadowmax).
    pub shadow_pool: u64,
    /// Lane ps1-shadow: issue cycles refunded so far (shadowmax).
    pub shadow_refunded: u64,
    /// Lane ps1-shadow: activate once the guest console printed `GL <n>`.
    gl_from: Option<u32>,
    /// Lane ps1-shadow: close the census once the guest console printed `GL <n>`.
    gl_until: Option<u32>,
    gl_reached: bool,
    gl_line: Vec<u8>,
}

impl LimitOracles {
    /// Read the `PSOXIDE_LIMIT_*` environment. Panics with a clear message
    /// on a malformed setting: a silently ignored oracle would report a
    /// plain run as a limit.
    pub fn from_env() -> Self {
        Self::from_settings(
            std::env::var("PSOXIDE_LIMIT_ORACLES").ok().as_deref(),
            std::env::var("PSOXIDE_LIMIT_FROM_POLL").ok().as_deref(),
            std::env::var("PSOXIDE_LIMIT_FROM_CYCLE").ok().as_deref(),
            std::env::var_os("PSOXIDE_LIMIT_FREE")
                .as_deref()
                .map(Path::new),
            std::env::var_os("PSOXIDE_LIMIT_WAIT")
                .as_deref()
                .map(Path::new),
            std::env::var_os("PSOXIDE_LIMIT_PROFILE")
                .as_deref()
                .map(Path::new),
        )
        .and_then(|limits| {
            limits.with_data_oracles(
                std::env::var_os("PSOXIDE_LIMIT_FAST_DATA")
                    .as_deref()
                    .map(Path::new),
                std::env::var_os("PSOXIDE_RAM_DATA_LOG").is_some(),
            )
        })
        .and_then(|limits| {
            limits.with_shadow_census(
                std::env::var_os("PSOXIDE_SHADOW_CENSUS")
                    .as_deref()
                    .map(Path::new),
                std::env::var("PSOXIDE_LIMIT_FROM_GL").ok().as_deref(),
                std::env::var("PSOXIDE_LIMIT_UNTIL_GL").ok().as_deref(),
            )
        })
        .unwrap_or_else(|error| panic!("PSOXIDE_LIMIT_*: {error}"))
    }

    /// Data-address oracles (`PSOXIDE_LIMIT_FAST_DATA`, `PSOXIDE_RAM_DATA_LOG`).
    ///
    /// `fast_data` names physical main-RAM ranges whose loads cost what the
    /// scratchpad costs once the oracles are live: "what if these bytes lived
    /// in the scratchpad". `log` records, per 32-byte main-RAM line, the stall
    /// cycles and the number of CPU loads (outside `PSOXIDE_LIMIT_WAIT`
    /// ranges), so a caller can rank lines and build such a range file.
    /// Neither changes timing unless a range file is given, and neither is
    /// active before `PSOXIDE_LIMIT_FROM_POLL` / `FROM_CYCLE`.
    ///
    /// # Errors
    ///
    /// When the `fast_data` range file cannot be read or parsed.
    pub fn with_data_oracles(
        mut self,
        fast_data: Option<&Path>,
        log: bool,
    ) -> Result<Self, String> {
        if let Some(path) = fast_data {
            self.fast_data = RangeSet::load(path)?;
            self.pending = true;
        }
        if log {
            self.data_log = Some(std::collections::HashMap::new());
            self.pending = true;
        }
        Ok(self)
    }

    /// Lane ps1-shadow: the shadow census (`PSOXIDE_SHADOW_CENSUS=FILE`) and the
    /// guest-console window (`PSOXIDE_LIMIT_FROM_GL=N`, `PSOXIDE_LIMIT_UNTIL_GL=M`:
    /// go live after the guest printed the line `GL N`, close the census after `GL M`).
    ///
    /// # Errors
    ///
    /// When a frame number does not parse.
    pub fn with_shadow_census(
        mut self,
        file: Option<&Path>,
        from_gl: Option<&str>,
        until_gl: Option<&str>,
    ) -> Result<Self, String> {
        let number = |name: &str, text: Option<&str>| -> Result<Option<u32>, String> {
            text.map(|t| {
                t.trim()
                    .parse()
                    .map_err(|error| format!("{name} {t}: {error}"))
            })
            .transpose()
        };
        self.gl_from = number("FROM_GL", from_gl)?;
        self.gl_until = number("UNTIL_GL", until_gl)?;
        if self.gl_from.is_some() {
            self.pending = true;
        }
        if let Some(path) = file {
            let funcs = self
                .profile
                .ranges()
                .iter()
                .map(|r| (r.name.clone(), r.start, r.end))
                .collect();
            self.census = Some(Box::new(crate::shadow_census::ShadowCensus::new(
                Some(path.to_path_buf()),
                funcs,
            )));
            self.configured |= CENSUS;
            self.track_pc = true;
            self.pending = true;
        }
        Ok(self)
    }

    /// Whether the activation still waits for a `GL` console line.
    #[inline]
    pub(crate) fn gl_blocks_activation(&self) -> bool {
        self.gl_from.is_some() && !self.gl_reached
    }

    /// Whether console bytes have to be watched for `GL` lines.
    #[inline]
    pub(crate) fn watches_console(&self) -> bool {
        self.gl_from.is_some() || self.gl_until.is_some()
    }

    /// One byte the guest wrote to the debug console, at bus cycle `cycle`.
    pub(crate) fn console_byte(&mut self, byte: u8, cycle: u64) {
        if byte != b'\n' {
            if byte != b'\r' && self.gl_line.len() < 64 {
                self.gl_line.push(byte);
            }
            return;
        }
        let line = std::mem::take(&mut self.gl_line);
        let Some(rest) = line.strip_prefix(b"GL ") else {
            return;
        };
        let word = rest.split(|b| *b == b' ').next().unwrap_or(&[]);
        let Ok(frame) = u32::from_str_radix(std::str::from_utf8(word).unwrap_or(""), 16) else {
            return;
        };
        if Some(frame) == self.gl_from {
            self.gl_reached = true;
        }
        if Some(frame) == self.gl_until {
            self.active &= !CENSUS;
            self.write_census(cycle);
            if self.configured & SHADOWMAX != 0 {
                eprintln!(
                    "[shadow] shadowmax at GL {frame}: refunded {} issue cycles, pool left {}",
                    self.shadow_refunded, self.shadow_pool
                );
            }
        }
    }

    /// Write the census file if it has not been written yet. `cycle` is the bus cycle now.
    pub fn write_census(&mut self, cycle: u64) {
        let window = match (self.census.as_ref(), self.activated_at_cycle) {
            (Some(_), Some(start)) => cycle.saturating_sub(start),
            _ => return,
        };
        let funcs: Vec<(String, u32, u32, u64, u64)> = self
            .profile
            .ranges()
            .iter()
            .zip(&self.profile_totals)
            .map(|(r, t)| (r.name.clone(), r.start, r.end, t.0, t.2))
            .collect();
        let wait_cycles = self.wait_cycles;
        let Some(census) = self.census.as_mut() else {
            return;
        };
        if census.written {
            return;
        }
        census.written = true;
        let text = census.render(window, wait_cycles, &funcs);
        if let Some(path) = census.path.as_ref() {
            match std::fs::write(path, text) {
                Ok(()) => eprintln!("[shadow] census -> {}", path.display()),
                Err(error) => eprintln!("[shadow] census write {}: {error}", path.display()),
            }
        }
    }

    /// Before an instruction runs under a shadow study switch.
    #[inline]
    pub(crate) fn shadow_pre(&mut self, instr: u32, base: u32, cycle: u64) {
        let in_wait = !self.wait.is_empty() && self.wait.find(self.cur_pc).is_some();
        if let Some(census) = self.census.as_mut() {
            if self.active & CENSUS != 0 {
                census.pre(instr, base, cycle, in_wait);
            }
        }
    }

    /// The instruction just run opened a shadow.
    #[inline]
    pub(crate) fn shadow_opened(&mut self, exceptions: u64) {
        if self.active & SHADOWMAX != 0 {
            self.shadow_pool += 4;
        }
        let func = self.profile.find(self.cur_pc);
        if self.active & CENSUS != 0 {
            if let Some(census) = self.census.as_mut() {
                census.opened(func, exceptions);
            }
        }
    }

    /// Take up to `cost` cycles from the shadowmax refund pool.
    #[inline]
    pub(crate) fn shadow_refund(&mut self, cost: u32) -> u32 {
        let take = self.shadow_pool.min(u64::from(cost));
        self.shadow_pool -= take;
        self.shadow_refunded += take;
        take as u32
    }

    fn from_settings(
        oracles: Option<&str>,
        from_poll: Option<&str>,
        from_cycle: Option<&str>,
        free: Option<&Path>,
        wait: Option<&Path>,
        profile: Option<&Path>,
    ) -> Result<Self, String> {
        let configured = match oracles {
            Some(list) => parse_oracles(list)?,
            None => 0,
        };
        let number = |name: &str, text: Option<&str>| -> Result<u64, String> {
            match text {
                Some(text) => text
                    .trim()
                    .parse()
                    .map_err(|error| format!("{name} {text}: {error}")),
                None => Ok(0),
            }
        };
        let from_poll = number("FROM_POLL", from_poll)?;
        let from_cycle = number("FROM_CYCLE", from_cycle)?;
        let load = |path: Option<&Path>| path.map(RangeSet::load).transpose();
        let mut limits = Self::new(
            configured,
            from_poll,
            load(free)?.unwrap_or_default(),
            load(wait)?.unwrap_or_default(),
            load(profile)?.unwrap_or_default(),
        );
        limits.from_cycle = from_cycle;
        Ok(limits)
    }

    /// Build a configuration directly (tests, tools).
    pub fn new(
        configured: u32,
        from_poll: u64,
        free: RangeSet,
        wait: RangeSet,
        profile: RangeSet,
    ) -> Self {
        let track_pc = !free.is_empty() || !wait.is_empty() || !profile.is_empty();
        let profile_totals = vec![(0, 0, 0); profile.ranges().len()];
        Self {
            configured,
            active: 0,
            pending: configured != 0 || track_pc,
            from_poll,
            free,
            wait,
            profile,
            track_pc,
            profile_totals,
            ..Self::default()
        }
    }

    /// Lane stall-free: a data load from `phys` costs like the scratchpad.
    #[inline]
    pub(crate) fn fast_data_hit(&self, phys: u32) -> bool {
        self.activated_at_cycle.is_some()
            && !self.fast_data.is_empty()
            && self.fast_data.find(phys).is_some()
    }

    /// Lane stall-free: record a main-RAM data load's stall.
    #[inline]
    pub(crate) fn log_data(&mut self, phys: u32, stalls: u32) {
        if self.activated_at_cycle.is_none() {
            return;
        }
        if !self.wait.is_empty() && self.wait.find(self.cur_pc).is_some() {
            return;
        }
        if let Some(log) = self.data_log.as_mut() {
            let entry = log.entry(phys >> 5).or_insert((0, 0));
            entry.0 += u64::from(stalls);
            entry.1 += 1;
        }
    }

    /// Lane stall-free: whether the data log is on.
    #[inline]
    pub(crate) fn logs_data(&self) -> bool {
        self.data_log.is_some()
    }

    /// Lane stall-free: write the per-line data log, hottest first.
    ///
    /// # Errors
    ///
    /// When the log file cannot be written.
    pub fn write_data_log(&self, path: &Path) -> Result<(), String> {
        let Some(log) = self.data_log.as_ref() else {
            return Ok(());
        };
        let mut rows: Vec<_> = log.iter().collect();
        rows.sort_by_key(|row| std::cmp::Reverse(row.1 .0));
        let mut text = String::from("line_addr,stall_cycles,loads\n");
        for (line, (stall, loads)) in rows {
            text.push_str(&format!("{:#010x},{stall},{loads}\n", line << 5));
        }
        std::fs::write(path, text).map_err(|e| format!("write {}: {e}", path.display()))
    }

    /// Whether any switch or range is configured.
    #[inline(always)]
    pub fn configured(&self) -> bool {
        self.configured != 0
            || self.track_pc
            || self.data_log.is_some()
            || !self.fast_data.is_empty()
    }

    /// Whether the switches are live (the start poll has been reached).
    pub fn is_active(&self) -> bool {
        self.activated_at_cycle.is_some()
    }

    /// Oracle names configured, comma-separated (for logs).
    pub fn describe(&self) -> String {
        let names: Vec<&str> = NAMES
            .iter()
            .filter(|(_, bit)| self.configured & bit != 0)
            .map(|(name, _)| *name)
            .collect();
        names.join(",")
    }

    /// Whether switch `bit` is on right now.
    #[inline]
    pub fn on(&self, bit: u32) -> bool {
        self.active & bit != 0
    }

    /// Activation is still waiting for its start poll.
    #[inline]
    pub(crate) fn pending(&self) -> bool {
        self.pending
    }

    /// The poll at which a pending configuration activates.
    pub(crate) fn start_poll(&self) -> u64 {
        self.from_poll
    }

    /// The bus cycle before which a pending configuration stays off.
    pub(crate) fn start_cycle(&self) -> u64 {
        self.from_cycle
    }

    /// Also wait for bus cycle `cycle` before activating.
    pub fn set_start_cycle(&mut self, cycle: u64) {
        self.from_cycle = cycle;
    }

    /// Turn the configured switches on. Returns the newly active mask.
    pub(crate) fn activate(&mut self, cycle: u64) -> u32 {
        self.pending = false;
        self.active = self.configured;
        self.activated_at_cycle = Some(cycle);
        self.active
    }

    /// Whether the CPU has to report each instruction's PC.
    #[inline]
    pub fn tracks_pc(&self) -> bool {
        self.track_pc && self.activated_at_cycle.is_some()
    }

    /// Whether the clock is frozen for the instruction being executed.
    #[inline]
    pub(crate) fn frozen(&self) -> bool {
        self.frozen
    }

    /// A frozen instruction touched hardware: charge it from here on.
    #[inline]
    pub(crate) fn thaw(&mut self) {
        if self.frozen {
            self.frozen = false;
            self.thawed_accesses += 1;
        }
    }

    /// Called before an instruction at `pc` runs; freezes the clock when
    /// `pc` is in a free range.
    #[inline]
    pub(crate) fn begin_instruction(&mut self, pc: u32) {
        self.cur_pc = pc;
        if self.free.is_empty() {
            return;
        }
        if self.free.find(pc).is_some() {
            if self.guard_escape {
                return;
            }
            self.free_run += 1;
            if self.free_run > FREE_GUARD {
                self.guard_trips += 1;
                self.guard_escape = true;
                self.free_run = 0;
                return;
            }
            self.frozen = true;
            self.free_instructions += 1;
        } else {
            self.free_run = 0;
            self.guard_escape = false;
        }
    }

    /// Called after the instruction at `pc` retired, with the cycles it
    /// charged and the cycles the freeze skipped.
    #[inline]
    /// Returns whether `pc` is in a wait range.
    pub(crate) fn end_instruction(&mut self, pc: u32, charged: u64, skipped: u64) -> bool {
        self.frozen = false;
        let waiting = !self.wait.is_empty() && self.wait.find(pc).is_some();
        if waiting {
            self.wait_cycles += charged;
        }
        if !self.profile.is_empty() {
            if let Some(index) = self.profile.find(pc) {
                let totals = &mut self.profile_totals[index];
                totals.0 += charged;
                totals.1 += skipped;
                totals.2 += 1;
            }
        }
        waiting
    }

    /// Add one waiting instruction's stalls (see [`Self::wait_stalls`]).
    #[inline]
    pub(crate) fn add_wait_stalls(&mut self, stalls: [u64; 6]) {
        for (total, add) in self.wait_stalls.iter_mut().zip(stalls) {
            *total += add;
        }
    }

    /// Record cycles a frozen instruction would have taken.
    #[inline]
    pub(crate) fn skip(&mut self, cycles: u64) {
        self.skipped_cycles += cycles;
    }

    /// Write the per-function totals as CSV.
    pub fn write_profile(&self, path: &Path) -> Result<(), String> {
        let mut text = String::from("start,end,name,charged_cycles,skipped_cycles,instructions\n");
        for (range, totals) in self.profile.ranges().iter().zip(&self.profile_totals) {
            if totals.2 == 0 {
                continue;
            }
            text.push_str(&format!(
                "{:#010x},{:#010x},{},{},{},{}\n",
                range.start, range.end, range.name, totals.0, totals.1, totals.2
            ));
        }
        std::fs::write(path, text).map_err(|error| format!("write {}: {error}", path.display()))
    }
}

fn parse_oracles(list: &str) -> Result<u32, String> {
    let mut mask = 0;
    for name in list
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        if name == "all" {
            mask |= NAMES.iter().map(|(_, bit)| bit).fold(0, |a, b| a | b) & !(SPRAM | SHADOWNONE | SHADOWDEP | SHADOWMAX);
            continue;
        }
        let bit = NAMES
            .iter()
            .find(|(known, _)| *known == name)
            .map(|(_, bit)| *bit)
            .ok_or_else(|| format!("unknown oracle {name:?}"))?;
        mask |= bit;
    }
    Ok(mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_configured_by_default() {
        let limits = LimitOracles::from_settings(None, None, None, None, None, None).unwrap();
        assert!(!limits.configured());
        assert!(!limits.pending());
        assert!(!limits.tracks_pc());
    }

    #[test]
    fn parses_oracle_names_and_rejects_unknown_ones() {
        assert_eq!(parse_oracles("gte, muldiv").unwrap(), GTE | MULDIV);
        assert_eq!(parse_oracles("all").unwrap(), 0x7f);
        assert_eq!(parse_oracles("spram").unwrap(), SPRAM);
        assert!(parse_oracles("gtee").is_err());
    }

    #[test]
    fn range_file_lookup() {
        let set = RangeSet::parse(
            "# comment\n0x80010000 80010010 a\n80010020 0x80010030\n80010008 8001000c dup\n",
        )
        .unwrap();
        assert_eq!(set.ranges().len(), 2);
        assert_eq!(set.find(0x8001_0000), Some(0));
        assert_eq!(set.find(0x8001_000c), Some(0));
        assert_eq!(set.find(0x8001_0010), None);
        assert_eq!(set.find(0x8001_0024), Some(1));
        assert_eq!(set.ranges()[1].name, "0x80010020");
        assert_eq!(set.find(0x8000_0000), None);
    }

    #[test]
    fn free_range_guard_trips_and_escapes() {
        let free = RangeSet::parse("80010000 80010010").unwrap();
        let mut limits = LimitOracles::new(0, 0, free, RangeSet::default(), RangeSet::default());
        limits.activate(0);
        for _ in 0..FREE_GUARD {
            limits.begin_instruction(0x8001_0000);
            assert!(limits.frozen());
            limits.end_instruction(0x8001_0000, 0, 1);
        }
        limits.begin_instruction(0x8001_0000);
        assert!(!limits.frozen());
        assert_eq!(limits.guard_trips, 1);
        limits.end_instruction(0x8001_0000, 1, 0);
        limits.begin_instruction(0x8002_0000);
        limits.end_instruction(0x8002_0000, 1, 0);
        limits.begin_instruction(0x8001_0000);
        assert!(limits.frozen());
    }

    #[test]
    fn data_oracles_are_off_until_configured_and_active() {
        let mut limits = LimitOracles::from_settings(None, None, None, None, None, None).unwrap();
        assert!(!limits.fast_data_hit(0x1000));
        limits.log_data(0x1000, 6);
        assert!(!limits.logs_data());

        let dir = std::env::temp_dir().join(format!("limits-fast-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ranges = dir.join("fast.txt");
        std::fs::write(&ranges, "00001000 00001040 hot\n").unwrap();
        let mut limits = LimitOracles::from_settings(None, None, None, None, None, None)
            .unwrap()
            .with_data_oracles(Some(&ranges), true)
            .unwrap();
        assert!(limits.configured());
        // Not live before activation.
        assert!(!limits.fast_data_hit(0x1000));
        limits.log_data(0x1000, 6);
        limits.activate(0);
        assert!(limits.fast_data_hit(0x1000));
        assert!(limits.fast_data_hit(0x103f));
        assert!(!limits.fast_data_hit(0x1040));
        // Two loads in one 32-byte line and one in the next.
        limits.log_data(0x1004, 6);
        limits.log_data(0x101c, 7);
        limits.log_data(0x1020, 5);
        let out = dir.join("log.csv");
        limits.write_data_log(&out).unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("line_addr,stall_cycles,loads"));
        assert_eq!(lines.next(), Some("0x00001000,13,2"));
        assert_eq!(lines.next(), Some("0x00001020,5,1"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn data_log_skips_loads_made_inside_wait_ranges() {
        let wait = RangeSet::parse("80010000 80010010").unwrap();
        let mut limits = LimitOracles::new(0, 0, RangeSet::default(), wait, RangeSet::default())
            .with_data_oracles(None, true)
            .unwrap();
        limits.activate(0);
        limits.begin_instruction(0x8001_0004);
        limits.log_data(0x2000, 6);
        limits.end_instruction(0x8001_0004, 7, 0);
        limits.begin_instruction(0x8002_0000);
        limits.log_data(0x2000, 6);
        limits.end_instruction(0x8002_0000, 7, 0);
        let dir = std::env::temp_dir().join(format!("limits-wait-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("log.csv");
        limits.write_data_log(&out).unwrap();
        assert_eq!(
            std::fs::read_to_string(&out).unwrap().lines().nth(1),
            Some("0x00002000,6,1")
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
