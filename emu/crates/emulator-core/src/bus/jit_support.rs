//! Bus facts for the native tier (`psoxide-jit`).

use super::*;

impl Bus {
    /// Address of the bus clock, for compiled code that compares it with a
    /// limit. Valid while the bus is not moved.
    #[doc(hidden)]
    #[inline]
    pub fn jit_cycles_ptr(&self) -> *const u64 {
        &self.cycles
    }

    /// Main RAM's page write counts ([`Bus::ram_stamp`]) and this RAM's id.
    pub(crate) fn jit_ram_pages(&self) -> (*const u32, u64) {
        (self.ram_pages.counts_ptr(), self.ram_pages.id)
    }

    /// Where the state a plain main-RAM load reads and writes lives, for
    /// compiled code: the end of the current I-cache fill, the next DRAM
    /// refresh deadline, the last CPU RAM access and the data-bus latch.
    pub(crate) fn jit_load_state_ptrs(&mut self) -> [usize; 4] {
        [
            &self.code_fill_busy_until as *const u64 as usize,
            &self.dram_refresh_deadline as *const u64 as usize,
            &mut self.last_cpu_ram_access_cycle as *mut u64 as usize,
            &mut self.data_bus_latch as *mut u32 as usize,
        ]
    }

    /// Whether the last instruction fetch left the RAM bus alone (a cache
    /// hit): then a RAM load competes with no code fetch.
    pub(crate) fn jit_fetch_off_ram_bus(&self) -> bool {
        !self.code_fetch_on_ram_bus
    }

    /// What [`Bus::advance_quiet`] does for `n` cycles besides moving the
    /// clock: the GPU's credit decay and a list walk's setup countdown.
    /// Compiled code moves the clock itself and applies these later in one
    /// go; inside the quiet span both add up.
    pub(crate) fn jit_settle_decay(&mut self, n: u64) {
        if n == 0 {
            return;
        }
        if let Some(list) = self.experimental_gpu_list.as_mut() {
            self.gpu.decay_busy_quiet(n);
            list.setup_cycles = list
                .setup_cycles
                .saturating_sub(n.min(u64::from(u32::MAX)) as u32);
        } else {
            self.gpu.decay_busy(n);
        }
    }

    /// Main RAM's bytes, for compiled code's look at the next instruction.
    pub fn jit_ram_ptr(&self) -> *const u8 {
        self.ram.as_ptr()
    }
}
