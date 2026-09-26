//! Bus facts the dynamic recompiler (`psoxide-jit`) needs to decide when
//! compiled code may run a stretch of instructions without changing what
//! the interpreter would have done. Read-only except for
//! [`Bus::jit_settle_stream`], which performs exactly the side effect of
//! the interpreter's next cached fetch.

use super::*;

impl Bus {
    /// Address of the bus clock, for compiled code that compares it with a
    /// limit. Valid while the bus is not moved.
    #[doc(hidden)]
    #[inline]
    pub fn jit_cycles_ptr(&self) -> *const u64 {
        &self.cycles
    }

    /// Bus cycle at which a per-instruction [`Bus::tick`] next runs the
    /// scheduler or the SPU. Below it, `tick` only advances the clock.
    #[doc(hidden)]
    #[inline]
    pub fn jit_next_due(&self) -> u64 {
        self.scheduler.lowest_target().min(self.spu_sample_deadline)
    }

    /// Whether `issue_cycles` more cycles of per-instruction ticks, with no
    /// memory access in between, dispatch nothing: every tick ends below
    /// [`Bus::jit_next_due`], so none drains the scheduler or runs the SPU,
    /// and the SPU catch-up at each instruction's start is a no-op too.
    #[doc(hidden)]
    #[inline]
    pub fn jit_quiet_for(&self, issue_cycles: u64) -> bool {
        self.cycles.saturating_add(issue_cycles) < self.jit_next_due()
    }

    /// Whether the clock is on its plain path: no frozen limit range, no
    /// experimental GPU list walk, no GPU DMA held back, and GPU credit
    /// decay is plain arithmetic. Then one advance of `n` cycles equals `n`
    /// advances of one.
    #[doc(hidden)]
    #[inline]
    pub fn jit_tick_is_plain(&self) -> bool {
        !(self.limits.frozen()
            || self.experimental_gpu_list.is_some()
            || self.gpu_dma_waiting_for_request)
            && self.gpu.decay_is_plain()
    }

    /// Charge `n` issue cycles at once, for a run of instructions the
    /// caller established [`Bus::jit_quiet_for`] for, while
    /// [`Bus::jit_tick_is_plain`] holds: the same as `n` per-instruction
    /// ticks, none of which reaches an event.
    #[doc(hidden)]
    #[inline]
    pub fn jit_tick_quiet(&mut self, n: u32) {
        debug_assert!(self.jit_tick_is_plain() && self.jit_quiet_for(u64::from(n)));
        self.advance_cycles(n);
    }

    /// No limit oracle is configured, pending or tracking PCs, so the CPU
    /// never needs the per-instruction limit hooks.
    #[doc(hidden)]
    #[inline]
    pub fn jit_limits_idle(&self) -> bool {
        !self.limits.on(u32::MAX) && !self.limits.pending() && !self.limits.tracks_pc()
    }

    /// Whether main RAM at physical address `phys` (any mirror) holds
    /// `bytes` (little-endian instruction words). False when the range runs
    /// past the end of RAM.
    #[doc(hidden)]
    #[inline]
    pub fn jit_ram_matches(&self, phys: u32, bytes: &[u8]) -> bool {
        let start = (phys as usize) % memory::ram::SIZE;
        self.ram.get(start..start + bytes.len()) == Some(bytes)
    }

    /// Whether the next cached fetch is free of a streaming line fill: no
    /// stream in flight, or one that settled already. Pure.
    #[doc(hidden)]
    #[inline]
    pub fn jit_stream_settled(&self) -> bool {
        !self.code_stream_active
            || self.cycles >= self.code_fill_busy_until + u64::from(Self::STREAM_RESTART_CYCLES)
    }

    /// The side effect of [`Bus::streaming_fill_wait`] when
    /// [`Bus::jit_stream_settled`] holds: a settled stream is retired and
    /// no wait is charged.
    #[doc(hidden)]
    #[inline]
    pub fn jit_settle_stream(&mut self) {
        debug_assert!(self.jit_stream_settled());
        self.code_stream_active = false;
    }
}
