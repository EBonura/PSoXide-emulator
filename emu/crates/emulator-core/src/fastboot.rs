//! Disc fast boot helpers.
//!
//! This path mirrors the BIOS loader's `SYSTEM.CNF -> PSX-EXE` work
//! without relying on the BIOS license-screen handoff. The disc stays
//! mounted in the CD-ROM controller; only the initial executable load
//! is short-circuited.

use psx_iso::{load_boot_exe_from_disc, BootError, Disc};

use crate::{gpu::GP1_ADDR, Bus, Cpu};

/// Summary of a successful disc fast boot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscFastBootInfo {
    /// Normalized `SYSTEM.CNF` boot path.
    pub boot_path: String,
    /// Executable entry point.
    pub initial_pc: u32,
    /// Executable load address.
    pub load_addr: u32,
    /// Bytes copied into RAM.
    pub payload_len: usize,
    /// Stack pointer applied to the CPU, if one was provided.
    pub stack_pointer: Option<u32>,
}

/// Load the disc executable and enable the built-in runtime.
pub fn fast_boot_disc(
    bus: &mut Bus,
    cpu: &mut Cpu,
    disc: &Disc,
) -> Result<DiscFastBootInfo, BootError> {
    let boot = load_boot_exe_from_disc(disc)?;
    let payload_len = boot.exe.payload.len();
    let stack_pointer = boot.stack_pointer.or_else(|| boot.exe.initial_sp());

    if let Some(sp) = stack_pointer {
        bus.clear_ram_range(0x8001_0000, sp);
    }
    bus.load_exe_payload(boot.exe.load_addr, &boot.exe.payload);
    bus.clear_exe_bss(boot.exe.bss_addr, boot.exe.bss_size);
    // OpenBIOS enables display immediately before Exec. Some retail
    // games rely on inheriting that shell state instead of issuing
    // GP1(03h) themselves during early startup.
    bus.write32(GP1_ADDR, 0x0300_0000);
    cpu.seed_from_exe_with_args(
        boot.exe.initial_pc,
        boot.exe.initial_gp,
        stack_pointer,
        1,
        0,
    );
    bus.enable_hle_bios();

    Ok(DiscFastBootInfo {
        boot_path: boot.boot_path,
        initial_pc: boot.exe.initial_pc,
        load_addr: boot.exe.load_addr,
        payload_len,
        stack_pointer,
    })
}
