//! Disc fast boot helpers.
//!
//! This path mirrors the BIOS loader's `SYSTEM.CNF -> PSX-EXE` work
//! without relying on the BIOS license-screen handoff. The disc stays
//! mounted in the CD-ROM controller; only the initial executable load
//! is short-circuited.

use psx_iso::{BootError, Disc};

use crate::cpu::ExecutionError;
use crate::system_cnf::{load_disc_boot, BOOT_ARG_ADDR};
use crate::{gpu::GP1_ADDR, Bus, Cpu};

/// Number of BIOS instructions to run before warm disc fast boot.
///
/// By this point SCPH1001 has installed the syscall tables, exception
/// vectors, and interrupt mask state that retail games expect, but it
/// has not spent time on the disc license path.
pub const DISC_FAST_BOOT_WARMUP_STEPS: u64 = 10_000_000;

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
    /// Stack pointer applied to the CPU. Always set: `SYSTEM.CNF`'s
    /// `STACK` (or its default) replaces the executable header's value.
    pub stack_pointer: Option<u32>,
    /// Frame pointer applied to the CPU.
    pub frame_pointer: u32,
    /// `STACK` as the BIOS evaluates it; 0 means the caller's stack was
    /// kept (see [`crate::system_cnf::CALLER_STACK_SP`]).
    pub cnf_stack: u32,
    /// `TCB` from `SYSTEM.CNF`.
    pub tcb: u32,
    /// `EVENT` from `SYSTEM.CNF`.
    pub event: u32,
    /// Argument from the `BOOT` line, copied to `0x180`.
    pub boot_arg: Option<String>,
}

/// Load a disc's boot EXE into RAM and seed the CPU at its entry point.
///
/// Callers should mount the same [`Disc`] in the CD-ROM controller
/// after this returns, so the running game can continue issuing normal
/// CD commands.
pub fn fast_boot_disc(
    bus: &mut Bus,
    cpu: &mut Cpu,
    disc: &Disc,
) -> Result<DiscFastBootInfo, BootError> {
    fast_boot_disc_with_hle(bus, cpu, disc, true)
}

/// Variant of [`fast_boot_disc`] that lets callers choose whether to
/// enable HLE BIOS dispatch after loading the EXE.
///
/// Set `enable_hle_bios` to `false` when the real BIOS has already run
/// far enough to install its RAM syscall and exception handlers.
pub fn fast_boot_disc_with_hle(
    bus: &mut Bus,
    cpu: &mut Cpu,
    disc: &Disc,
    enable_hle_bios: bool,
) -> Result<DiscFastBootInfo, BootError> {
    let boot = load_disc_boot(disc)?;
    let payload_len = boot.exe.payload.len();
    // The loader reads the EXE and stops: the game's first seek starts from
    // the sector after it (measured at entry under a real BIOS).
    let (exe_lba, exe_size) = crate::system_cnf::file_extent(disc, &boot.cnf.boot_path)?;
    bus.cdrom.park_head(exe_lba + exe_size.div_ceil(2048));
    let (sp, fp) = boot.cnf.entry_stack();

    bus.clear_ram_range(0x8001_0000, sp);
    bus.load_exe_payload(boot.exe.load_addr, &boot.exe.payload);
    bus.clear_exe_bss(boot.exe.bss_addr, boot.exe.bss_size);
    if let Some(arg) = boot.cnf.boot_arg_bytes() {
        for (offset, byte) in arg.into_iter().enumerate() {
            bus.write8_safe(BOOT_ARG_ADDR + offset as u32, byte);
        }
    }
    if enable_hle_bios {
        apply_hle_entry_state(bus);
    } else {
        // The abbreviated BIOS warmup installs kernel state but intentionally
        // stops before the license/shell path. PA5 silicon telemetry proves
        // that disc executables normally inherit the shell's configured SPU
        // reverb preset, so restore that observable handoff explicitly.
        bus.apply_retail_bios_shell_audio_profile();
    }
    // OpenBIOS enables display immediately before Exec. Some retail
    // games rely on inheriting that shell state instead of issuing
    // GP1(03h) themselves during early startup.
    bus.write32(GP1_ADDR, 0x0300_0000);
    cpu.seed_from_exe_with_args(boot.exe.initial_pc, boot.exe.initial_gp, Some(sp), 1, 0);
    cpu.seed_frame_pointer(fp);
    if enable_hle_bios {
        bus.enable_hle_bios_with(crate::hle_kernel::KernelConfig {
            tcb: boot.cnf.tcb,
            event: boot.cnf.event,
            stack: boot.cnf.stack,
        });
    }

    Ok(DiscFastBootInfo {
        boot_path: boot.cnf.boot_path,
        initial_pc: boot.exe.initial_pc,
        load_addr: boot.exe.load_addr,
        payload_len,
        stack_pointer: Some(sp),
        frame_pointer: fp,
        cnf_stack: boot.cnf.stack,
        tcb: boot.cnf.tcb,
        event: boot.cnf.event,
        boot_arg: boot.cnf.boot_arg,
    })
}

/// DMA control register.
const DPCR_ADDR: u32 = 0x1F80_10F0;
/// Interrupt mask register.
const I_MASK_ADDR: u32 = 0x1F80_1074;
/// GPU command port.
const GP0_ADDR: u32 = 0x1F80_1810;

/// Hardware state a disc executable inherits from a real boot, for the HLE
/// path that never runs the BIOS. Every value was constant across the 38
/// census runs (31 discs) captured at EXE entry under SCPH1001; see
/// docs/hle-bios-provenance.md. The warm fast boot inherits the same state
/// from the real BIOS instead.
///
/// Deliberately not reproduced: I_STAT (a VBlank latched while the BIOS
/// held interrupts off, which the first emulated VBlank recreates), the CD
/// drive's mode and motor state, and leftover values such as CAUSE, the
/// SPU transfer address and pending key-offs.
fn apply_hle_entry_state(bus: &mut Bus) {
    // DMA: MDEC-in, MDEC-out and CD-ROM enabled at priority 1; DICR has
    // master enable, the GPU and CD-ROM channel enables and their
    // completion flags set, so the master flag is high.
    bus.write32(DPCR_ADDR, 0x0000_9099);
    bus.set_dicr_raw(0x8C8C_0000);
    // CD-ROM and DMA interrupts unmasked.
    bus.write32(I_MASK_ADDR, 0x0000_000C);
    bus.apply_hle_entry_audio_profile();
    // Shell display: 640x480 interlaced, 15-bit, NTSC (an NTSC BIOS leaves
    // this even for PAL discs), with dithering and drawing to the displayed
    // field enabled.
    bus.write32(GP1_ADDR, 0x0800_0027);
    bus.write32(GP0_ADDR, 0xE100_0600);
}

/// Run the real BIOS long enough to install its RAM kernel state.
pub fn warm_bios_for_disc_fast_boot(
    bus: &mut Bus,
    cpu: &mut Cpu,
    steps: u64,
) -> Result<(), ExecutionError> {
    for _ in 0..steps {
        cpu.step(bus)?;
        bus.run_spu_to_current_cycle();
        if bus.spu.audio_queue_len() != 0 {
            let _ = bus.spu.drain_audio();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use psx_iso::{IsoBuilder, EXE_HEADER_BYTES};

    fn disc(system_cnf: &[u8]) -> Disc {
        let mut exe = vec![0u8; EXE_HEADER_BYTES];
        exe[..8].copy_from_slice(b"PS-X EXE");
        exe[0x10..0x14].copy_from_slice(&0x8001_0000u32.to_le_bytes());
        exe[0x18..0x1C].copy_from_slice(&0x8001_0000u32.to_le_bytes());
        exe[0x1C..0x20].copy_from_slice(&4u32.to_le_bytes());
        // Header stack fields are ignored on disc boot.
        exe[0x30..0x34].copy_from_slice(&0x801F_0000u32.to_le_bytes());
        exe.extend_from_slice(&[0; 4]);
        let mut builder = IsoBuilder::new();
        builder.add_file("SYSTEM.CNF", system_cnf.to_vec());
        builder.add_file("GAME.EXE", exe);
        Disc::from_bin(builder.build_bin())
    }

    #[test]
    fn stack_line_replaces_the_header_and_prefix_keeps_the_caller_stack() {
        for (cnf, sp, fp) in [
            (
                &b"BOOT = cdrom:\\GAME.EXE;1\r\nSTACK = 801FFFF0\r\n"[..],
                0x801F_FFF0,
                0x801F_FFF0,
            ),
            (b"BOOT = cdrom:\\GAME.EXE;1\r\n", 0x801F_FF00, 0x801F_FF00),
            (
                b"BOOT = cdrom:\\GAME.EXE;1\r\nSTACK = 0x801FFFF0\r\n",
                0x801F_FDD8,
                0x801F_FF00,
            ),
        ] {
            let mut bus = Bus::new_without_bios();
            let mut cpu = Cpu::new();
            let info = fast_boot_disc(&mut bus, &mut cpu, &disc(cnf)).unwrap();
            assert_eq!(info.stack_pointer, Some(sp));
            assert_eq!((cpu.gpr(29), cpu.gpr(30)), (sp, fp));
            assert_eq!((cpu.gpr(4), cpu.gpr(5)), (1, 0));
        }
    }

    #[test]
    fn hle_boot_reproduces_the_measured_entry_state() {
        let mut bus = Bus::new_without_bios();
        let mut cpu = Cpu::new();
        fast_boot_disc(&mut bus, &mut cpu, &disc(b"BOOT = cdrom:\\GAME.EXE;1\r\n")).unwrap();
        assert_eq!(bus.read32(DPCR_ADDR), 0x0000_9099);
        assert_eq!(bus.read32(0x1F80_10F4), 0x8C8C_0000);
        assert_eq!(bus.read32(I_MASK_ADDR) & 0xFFFF, 0x000C);
        assert_eq!(bus.read32(0x60), 2);
        assert_eq!(bus.read16(0x1F80_1D80), 0x3FFF);
        assert_eq!(bus.read16(0x1F80_1D82), 0x37EF);
        assert_eq!(bus.read16(0x1F80_1DAA), 0xC085);
        assert_eq!(bus.read16(0x1F80_1DAC), 0x0004);
        assert_eq!(bus.read16(0x1F80_1DB0), 0);
        let gpustat = bus.read32(0x1F80_1814);
        // 640x480 interlaced, NTSC, display on, dither and draw-to-display.
        assert_eq!(gpustat & 0x00FF_0600, 0x004E_0600);
        assert_eq!(gpustat & (1 << 23), 0);
        // The IRQ mask must not raise anything at entry.
        assert!(!bus.external_interrupt_pending());
    }

    #[test]
    fn hle_boot_leaves_the_cd_head_after_the_executable() {
        // A real boot reads the EXE and stops there: at entry the head is on
        // the sector after its last one (measured on CTR, Tekken 3, Crash,
        // MGS, Resident Evil 2). The game's first seek starts from there.
        let disc = disc(b"BOOT = cdrom:\\GAME.EXE;1\r\n");
        let (lba, size) = crate::system_cnf::file_extent(&disc, "GAME.EXE").unwrap();
        let mut bus = Bus::new_without_bios();
        let mut cpu = Cpu::new();
        fast_boot_disc(&mut bus, &mut cpu, &disc).unwrap();
        bus.cdrom.insert_disc(Some(disc));
        assert_eq!(bus.cdrom.debug_read_lba(), lba + size.div_ceil(2048));
    }

    #[test]
    fn boot_argument_lands_at_0x180() {
        let mut bus = Bus::new_without_bios();
        let mut cpu = Cpu::new();
        let info = fast_boot_disc(
            &mut bus,
            &mut cpu,
            &disc(b"BOOT = cdrom:\\GAME.EXE;1 go\r\nTCB = 8\r\nEVENT = 20\r\n"),
        )
        .unwrap();
        assert_eq!(info.boot_arg.as_deref(), Some("go"));
        assert_eq!((info.tcb, info.event), (8, 0x20));
        let arg: Vec<u8> = (0..3).map(|i| bus.try_read8(0x180 + i).unwrap()).collect();
        assert_eq!(arg, b"go\0");
    }
}
