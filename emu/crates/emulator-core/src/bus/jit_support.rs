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
}
