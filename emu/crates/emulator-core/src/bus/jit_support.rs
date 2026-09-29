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
            // DPCR pauses the list sequencer, but not GPU execution credit.
            if list.setup_cycles != 0 && self.dma.is_channel_enabled(2) {
                list.setup_cycles = list
                    .setup_cycles
                    .saturating_sub(n.min(u64::from(u32::MAX)) as u32);
            }
        } else {
            self.gpu.decay_busy(n);
        }
    }

    /// Main RAM's bytes, for compiled code's look at the next instruction.
    pub fn jit_ram_ptr(&self) -> *const u8 {
        self.ram.as_ptr()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jit_decay_preserves_paused_gpu_list_setup() {
        let make = || {
            let mut bus = Bus::new_without_bios();
            bus.gpu.enable_experimental_dma_fifo();
            bus.gpu.write32(crate::gpu::GP1_ADDR, 0x0400_0002);
            bus.write32(0x300, 0x01ff_ffff);
            bus.write32(0x304, 0);
            bus.dma.dpcr = 1 << (2 * 4 + 3);
            bus.dma.channels[2].base = 0x300;
            bus.dma.channels[2].channel_control = 0x0100_0401;
            bus.run_dma_channel(2);
            bus.add_cycles(1);
            assert_eq!(bus.experimental_gpu_list.as_ref().unwrap().setup_cycles, 15);
            bus.write32(Dma::BASE + Dma::DPCR_OFFSET, 0);
            bus
        };
        let mut native = make();
        let mut reference = make();
        let advance = |native: &mut Bus, reference: &mut Bus, cycles: u64| {
            assert!(cycles == 0 || native.cycles + cycles < native.quiet_limit());
            // Native code advances this clock itself, then settles credits
            // before the next external observation or MMIO operation.
            native.cycles += cycles;
            native.jit_settle_decay(cycles);
            for _ in 0..cycles {
                reference.cycles += 1;
                reference.gpu.decay_busy(1);
                reference.advance_experimental_gpu_list();
            }
            assert!(
                postcard::to_allocvec(native).unwrap() == postcard::to_allocvec(reference).unwrap(),
                "batch {cycles}: setup {:?} vs {:?}",
                native
                    .experimental_gpu_list
                    .as_ref()
                    .map(|l| l.setup_cycles),
                reference
                    .experimental_gpu_list
                    .as_ref()
                    .map(|l| l.setup_cycles)
            );
        };
        for cycles in [0, 1, 2, 7, 32, 128] {
            advance(&mut native, &mut reference, cycles);
        }
        native.write32(Dma::BASE + Dma::DPCR_OFFSET, 1 << (2 * 4 + 3));
        reference.write32(Dma::BASE + Dma::DPCR_OFFSET, 1 << (2 * 4 + 3));
        for cycles in [0, 1, 2, 7, 5] {
            advance(&mut native, &mut reference, cycles);
        }
        assert_eq!(
            native.experimental_gpu_list.as_ref().unwrap().setup_cycles,
            0
        );
    }
}
