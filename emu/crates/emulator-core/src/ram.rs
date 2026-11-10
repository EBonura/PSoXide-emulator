//! Main RAM size for this build.
//!
//! The retail console has 2 MiB, mirrored four times across
//! `0x0000_0000..0x0080_0000`. The `devkit-ram` feature models a development
//! board (DTL-H2000 class) instead: 8 MiB filling that whole window, no
//! mirrors. Some 1990s development builds were linked for it and overrun a
//! retail console.
//!
//! The size lives here rather than in `psx-hw`, whose `memory::ram::SIZE`
//! stays the retail 2 MiB: `psx-hw` is vendored from the SDK and must match
//! its pinned revision. Emulator code that sizes or masks main RAM uses this
//! module; the retail build compiles to the same constants as before.

/// Main RAM size in bytes: 2 MiB, or 8 MiB with `devkit-ram`.
#[cfg(not(feature = "devkit-ram"))]
pub const SIZE: usize = psx_hw::memory::ram::SIZE;
/// Main RAM size in bytes: 2 MiB, or 8 MiB with `devkit-ram`.
#[cfg(feature = "devkit-ram")]
pub const SIZE: usize = 8 * 1024 * 1024;

/// Offset mask for a physical RAM address (`SIZE - 1`).
pub const MASK: u32 = SIZE as u32 - 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ram_fits_the_mirror_window_a_whole_number_of_times() {
        let window = psx_hw::memory::ram::MIRROR_END as usize;
        assert!(SIZE.is_power_of_two());
        assert_eq!(window % SIZE, 0);
        assert_eq!(MASK as usize, SIZE - 1);
    }
}
