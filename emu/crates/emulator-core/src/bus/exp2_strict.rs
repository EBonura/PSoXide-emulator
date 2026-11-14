//! Strict Expansion 2 stores.
//!
//! The Expansion 2 delay/size register (`0x1F80101C`, bits 16 to 20) gives the
//! region a window of `1 << n` bytes. The BIOS leaves it at 128 bytes
//! (`0x00070777`). A byte stored beyond the window is a data bus error on a
//! console: a disc whose program stored to the debug console port at
//! `0x1F802080` stopped at boot on its fault panel with exception code 7.
//!
//! The emulator accepts such stores by default, because `0x1F802080` is also
//! its debug text port and the gates read that text. `PSOXIDE_STRICT_EXP2=1`
//! (or `--strict-exp2` on the headless tools) turns the fault on. The CPU asks
//! [`Bus::expansion2_store_faults`] before it lets a store reach the bus.
//!
//! Only stores are modelled: the one console observation is a store, and
//! nothing recorded says whether a load from the same offsets faults too.

use super::Bus;
use psx_hw::memory;

pub(super) fn exp2_strict_from_env() -> bool {
    crate::strict_exp2_requested()
}

impl Bus {
    /// Turn the strict Expansion 2 store fault on or off.
    pub fn set_expansion2_strict(&mut self, strict: bool) {
        self.exp2_strict = strict;
    }

    /// Whether strict Expansion 2 stores are on.
    #[must_use]
    pub fn expansion2_strict(&self) -> bool {
        self.exp2_strict
    }

    /// Whether a store to `virt` is a data bus error: strict mode is on and
    /// the address is in the Expansion 2 region beyond the size the delay/size
    /// register currently configures.
    #[inline(always)]
    #[must_use]
    pub fn expansion2_store_faults(&self, virt: u32) -> bool {
        self.exp2_strict && self.expansion2_beyond_window(virt)
    }

    #[inline(never)]
    fn expansion2_beyond_window(&self, virt: u32) -> bool {
        let offset = memory::to_physical(virt).wrapping_sub(memory::expansion2::BASE);
        offset < memory::expansion2::SIZE as u32
            && offset >= self.memory_control.exp2_window_bytes()
    }
}
