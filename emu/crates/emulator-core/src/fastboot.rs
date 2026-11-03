//! Disc fast boot helpers.
//!
//! PSoXide has no BIOS: every disc boots on the built-in HLE kernel. This
//! path does the loader's `SYSTEM.CNF -> PSX-EXE` work, hands the executable
//! the hardware state a retail boot leaves behind, and enables the kernel.
//! The disc stays mounted in the CD-ROM controller.

use psx_iso::{BootError, Disc};

use crate::entry_state::EntryState;
use crate::gpu::GP1_ADDR;
use crate::system_cnf::{load_disc_boot, BOOT_ARG_ADDR};
use crate::{Bus, Cpu};

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

/// Load a disc's boot EXE into RAM, seed the CPU at its entry point and
/// start the HLE kernel.
///
/// Callers should mount the same [`Disc`] in the CD-ROM controller
/// after this returns, so the running game can continue issuing normal
/// CD commands.
pub fn fast_boot_disc(
    bus: &mut Bus,
    cpu: &mut Cpu,
    disc: &Disc,
) -> Result<DiscFastBootInfo, BootError> {
    fast_boot_disc_with(bus, cpu, disc, &EntryState::from_env())
}

/// [`fast_boot_disc`] with the entry state named by the caller instead of by
/// `PSOXIDE_ENTRY_STATE`. [`EntryState::Default`] is the retail-modelled boot,
/// byte for byte what [`fast_boot_disc`] did before entry states existed.
///
/// # Errors
///
/// The disc has no bootable `SYSTEM.CNF` or executable.
pub fn fast_boot_disc_with(
    bus: &mut Bus,
    cpu: &mut Cpu,
    disc: &Disc,
    state: &EntryState,
) -> Result<DiscFastBootInfo, BootError> {
    let plan = state.plan(disc.sector_count() as u32);
    if let Some(plan) = &plan {
        eprintln!("{}", plan.describe());
    }
    let boot = load_disc_boot(disc)?;
    let payload_len = boot.exe.payload.len();
    // The loader reads the EXE and stops: the game's first seek starts from
    // the sector after it (measured at entry under a real BIOS).
    let (exe_lba, exe_size) = crate::system_cnf::file_extent(disc, &boot.cnf.boot_path)?;
    bus.cdrom.park_head(exe_lba + exe_size.div_ceil(2048));
    let (sp, fp) = boot.cnf.entry_stack();

    let filled = plan.as_ref().is_some_and(|plan| plan.fill_memory(bus));
    if !filled {
        bus.clear_ram_range(0x8001_0000, sp);
    }
    bus.load_exe_payload(boot.exe.load_addr, &boot.exe.payload);
    bus.clear_exe_bss(boot.exe.bss_addr, boot.exe.bss_size);
    if let Some(arg) = boot.cnf.boot_arg_bytes() {
        for (offset, byte) in arg.into_iter().enumerate() {
            bus.write8_safe(BOOT_ARG_ADDR + offset as u32, byte);
        }
    }
    match &plan {
        None => {
            apply_hle_entry_state(bus);
            // The entry state measured under the retail kernel has the display
            // enabled (GPUSTAT bit 23 clear; see the entry state test below), and
            // some retail games rely on inheriting that instead of issuing GP1(03h)
            // themselves during early startup.
            bus.write32(GP1_ADDR, 0x0300_0000);
        }
        Some(plan) => plan.apply(bus),
    }
    cpu.seed_from_exe_with_args(boot.exe.initial_pc, boot.exe.initial_gp, Some(sp), 1, 0);
    cpu.seed_frame_pointer(fp);
    bus.enable_hle_bios_with(crate::hle_kernel::KernelConfig {
        tcb: boot.cnf.tcb,
        event: boot.cnf.event,
        stack: boot.cnf.stack,
    });
    if let Some(plan) = &plan {
        plan.apply_cpu(cpu);
        plan.apply_vector(bus);
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
const DPCR_ADDR: u32 = 0x1f80_10f0;
/// Interrupt mask register.
const I_MASK_ADDR: u32 = 0x1f80_1074;
/// GPU command port.
const GP0_ADDR: u32 = 0x1f80_1810;

/// Hardware state a disc executable inherits from a real boot, for the HLE
/// HLE boot. Every value was constant across the 38 census runs (31 discs)
/// captured at EXE entry under a retail BIOS during development; see
/// docs/hle-bios-provenance.md.
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
    bus.set_dicr_raw(0x8c8c_0000);
    // CD-ROM and DMA interrupts unmasked.
    bus.write32(I_MASK_ADDR, 0x0000_000c);
    bus.apply_hle_entry_audio_profile();
    // Shell display: 640x480 interlaced, 15-bit, NTSC (an NTSC BIOS leaves
    // this even for PAL discs), with dithering and drawing to the displayed
    // field enabled.
    bus.write32(GP1_ADDR, 0x0800_0027);
    bus.write32(GP0_ADDR, 0xe100_0600);
}

#[cfg(test)]
mod tests {
    use super::*;
    use psx_iso::{IsoBuilder, EXE_HEADER_BYTES};

    fn disc(system_cnf: &[u8]) -> Disc {
        let mut exe = vec![0u8; EXE_HEADER_BYTES];
        exe[..8].copy_from_slice(b"PS-X EXE");
        exe[0x10..0x14].copy_from_slice(&0x8001_0000u32.to_le_bytes());
        exe[0x18..0x1c].copy_from_slice(&0x8001_0000u32.to_le_bytes());
        exe[0x1c..0x20].copy_from_slice(&4u32.to_le_bytes());
        // Header stack fields are ignored on disc boot.
        exe[0x30..0x34].copy_from_slice(&0x801f_0000u32.to_le_bytes());
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
                0x801f_fff0,
                0x801f_fff0,
            ),
            (b"BOOT = cdrom:\\GAME.EXE;1\r\n", 0x801f_ff00, 0x801f_ff00),
            (
                b"BOOT = cdrom:\\GAME.EXE;1\r\nSTACK = 0x801FFFF0\r\n",
                0x801f_fdd8,
                0x801f_ff00,
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
        assert_eq!(bus.read32(0x1f80_10f4), 0x8c8c_0000);
        assert_eq!(bus.read32(I_MASK_ADDR) & 0xffff, 0x000c);
        assert_eq!(bus.read32(0x60), 2);
        assert_eq!(bus.read16(0x1f80_1d80), 0x3fff);
        assert_eq!(bus.read16(0x1f80_1d82), 0x37ef);
        assert_eq!(bus.read16(0x1f80_1daa), 0xc085);
        assert_eq!(bus.read16(0x1f80_1dac), 0x0004);
        assert_eq!(bus.read16(0x1f80_1db0), 0);
        let gpustat = bus.read32(0x1f80_1814);
        // 640x480 interlaced, NTSC, display on, dither and draw-to-display.
        assert_eq!(gpustat & 0x00ff_0600, 0x004e_0600);
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
    fn an_exe_the_linker_left_short_of_its_last_sector_boots_with_a_zero_tail() {
        // psoxide.ld starts .bss where .data ends, so the file `ld.lld` writes
        // stops inside the last sector `t_size` counts. A disc builder that
        // skipped the post-link pad stores it as is; the loader sees the
        // sector's remaining bytes as zeros, which is what padding writes.
        let mut exe = vec![0u8; EXE_HEADER_BYTES];
        exe[..8].copy_from_slice(b"PS-X EXE");
        exe[0x10..0x14].copy_from_slice(&0x8001_0000u32.to_le_bytes());
        exe[0x18..0x1c].copy_from_slice(&0x8001_0000u32.to_le_bytes());
        exe[0x1c..0x20].copy_from_slice(&2048u32.to_le_bytes());
        exe.extend_from_slice(&[0x5a; 1500]);
        let mut builder = IsoBuilder::new();
        builder.add_file("SYSTEM.CNF", b"BOOT = cdrom:\\GAME.EXE;1\r\n".to_vec());
        builder.add_file("GAME.EXE", exe);
        let disc = Disc::from_bin(builder.build_bin());

        let mut bus = Bus::new_without_bios();
        let mut cpu = Cpu::new();
        let info = fast_boot_disc(&mut bus, &mut cpu, &disc).unwrap();
        assert_eq!(info.payload_len, 2048);
        assert_eq!(bus.try_read8(0x8001_0000 + 1499), Some(0x5a));
        assert_eq!(bus.try_read8(0x8001_0000 + 1500), Some(0));
        assert_eq!(bus.try_read8(0x8001_0000 + 2047), Some(0));
    }

    /// A disc whose executable is `words`, loaded and entered at 0x80010000.
    fn disc_running(words: &[u32]) -> Disc {
        let mut exe = vec![0u8; EXE_HEADER_BYTES];
        exe[..8].copy_from_slice(b"PS-X EXE");
        exe[0x10..0x14].copy_from_slice(&0x8001_0000u32.to_le_bytes());
        exe[0x18..0x1c].copy_from_slice(&0x8001_0000u32.to_le_bytes());
        let mut payload: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        payload.resize(payload.len().div_ceil(2048) * 2048, 0);
        exe[0x1c..0x20].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        exe.extend_from_slice(&payload);
        let mut builder = IsoBuilder::new();
        builder.add_file("SYSTEM.CNF", b"BOOT = cdrom:\\GAME.EXE;1\r\n".to_vec());
        builder.add_file("GAME.EXE", exe);
        Disc::from_bin(builder.build_bin())
    }

    /// EnterCriticalSection (a SYSCALL), then `t0 = 0xdeadbeef` and a spin.
    /// Reaching the spin means the exception went through the vector to a
    /// handler that returned.
    fn syscall_program(prologue: impl FnOnce(&mut crate::hle_asm::Asm)) -> Vec<u32> {
        use crate::hle_asm::*;
        let mut a = Asm::new(0x8001_0000);
        prologue(&mut a);
        a.li(A0, 1);
        a.syscall();
        a.nop();
        a.li(T0, 0xdead_beef);
        a.label("spin");
        a.b("spin");
        a.nop();
        a.finish()
    }

    /// Boot `words` under `state` and run a few thousand instructions.
    fn boot_and_run(words: &[u32], state: &EntryState) -> (Bus, Cpu) {
        let mut bus = Bus::new_without_bios();
        let mut cpu = Cpu::new();
        let disc = disc_running(words);
        fast_boot_disc_with(&mut bus, &mut cpu, &disc, state).unwrap();
        bus.cdrom.insert_disc(Some(disc));
        for _ in 0..4000 {
            cpu.step(&mut bus).unwrap();
        }
        (bus, cpu)
    }

    fn spin_address(words: &[u32]) -> u32 {
        0x8001_0000 + 4 * (words.len() as u32 - 2)
    }

    #[test]
    fn the_default_state_is_the_boot_that_existed_before_entry_states() {
        let disc = disc(b"BOOT = cdrom:\\GAME.EXE;1\r\n");
        let mut one = (Bus::new_without_bios(), Cpu::new());
        let mut two = (Bus::new_without_bios(), Cpu::new());
        fast_boot_disc(&mut one.0, &mut one.1, &disc).unwrap();
        fast_boot_disc_with(&mut two.0, &mut two.1, &disc, &EntryState::Default).unwrap();
        for addr in (0..0x1_0000u32)
            .step_by(4)
            .chain([0x8001_0000, 0x801f_0000])
        {
            assert_eq!(one.0.read32(addr), two.0.read32(addr), "{addr:#x}");
        }
        for reg in [
            DPCR_ADDR,
            0x1f80_10f4,
            I_MASK_ADDR,
            0x1f80_1814,
            0x1f80_1d80,
            0x1f80_1daa,
        ] {
            assert_eq!(one.0.read32(reg), two.0.read32(reg), "{reg:#x}");
        }
    }

    #[test]
    fn openbios_entry_state_has_the_modelled_values() {
        let mut bus = Bus::new_without_bios();
        let mut cpu = Cpu::new();
        let state = EntryState::from_spec("openbios").unwrap();
        let disc = disc_running(&[0; 4]);
        fast_boot_disc_with(&mut bus, &mut cpu, &disc, &state).unwrap();
        bus.cdrom.insert_disc(Some(disc));
        let base = crate::entry_state::OPENBIOS_HANDLER_BASE;
        // lui k0,0; addiu k0,k0,base; jr k0; nop
        assert_eq!(bus.read32(0x80), 0x3c1a_0000);
        assert_eq!(bus.read32(0x84), 0x275a_0000 | base);
        assert_eq!(bus.read32(0x88), 0x0340_0008);
        assert_eq!(bus.read32(0x8c), 0);
        assert_eq!(bus.read32(crate::hle_kernel::C0_TABLE + 4 * 6), base);
        // The retail handler address spins; the handler is where C(06h) says.
        assert_eq!(bus.read32(0xc80), 0x1000_ffff);
        assert_ne!(bus.read32(base), 0x1000_ffff);
        assert_eq!(bus.read32(DPCR_ADDR), 0x076f_c321);
        assert_eq!(bus.read32(0x1f80_10f4), 0x0088_0000);
        assert_eq!(bus.read32(I_MASK_ADDR) & 0xffff, 0x000c);
        assert_eq!(bus.read16(0x1f80_1d80), 0x3800);
        assert_eq!(bus.read16(0x1f80_1d82), 0x3800);
        assert_eq!(bus.read16(0x1f80_1daa), 0xc000);
        assert_eq!(bus.read16(0x1f80_1db0), 0);
        assert_eq!(bus.read16(0x1f80_1db4), 0);
        assert_eq!(bus.cdrom.debug_mode(), 0x00);
        // RAM outside the program is not zeroed.
        assert_eq!(bus.read32(0x8002_0000), 0xaaaa_aaaa);
        assert_eq!(bus.read32(0x801f_0000), 0xaaaa_aaaa);
        assert!(!bus.external_interrupt_pending());
    }

    #[test]
    fn exceptions_reach_the_handler_in_every_state_that_moves_it() {
        let words = syscall_program(|_| {});
        let spin = spin_address(&words);
        for spec in [
            "default",
            "openbios",
            "hostile:1",
            "hostile:2",
            "hostile:3",
            "hostile:4",
        ] {
            let state = EntryState::from_spec(spec).unwrap();
            let (_, cpu) = boot_and_run(&words, &state);
            assert_eq!(cpu.pc() & !0x2000_0000, spin, "{spec}");
            assert_eq!(cpu.gpr(8), 0xdead_beef, "{spec}");
        }
        for seed in 5..40 {
            let state = EntryState::from_spec(&format!("hostile:{seed}")).unwrap();
            let (_, cpu) = boot_and_run(&words, &state);
            assert_eq!(cpu.pc() & !0x2000_0000, spin, "hostile:{seed}");
        }
    }

    #[test]
    fn a_hand_written_retail_vector_spins_where_the_handler_is_not() {
        // kernel_timing writes the retail vector words itself; under another
        // BIOS they lead to 0xC80, which holds something else there.
        let words = syscall_program(|a| {
            use crate::hle_asm::*;
            a.li(T1, 0x8000_0080);
            for word in [0x3c1a_0000u32, 0x275a_0c80, 0x0340_0008, 0] {
                a.li(T2, word);
                a.sw(T2, 0, T1);
                a.addiu(T1, T1, 4);
            }
        });
        let spin = spin_address(&words);
        let (_, cpu) = boot_and_run(&words, &EntryState::Default);
        assert_eq!(cpu.pc() & !0x2000_0000, spin);
        let (_, cpu) = boot_and_run(&words, &EntryState::from_spec("openbios").unwrap());
        assert!(
            (0xc80..0xf00).contains(&(cpu.pc() & 0x1fff_ffff)),
            "{:#x}",
            cpu.pc()
        );
        assert_ne!(cpu.gpr(8), 0xdead_beef);
    }

    #[test]
    fn skipping_every_category_is_the_default_boot() {
        let skip_all = "hostile:3,skip=vector,skip=dma,skip=irq,skip=spu,skip=cd,skip=ram,\
                        skip=vram,skip=scratch,skip=gpu,skip=timers,skip=cpu";
        let words = syscall_program(|_| {});
        let (mut a, _) = boot_and_run(&words, &EntryState::Default);
        let (mut b, _) = boot_and_run(&words, &EntryState::from_spec(skip_all).unwrap());
        for addr in (0..0x1_0000u32)
            .step_by(4)
            .chain([0x8002_0000, 0x801f_0000])
        {
            assert_eq!(a.read32(addr), b.read32(addr), "{addr:#x}");
        }
        for reg in [
            DPCR_ADDR,
            0x1f80_10f4,
            I_MASK_ADDR,
            0x1f80_1814,
            0x1f80_1d80,
            0x1f80_1daa,
        ] {
            assert_eq!(a.read32(reg), b.read32(reg), "{reg:#x}");
        }
        assert_eq!(a.cdrom.debug_mode(), b.cdrom.debug_mode());
    }

    #[test]
    fn hostile_randomises_ram_and_leaves_the_program_alone() {
        let words = syscall_program(|_| {});
        let (mut one, _) = boot_and_run(&words, &EntryState::from_spec("hostile:1").unwrap());
        let (mut two, _) = boot_and_run(&words, &EntryState::from_spec("hostile:2").unwrap());
        assert_ne!(one.read32(0x8003_0000), two.read32(0x8003_0000));
        assert_eq!(one.read32(0x8001_0000), two.read32(0x8001_0000));
        assert_ne!(one.read32(0x8003_0000), 0);
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
