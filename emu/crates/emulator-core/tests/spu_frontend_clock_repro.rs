//! Investigation regressions: frontend drain cadence must not change guest-visible SPU time.
use emulator_core::{spu, Bus};
fn mixed_volume_transition(eager: bool) -> Vec<(i16, i16)> {
    let mut b = Bus::new_without_bios();
    b.spu.feed_cd_audio(&[(12000, 8000); 8]);
    b.write16(spu::SPUCNT, 0xc001);
    b.write16(spu::CD_VOL_L, 0x7fff);
    b.write16(spu::CD_VOL_R, 0x7fff);
    b.write16(spu::MAIN_VOL_L, 0x7fff);
    b.write16(spu::MAIN_VOL_R, 0x7fff);
    b.tick((spu::SAMPLE_CYCLES * 4) as u32);
    if eager {
        b.run_spu_to_current_cycle();
    }
    b.write16(spu::MAIN_VOL_L, 0);
    b.write16(spu::MAIN_VOL_R, 0);
    b.tick((spu::SAMPLE_CYCLES * 4) as u32);
    b.run_spu_to_current_cycle();
    b.spu.drain_audio()
}
#[test]
fn volume_write_cannot_retroactively_mute_elapsed_audio() {
    let eager = mixed_volume_transition(true);
    let lazy = mixed_volume_transition(false);
    eprintln!("eager={eager:?} lazy={lazy:?}");
    assert!(eager[..4].iter().all(|s| s.0 > 0));
    assert_eq!(lazy, eager);
}
#[test]
fn irq_read_observes_elapsed_spu_clock_without_frontend_pump() {
    let mut b = Bus::new_without_bios();
    b.write16(spu::IRQ_ADDR, 0x80);
    b.write16(spu::SPUCNT, 1 << 6);
    b.tick(spu::SAMPLE_CYCLES as u32);
    let lazy = b.read16(spu::SPUSTAT);
    b.run_spu_to_current_cycle();
    let eager = b.read16(spu::SPUSTAT);
    eprintln!("lazy={lazy:#06x} eager={eager:#06x}");
    assert_eq!(lazy, eager);
    assert_ne!(eager & (1 << 6), 0);
}
#[test]
fn inclusive_clock_runs_without_frontend_and_same_cycle_pumps_are_idempotent() {
    let mut b = Bus::new_without_bios();
    b.tick(767);
    assert_eq!(b.spu.samples_produced(), 0);
    b.tick(1);
    assert_eq!(b.spu.samples_produced(), 1);
    assert_eq!(b.run_spu_to_current_cycle(), 0);
    b.tick(0);
    assert_eq!(b.spu.samples_produced(), 1);
}
#[test]
fn all_spu_mmio_widths_catch_up_after_an_intra_instruction_stall() {
    for width in [1, 2, 4] {
        for writing in [false, true] {
            let mut b = Bus::new_without_bios();
            b.add_cycles(768);
            assert_eq!(b.spu.samples_produced(), 0);
            match (width, writing) {
                (1, false) => {
                    b.read8(spu::SPUSTAT);
                }
                (2, false) => {
                    b.read16(spu::SPUSTAT);
                }
                (4, false) => {
                    b.read32(spu::SPUSTAT);
                }
                (1, true) => b.write8(spu::MAIN_VOL_L, 0),
                (2, true) => b.write16(spu::MAIN_VOL_L, 0),
                (4, true) => b.write32(spu::MAIN_VOL_L, 0),
                _ => unreachable!(),
            }
            assert_eq!(
                b.spu.samples_produced(),
                1,
                "width {width}, write {writing}"
            );
            assert_eq!(b.run_spu_to_current_cycle(), 0);
        }
    }
}
#[test]
fn due_spu_irq_is_observed_before_acknowledgement() {
    let mut b = Bus::new_without_bios();
    b.write16(spu::IRQ_ADDR, 0x80);
    b.write16(spu::SPUCNT, 1 << 6);
    b.write32(0x1f801074, 1 << 9);
    b.add_cycles(768);
    assert!(b.external_interrupt_pending());
    assert_ne!(b.read32(0x1f801070) & (1 << 9), 0);
    b.write16(spu::SPUCNT, 0);
    b.write32(0x1f801070, !(1 << 9));
    assert_eq!(b.read32(0x1f801070) & (1 << 9), 0);
    b.run_spu_to_current_cycle();
    assert_eq!(b.read32(0x1f801070) & (1 << 9), 0);
}
#[test]
fn restored_serialized_deadline_needs_no_spu_scheduler_slot() {
    use emulator_core::snapshot::{EmulatorState, EmulatorStateRef};
    use emulator_core::Cpu;
    use psoxide_settings::savestate::SaveStateV1;
    let cpu = Cpu::new();
    let mut b = Bus::new_without_bios();
    b.tick(511);
    let bytes = SaveStateV1::new(EmulatorStateRef { cpu: &cpu, bus: &b }, "spu-deadline", 0)
        .to_bytes()
        .unwrap();
    let state: SaveStateV1<EmulatorState> = SaveStateV1::from_bytes(&bytes).unwrap();
    let mut restored = state.payload.bus;
    restored.tick(256);
    assert_eq!(restored.spu.samples_produced(), 0);
    restored.tick(1);
    assert_eq!(restored.spu.samples_produced(), 1);
    assert_eq!(restored.run_spu_to_current_cycle(), 0);
}
fn dma_capture_transfer(eager: bool, to_spu: bool) -> u32 {
    let mut b = Bus::new_without_bios();
    b.spu.feed_cd_audio(&[(12000, 8000); 4]);
    b.write16(spu::CD_VOL_L, 0x7fff);
    b.write16(spu::CD_VOL_R, 0x7fff);
    b.write16(spu::TRANSFER_ADDR, 0);
    b.write16(spu::SPUCNT, if to_spu { 2 << 4 } else { 3 << 4 });
    b.write32(0x1f801014, 0x200931e1);
    b.write32(0x1f8010f0, 1 << 19);
    b.write32(0x100, 0x12345678);
    b.write32(0x1f8010c0, 0x100);
    b.write32(0x1f8010c4, 1);
    b.add_cycles(768);
    if eager {
        b.run_spu_to_current_cycle();
    }
    b.write32(0x1f8010c8, 0x11000000 | u32::from(to_spu));
    b.run_spu_to_current_cycle();
    if to_spu {
        u32::from(b.spu.ram_halfwords()[0]) | u32::from(b.spu.ram_halfwords()[1]) << 16
    } else {
        b.read32(0x100)
    }
}
#[test]
fn spu_dma_reads_fresh_capture_and_writes_after_elapsed_capture() {
    for to_spu in [false, true] {
        let eager = dma_capture_transfer(true, to_spu);
        let lazy = dma_capture_transfer(false, to_spu);
        assert_eq!(lazy, eager, "to_spu={to_spu}");
        if to_spu {
            assert_eq!(lazy, 0x12345678);
        } else {
            assert_ne!(lazy, 0);
        }
    }
}
#[test]
fn acknowledgement_catches_up_before_clearing_a_due_irq() {
    let mut b = Bus::new_without_bios();
    b.write16(spu::IRQ_ADDR, 0x80);
    b.write16(spu::SPUCNT, 1 << 6);
    b.add_cycles(768);
    // No prior read or interrupt query may do the catch-up for this write.
    b.write32(0x1f801070, !(1 << 9));
    assert_eq!(b.spu.samples_produced(), 1);
    assert_eq!(b.read32(0x1f801070) & (1 << 9), 0);
    assert_ne!(b.read16(spu::SPUSTAT) & (1 << 6), 0);
}

#[test]
fn already_produced_audio_is_available_when_catchup_produces_nothing() {
    let mut bus = Bus::new_without_bios();
    bus.tick(768);
    // A frontend must drain the pending output, not gate the drain on how
    // many additional samples this optional catch-up call produced.
    assert_eq!(bus.run_spu_to_current_cycle(), 0);
    assert_eq!(bus.spu.audio_queue_len(), 1);
    assert_eq!(bus.spu.drain_audio().len(), 1);
    assert_eq!(bus.spu.audio_queue_len(), 0);
}
