use super::*;

fn write_adpcm_block(spu: &mut Spu, byte_addr: u32, block: &[u8; 16]) {
    for i in 0..8 {
        let lo = block[i * 2] as u16;
        let hi = block[i * 2 + 1] as u16;
        spu.ram[((byte_addr as usize + i * 2) / 2) & (SPU_RAM_HALFWORDS - 1)] = lo | (hi << 8);
    }
}

// -- address range --

#[test]
fn contains_covers_whole_range() {
    assert!(Spu::contains(SPU_BASE));
    assert!(Spu::contains(SPU_END - 1));
    assert!(!Spu::contains(SPU_END));
    assert!(!Spu::contains(SPU_BASE - 1));
}

// -- register round-trip --

#[test]
fn spucnt_round_trip_and_spustat_mirror() {
    let mut s = Spu::new();
    s.write16(SPUCNT, 0x8010);
    assert_eq!(s.spucnt(), 0x8010);
    // SPUSTAT lower 6 bits mirror SPUCNT lower 6 bits.
    assert_eq!(s.spustat() & 0x3F, 0x10);
}

#[test]
fn spustat_read_only_drops_writes() {
    let mut s = Spu::new();
    s.write16(SPUCNT, 0x8010);
    s.write16(SPUSTAT, 0xFFFF);
    assert_eq!(s.spustat() & 0x3F, 0x10);
}

#[test]
fn retail_bios_shell_audio_profile_matches_pa5_hardware_snapshot() {
    let mut s = Spu::new();
    s.apply_retail_bios_shell_audio_profile();

    assert_eq!(s.read16(SPUCNT), 0xC085);
    assert_eq!(s.read16(SPUSTAT), 0x0805);
    assert_eq!(s.read16(REVERB_VOL_L), 0x5EBC);
    assert_eq!(s.read16(REVERB_VOL_R), 0x5EBC);
    assert_eq!(s.read16(REVERB_BASE), 0xE128);
    assert_eq!(s.read16(EON_LO), 0xFFFF);
    assert_eq!(s.read16(EON_HI), 0x00FF);
    assert_eq!(s.read16(REVERB_CFG_BASE), 0x033D);
    assert_eq!(s.read16(REVERB_CFG_BASE + 62), 0x8000);
}

#[test]
fn voice_bank_round_trips_volume_pitch_start_loop() {
    let mut s = Spu::new();
    let base = VOICE_BASE + 5 * 16;
    s.write16(base + voice_offset::VOLUME_L, 0x3FFF);
    s.write16(base + voice_offset::VOLUME_R, 0x1234);
    s.write16(base + voice_offset::PITCH, 0x1000);
    s.write16(base + voice_offset::START_ADDR, 0x0020);
    s.write16(base + voice_offset::REPEAT_ADDR, 0x0040);
    s.write16(base + voice_offset::ADSR_LO, 0x80FF);
    s.write16(base + voice_offset::ADSR_HI, 0x1F20);
    assert_eq!(s.read16(base + voice_offset::VOLUME_L), 0x3FFF);
    assert_eq!(s.read16(base + voice_offset::VOLUME_R), 0x1234);
    assert_eq!(s.read16(base + voice_offset::PITCH), 0x1000);
    assert_eq!(s.read16(base + voice_offset::START_ADDR), 0x0020);
    assert_eq!(s.read16(base + voice_offset::REPEAT_ADDR), 0x0040);
    assert_eq!(s.read16(base + voice_offset::ADSR_LO), 0x80FF);
    assert_eq!(s.read16(base + voice_offset::ADSR_HI), 0x1F20);
}

#[test]
fn voice_adsr_current_reads_live_envelope() {
    // ENVX exposes the live envelope level (hardware behavior). Games
    // poll it -- some titles spin until every voice reaches 0
    // before advancing past its intro -- so it must reflect the real
    // envelope, not a pinned constant. See `read_voice_reg`.
    let mut s = Spu::new();
    s.voices[0].phase = AdsrPhase::Attack;
    s.voices[0].envelope = 0x4000;
    assert_eq!(s.read16(VOICE_BASE + voice_offset::ADSR_CURRENT), 0x4000);
    s.voices[0].envelope = 0;
    assert_eq!(s.read16(VOICE_BASE + voice_offset::ADSR_CURRENT), 0);
}

#[test]
fn voice_adsr_current_manual_write_preserves_signed_16_bit_value() {
    let mut s = Spu::new();
    let envx = VOICE_BASE + voice_offset::ADSR_CURRENT;

    s.write16(envx, 0xFFFF);
    assert_eq!(s.read16(envx), 0xFFFF);
    assert_eq!(s.voices[0].envelope, -1);

    s.write16(envx, 0x8000);
    assert_eq!(s.read16(envx), 0x8000);
    assert_eq!(s.voices[0].envelope, i16::MIN as i32);

    // An inactive ADSR generator does not erase a manual ENVX write.
    s.voices[0].step_envelope();
    assert_eq!(s.read16(envx), 0x8000);
}

#[test]
fn current_main_volume_regs_do_not_mirror_main_volume() {
    let mut s = Spu::new();
    s.write16(MAIN_VOL_L, 0x3FFF);
    s.write16(MAIN_VOL_R, 0x2000);

    assert_eq!(s.read16(MAIN_VOL_L), 0x3FFF);
    assert_eq!(s.read16(MAIN_VOL_R), 0x2000);
    assert_eq!(s.read16(CURRENT_MAIN_VOL_L), 0);
    assert_eq!(s.read16(CURRENT_MAIN_VOL_R), 0);

    s.write16(CURRENT_MAIN_VOL_L, 0x1111);
    s.write16(CURRENT_MAIN_VOL_R, 0x2222);
    assert_eq!(s.read16(CURRENT_MAIN_VOL_L), 0x1111);
    assert_eq!(s.read16(CURRENT_MAIN_VOL_R), 0x2222);
    assert_eq!(s.read16(MAIN_VOL_L), 0x3FFF);
    assert_eq!(s.read16(MAIN_VOL_R), 0x2000);
}

// -- KON / KOFF / ENDX --

#[test]
fn kon_queues_and_apply_starts_voice() {
    let mut s = Spu::new();
    s.write16(KON_LO, 0x0005); // voices 0 and 2
    assert_eq!(s.kon_pending, 0x5);
    s.apply_kon_koff();
    assert_eq!(s.voices[0].phase, AdsrPhase::Attack);
    assert_eq!(s.voices[2].phase, AdsrPhase::Attack);
    assert_eq!(s.voices[1].phase, AdsrPhase::Off);
}

#[test]
fn kon_koff_reads_round_trip_after_tick_drains_pending() {
    // Real hardware: KON/KOFF reads return what was written, even
    // after the SPU has consumed the pending bits internally. The
    // BIOS's SPU-init probe writes 0xFFFF to KOFF then reads it
    // back -- we must echo the written value.
    let mut s = Spu::new();
    s.write16(KON_LO, 0xFFFF);
    s.write16(KON_HI, 0x00FF);
    s.write16(KOFF_LO, 0xFFFF);
    s.write16(KOFF_HI, 0x00FF);
    // Drain pending by ticking the SPU.
    s.tick_sample(0);
    // Reads must still return the raw written values.
    assert_eq!(s.read16(KON_LO), 0xFFFF);
    assert_eq!(s.read16(KON_HI), 0x00FF);
    assert_eq!(s.read16(KOFF_LO), 0xFFFF);
    assert_eq!(s.read16(KOFF_HI), 0x00FF);
}

#[test]
fn koff_queues_and_transitions_to_release() {
    let mut s = Spu::new();
    s.voices[3].phase = AdsrPhase::Sustain;
    s.write16(KOFF_LO, 1 << 3);
    s.apply_kon_koff();
    assert_eq!(s.voices[3].phase, AdsrPhase::Release);
}

#[test]
fn kon_wins_over_same_sample_koff() {
    let mut s = Spu::new();
    s.write16(KOFF_LO, 0x0001);
    s.write16(KON_LO, 0x0001);
    s.tick_sample(SAMPLE_CYCLES);

    assert_eq!(
        s.voices[0].phase,
        AdsrPhase::Attack,
        "a key-on in the same batch as a key-off must not release the voice"
    );
}

#[test]
fn endx_write_one_clears_bits() {
    let mut s = Spu::new();
    s.endx_latched = 0xFFFF_FFFF;
    s.write16(ENDX_LO, 0x00F0);
    assert_eq!(s.endx_latched & 0xFFFF, 0xFF0F);
}

#[test]
fn kon_clears_endx_for_started_voices() {
    let mut s = Spu::new();
    s.endx_latched = 0xFFFF_FFFF;
    s.write16(KON_LO, 0x0003);
    // KON queue path clears ENDX immediately for those bits.
    assert_eq!(s.endx_latched & 0x3, 0);
    // Other bits untouched.
    assert_eq!(s.endx_latched >> 2, 0x3FFF_FFFF);
}

// -- Transfer FIFO --

#[test]
fn transfer_fifo_writes_into_spu_ram_and_advances() {
    let mut s = Spu::new();
    s.write16(SPUCNT, 1 << 4); // arm Manual-Write transfer mode
    s.tick_sample(SAMPLE_CYCLES);
    s.write16(TRANSFER_ADDR, 0x0010); // 0x10 * 8 = 0x80 bytes
    s.write16(TRANSFER_FIFO, 0xBEEF);
    s.write16(TRANSFER_FIFO, 0xCAFE);
    assert_eq!(s.ram[0x80 >> 1], 0xBEEF);
    assert_eq!(s.ram[(0x80 >> 1) + 1], 0xCAFE);
    // Transfer addr advanced by 4 bytes.
    assert_eq!(s.transfer_addr, 0x84);
}

#[test]
fn stopped_mode_fifo_write_waits_for_manual_transfer() {
    // JaCzekanski's public real-hardware memory-transfer test fills the FIFO
    // first and only then selects ManualWrite. The data must remain queued
    // while stopped and drain in order when mode 1 is selected.
    let mut s = Spu::new();
    s.write16(TRANSFER_ADDR, 0x0010); // 0x80 bytes; mode still Stop (0)
    s.write16(TRANSFER_FIFO, 0xBEEF);
    s.write16(TRANSFER_FIFO, 0xCAFE);
    assert_eq!(
        s.ram[0x80 >> 1],
        0,
        "Stop-mode FIFO data must wait for a transfer mode"
    );
    s.write16(SPUCNT, 1 << 4);
    s.tick_sample(SAMPLE_CYCLES);
    assert_eq!(s.ram[0x80 >> 1], 0xBEEF);
    assert_eq!(s.ram[(0x80 >> 1) + 1], 0xCAFE);
    assert_eq!(s.transfer_addr, 0x84);
}

#[test]
fn stopped_mode_transfer_fifo_is_bounded_to_32_halfwords() {
    let mut s = Spu::new();
    s.write16(TRANSFER_ADDR, 0x0010);
    for value in 0..40u16 {
        s.write16(TRANSFER_FIFO, 0x4000 + value);
    }
    s.write16(SPUCNT, 1 << 4);
    s.tick_sample(SAMPLE_CYCLES);
    for i in 0..32usize {
        assert_eq!(s.ram[(0x80 >> 1) + i], 0x4000 + i as u16);
    }
    assert_eq!(s.ram[(0x80 >> 1) + 32], 0);
}

#[test]
fn dma_write_fills_spu_ram_contiguously() {
    let mut s = Spu::new();
    s.write16(TRANSFER_ADDR, 0); // byte addr 0
    let payload: Vec<u16> = (0..16).map(|i| 0x1000 + i).collect();
    s.dma_write(&payload);
    for (i, w) in payload.iter().enumerate() {
        assert_eq!(s.ram[i], *w);
    }
}

// -- IRQ on address match --

#[test]
fn irq_fires_when_transfer_hits_irq_addr_with_enable() {
    let mut s = Spu::new();
    s.write16(SPUCNT, (1 << 6) | (1 << 4)); // IRQ enable + Manual-Write transfer mode
    s.tick_sample(SAMPLE_CYCLES);
    s.write16(IRQ_ADDR, 0x0010); // 0x10 * 8 = 0x80 bytes
    s.write16(TRANSFER_ADDR, 0x0010);
    s.write16(TRANSFER_FIFO, 0xAAAA);
    assert!(s.take_irq_pending());
    // Subsequent read clears the pending flag.
    assert!(!s.take_irq_pending());
    // STATUS bit 6 is latched.
    assert_ne!(s.spustat() & (1 << 6), 0);
}

#[test]
fn irq_does_not_fire_without_enable_bit() {
    let mut s = Spu::new();
    s.write16(IRQ_ADDR, 0x0010);
    s.write16(TRANSFER_ADDR, 0x0010);
    s.write16(TRANSFER_FIFO, 0xAAAA);
    assert!(!s.take_irq_pending());
}

#[test]
fn decoded_buffer_irq_fires_in_low_capture_banks() {
    let mut s = Spu::new();
    s.write16(SPUCNT, (1 << 6) | (1 << 4)); // IRQ enable + Manual-Write transfer mode
    s.write16(IRQ_ADDR, 0x0080); // 0x80 * 8 = 0x400 bytes

    s.tick_sample(0);

    assert!(s.take_irq_pending());
    assert_ne!(s.spustat() & (1 << 6), 0);
}

#[test]
fn decoded_buffer_irq_fires_at_zero_irq_address() {
    // Address 0 (CD-left capture buffer start) is a valid SPU IRQ
    // target. CTR arms its SCEA advance-timer SPU IRQ there; gating
    // it off (the old behavior) froze the game on the SCEA screen.
    // SPUCNT bit 6 already guards against firing before a game arms.
    let mut s = Spu::new();
    s.write16(SPUCNT, (1 << 6) | (1 << 4)); // IRQ enable + Manual-Write transfer mode
    s.write16(IRQ_ADDR, 0x0000);

    s.tick_sample(0);

    assert!(s.take_irq_pending());
    assert_ne!(s.spustat() & (1 << 6), 0);
}

#[test]
fn clearing_spucnt_irq_enable_acks_status_bit() {
    let mut s = Spu::new();
    s.write16(SPUCNT, (1 << 6) | (1 << 4)); // IRQ enable + Manual-Write transfer mode
    s.tick_sample(SAMPLE_CYCLES);
    s.write16(IRQ_ADDR, 0x0010);
    s.write16(TRANSFER_ADDR, 0x0010);
    s.write16(TRANSFER_FIFO, 0x1234);
    assert_ne!(s.spustat & (1 << 6), 0);
    s.write16(SPUCNT, 0); // drop IRQ enable
    assert_eq!(s.spustat & (1 << 6), 0);
}

// -- DMA enable gating --

#[test]
fn dma_transfer_enabled_reads_spucnt_bits_5_4() {
    let mut s = Spu::new();
    s.write16(SPUCNT, 0); // Stop
    assert!(!s.dma_transfer_enabled());
    s.write16(SPUCNT, 1 << 4); // ManualWrite
    assert!(!s.dma_transfer_enabled());
    s.write16(SPUCNT, 2 << 4); // DMA write
    assert!(s.dma_transfer_enabled());
    s.write16(SPUCNT, 3 << 4); // DMA read
    assert!(s.dma_transfer_enabled());
}

// -- ADPCM decoder --

#[test]
fn adpcm_silence_block_decodes_to_zero_samples() {
    let mut s = Spu::new();
    // Shift=0, predictor=0, flags=0. All zero block = 28 zero samples.
    write_adpcm_block(&mut s, 0x20, &[0; 16]);
    s.voices[0].current_addr = 0x20;
    s.decode_next_block(0);
    assert_eq!(s.voices[0].sample_buf, [0; 28]);
    assert_eq!(s.voices[0].current_addr, 0x30);
}

#[test]
fn adpcm_decode_shifts_the_scaled_nibble_right() {
    let mut s = Spu::new();
    let mut block = [0u8; 16];
    // Predictor 0, shift 0. The first packed byte contains two
    // signed 4-bit samples: +1 then +2.
    block[0] = 0x00;
    block[2] = 0x21;
    write_adpcm_block(&mut s, 0x20, &block);
    s.voices[0].current_addr = 0x20;

    s.decode_next_block(0);

    assert_eq!(s.voices[0].sample_buf[0], 0x1000);
    assert_eq!(s.voices[0].sample_buf[1], 0x2000);
}

#[test]
fn adpcm_decode_clamps_each_sample_to_i16_like_hardware() {
    // Hardware (nocash: MinMax(-8000h,+7FFFh)) saturates each decoded
    // sample to i16 BEFORE it feeds the predictor history. With predictor 1 and max-positive nibbles the prediction
    // overshoots +0x7FFF on sample 1, so it must read back clamped.
    let mut s = Spu::new();
    let mut block = [0u8; 16];
    block[0] = 0x10; // predictor 1, shift 0
    block[2..].fill(0x77);
    write_adpcm_block(&mut s, 0x20, &block);
    s.voices[0].current_addr = 0x20;

    s.decode_next_block(0);

    assert_eq!(
        s.voices[0].sample_buf[1], 0x7FFF,
        "decoded ADPCM sample must saturate to i16 (was unclamped 0xD900)"
    );
    assert!(
        s.voices[0].s_1 <= 0x7FFF,
        "ADPCM filter history must be i16-saturated, not the raw i32 sum: {}",
        s.voices[0].s_1
    );
}

#[test]
fn adpcm_flag_1_2_loops_back_to_loop_addr() {
    let mut s = Spu::new();
    s.voices[0].loop_addr = 0x100;
    s.voices[0].current_addr = 0x20;
    let mut block = [0u8; 16];
    block[1] = 0x3; // flag 1 (end) + flag 2 (repeat)
    write_adpcm_block(&mut s, 0x20, &block);
    s.decode_next_block(0);
    assert_eq!(s.voices[0].current_addr, 0x100);
}

#[test]
fn adpcm_end_flag_with_repeat_bit_loops_even_with_other_bits_set() {
    // The repeat bit (bit 1) is tested on its own: any loop-end block with
    // bit 1 set loops, regardless of the other flag bits, so 0x7 loops the
    // same as 0x3 (PSX-SPX).
    let mut s = Spu::new();
    s.voices[0].loop_addr = 0x100;
    s.voices[0].loop_addr_locked = true; // ignore flag-4 self-update of loop_addr
    s.voices[0].current_addr = 0x20;
    let mut block = [0u8; 16];
    block[1] = 0x7; // loop-start + repeat + end
    write_adpcm_block(&mut s, 0x20, &block);

    s.decode_next_block(0);

    assert_eq!(
        s.voices[0].current_addr, 0x100,
        "loop-end+repeat (0x7) must redirect to the loop address"
    );
    assert!(
        !s.voices[0].stop_after_block,
        "repeat bit set: voice must not be stopping"
    );
}

#[test]
fn adpcm_flag_1_alone_stops_voice() {
    let mut s = Spu::new();
    s.voices[0].current_addr = 0x40;
    s.voices[0].phase = AdsrPhase::Attack;
    let mut block = [0u8; 16];
    block[1] = 0x1; // flag 1 only (loop-end, no repeat)
    write_adpcm_block(&mut s, 0x40, &block);
    s.decode_next_block(0);
    assert_eq!(s.voices[0].phase, AdsrPhase::Attack);
    assert!(s.voices[0].stop_after_block);
    // ENDX is deferred to the next block boundary (after this loop-end
    // block's 28 samples play), so right after decode it is only pending,
    // not yet latched.
    assert!(s.voices[0].endx_pending);
    assert_eq!(s.endx_latched & 1, 0);
}

#[test]
fn adpcm_stop_flag_turns_voice_off_after_final_block_is_consumed() {
    let mut s = Spu::new();
    s.voices[0].phase = AdsrPhase::Attack;
    s.voices[0].envelope = 0x7FFF;
    s.voices[0].counter = COUNTER_ONE;
    s.voices[0].sample_index = ADPCM_SAMPLES_PER_BLOCK;
    s.voices[0].stop_after_block = true;

    let out = s.fetch_voice_sample(0);

    assert_eq!(out, 0);
    assert_eq!(s.voices[0].phase, AdsrPhase::Off);
    assert_eq!(s.voices[0].envelope, 0);
    assert!(!s.voices[0].stop_after_block);
}

#[test]
fn adpcm_flag_4_updates_loop_addr_when_unlocked() {
    let mut s = Spu::new();
    s.voices[0].current_addr = 0x80;
    let mut block = [0u8; 16];
    block[1] = 0x4; // flag 4 = loop-start
    write_adpcm_block(&mut s, 0x80, &block);
    s.decode_next_block(0);
    assert_eq!(s.voices[0].loop_addr, 0x80);
}

#[test]
fn adpcm_flag_4_ignored_when_software_locked_loop_addr() {
    let mut s = Spu::new();
    s.voices[0].current_addr = 0x80;
    s.voices[0].loop_addr = 0xAAA0;
    s.voices[0].loop_addr_locked = true;
    let mut block = [0u8; 16];
    block[1] = 0x4;
    write_adpcm_block(&mut s, 0x80, &block);
    s.decode_next_block(0);
    assert_eq!(s.voices[0].loop_addr, 0xAAA0);
}

// -- ADSR envelope --

#[test]
fn adsr_attack_linear_ramps_envelope_up() {
    let mut s = Spu::new();
    // Linear attack, rate=0 (fastest linear rate).
    s.voices[0].adsr_lo = 0; // linear attack, shift 0
    s.voices[0].phase = AdsrPhase::Attack;
    // After a single step, envelope should have risen from 0.
    s.voices[0].step_envelope();
    assert!(
        s.voices[0].envelope > 0,
        "env after 1 step: {}",
        s.voices[0].envelope
    );
}

#[test]
fn adsr_attack_saturates_and_transitions_to_decay() {
    let mut s = Spu::new();
    s.voices[0].adsr_lo = 0; // linear attack, shift 0
    s.voices[0].phase = AdsrPhase::Attack;
    // Force envelope near max and step once -- should transition.
    s.voices[0].envelope = 0x7FFE;
    s.voices[0].step_envelope();
    assert_eq!(s.voices[0].envelope, 0x7FFF);
    assert_eq!(s.voices[0].phase, AdsrPhase::Decay);
}

#[test]
fn adsr_decay_reaches_sustain_and_transitions() {
    let mut s = Spu::new();
    s.voices[0].adsr_lo = 0; // decay shift 0, sustain level 0
    s.voices[0].adsr_hi = 1 << 5;
    s.voices[0].phase = AdsrPhase::Decay;
    s.voices[0].envelope = 0x7FFF;
    for _ in 0..10000 {
        s.voices[0].step_envelope();
        if s.voices[0].phase == AdsrPhase::Sustain {
            break;
        }
    }
    assert_eq!(s.voices[0].phase, AdsrPhase::Sustain);
}

#[test]
fn adsr_decay_is_independent_of_release_mode_bit() {
    // Decay is ALWAYS exponential on hardware (PSX-SPX resets Decay
    // with exponential=true unconditionally; PSX-SPX hard-codes
    // EnvelopeMode::Exponential; PSX-SPX: "decay mode is always
    // Exponential decrease"). The release-mode bit must not change it.
    let mut linear = Voice {
        adsr_lo: 0,
        adsr_hi: 0,
        phase: AdsrPhase::Decay,
        envelope: 0x7000,
        ..Voice::default()
    };
    linear.step_envelope();

    let mut exponential = linear.clone();
    exponential.adsr_hi = 1 << 5;
    exponential.envelope = 0x7000;
    exponential.pacer.reset();
    exponential.step_envelope();

    // Exponential decrement from 0x7000: 0x7000 + ((dec*0x7000)>>15) = 0x3800.
    assert_eq!(linear.envelope, 0x3800);
    assert_eq!(
        linear.envelope, exponential.envelope,
        "decay is always exponential and must not depend on the release-mode bit"
    );
}

#[test]
fn adsr_release_linear_decays_to_zero_and_stops_voice() {
    let mut s = Spu::new();
    s.voices[0].adsr_hi = 0; // linear release, shift 0
    s.voices[0].phase = AdsrPhase::Release;
    s.voices[0].envelope = 0x1000;
    for _ in 0..10000 {
        s.voices[0].step_envelope();
        if s.voices[0].phase == AdsrPhase::Off {
            break;
        }
    }
    assert_eq!(s.voices[0].phase, AdsrPhase::Off);
    assert_eq!(s.voices[0].envelope, 0);
}

#[test]
fn adsr_release_stops_when_envelope_reaches_zero() {
    // Reaching level 0 ends the release (voice Off) -- PSX-SPX and
    // PSX-SPX transition Release->Off when the level reaches the target
    // (0 for Release), i.e. at exactly 0, not only on strict underflow.
    let mut voice = Voice {
        adsr_hi: 0,
        phase: AdsrPhase::Release,
        envelope: 8 << 11, // exactly one release step at shift 0
        ..Voice::default()
    };

    voice.step_envelope();
    assert_eq!(voice.envelope, 0);
    assert_eq!(
        voice.phase,
        AdsrPhase::Off,
        "release must transition to Off the moment the envelope reaches 0"
    );
}

#[test]
fn adsr_mix_uses_full_q15_envelope() {
    // SB4 silicon 2026-08-07: (sample * env) >> 15, floor rounding. Seven
    // consecutive NOISE onset samples pinned this against the previous
    // ten-bit (env>>5)/1023 form.
    assert_eq!(apply_adsr_volume(0x4000, 31), 15);
    assert_eq!(apply_adsr_volume(1023, 0x7FFF), 1022);
    assert_eq!(apply_adsr_volume(-1023, 0x7FFF), -1023);
    // The console-measured onset: LFSR value 2472 through the FLAT_ADSR
    // attack/decay/sustain envelope sequence.
    for (env, want) in [
        (0x3800, 1081),
        (0x7000, 2163),
        (0x7FFF, 2471),
        (0x3FFF, 1235),
        (0x77FF, 2317),
    ] {
        assert_eq!(apply_adsr_volume(2472, env), want);
    }
}

// -- Output mixing --

#[test]
fn tick_sample_pushes_to_audio_queue() {
    let mut s = Spu::new();
    assert_eq!(s.audio_queue_len(), 0);
    s.tick_sample(SAMPLE_CYCLES);
    assert_eq!(s.audio_queue_len(), 1);
    assert_eq!(s.samples_produced(), 1);
}

#[test]
fn stress_test_many_voices_at_high_pitch_no_panic() {
    // Regression / stress test: exercises the voice advance
    // loop at maximum pitch (which triggers the block-
    // boundary fraction leak) across many voices and many
    // samples. Would have caught the Gaussian OOB before the
    // Crash 1 run did.
    let mut s = Spu::new();
    s.main_vol_l.write(0x3FFF);
    s.main_vol_r.write(0x3FFF);
    s.write16(SPUCNT, SPUCNT_UNMUTE);
    // Seed SPU RAM with a repeating ADPCM block so each voice
    // has something to decode.
    let mut block = [0u8; 16];
    block[0] = 0x0C; // shift=0x0C, filter=0
    block[1] = 0x02; // flag 2 = repeat
                     // Loop this one block for the first 0x1000 bytes of RAM.
    for base in (0..0x1000).step_by(16) {
        for (i, block_byte) in block.iter().enumerate() {
            let idx = (base + i) / 2;
            let byte = *block_byte as u16;
            if (base + i) & 1 == 0 {
                s.ram[idx] = (s.ram[idx] & 0xFF00) | byte;
            } else {
                s.ram[idx] = (s.ram[idx] & 0x00FF) | (byte << 8);
            }
        }
    }
    // Configure all 24 voices: max pitch, KON, loud volume.
    for v in 0..NUM_VOICES {
        let base = VOICE_BASE + (v as u32) * 16;
        s.write16(base + voice_offset::VOLUME_L, 0x3FFF);
        s.write16(base + voice_offset::VOLUME_R, 0x3FFF);
        s.write16(base + voice_offset::PITCH, 0x3FFF);
        s.write16(base + voice_offset::START_ADDR, 0);
        s.write16(base + voice_offset::ADSR_LO, 0x00FF);
        s.write16(base + voice_offset::ADSR_HI, 0x0000);
    }
    s.write16(KON_LO, 0xFFFF);
    s.write16(KON_HI, 0x00FF);
    // Tick through one NTSC frame's worth of samples. If any
    // voice's state goes out of bounds we'd panic here.
    for _ in 0..735 {
        s.tick_sample(SAMPLE_CYCLES);
    }
    // Output should contain 735 samples and no crashes.
    assert_eq!(s.samples_produced(), 735);
}

#[test]
fn silent_spu_outputs_zero() {
    let mut s = Spu::new();
    // 0x3FFF = max static volume (bits 0..=13 all set, bits
    // 14/15 clear → no phase-invert, no sweep).
    s.main_vol_l.write(0x3FFF);
    s.main_vol_r.write(0x3FFF);
    s.tick_sample(SAMPLE_CYCLES);
    let out = s.drain_audio();
    assert_eq!(out, vec![(0, 0)]);
}

#[test]
fn silent_voice_contributes_zero_even_with_main_volume() {
    let mut s = Spu::new();
    s.main_vol_l.write(0x3FFF);
    s.main_vol_r.write(0x3FFF);
    for _ in 0..10 {
        s.tick_sample(SAMPLE_CYCLES);
    }
    let out = s.drain_audio();
    assert!(out.iter().all(|&(l, r)| l == 0 && r == 0));
}

#[test]
fn cd_audio_input_routes_through_cd_volume() {
    let mut s = Spu::new();
    s.main_vol_l.write(0x3FFF);
    s.main_vol_r.write(0x3FFF);
    s.write16(SPUCNT, SPUCNT_CD_AUDIO_ENABLE);
    s.write16(CD_VOL_L, 0x3FFF);
    s.write16(CD_VOL_R, 0x3FFF);
    // Push one stereo sample and tick.
    s.feed_cd_audio(&[(0x4000, 0x4000)]);
    s.tick_sample(SAMPLE_CYCLES);
    let out = s.drain_audio();
    // Main mix should be nonzero since CD input was nonzero.
    assert_eq!(out.len(), 1);
    let (l, r) = out[0];
    assert!(l > 0, "left should carry CD input: {l}");
    assert!(r > 0, "right should carry CD input: {r}");
}

#[test]
fn cd_audio_underrun_repeats_the_last_frame_briefly_then_goes_silent() {
    let mut s = Spu::new();
    s.main_vol_l.write(0x7FFF);
    s.main_vol_r.write(0x7FFF);
    s.write16(SPUCNT, SPUCNT_CD_AUDIO_ENABLE);
    s.write16(CD_VOL_L, 0x7FFF);
    s.write16(CD_VOL_R, 0x7FFF);
    // Nothing queued yet: silence, and no repeat of a frame never played.
    s.tick_sample(SAMPLE_CYCLES);
    assert_eq!(s.drain_audio()[0], (0, 0));
    s.feed_cd_audio(&[(0x4000, 0x4000), (0x2000, 0x2000)]);
    for n in 2..=12 {
        s.tick_sample(SAMPLE_CYCLES * n);
    }
    let out = s.drain_audio();
    let nonzero: Vec<bool> = out.iter().map(|&(l, _)| l != 0).collect();
    // Two real frames, CD_UNDERRUN_HOLD_SAMPLES repeats of the second, then
    // silence.
    let expected_on = 2 + usize::from(CD_UNDERRUN_HOLD_SAMPLES);
    assert!(nonzero[..expected_on].iter().all(|&on| on), "{nonzero:?}");
    assert!(nonzero[expected_on..].iter().all(|&on| !on), "{nonzero:?}");
    assert_eq!(out[1], out[2], "the held frame repeats the second frame");
}

#[test]
fn main_volume_scales_the_final_mix() {
    // Main volume is applied as the final stage (PSX-SPX /
    // PSX-SPX): out = (clamp(dry + wet) * main_vol_raw) >> 15. Raw 0 silences
    // the mix; 0x3FFF (~half in Q15) is ~half of full-scale 0x7FFF.
    let cap = |mv: u16| {
        let mut s = Spu::new();
        s.main_vol_l.write(mv);
        s.main_vol_r.write(mv);
        s.write16(SPUCNT, SPUCNT_CD_AUDIO_ENABLE);
        s.write16(CD_VOL_L, 0x7FFF);
        s.write16(CD_VOL_R, 0x7FFF);
        s.feed_cd_audio(&[(0x4000, 0x4000)]);
        s.tick_sample(SAMPLE_CYCLES);
        s.drain_audio()[0]
    };
    assert_eq!(cap(0), (0, 0), "main volume 0 must silence the mix");
    let (half_l, _) = cap(0x3FFF);
    let (full_l, _) = cap(0x7FFF);
    assert!(
        half_l > 0 && full_l > 0,
        "positive main volume must pass audio"
    );
    assert!(
        (half_l as i32 * 2 - full_l as i32).abs() <= 4,
        "0x3FFF main volume should be ~half of 0x7FFF: {half_l} vs {full_l}"
    );
}

#[test]
fn cd_audio_input_respects_spucnt_route_enable() {
    let mut s = Spu::new();
    s.main_vol_l.write(0x3FFF);
    s.main_vol_r.write(0x3FFF);
    s.write16(CD_VOL_L, 0x7FFF);
    s.write16(CD_VOL_R, 0x7FFF);

    s.feed_cd_audio(&[(0x4000, 0x4000)]);
    s.tick_sample(SAMPLE_CYCLES);
    let (mut l, mut r) = s.drain_audio()[0];
    assert_eq!(
        (l, r),
        (0, 0),
        "CD input must not route while SPUCNT bit 0 is clear"
    );

    s.write16(SPUCNT, SPUCNT_CD_AUDIO_ENABLE);
    s.feed_cd_audio(&[(0x4000, 0x4000)]);
    s.tick_sample(SAMPLE_CYCLES);
    (l, r) = s.drain_audio()[0];
    assert!(l > 0, "CD input should route when SPUCNT bit 0 is set: {l}");
    assert!(r > 0, "CD input should route when SPUCNT bit 0 is set: {r}");
}

#[test]
fn cd_audio_volume_is_signed_q15_not_voice_sweep_volume() {
    let mut s = Spu::new();
    // Full-unity main volume so this isolates the CD-input volume (main volume
    // now scales the final mix, so 0x3FFF here would halve the level under test).
    s.main_vol_l.write(0x7FFF);
    s.main_vol_r.write(0x7FFF);
    s.write16(SPUCNT, SPUCNT_CD_AUDIO_ENABLE);
    s.write16(CD_VOL_L, 0x7FFF);
    s.write16(CD_VOL_R, 0x8000);

    s.feed_cd_audio(&[(0x4000, 0x4000)]);
    s.tick_sample(SAMPLE_CYCLES);
    let (l, r) = s.drain_audio()[0];

    assert!(l > 0, "0x7fff CD volume must be positive: {l}");
    assert!(r < 0, "0x8000 CD volume must be negative: {r}");
    assert!(
        l.unsigned_abs() > 0x3000,
        "0x7fff should be near unity, not half-scale/phase-inverted: {l}"
    );
}

fn write_reverb_cfg(s: &mut Spu, reg: usize, value: u16) {
    s.write16(REVERB_CFG_BASE + (reg as u32 * 2), value);
}

fn configure_passthrough_reverb(s: &mut Spu) {
    use reverb_reg::*;

    write_reverb_cfg(s, V_IIR, 0x7FFF);
    write_reverb_cfg(s, V_COMB1, 0x7FFF);
    write_reverb_cfg(s, V_LIN, 0x7FFF);
    write_reverb_cfg(s, V_RIN, 0x7FFF);

    // Separate L/R and A/B cells so the test fixture doesn't stomp one
    // channel with another while using a tiny synthetic preset. Real games
    // write full preset tables here.
    write_reverb_cfg(s, M_LSAME, 0);
    write_reverb_cfg(s, M_RSAME, 1);
    write_reverb_cfg(s, M_LDIFF, 2);
    write_reverb_cfg(s, M_RDIFF, 3);
    write_reverb_cfg(s, M_LCOMB1, 0);
    write_reverb_cfg(s, M_RCOMB1, 1);
    write_reverb_cfg(s, M_LAPF1, 0);
    write_reverb_cfg(s, M_RAPF1, 1);
    write_reverb_cfg(s, M_LAPF2, 2);
    write_reverb_cfg(s, M_RAPF2, 3);
}

/// One left pass and one right pass of the reverb with silent input.
fn run_reverb_pair(s: &mut Spu) {
    s.mix_reverb(0, 0);
    s.mix_reverb(0, 0);
}

#[test]
fn reverb_base_roundtrips_and_resets_work_cursor() {
    let mut s = Spu::new();
    s.write16(REVERB_BASE, 0x1000);
    assert_eq!(s.read16(REVERB_BASE), 0x1000);
    assert_eq!(s.reverb_base, 0x8000);
    assert_eq!(s.reverb.curr_addr, 0x4000);

    s.reverb.curr_addr = 0x5000;
    s.write16(REVERB_BASE, 0x1200);
    assert_eq!(s.read16(REVERB_BASE), 0x1200);
    assert_eq!(s.reverb.curr_addr, 0x4800);

    s.write16(REVERB_BASE, 0x0200);
    assert_eq!(s.read16(REVERB_BASE), 0x0200);
    assert_eq!(s.reverb_base, 0);
    assert_eq!(s.reverb.curr_addr, 0);
}

#[test]
fn reverb_address_before_the_base_wraps_to_the_end_of_the_buffer() {
    let mut s = Spu::new();
    s.write16(REVERB_BASE, 0x1000);
    s.reverb.curr_addr = s.reverb_base_halfword();

    // One halfword before mBASE is the last halfword of RAM.
    assert_eq!(s.reverb_ram_index(0, -1), SPU_RAM_HALFWORDS - 1);
}

#[test]
fn reverb_addresses_wrap_within_the_work_area() {
    let mut s = Spu::new();
    // 0xFFFE is where psx-spu's init parks the work area: eight halfwords at
    // the top of RAM.
    for base in [0x1000u16, 0xE128, 0xFFFE] {
        s.write16(REVERB_BASE, base);
        let start = s.reverb_base_halfword() as i32;
        let span = SPU_RAM_HALFWORDS as i32 - start;
        for curr in [start, start + 3, 0x3FFFF] {
            s.reverb.curr_addr = curr as u32;
            for offset in [-0x4000, -0x11, -1, 0, 1, 0x11, 0x3FFF, 0x7FFF] {
                for extra in [0, 1, -1] {
                    let idx = s.reverb_ram_index(offset, extra) as i32;
                    assert!(
                        (start..start + span).contains(&idx),
                        "base {base:#x} curr {curr:#x} offset {offset}: {idx:#x} outside the work area"
                    );
                    // Moving a whole work area's worth lands on the same cell.
                    let shifted = curr + offset * 4 + extra + span;
                    assert_eq!((shifted - start).rem_euclid(span) + start, idx);
                }
            }
        }
    }
}

#[test]
fn reverb_network_turns_bus_input_into_wet_output() {
    let mut s = Spu::new();
    s.write16(REVERB_BASE, 0x1000);
    s.write16(REVERB_VOL_L, 0x7FFF);
    s.write16(REVERB_VOL_R, 0x7FFF);
    s.write16(SPUCNT, SPUCNT_REVERB_MASTER_ENABLE);
    configure_passthrough_reverb(&mut s);

    let mut heard_wet = false;
    for _ in 0..8 {
        let (l, r) = s.mix_reverb(0x4000, 0x4000);
        heard_wet |= l != 0 || r != 0;
    }

    assert!(heard_wet, "reverb bus input never reached wet output");
}

#[test]
fn reverb_output_depth_register_is_q14() {
    assert_eq!(Spu::scale_reverb_output(0x4000, 0x4000), 0x4000);
    assert_eq!(Spu::scale_reverb_output(0x4000, 0x3FFF), 0x3FFF);
    assert_eq!(Spu::scale_reverb_output(0x4000, 0xC000u16 as i16), -0x4000);
}

#[test]
fn reverb_channels_alternate_and_hold_their_output() {
    let mut s = Spu::new();
    s.write16(REVERB_BASE, 0x1000);
    s.reverb.wet_l = 30;
    s.reverb.wet_r = 300;

    // The left pass recomputes only the left output; the right output holds
    // until the right pass, and the address advances once per pair.
    let start = s.reverb.curr_addr;
    let (_, r) = s.mix_reverb(0, 0);
    assert_eq!(r, 300);
    assert_eq!(s.reverb.curr_addr, start);
    let (_, _) = s.mix_reverb(0, 0);
    assert_eq!(s.reverb.curr_addr, start + 1);
}

#[test]
fn reverb_wet_path_is_scaled_by_main_volume() {
    // Reverb (wet) is added to the dry sum BEFORE main volume (PSX-SPX /
    // PSX-SPX order), so main volume 0 silences the wet path too, and a
    // positive main volume passes the EON voice through reverb.
    let run = |mv: u16| {
        let mut s = Spu::new();
        s.main_vol_l.write(mv);
        s.main_vol_r.write(mv);
        s.write16(REVERB_BASE, 0x1000);
        s.write16(REVERB_VOL_L, 0x7FFF);
        s.write16(REVERB_VOL_R, 0x7FFF);
        s.write16(SPUCNT, SPUCNT_UNMUTE | SPUCNT_REVERB_MASTER_ENABLE);
        s.write16(EON_LO, 0x0001);
        configure_passthrough_reverb(&mut s);

        s.voices[0].vol_l.write(0x3FFF);
        s.voices[0].vol_r.write(0x3FFF);
        s.voices[0].phase = AdsrPhase::Sustain;
        s.voices[0].envelope = 0x7FFF;
        s.voices[0].raw_pitch = 0x1000;
        s.voices[0].sample_buf = [0x4000; ADPCM_SAMPLES_PER_BLOCK];
        s.voices[0].sample_index = 0;
        s.voices[0].counter = 0;
        s.voices[0].taps = [0x4000; 4];

        for n in 0..12 {
            s.tick_sample(n * SAMPLE_CYCLES);
        }
        s.drain_audio()
    };

    assert!(
        run(0).iter().all(|&(l, r)| l == 0 && r == 0),
        "main volume 0 silences the wet path too (reverb is added before main volume)"
    );
    assert!(
        run(0x7FFF).iter().any(|&(l, r)| l != 0 || r != 0),
        "EON voice should be audible through reverb at full main volume"
    );
}

#[test]
fn volume_envelope_static_mode_snaps_level_on_write() {
    // Fixed mode stores the signed 15-bit field * 2 (full Q15), so
    // 0x3FFF -> +0x7FFE and a set bit14 is a real negative volume.
    let mut env = VolumeEnvelope::new();
    env.write(0x3FFF); // max positive (+0x7FFE in Q15)
    assert_eq!(env.current, 0x7FFE);
    env.write(0x4100); // bit14 set -> negative: signed15(0x4100)= -0x3F00, *2 = -0x7E00
    assert_eq!(env.current, -0x7E00);
}

#[test]
fn volume_envelope_sweep_mode_animates_from_prior_level() {
    // bit15=1 is a real animated sweep, not an immediate gain. A sweep
    // write leaves `current` at its prior value and ramps it via tick.
    let mut env = VolumeEnvelope::new();
    env.write(0x8000); // rate 0, increasing linear: large +step/sample
    assert_eq!(env.current, 0, "sweep write does not jump the level");
    assert!(env.sweep_active);
    env.tick();
    assert!(
        env.current > 0,
        "increasing sweep ramps up: {}",
        env.current
    );

    // Rate 0x7F is the hardware never-ticks case: level frozen.
    let mut frozen = VolumeEnvelope::new();
    frozen.current = 0x3000;
    frozen.write(0x807F);
    assert!(!frozen.sweep_active);
    for _ in 0..10 {
        frozen.tick();
    }
    assert_eq!(frozen.current, 0x3000);
}

#[test]
fn volume_envelope_sweep_decrease_ramps_down_from_prior_level() {
    // Decreasing sweep ramps `current` down from its prior value toward
    // zero, one step per tick (not an immediate gain).
    let mut env = VolumeEnvelope::new();
    env.current = 0x4000;
    env.write(0x8000 | (1 << 13) | 0x0010); // rate 0x10 decreasing linear
    assert!(env.sweep_active);
    let before = env.current;
    env.tick();
    assert!(
        env.current < before,
        "decreasing sweep falls: {} !< {before}",
        env.current
    );
}

#[test]
fn volume_envelope_static_tick_is_noop() {
    // Fixed mode: 0x2000 -> signed15 * 2 = +0x4000; tick is a no-op.
    let mut env = VolumeEnvelope::new();
    env.write(0x2000);
    assert_eq!(env.current, 0x4000);
    env.tick();
    assert_eq!(env.current, 0x4000);
}

#[test]
fn gaussian_interp_of_silence_is_silence() {
    let out = gauss_interpolate([0, 0, 0, 0], 0);
    assert_eq!(out, 0);
}

#[test]
fn gaussian_interp_nonzero_input_produces_output() {
    // All four samples at max positive -- output should be non-
    // zero and in range.
    let out = gauss_interpolate([0x7FFF, 0x7FFF, 0x7FFF, 0x7FFF], 0x80);
    assert!(out > 0);
}

#[test]
fn gaussian_interp_handles_frac_past_0x10000() {
    // Defensive clamp: the caller keeps the remainder below one
    // source sample, but out-of-range values still shouldn't
    // index past the end of the coefficient table.
    for frac in [0x10000, 0x10004, 0x1FFFF, 0xFFFF_FFFF] {
        let _ = gauss_interpolate([0, 0, 0, 0], frac);
        let _ = gauss_interpolate([0x1234, 0x5678, -0x100, 0x7FFF], frac);
    }
}

#[test]
fn interpolation_ring_preserves_previous_block_tail() {
    let mut voice = Voice::default();
    voice.push_tap(10);
    voice.push_tap(20);
    voice.push_tap(30);
    voice.push_tap(40);
    voice.push_tap(50);
    assert_eq!(voice.taps, [20, 30, 40, 50]);
}

#[test]
fn xa_decoder_silent_block_stays_silent() {
    let mut state = XaDecoderState::new();
    let mut out = [1i16; 28];
    xa_decode_block(&mut state, 0x00, &[0; 28], &mut out);
    assert!(out.iter().all(|&s| s == 0));
}

#[test]
fn xa_decoder_filter_zero_only_scales_by_range() {
    // PSX-SPX: a 4-bit sample is widened with `<< 12` and shifted right
    // by the range, so with range 12 it comes out unchanged.
    let mut widened = [0i16; 28];
    widened[0] = 1 << 12;
    widened[1] = -1 << 12;
    widened[2] = 7 << 12;
    widened[3] = -8 << 12;
    let mut state = XaDecoderState::new();
    let mut out = [0i16; 28];
    xa_decode_block(&mut state, 0x0C, &widened, &mut out);
    assert_eq!(&out[..5], &[1, -1, 7, -8, 0]);
}

#[test]
fn xa_decoder_filter_one_predicts_from_the_newest_sample() {
    // Filter 1 weighs the newest previous sample by 60/64 (truncated):
    // 4096 -> 3840 -> 3600.
    let mut widened = [0i16; 28];
    widened[0] = 4096;
    let mut state = XaDecoderState::new();
    let mut out = [0i16; 28];
    xa_decode_block(&mut state, 0x10, &widened, &mut out);
    assert_eq!(&out[..3], &[4096, 3840, 3600]);
}

#[test]
fn xa_decoder_filter_two_uses_both_previous_samples() {
    // Filter 2: 115/64 on the newest sample, -52/64 on the older one.
    // History in 1/16 steps (older = 0, newest = 16000): 1000 -> 1796 (the
    // exact 1796.875 is truncated on output) -> 2416.
    let mut widened = [0i16; 28];
    widened[0] = 1000;
    let mut state = XaDecoderState::new();
    let mut out = [0i16; 28];
    xa_decode_block(&mut state, 0x20, &widened, &mut out);
    assert_eq!(&out[..3], &[1000, 1796, 2416]);
}

#[test]
fn xa_decoder_history_keeps_fractions_below_one_output_step() {
    // A sample of 1 followed by silence. With whole-sample history the
    // 60/64 prediction would keep rounding back up to 1; with four
    // fractional bits it decays through 15/16 and 14/16 of a step.
    let mut widened = [0i16; 28];
    widened[0] = 1 << 12;
    let mut state = XaDecoderState::new();
    let mut out = [0i16; 28];
    xa_decode_block(&mut state, 0x1C, &widened, &mut out);
    assert_eq!(&out[..3], &[1, 0, 0]);
}

#[test]
fn xa_decoder_clamps_output_but_not_history() {
    // Filter 1, a loud tail: the second-to-last sample is 0x7000 and the
    // last would be 0x7000 + 60/64 of it, well past the 16-bit range.
    let mut widened = [0i16; 28];
    widened[26] = 0x7000;
    widened[27] = 0x7000;
    let mut state = XaDecoderState::new();
    let mut out = [0i16; 28];
    xa_decode_block(&mut state, 0x10, &widened, &mut out);
    assert_eq!(out[26], 0x7000);
    assert_eq!(out[27], i16::MAX);
    // The next block still sees the unclamped 55552: 60/64 of it is
    // 52080, far above what the clamped 32767 would predict (30720).
    let mut next = [0i16; 28];
    xa_decode_block(&mut state, 0x10, &[0; 28], &mut next);
    assert_eq!(next[0], i16::MAX);
}

#[test]
fn xa_decoder_reserved_range_acts_like_nine_and_high_header_bits_are_ignored() {
    let mut widened = [0i16; 28];
    widened[0] = 0x4000;
    for header in [0x0D, 0x0E, 0x0F] {
        let mut state = XaDecoderState::new();
        let mut out = [0i16; 28];
        xa_decode_block(&mut state, header, &widened, &mut out);
        assert_eq!(out[0], 0x4000 >> 9, "header {header:#04x}");
    }
    let mut plain = [0i16; 28];
    xa_decode_block(&mut XaDecoderState::new(), 0x19, &widened, &mut plain);
    let mut dirty = [0i16; 28];
    xa_decode_block(&mut XaDecoderState::new(), 0xD9, &widened, &mut dirty);
    assert_eq!(plain, dirty);
}

// -- Volume register decoding --

#[test]
fn volume_envelope_write_static_level() {
    // Full signed-Q15 fixed level: bits 0..14 sign-extended (bit14 is
    // the sign), times 2. 0x3FFF -> +0x7FFE; 0x4100 -> -0x7E00.
    let mut env = VolumeEnvelope::new();
    env.write(0x3FFF);
    assert_eq!(env.current, 0x7FFE);
    env.write(0x4100);
    assert_eq!(env.current, -0x7E00);
}

// -- Output buffer cap --

#[test]
fn audio_queue_caps_at_max() {
    let mut s = Spu::new();
    for _ in 0..(OUTPUT_BUFFER_CAP + 100) {
        s.tick_sample(SAMPLE_CYCLES);
    }
    assert!(s.audio_queue_len() <= OUTPUT_BUFFER_CAP);
}

// -- Noise generator --

#[test]
fn noise_seed_is_one() {
    // The noise level powers on as 1 (a zero level would still take in a
    // 1 on its first update, since the XNOR of four clear bits is 1).
    let s = Spu::new();
    assert_eq!(s.noise_val, 1);
}

#[test]
fn noise_advances_when_clock_set() {
    // SPUCNT bits 13..10 = 15 is the fastest shift: the timer reload is
    // 0x20000 >> 15 = 4, equal to the smallest step, so the level changes
    // on every sample.
    let mut s = Spu::new();
    s.write16(SPUCNT, 0x3C00);
    for _ in 0..8 {
        let v0 = s.noise_val;
        s.noise_tick();
        assert_ne!(s.noise_val, v0, "noise should shift at the fastest rate");
    }
}

#[test]
fn noise_period_grows_with_lower_shift() {
    // The first sample always updates (the timer powers on at zero);
    // afterwards shift 0 waits (0x20000 - 4) / 4 samples for the next one.
    let mut s = Spu::new();
    s.write16(SPUCNT, 0x0000); // shift 0, step 4
    s.noise_tick();
    let after_first = s.noise_val;
    for _ in 0..30_000 {
        s.noise_tick();
    }
    assert_eq!(s.noise_val, after_first);
    for _ in 0..3_000 {
        s.noise_tick();
    }
    assert_ne!(s.noise_val, after_first);
}

#[test]
fn noise_level_takes_in_the_xnor_of_bits_15_12_11_10() {
    let mut s = Spu::new();
    s.write16(SPUCNT, 0x3C00);
    // Bits 15/12/11/10 all clear: XNOR is 1.
    s.noise_val = 0;
    s.noise_tick();
    assert_eq!(s.noise_val, 1);
    // Bit 15 only: XNOR is 0, and the old bit 15 shifts out.
    s.noise_val = i16::MIN;
    s.noise_tick();
    assert_eq!(s.noise_val, 0);
    // Bits 15 and 12: XNOR is 1.
    s.noise_val = (0x9000u16) as i16;
    s.noise_tick();
    assert_eq!(s.noise_val as u16, 0x2001);
}

// -- FMod / pitch modulation --

#[test]
fn fmod_modulator_voice_stays_audible() {
    // Voice 0 modulates voice 1's pitch. Both emit a known non-zero sample
    // and both reach the mix: a modulator is silenced only by its volume.
    let render = |voices: std::ops::Range<usize>| {
        let mut s = Spu::new();
        s.main_vol_l.write(0x3FFF);
        s.main_vol_r.write(0x3FFF);
        s.write16(SPUCNT, SPUCNT_UNMUTE);
        s.write16(PMON_LO, 0x0002); // voice 1 is modulated by voice 0
        for v in voices {
            let base = VOICE_BASE + (v as u32) * 16;
            s.write16(base + voice_offset::VOLUME_L, 0x3FFF);
            s.write16(base + voice_offset::VOLUME_R, 0x3FFF);
            s.voices[v].phase = AdsrPhase::Sustain;
            s.voices[v].envelope = 0x7FFF;
            s.voices[v].last_sample = 0x4000;
            s.voices[v].sample_buf = [0x4000; ADPCM_SAMPLES_PER_BLOCK];
            s.voices[v].sample_index = 0;
        }
        s.tick_sample(SAMPLE_CYCLES);
        s.drain_audio()[0]
    };
    let (alone_l, alone_r) = render(1..2);
    let (both_l, both_r) = render(0..2);
    assert!(alone_l > 0 && alone_r > 0, "voice 1 still plays");
    assert!(
        both_l > alone_l,
        "voice 0 must add to L: {both_l} vs {alone_l}"
    );
    assert!(
        both_r > alone_r,
        "voice 0 must add to R: {both_r} vs {alone_r}"
    );
}

#[test]
fn spucnt_mute_zeroes_voice_sample_history() {
    let mut s = Spu::new();
    s.main_vol_l.write(0x3FFF);
    s.main_vol_r.write(0x3FFF);
    s.voices[0].phase = AdsrPhase::Sustain;
    s.voices[0].envelope = 0x7FFF;
    s.voices[0].sample_buf = [0x4000; ADPCM_SAMPLES_PER_BLOCK];
    s.voices[0].sample_index = 0;

    s.tick_sample(SAMPLE_CYCLES);
    let (l, r) = s.drain_audio()[0];

    assert_eq!((l, r), (0, 0));
    assert_eq!(s.voices[0].taps, [0, 0, 0, 0]);
}

#[test]
fn fmod_modulator_still_updates_last_sample() {
    // Even though voice 0's L/R is suppressed, its last_sample
    // must still update so voice 1's FMod reads the right value.
    let mut s = Spu::new();
    s.write16(PMON_LO, 0x0002);
    s.voices[0].phase = AdsrPhase::Sustain;
    s.voices[0].envelope = 0x7FFF;
    s.voices[0].sample_buf = [0x1234; ADPCM_SAMPLES_PER_BLOCK];
    s.voices[0].sample_index = 0;
    s.voices[1].raw_pitch = 0x1000;
    s.voices[1].phase = AdsrPhase::Sustain;
    s.voices[1].envelope = 0x7FFF;

    s.tick_sample(SAMPLE_CYCLES);
    // last_sample for voice 0 should be approximately envelope *
    // sample (saturated), not zero.
    assert!(
        s.voices[0].last_sample != 0,
        "modulator's last_sample was zeroed"
    );
}

#[test]
fn noise_value_substitutes_for_voice_sample() {
    // Voice 5 is in noise mode; fetch_voice_sample returns
    // noise_val unchanged when the voice is active.
    let mut s = Spu::new();
    s.noise_on = 1 << 5;
    s.noise_val = 0x1234;
    s.voices[5].phase = AdsrPhase::Attack;
    let out = s.fetch_voice_sample(5);
    assert_eq!(out, 0x1234);
}

#[test]
fn off_noise_voice_stays_silent() {
    let mut s = Spu::new();
    s.noise_on = 1 << 5;
    s.noise_val = 0x1234;
    s.voices[5].phase = AdsrPhase::Off;
    let out = s.fetch_voice_sample(5);
    assert_eq!(out, 0);
}

// ---- loop-flags accuracy tests ----
// -- loop-flags (findings #2, #11, #12) --

#[test]
fn single_block_loop_with_flag7_keeps_playing() {
    // A single ADPCM block that sets loop-start+repeat+end (flags 0x7)
    // must loop and keep sounding, not force-off after one block: the
    // repeat bit (bit 1) is tested on its own, so 0x7 loops just like 0x3.
    let mut s = Spu::new();
    // Unmute the voice sample path: with SPUCNT bit 14 clear, decoded
    // samples are zeroed before entering the interpolation history (see
    // `mute_voice_sample` in fetch_voice_sample), so the loop would be
    // inaudible regardless of correctness. The loop logic under test is
    // unaffected by this bit.
    s.write16(SPUCNT, SPUCNT_UNMUTE);
    let mut block = [0u8; 16];
    block[0] = 0x00; // predictor 0, shift 0
    block[1] = 0x07; // loop-start + repeat + end
    block[2] = 0x77; // non-zero samples so the voice is audible
    block[3] = 0x77;
    block[4] = 0x77;
    block[5] = 0x77;
    write_adpcm_block(&mut s, 0x20, &block);
    s.voices[0].start_addr = 0x20;
    s.voices[0].raw_pitch = 0x1000; // one input sample per output sample
    s.voices[0].phase = AdsrPhase::Sustain;
    s.voices[0].envelope = 0x7FFF;
    s.voices[0].current_addr = 0x20;
    s.voices[0].sample_index = ADPCM_SAMPLES_PER_BLOCK; // force first decode
    s.voices[0].counter = COUNTER_ONE;

    // Play well past one block (28 samples). The voice must stay on and
    // keep emitting once the interpolation window has filled.
    let mut nonzero_after_first_block = false;
    for n in 0..120 {
        let out = s.fetch_voice_sample(0);
        if n > 28 && out != 0 {
            nonzero_after_first_block = true;
        }
    }
    assert_ne!(
        s.voices[0].phase,
        AdsrPhase::Off,
        "0x7 (loop-start+repeat+end) must loop, not force-off"
    );
    assert!(
        !s.voices[0].stop_after_block,
        "repeat bit set: voice must not be marked stopping"
    );
    assert!(
        nonzero_after_first_block,
        "looped voice must keep emitting samples past the first block"
    );
}

#[test]
fn endx_latches_after_loop_end_block_finishes_not_when_decoded() {
    // Finding #11: ENDX must latch only after the loop-end block's 28
    // samples have played (PSX-SPX latch at the block-
    // boundary crossing), not the moment the block is decoded a block
    // earlier.
    let mut s = Spu::new();
    let mut block = [0u8; 16];
    block[1] = 0x3; // loop-end + repeat, loops to itself
    write_adpcm_block(&mut s, 0x20, &block);
    s.voices[0].start_addr = 0x20;
    s.voices[0].loop_addr = 0x20;
    s.voices[0].raw_pitch = 0x1000; // one input sample per output sample
    s.voices[0].phase = AdsrPhase::Attack;
    s.voices[0].envelope = 0x7FFF;
    s.voices[0].current_addr = 0x20;
    s.voices[0].sample_index = ADPCM_SAMPLES_PER_BLOCK; // force first decode
    s.voices[0].counter = COUNTER_ONE;

    // First fetch decodes the loop-end block but must NOT latch ENDX yet;
    // it is only pending.
    s.fetch_voice_sample(0);
    assert_eq!(
        s.endx_latched & 1,
        0,
        "ENDX must not latch when the loop-end block is decoded"
    );
    assert!(
        s.voices[0].endx_pending,
        "ENDX should be pending until the block finishes playing"
    );

    // Consume the rest of the block; ENDX latches when the next boundary
    // is crossed (after the 28th sample), not before.
    let mut latched = false;
    for _ in 0..60 {
        s.fetch_voice_sample(0);
        if s.endx_latched & 1 != 0 {
            latched = true;
            break;
        }
    }
    assert!(
        latched,
        "ENDX must latch once the loop-end block's 28 samples are consumed"
    );
}

#[test]
fn repeat_addr_write_during_first_block_lets_loop_start_flag_win() {
    // Finding #12: hardware lets a freshly key-on'd voice's own loop-start
    // flag override a REPEAT_ADDR write made while the voice is on and
    // still in its first ADPCM block (PSX-SPX:
    // the loop-address lock stays false in that window). Tron Bonne /
    // Valkyrie Profile / Re-Loaded depend on this.
    let mut s = Spu::new();
    // Block at byte 0x80 carries loop-start (flag 4).
    let mut block = [0u8; 16];
    block[1] = 0x4;
    write_adpcm_block(&mut s, 0x80, &block);
    s.voices[0].start_addr = 0x80;

    s.write16(KON_LO, 0x0001);
    s.apply_kon_koff(); // key-on: clears lock, decoded_block_count = 0

    // Software writes REPEAT_ADDR = B (0x20 -> byte 0x100) while still in
    // the first block. It must not lock the loop address.
    s.write16(VOICE_BASE + voice_offset::REPEAT_ADDR, 0x0020);
    assert!(
        !s.voices[0].loop_addr_locked,
        "REPEAT_ADDR write during the first block must not lock"
    );

    // The first block now decodes; its loop-start flag overrides B with A.
    s.decode_next_block(0);
    assert_eq!(
        s.voices[0].loop_addr, 0x80,
        "loop-start flag must win over the first-block REPEAT_ADDR write"
    );

    // Off-voice case: a REPEAT_ADDR write while the voice is off locks
    // immediately (phase == Off path).
    let mut s2 = Spu::new();
    s2.write16(VOICE_BASE + voice_offset::REPEAT_ADDR, 0x0020);
    assert!(
        s2.voices[0].loop_addr_locked,
        "REPEAT_ADDR write to an off voice must lock the loop address"
    );
}

// ---- adsr accuracy tests ----
// -- ADSR decay: always exponential (finding #3) --

#[test]
fn adsr_decay_is_always_exponential_regardless_of_release_mode() {
    // Decay is always an exponential decrease on hardware; it must NOT
    // depend on the release-mode bit. PSX-SPX resets Decay with
    // exponential=true unconditionally; PSX-SPX hard-codes
    // EnvelopeMode::Exponential for Decay; PSX-SPX: "decay mode is always
    // Exponential decrease". So a voice with release mode = Linear must
    // still decay exponentially.
    //
    // decay_rate=8, sustain_level=2: exponential decay reaches the sustain
    // level in ~840 steps; a (wrong) linear decay would reach it in ~416.
    let count_steps = |release_exp: bool| -> u32 {
        let mut v = Voice {
            adsr_lo: (8 << 4) | 2,
            adsr_hi: if release_exp { 1 << 5 } else { 0 },
            phase: AdsrPhase::Decay,
            envelope: 0x7FFF,
            ..Voice::default()
        };
        v.pacer.reset();
        let mut steps = 0u32;
        while v.phase == AdsrPhase::Decay && steps < 100_000 {
            v.step_envelope();
            steps += 1;
        }
        steps
    };

    let steps_linear_mode = count_steps(false);
    let steps_exp_mode = count_steps(true);

    // Release-mode bit must not change the decay trajectory at all.
    assert_eq!(
        steps_linear_mode, steps_exp_mode,
        "decay must be exponential independent of release mode"
    );
    // And it must be the exponential count (~840), not the linear one (~416).
    assert_eq!(
        steps_linear_mode, 840,
        "decay with release mode = Linear must still take the exponential \
         step count to reach sustain (840), not the linear count (416)"
    );
}

#[test]
fn adsr_decay_single_step_matches_exponential_with_linear_release_mode() {
    // One decay step from env=0x7000 at decay_rate=0 must be the
    // exponential result (0x3800 = 14336), even when release mode is
    // Linear. Cross-checked against PSX-SPX exponential
    // decrease `(step * level) >> 15`.
    let mut v = Voice {
        adsr_lo: 0,
        adsr_hi: 0, // linear release mode
        phase: AdsrPhase::Decay,
        envelope: 0x7000,
        ..Voice::default()
    };
    v.pacer.reset();
    v.step_envelope();
    assert_eq!(
        v.envelope, 0x3800,
        "decay must apply the exponential decrement (dec*env>>15) \
         regardless of release mode"
    );
}

// -- ADSR release: exponential release reaches Off at level 0 (finding #4) --

#[test]
fn adsr_release_exponential_reaches_off_at_zero() {
    // Exponential release decrements by `(dec * level) >> 15`, which lands
    // on exactly 0 and never goes strictly negative. Reaching 0 must end
    // the release (voice Off): PSX-SPX transition when the
    // level reaches the target (0 for Release). Previously the voice was
    // stuck in Release forever because the Off gate tested `< 0`.
    let mut v = Voice {
        adsr_hi: 1 << 5,
        phase: AdsrPhase::Release,
        envelope: 0x7FFF,
        ..Voice::default()
    };

    let mut steps = 0u32;
    while v.phase == AdsrPhase::Release && steps < 200 {
        v.step_envelope();
        steps += 1;
    }

    assert_eq!(
        v.phase,
        AdsrPhase::Off,
        "exponential release must reach Off when the envelope hits 0, \
         not plateau in Release forever"
    );
    assert_eq!(v.envelope, 0);
    // rate-0 exponential release from 0x7FFF reaches 0 on the 15th step.
    assert_eq!(
        steps, 15,
        "rate-0 exponential release should hit 0 at step 15"
    );
}

// ---- voice-volume accuracy tests ----
// -- Voice volume: sweep animation, signed-Q15 fixed level, >>15 application --

#[test]
fn volume_envelope_increasing_linear_sweep_ramps_each_tick() {
    // bit15=1 selects a sweep. rate=0x10 (linear, increasing) gives a
    // per-sample step of +896; the level must climb monotonically from
    // its prior value (0 on a fresh register) and reach full scale.
    // Golden values cross-checked against PSX-SPX Envelope::tick /
    // PSX-SPX VolumeEnvelope::Tick.
    let mut env = VolumeEnvelope::new();
    env.write(0x8000 | 0x0010);
    assert_eq!(env.current, 0, "sweep write keeps the prior current level");
    assert!(env.sweep_active, "rate 0x10 sweep must be animating");

    let mut prev = env.current;
    let levels: Vec<i16> = (0..6)
        .map(|_| {
            env.tick();
            let c = env.current;
            assert!(c > prev, "sweep must rise: {c} !> {prev}");
            prev = c;
            c
        })
        .collect();
    assert_eq!(levels, vec![896, 1792, 2688, 3584, 4480, 5376]);

    // Ramps to and saturates at full scale, then stops animating.
    for _ in 0..200 {
        env.tick();
    }
    assert_eq!(env.current, 0x7FFF);
    assert!(!env.sweep_active, "sweep deactivates once it pins at max");
}

#[test]
fn volume_envelope_decreasing_linear_sweep_falls_to_zero() {
    // rate 0x10, decreasing linear: step -1024 per sample from a
    // positive starting level, clamped at 0.
    let mut env = VolumeEnvelope::new();
    env.current = 0x4000; // prior level the sweep ramps down from
    env.write(0x8000 | (1 << 13) | 0x0010);
    assert!(env.sweep_active);

    let levels: Vec<i16> = (0..6)
        .map(|_| {
            env.tick();
            env.current
        })
        .collect();
    assert_eq!(levels, vec![15360, 14336, 13312, 12288, 11264, 10240]);

    for _ in 0..200 {
        env.tick();
    }
    assert_eq!(env.current, 0, "decreasing sweep bottoms out at 0");
    assert!(!env.sweep_active);
}

#[test]
fn volume_envelope_rate_0x7f_sweep_never_ticks() {
    // Rate 0x7F is the hardware "never ticks" special case
    // (counter_increment collapses to 0). The level must stay put.
    let mut env = VolumeEnvelope::new();
    env.current = 0x1234;
    env.write(0x8000 | 0x007F);
    assert!(
        !env.sweep_active,
        "rate 0x7F has zero increment -> inactive"
    );
    for _ in 0..50 {
        env.tick();
    }
    assert_eq!(
        env.current, 0x1234,
        "never-ticking sweep leaves level unchanged"
    );
}

#[test]
fn volume_envelope_fixed_level_is_signed_q15_times_two() {
    // Fixed mode (bit15=0): bits 0..14 are a signed 15-bit value
    // representing Volume/2, so current = signed15 * 2. A set bit14 is
    // a genuine negative (phase-inverted) volume. Table matches
    // PSX-SPX fixed-mode volume (signed-15 value * 2) /
    // PSX-SPX fixed_volume() as i16 * 2.
    let cases: &[(u16, i16)] = &[
        (0x0000, 0),
        (0x1000, 0x2000),
        (0x3FFF, 0x7FFE),
        (0x4000, -0x8000),
        (0x6000, -0x4000),
        (0x7FFF, -2),
    ];
    for &(raw, want) in cases {
        let mut env = VolumeEnvelope::new();
        env.write(raw);
        assert_eq!(env.current, want, "fixed volume {raw:#06x}");
        assert!(
            !env.sweep_active,
            "fixed volume must not animate: {raw:#06x}"
        );
    }
}

#[test]
fn negative_fixed_voice_volume_inverts_output_sign() {
    // A negative per-channel fixed volume (bit14 set) must invert the
    // sample polarity, not silence it or flip magnitude. Feed a steady
    // positive voice sample and read the mixed L/R sign. raw 0x4000 ->
    // current -0x8000 -> output negative; raw 0x3FFF -> +0x7FFE ->
    // output positive of (near) equal magnitude. Main volume is unity
    // so this isolates the per-voice Q15 application (>>15).
    let mix = |vol_raw: u16| {
        let mut s = Spu::new();
        s.main_vol_l.write(0x7FFF);
        s.main_vol_r.write(0x7FFF);
        s.write16(SPUCNT, SPUCNT_UNMUTE);
        s.voices[0].vol_l.write(vol_raw);
        s.voices[0].vol_r.write(vol_raw);
        s.voices[0].phase = AdsrPhase::Sustain;
        s.voices[0].envelope = 0x7FFF;
        s.voices[0].sample_buf = [0x4000; ADPCM_SAMPLES_PER_BLOCK];
        s.voices[0].sample_index = 0;
        s.voices[0].counter = 0;
        s.voices[0].taps = [0x4000; 4];
        s.tick_sample(SAMPLE_CYCLES);
        s.drain_audio()[0]
    };

    let (pos_l, _) = mix(0x3FFF); // +0x7FFE current
    let (neg_l, _) = mix(0x4000); // -0x8000 current
    assert!(
        pos_l > 0,
        "positive fixed volume must stay positive: {pos_l}"
    );
    assert!(
        neg_l < 0,
        "negative fixed volume (0x4000) must invert sign: {neg_l}"
    );
    assert!(
        (pos_l as i32 + neg_l as i32).abs() <= 4,
        "+0x7FFE and -0x8000 volumes should be near mirror images: {pos_l} vs {neg_l}"
    );
}

// ---- reverb accuracy tests ----
#[test]
fn reverb_wet_output_is_the_second_all_pass_result() {
    // The wet sample is the output of the second all-pass filter times the
    // output volume. With every coefficient zero except a unity output
    // volume, both all-pass stages pass their delayed tap straight through
    // (`out = out * 0 + [m - d]`), so the wet sample is the value in the
    // second all-pass buffer cell, and no stage writes (master bit clear).
    use reverb_reg::*;
    let mut s = Spu::new();
    s.write16(REVERB_BASE, 0x1000); // base active; curr_addr = 0x4000
    s.write16(REVERB_VOL_L, 0x4000); // unity in scale_reverb_output (/0x4000)
    s.write16(REVERB_VOL_R, 0x4000);

    write_reverb_cfg(&mut s, M_LSAME, 4);
    write_reverb_cfg(&mut s, M_RSAME, 5);
    write_reverb_cfg(&mut s, M_LDIFF, 6);
    write_reverb_cfg(&mut s, M_RDIFF, 7);
    write_reverb_cfg(&mut s, M_LAPF1, 8);
    write_reverb_cfg(&mut s, M_RAPF1, 9);
    write_reverb_cfg(&mut s, M_LAPF2, 10);
    write_reverb_cfg(&mut s, M_RAPF2, 11);

    let idx_l = s.reverb_ram_index(10, 0);
    let idx_r = s.reverb_ram_index(11, 0);
    s.ram[idx_l] = 1234i16 as u16;
    s.ram[idx_r] = (-567i16) as u16;

    let (l, _) = s.mix_reverb(0, 0);
    let (_, r) = s.mix_reverb(0, 0);
    assert_eq!(l, 1234, "left wet must be the second all-pass result");
    assert_eq!(r, -567, "right wet must keep its sign");
}

#[test]
fn reverb_reflection_reads_one_cell_behind_its_write() {
    // A reflection filter reads [mLSAME-2] (one halfword behind) and writes
    // its result at the cell itself. With vIIR = 0 the result is exactly the
    // value in the cell behind, so the cell written must equal the seeded
    // -1 cell, and the cell ahead stays clear.
    use reverb_reg::*;
    let mut s = Spu::new();
    s.write16(REVERB_BASE, 0x1000);
    s.write16(SPUCNT, SPUCNT_REVERB_MASTER_ENABLE);
    write_reverb_cfg(&mut s, V_IIR, 0);

    write_reverb_cfg(&mut s, M_LSAME, 8); // write cell, base+32
    write_reverb_cfg(&mut s, M_RSAME, 9);
    write_reverb_cfg(&mut s, M_LDIFF, 12);
    write_reverb_cfg(&mut s, M_RDIFF, 13);
    // Keep the all-pass buffers off the cells under test.
    write_reverb_cfg(&mut s, M_LAPF1, 20);
    write_reverb_cfg(&mut s, M_RAPF1, 21);
    write_reverb_cfg(&mut s, M_LAPF2, 22);
    write_reverb_cfg(&mut s, M_RAPF2, 23);

    let idx_behind = s.reverb_ram_index(8, -1); // base+31, the read tap
    let idx_at = s.reverb_ram_index(8, 0); // base+32, the write target
    let idx_ahead = s.reverb_ram_index(8, 1); // base+33
    s.ram[idx_behind] = 1000i16 as u16;

    run_reverb_pair(&mut s);

    assert_eq!(
        s.ram[idx_at], 1000i16 as u16,
        "the reflection must read one cell behind (-1) and write at the cell (+0)"
    );
    assert_eq!(s.ram[idx_ahead], 0, "it must not write one cell ahead (+1)");
    assert_eq!(s.ram[idx_behind], 1000i16 as u16);
}

#[test]
fn pa5_master_off_reverb_reads_dma_work_area_until_output_depth_is_zero() {
    // Real-console PA5 DEPTH0 captured the BIOS handoff exactly:
    // vLOUT=5EBC/5EBC, mBASE=E128, all 32 config words populated, then
    // SDK init leaves that state intact while clearing EON and the reverb
    // master bit. Uploading the t0a0 map into the overlapping work area
    // produced -30 dBFS noise. Zeroing only vLOUT suppressed it to the
    // -86.8 dBFS capture floor. Reproduce that causal mechanism here.
    const BIOS_REVERB_CFG: [u16; 32] = [
        0x033D, 0x0231, 0x7E00, 0x5000, 0xB400, 0xB000, 0x4C00, 0xB000, 0x6000, 0x5400, 0x1ED6,
        0x1A31, 0x1D14, 0x183B, 0x1BC2, 0x16B2, 0x1A32, 0x15EF, 0x15EE, 0x1055, 0x1334, 0x0F2D,
        0x11F6, 0x0C5D, 0x1056, 0x0AE1, 0x0AE0, 0x07A2, 0x0464, 0x0232, 0x8000, 0x8000,
    ];

    let mut s = Spu::new();
    s.write16(REVERB_VOL_L, 0x5EBC);
    s.write16(REVERB_VOL_R, 0x5EBC);
    s.write16(REVERB_BASE, 0xE128);
    for (index, value) in BIOS_REVERB_CFG.into_iter().enumerate() {
        write_reverb_cfg(&mut s, index, value);
    }
    s.write16(EON_LO, 0);
    s.write16(EON_HI, 0);
    s.write16(SPUCNT, SPUCNT_UNMUTE); // reverb master remains clear

    // Deterministic stand-in for the PA5 map DMA overlapping the BIOS work
    // area. The network must read it but must not write it while master-off.
    let work_base = s.reverb_base_halfword() as usize;
    for (index, word) in s.ram[work_base..].iter_mut().enumerate() {
        *word = ((index as u16).wrapping_mul(0x1357) ^ 0x7878).rotate_left(3);
    }
    let before_hash = s.ram.iter().fold(0x811C_9DC5u32, |hash, &word| {
        (hash ^ word as u32).wrapping_mul(0x0100_0193)
    });

    let mut wet_peak = 0i32;
    for _ in 0..64 {
        let (left, right) = s.mix_reverb(0, 0);
        wet_peak = wet_peak.max(left.abs()).max(right.abs());
    }
    assert!(
        wet_peak > 100,
        "master-off read/APF path must expose uploaded reverb-work data: {wet_peak}"
    );
    let after_hash = s.ram.iter().fold(0x811C_9DC5u32, |hash, &word| {
        (hash ^ word as u32).wrapping_mul(0x0100_0193)
    });
    assert_eq!(
        before_hash, after_hash,
        "master-off must suppress reverb RAM feedback writes"
    );

    s.write16(REVERB_VOL_L, 0);
    s.write16(REVERB_VOL_R, 0);
    // Each channel holds its last computed output until its next pass, so
    // allow two full pairs for the new volume to reach both outputs.
    for _ in 0..4 {
        let _ = s.mix_reverb(0, 0);
    }
    let mut muted_peak = 0i32;
    for _ in 0..8 {
        let (left, right) = s.mix_reverb(0, 0);
        muted_peak = muted_peak.max(left.abs()).max(right.abs());
    }
    assert_eq!(
        muted_peak, 0,
        "PA5 DEPTH0 must silence the master-off read/APF path"
    );
}

// ---- spu-irq accuracy tests ----
#[test]
fn spu_irq_does_not_relatch_until_acknowledged() {
    // Finding #18 (spu-irq re-arm gate): an SPU RAM IRQ is sticky. Once it
    // latches (SPUSTAT bit 6 set), no further match may re-raise irq_pending
    // until software acknowledges by clearing SPUCNT bit 6. Matches
    // PSX-SPX is_irq_triggerable / PSX-SPX IsRAMIRQTriggerable and the
    // nocash PSX-SPX sticky-IRQ9 semantics.
    let mut s = Spu::new();
    s.write16(SPUCNT, (1 << 6) | (1 << 4)); // IRQ enable + Manual-Write transfer mode
    s.tick_sample(SAMPLE_CYCLES);
    s.write16(IRQ_ADDR, 0x0010); // byte addr 0x80
    s.write16(TRANSFER_ADDR, 0x0010); // start the transfer pointer at byte 0x80

    // First halfword write hits the IRQ address: latch fires once.
    s.write16(TRANSFER_FIFO, 0x1111);
    assert!(s.take_irq_pending(), "first IRQ-addr match must latch");
    assert_ne!(
        s.spustat() & (1 << 6),
        0,
        "SPUSTAT IRQ9 flag must be sticky"
    );

    // Two more writes still fall inside the same 8-byte IRQ window. With the
    // re-arm gate, the already-latched (unacknowledged) IRQ9 flag blocks any
    // further latch, so irq_pending stays false. Before the fix these would
    // re-raise irq_pending every match (the interrupt-storm bug).
    s.write16(TRANSFER_FIFO, 0x2222);
    s.write16(TRANSFER_FIFO, 0x3333);
    assert!(
        !s.take_irq_pending(),
        "subsequent matches must NOT re-latch while IRQ9 is unacknowledged"
    );

    // Acknowledge: clear SPUCNT bit 6 (clears the SPUSTAT flag), then re-enable.
    s.write16(SPUCNT, 0);
    assert_eq!(s.spustat() & (1 << 6), 0, "clearing SPUCNT.6 acks the flag");
    s.write16(SPUCNT, (1 << 6) | (1 << 4)); // re-enable IRQ + Manual-Write mode
    s.tick_sample(SAMPLE_CYCLES * 2);

    // A fresh match after acknowledge latches again.
    s.write16(TRANSFER_ADDR, 0x0010);
    s.write16(TRANSFER_FIFO, 0x4444);
    assert!(
        s.take_irq_pending(),
        "after acknowledge, a new IRQ-addr match must latch again"
    );
}

// ---- adpcm-decode accuracy tests ----
#[test]
fn adpcm_reserved_shift_13_to_15_acts_as_shift_9() {
    // Finding #10: header shift nibbles 13..15 are reserved and must act
    // like shift=9 (nocash PSX-SPX; PSX-SPX GetShift; PSX-SPX
    // decode_block), NOT clamp to 12. Predictor 0 (filter (0,0)) isolates
    // the shift: sample0 = sign_extend4(nibble) << 12 >> 9.
    let mut s = Spu::new();
    let mut block = [0u8; 16];
    // ADPCM header byte: shift is the LOW nibble (bits 0..3), filter the
    // high nibble. So
    // shift 15 with predictor 0 is 0x0F, NOT 0xF0 (which would be shift 0).
    block[0] = 0x0F; // predictor 0, shift 15 (reserved)
    block[2] = 0x01; // sample0 nibble = 1, sample1 nibble = 0
    write_adpcm_block(&mut s, 0x20, &block);
    s.voices[0].current_addr = 0x20;

    s.decode_next_block(0);

    // (1 << 12) >> 9 == 8 for shift=9; the old `.min(12)` path gave
    // (1 << 12) >> 12 == 1.
    assert_eq!(
        s.voices[0].sample_buf[0], 8,
        "reserved shift 15 must decode as shift=9 (>>9), not shift=12 (>>12)"
    );
}

#[test]
fn adpcm_decode_clamps_predictor_history_to_i16() {
    // Finding #9: each decoded sample is saturated to i16 before it feeds
    // the IIR predictor history, so s_1/s_2 (and sample_buf) never exceed
    // the 16-bit range. Predictor 1 (f1=60) with max-positive nibbles
    // drives the prediction past +0x7FFF within two samples.
    let mut s = Spu::new();
    let mut block = [0u8; 16];
    block[0] = 0x10; // predictor 1, shift 0
    block[2..].fill(0x77); // every nibble = +7
    write_adpcm_block(&mut s, 0x20, &block);
    s.voices[0].current_addr = 0x20;

    s.decode_next_block(0);

    // History must be saturated, matching PSX-SPX/nocash.
    assert!(
        s.voices[0].s_1 <= 0x7FFF && s.voices[0].s_1 >= -0x8000,
        "s_1 must be i16-saturated: {}",
        s.voices[0].s_1
    );
    assert!(
        s.voices[0].s_2 <= 0x7FFF && s.voices[0].s_2 >= -0x8000,
        "s_2 must be i16-saturated: {}",
        s.voices[0].s_2
    );
    // Hand-computed clamp-each-step reference (predictor 1, shift 0,
    // nibble +7): sample0 = 0x7000; sample1 = 0x7000 + (0x7000*60>>6)
    // = 0xD900 -> clamped to 0x7FFF; every later sample also clamps.
    assert_eq!(s.voices[0].sample_buf[0], 0x7000);
    assert_eq!(s.voices[0].sample_buf[1], 0x7FFF);
    assert!(
        s.voices[0].sample_buf[2..].iter().all(|&v| v == 0x7FFF),
        "all subsequent samples stay at the +0x7FFF rail once saturated"
    );
}

// ---- gaussian accuracy tests ----
#[test]
fn gaussian_table_is_hardware_nocash_table() {
    // The shipped table must be the 512-entry nocash/hardware Gaussian
    // table (peak 0x59B3), not an 11-bit 1024-entry curve (peak 0x519).
    assert_eq!(GAUSS_TABLE.len(), 0x200);
    assert_eq!(GAUSS_TABLE[0], -1);
    assert_eq!(GAUSS_TABLE[0x1FF], 0x59B3);
    // First 16 coefficients are -1 on hardware.
    for (k, entry) in GAUSS_TABLE.iter().enumerate().take(16) {
        assert_eq!(*entry, -1, "entry {k}");
    }
}

#[test]
fn gaussian_interp_matches_hardware_golden_vector() {
    // Golden 4-tap window at phase i=250 (counter fraction 0xFA1).
    // The hardware table gives 13344; the 11-bit curve gives 9153, so this
    // discriminates the two.
    let out = gauss_interpolate([-31831, -31193, 32367, -31905], 0xFA1);
    assert_eq!(out, 13344);
}

#[test]
fn gaussian_interp_dc_gain_has_hardware_droop() {
    // Four equal full-scale samples must reproduce the SPU's ~0.4%
    // gain droop, not pass through at unity. At phase 0 the hardware
    // table yields 32639 for +32767 and -32640 for -32768 (the 4-tap
    // sum at this phase is 0x7F80 = 32640). Unity would be 32767.
    assert_eq!(
        gauss_interpolate([32767, 32767, 32767, 32767], 0x0000),
        32639
    );
    assert_eq!(
        gauss_interpolate([-32768, -32768, -32768, -32768], 0x0000),
        -32640
    );
}

#[test]
fn gaussian_interp_phase_index_is_bits_4_to_11_of_the_counter() {
    // A window with a single non-zero newest tap scales purely by
    // GAUSS_TABLE[i], where i = (counter >> 4) & 0xFF.
    for counter in [0x000u32, 0x012, 0x800, 0xFA1, 0xFFF] {
        let i = ((counter >> 4) & 0xFF) as usize;
        let expected = saturate_i16((GAUSS_TABLE[i] * 0x4000) >> 15);
        assert_eq!(
            gauss_interpolate([0, 0, 0, 0x4000], counter),
            expected,
            "counter={counter:#06x}"
        );
    }
}

// ---- keyon-keyoff accuracy tests ----
#[test]
fn kon_applies_after_sample_emit_not_before_for_hardware_parity() {
    // KON must be latched and acted on at the END of the tick (after the
    // sample is emitted), matching PSX-SPX update_keystatus() (run after
    // push_sample) and PSX-SPX KeyOn (run after WriteToCaptureBuffer).
    // So the tick during which KON is consumed still samples the voice in
    // its pre-KON (Off) state, and the voice's first Attack sample only
    // appears on the NEXT tick.
    let mut s = Spu::new();
    s.write16(SPUCNT, SPUCNT_UNMUTE);
    s.main_vol_l.write(0x7FFF);
    s.main_vol_r.write(0x7FFF);

    // A constant +0x1000 ADPCM block (predictor 0, shift 0, all data
    // nibbles = 1) at byte 0x20, so a keyed voice would be audible.
    let mut block = [0u8; 16];
    block[0] = 0x00; // predictor 0, shift 0
    for b in block[2..].iter_mut() {
        *b = 0x11;
    }
    write_adpcm_block(&mut s, 0x20, &block);

    let base = VOICE_BASE; // voice 0
    s.write16(base + voice_offset::VOLUME_L, 0x3FFF);
    s.write16(base + voice_offset::VOLUME_R, 0x3FFF);
    s.write16(base + voice_offset::PITCH, 0x1000);
    s.write16(base + voice_offset::START_ADDR, 0x0004); // 0x4 << 3 = byte 0x20
    s.write16(base + voice_offset::ADSR_LO, 0x000F); // linear attack, fastest rate
    s.write16(base + voice_offset::ADSR_HI, 0x0000);

    // Voice is Off before KON.
    assert_eq!(s.voices[0].phase, AdsrPhase::Off);

    s.write16(KON_LO, 0x0001);
    // Pending, but voice state must NOT have changed yet.
    assert_eq!(s.voices[0].phase, AdsrPhase::Off);

    // Tick #1: the voice is sampled while still Off (KON applied only at
    // end-of-tick), so it contributes silence this sample. After the tick
    // the voice is in Attack with envelope still 0 (not yet advanced).
    s.tick_sample(SAMPLE_CYCLES);
    assert_eq!(
        s.drain_audio(),
        vec![(0, 0)],
        "the KON tick must still emit the pre-KON (silent) sample"
    );
    assert_eq!(
        s.voices[0].phase,
        AdsrPhase::Attack,
        "KON should have been applied at the end of the first tick"
    );
    assert_eq!(
        s.voices[0].envelope, 0,
        "envelope must not have advanced on the KON tick (voice keyed at tick end)"
    );

    // Ticks #2..#9: the v1.17 SB4 capture shows nine zero samples before the
    // first envelope step, so a keyed voice stays silent, envelope included,
    // until that delay drains.
    for t in 2..=9u64 {
        s.tick_sample(t * SAMPLE_CYCLES);
    }
    assert_eq!(
        s.voices[0].envelope, 0,
        "envelope must not advance during the silicon start delay"
    );
    // Tick #10: the first Attack step lands.
    s.tick_sample(10 * SAMPLE_CYCLES);
    assert!(
        s.voices[0].envelope > 0,
        "first Attack step must land once the start delay has drained"
    );
}

// ---- spu-capture-status faithfulness tests ----
// ---- spu capture-buffer + SPUSTAT status accuracy tests ----

#[test]
fn cd_audio_writes_l_r_capture_buffers() {
    // PSX-SPX: the SPU mirrors the CD input (post CD-input volume) into the
    // CD-L capture buffer at SPU RAM 0x000 and CD-R at 0x400 every sample.
    // Previously these stayed frozen at whatever DMA last left there.
    let mut s = Spu::new();
    s.write16(SPUCNT, SPUCNT_CD_AUDIO_ENABLE);
    s.write16(CD_VOL_L, 0x4000); // +0.5 in Q15
    s.write16(CD_VOL_R, 0x2000); // +0.25 in Q15
    let sample = 0x4000i16;
    s.feed_cd_audio(&[(sample, sample)]);
    s.tick_sample(SAMPLE_CYCLES);

    let want_l = ((sample as i32 * 0x4000) >> 15) as u16;
    let want_r = ((sample as i32 * 0x2000) >> 15) as u16;
    assert_eq!(
        s.ram[0x000 >> 1],
        want_l,
        "CD-L capture buffer (post CD volume)"
    );
    assert_eq!(
        s.ram[0x400 >> 1],
        want_r,
        "CD-R capture buffer (post CD volume)"
    );

    // The ring advances one halfword (2 bytes) per sample: a second sample
    // lands at the next slot, not back at 0.
    let s2 = 0x2000i16;
    s.feed_cd_audio(&[(s2, s2)]);
    s.tick_sample(2 * SAMPLE_CYCLES);
    let want_l2 = ((s2 as i32 * 0x4000) >> 15) as u16;
    assert_eq!(
        s.ram[2 >> 1],
        want_l2,
        "capture ring must advance 2 bytes per sample"
    );
}

#[test]
fn voice1_voice3_outputs_written_to_capture_buffers() {
    // PSX-SPX: Voice1's post-ADSR output is captured at SPU RAM 0x800 and
    // Voice3's at 0xC00 every sample. Drive voice 1 with a constant decoded
    // sample so it produces a known non-zero output, leave voice 3 idle.
    let mut s = Spu::new();
    s.write16(SPUCNT, SPUCNT_UNMUTE);
    s.voices[1].phase = AdsrPhase::Sustain;
    s.voices[1].envelope = 0x7FFF;
    s.voices[1].sample_buf = [0x4000; ADPCM_SAMPLES_PER_BLOCK];
    s.voices[1].sample_index = 0;
    s.voices[1].counter = 0;
    s.voices[1].taps = [0x4000; 4];

    s.tick_sample(SAMPLE_CYCLES);

    assert_ne!(
        s.voices[1].last_sample, 0,
        "voice 1 must have produced a sample"
    );
    assert_eq!(
        s.ram[0x800 >> 1],
        s.voices[1].last_sample as u16,
        "voice 1 post-ADSR output must land in its capture buffer (0x800)"
    );
    assert_eq!(
        s.ram[0xC00 >> 1],
        0,
        "idle voice 3 captures silence (0xC00)"
    );
}

#[test]
fn capture_write_can_latch_spu_irq_on_low_ram() {
    // An SPU IRQ armed on the capture region must latch from a capture
    // write, not only from the decode cursor. Arm IRQ at byte 0x800
    // (Voice1 capture bank start) and tick: the voice-1 capture write at
    // ring pos 0 hits it.
    let mut s = Spu::new();
    s.write16(SPUCNT, (1 << 6) | (1 << 4)); // IRQ enable + Manual-Write mode
    s.write16(IRQ_ADDR, 0x0100); // 0x100 * 8 = byte 0x800

    s.tick_sample(SAMPLE_CYCLES);

    assert!(
        s.take_irq_pending(),
        "capture write at 0x800 must latch the armed IRQ"
    );
    assert_ne!(s.spustat() & (1 << 6), 0, "SPUSTAT IRQ9 flag must be set");
}

#[test]
fn spustat_exposes_dma_request_bits_from_transfer_mode() {
    // PSX-SPX SPUSTAT: bit 7 = DMA read/write request (mirrors SPUCNT bit
    // 5, set for both DMA modes), bit 8 = DMA write request (mode 2), bit
    // 9 = DMA read request (mode 3). These previously read back 0 forever.
    let mut s = Spu::new();

    s.write16(SPUCNT, 2 << 4); // DMA write mode
    assert_eq!(s.spustat() & 0x0380, 0, "mode alone does not request DMA");
    s.begin_dma(0);
    assert_ne!(
        s.spustat() & (1 << 7),
        0,
        "DMA write mode sets the request bit (7)"
    );
    assert_ne!(
        s.spustat() & (1 << 8),
        0,
        "DMA write mode sets the write-request bit (8)"
    );
    assert_eq!(
        s.spustat() & (1 << 9),
        0,
        "DMA write mode must not set read-request bit (9)"
    );

    s.write16(SPUCNT, 3 << 4); // DMA read mode
    assert_ne!(
        s.spustat() & (1 << 7),
        0,
        "DMA read mode sets the request bit (7)"
    );
    assert_ne!(
        s.spustat() & (1 << 9),
        0,
        "DMA read mode sets the read-request bit (9)"
    );
    assert_eq!(
        s.spustat() & (1 << 8),
        0,
        "DMA read mode must not set write-request bit (8)"
    );

    s.write16(SPUCNT, 0); // Stop mode
    assert_eq!(
        s.spustat() & 0x0380,
        0,
        "Stop mode clears DMA request bits 7/8/9"
    );
    s.write16(SPUCNT, 1 << 4); // Manual-Write mode
    assert_eq!(
        s.spustat() & 0x0380,
        0,
        "Manual-Write mode sets no DMA request bits"
    );
    s.end_dma();
}

#[test]
fn spustat_control_mirror_updates_on_next_sample_boundary() {
    let mut s = Spu::new();
    s.write16_at(SPUCNT, 0x002f, 100);

    assert_eq!(s.spucnt(), 0x002f, "SPUCNT write latch is immediate");
    assert_eq!(
        s.spustat_at(767) & 0x3f,
        0,
        "mirror remains old before edge"
    );
    assert_eq!(s.spustat_at(768) & 0x3f, 0x2f, "mirror lands at edge");

    s.tick_sample(768);
    assert_eq!(s.spustat_control, 0x2f);
}

#[test]
fn dma_read_control_mirror_crosses_without_channel_arm() {
    // SB4 silicon 2026-08-07: the mode-3 SPUSTAT mirror settles at the next
    // sample boundary (24-27 polls) with the DMA channel verifiably not yet
    // armed. The earlier wait-for-arm pin came from a capture that could not
    // separate the mirror from the arm.
    let mut s = Spu::new();
    s.write16_at(SPUCNT, 0x0030, 100);
    assert_eq!(s.spustat_at(767) & 0x3f, 0);
    assert_eq!(s.spustat_at(768) & 0x3f, 0x30);
}

#[test]
fn dma_write_waits_for_sample_domain_mode_apply() {
    let mut s = Spu::new();
    s.write16_at(SPUCNT, 0x0020, 100);
    assert!(!s.dma_write_ready_at(767));
    assert!(s.dma_write_ready_at(768));
}

#[test]
fn cancelled_manual_mode_leaves_queued_fifo_data_uncommitted() {
    let mut s = Spu::new();
    s.apply_scph_9902_profile();
    s.write16_at(TRANSFER_ADDR, 0, 100);
    s.write16_at(SPUCNT, 0x0010, 100);
    s.write16_at(TRANSFER_FIFO, 0xCAFE, 110);
    s.write16_at(SPUCNT, 0, 120);
    s.tick_sample(768);

    s.write16(TRANSFER_ADDR, 0);
    let mut readback = [0u16; 1];
    s.dma_read(&mut readback);
    assert_eq!(readback, [0]);
}

#[test]
fn default_manual_mode_accepts_fifo_before_sample_boundary() {
    let mut s = Spu::new();
    s.write16_at(TRANSFER_ADDR, 0, 100);
    s.write16_at(SPUCNT, 0x0010, 100);
    s.write16_at(TRANSFER_FIFO, 0xCAFE, 110);

    s.write16(TRANSFER_ADDR, 0);
    let mut readback = [0u16; 1];
    s.dma_read(&mut readback);
    assert_eq!(readback, [0xCAFE]);
}

#[test]
fn unstable_dma_read_inserts_ffff_and_drops_each_block_tail() {
    let mut s = Spu::new();
    s.write16(TRANSFER_ADDR, 0);
    s.dma_write(&[0, 1, 2, 3, 4, 5, 6, 7]);

    s.write16(TRANSFER_ADDR, 0);
    let mut unstable = [0u16; 8];
    s.dma_read_blocks(&mut unstable, 4, false);
    assert_eq!(unstable, [0xFFFF, 0, 1, 2, 0xFFFF, 4, 5, 6]);

    s.write16(TRANSFER_ADDR, 0);
    let mut stable = [0u16; 8];
    s.dma_read_blocks(&mut stable, 4, true);
    assert_eq!(stable, [0, 1, 2, 3, 4, 5, 6, 7]);
}

#[test]
fn spustat_capture_half_flag_toggles_at_ring_midpoint() {
    // PSX-SPX SPUSTAT bit 11 = which half of each 0x400 capture ring is
    // being written. From reset the ring index starts at 0 and advances 2
    // bytes per tick, wrapping at 0x400; the flag is updated after each
    // advance (1 for pos < 0x200, 0 for pos >= 0x200) on SCPH-9902.
    let mut s = Spu::new();
    s.apply_scph_9902_profile();
    for tick in 1..=512u32 {
        s.tick_sample(tick as u64 * SAMPLE_CYCLES);
        let pos = (tick * 2) % 0x400;
        // SB4 silicon 2026-08-07 (edge-keyed, both phases exercised):
        // bit 11 = 1 while the SECOND half is being written, on the
        // SCPH-9902 profile too.
        let want = if pos >= 0x200 { 0x800 } else { 0 };
        assert_eq!(
            s.read16(SPUSTAT) & 0x800,
            want,
            "tick {tick}: capture-half flag must follow ring pos {pos:#06x}"
        );
    }
}

#[test]
fn default_capture_half_flag_uses_conventional_polarity() {
    let mut s = Spu::new();
    assert_eq!(s.read16(SPUSTAT) & 0x800, 0);

    for tick in 1..256u64 {
        s.tick_sample(tick * SAMPLE_CYCLES);
    }
    assert_eq!(s.read16(SPUSTAT) & 0x800, 0);

    s.tick_sample(256 * SAMPLE_CYCLES);
    assert_eq!(s.read16(SPUSTAT) & 0x800, 0x800);
}

// ---- the demo-disc parked-voice tone ------------------------------------
//
// psx-sfx appends a parking block after every sample so a finished one-shot
// lands on something instead of running into the next sample: shift/filter 0,
// flags LOOP_START|REPEAT|END, then zero nibbles. Decoded correctly that is
// exact silence, which is why a voice left parked on it is inaudible here.
//
// On console the 2026-08-07 NitroXide recording had two voices audibly
// looping theirs, at exactly 44100/28 Hz and a harmonic of it -- one ADPCM
// block per cycle. These two tests pin both halves of that: a correct parking
// block is silent, and a block whose header has been corrupted the way the
// unstable SPU transfer corrupts data (0xFFFF in the first halfword, which
// sets every flag bit including REPEAT and LOOP_START) self-loops and is
// LOUD. The audible difference is the whole bug; what remains open is which
// step corrupts the block on the way in, which conformance 0xBC-0xBE ask.

fn park_voice_and_render(block: &[u8; 16], samples: usize) -> Vec<i16> {
    let mut s = Spu::new();
    s.write16(SPUCNT, SPUCNT_UNMUTE);
    s.main_vol_l.write(0x3FFF);
    s.main_vol_r.write(0x3FFF);
    write_adpcm_block(&mut s, 0x1000, block);
    let base = VOICE_BASE;
    s.write16(base + voice_offset::VOLUME_L, 0x3FFF);
    s.write16(base + voice_offset::VOLUME_R, 0x3FFF);
    s.write16(base + voice_offset::PITCH, 0x1000); // unity: 28 samples/block
    s.write16(base + voice_offset::START_ADDR, (0x1000 >> 3) as u16);
    s.write16(base + voice_offset::ADSR_LO, 0x00FF); // instant attack, full sustain
    s.write16(base + voice_offset::ADSR_HI, 0x0000);
    s.write16(KON_LO, 0x0001);
    let mut out = Vec::with_capacity(samples);
    for t in 1..=samples as u64 {
        s.tick_sample(t * SAMPLE_CYCLES);
    }
    for (l, _r) in s.drain_audio() {
        out.push(l);
    }
    out
}

#[test]
fn a_correct_parking_block_is_silent_however_long_it_loops() {
    // shift/filter 0, LOOP_START|REPEAT|END, zero nibbles.
    let block = [0x00, 0x07, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let out = park_voice_and_render(&block, 600);
    let peak = out.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    assert_eq!(peak, 0, "a parked voice on a zero block must be silent");
}

#[test]
fn a_corrupted_parking_block_self_loops_and_sounds() {
    // The same block with 0xFFFF as its first halfword, which is what the
    // unstable SPU transfer inserts at a block boundary. Flags 0xFF carry
    // REPEAT and LOOP_START, so the voice parks on it exactly as before --
    // but the data nibbles are no longer zero.
    let block = [
        0xFF, 0xFF, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66,
        0x77,
    ];
    let out = park_voice_and_render(&block, 600);
    let peak = out.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    // Around 831 here: shift 0xF is reserved and decodes as 9, so the block is
    // heavily attenuated but plainly not silent. Against the zero block's
    // exact 0, that difference is the whole audible bug.
    assert!(
        peak > 200,
        "a corrupted parking block must be audible, peak was {peak}"
    );
    // And it must still be looping at the end of the window rather than having
    // decayed: one block per 28 samples, forever.
    let tail_peak = out[out.len() - 112..]
        .iter()
        .map(|s| s.unsigned_abs())
        .max()
        .unwrap_or(0);
    assert!(
        tail_peak > 200,
        "the loop must sustain, tail peak was {tail_peak}"
    );
}

#[test]
fn sampled_one_shot_survives_delayed_waveform_attack_and_releases_quickly() {
    fn voice_with(envelope: psx_spu::Adsr) -> Voice {
        let mut spu = Spu::new();
        spu.write16(VOICE_BASE + voice_offset::ADSR_LO, envelope.lower);
        spu.write16(VOICE_BASE + voice_offset::ADSR_HI, envelope.upper);
        spu.voices[0].phase = AdsrPhase::Attack;
        spu.voices[0].clone()
    }
    let mut sustained = voice_with(psx_spu::Adsr::sample_one_shot());
    let mut percussive = voice_with(psx_spu::Adsr::percussive());
    // Cortex's swing reaches its audible attack at 40 ms. The old
    // percussive sustain has already silenced it by then.
    for _ in 0..1764 {
        sustained.step_envelope();
        percussive.step_envelope();
    }
    assert!(sustained.envelope > 32000);
    assert_eq!(percussive.envelope, 0);
    sustained.phase = AdsrPhase::Release;
    for _ in 0..4410 {
        sustained.step_envelope();
    }
    assert_eq!(
        sustained.envelope, 0,
        "END/key-off must not leave an infinite release"
    );
}

// -- XA sample-rate conversion --

/// Least-squares fit of `a sin + b cos` at `freq` to `got`, sampled at
/// `rate`; returns (amplitude, error-to-signal ratio in dB of the residual).
fn fit_tone(got: &[f64], freq: f64, rate: f64) -> (f64, f64) {
    let (mut ss, mut cc, mut sc, mut sy, mut cy) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for (n, &y) in got.iter().enumerate() {
        let (s, c) = (2.0 * std::f64::consts::PI * freq * n as f64 / rate).sin_cos();
        ss += s * s;
        cc += c * c;
        sc += s * c;
        sy += s * y;
        cy += c * y;
    }
    let det = ss * cc - sc * sc;
    let a = (sy * cc - cy * sc) / det;
    let b = (cy * ss - sy * sc) / det;
    let mut err = 0.0;
    let mut sig = 0.0;
    for (n, &y) in got.iter().enumerate() {
        let (s, c) = (2.0 * std::f64::consts::PI * freq * n as f64 / rate).sin_cos();
        let fit = a * s + b * c;
        err += (y - fit) * (y - fit);
        sig += fit * fit;
    }
    ((a * a + b * b).sqrt(), 10.0 * (err / sig).log10())
}

/// Gain of the PSX-SPX zigzag tables at low frequencies (about 0.906).
const ZIGZAG_GAIN: f64 = 0x7400 as f64 / 32768.0;

/// Convert a sine of `freq` Hz sampled at `src_rate` (through the 18.9 kHz
/// path when `half_rate`) and return the left channel at 44.1 kHz with the filter's
/// start-up transient removed.
fn convert_tone(freq: f64, src_rate: f64, half_rate: bool) -> Vec<f64> {
    let mut resampler = XaResampler::new();
    let mut out = Vec::new();
    for n in 0..6000 {
        let v = (16_000.0 * (2.0 * std::f64::consts::PI * freq * n as f64 / src_rate).sin()).round()
            as i16;
        if half_rate {
            resampler.push_half_rate((v, v), &mut out);
        } else {
            resampler.push((v, v), &mut out);
        }
    }
    out.iter().skip(400).map(|&(l, _)| f64::from(l)).collect()
}

#[test]
fn xa_resampler_zigzag_weights_have_the_documented_dc_gain() {
    // PSX-SPX's table is not normalised to 8000h: each of the seven output
    // tables sums to about 0.906 of unity (73E5h..741Dh). Pin that, so a
    // change to the table or to the shift is a visible decision.
    for phase in 0..7 {
        let sum: i32 = xa_resample::ZIGZAG.iter().map(|row| row[phase]).sum();
        assert!(
            (0x73E0..=0x7420).contains(&sum),
            "table {} sums to {sum:#x}",
            phase + 1
        );
    }
}

#[test]
fn xa_resampler_emits_seven_frames_per_six_and_is_chunk_independent() {
    let frames: Vec<(i16, i16)> = (0..120)
        .map(|n| (n as i16 * 37, -(n as i16) * 11))
        .collect();
    let mut whole = XaResampler::new();
    let mut a = Vec::new();
    for &f in &frames {
        whole.push(f, &mut a);
    }
    assert_eq!(a.len(), 120 / 6 * 7);
    // Interleaving other state resets must not matter: a clone mid-stream
    // continues identically.
    let mut split = XaResampler::new();
    let mut b = Vec::new();
    for &f in &frames[..50] {
        split.push(f, &mut b);
    }
    let mut cont = split.clone();
    for &f in &frames[50..] {
        cont.push(f, &mut b);
    }
    assert_eq!(a, b);
}

#[test]
fn xa_resampler_reproduces_music_band_tones_at_the_44k_rate() {
    // 37.8 kHz source: tones through the audible band come out at the
    // table's DC gain (0.906) with the residual (images, rounding) at
    // least 35 dB down.
    for freq in [500.0, 1_000.0, 4_000.0, 8_000.0] {
        let got = convert_tone(freq, 37_800.0, false);
        let (amp, resid_db) = fit_tone(&got, freq, 44_100.0);
        assert!(
            (amp / 16_000.0 - ZIGZAG_GAIN).abs() < 0.02,
            "{freq} Hz amplitude {amp}"
        );
        assert!(resid_db < -35.0, "{freq} Hz residual {resid_db:.1} dB");
    }
}

#[test]
fn xa_resampler_beats_nearest_sample_by_a_wide_margin() {
    // The old nearest-sample conversion, for comparison: the same tone
    // gives a residual within about 20 dB of the signal.
    let freq = 5_000.0;
    let src: Vec<f64> = (0..6000)
        .map(|n| {
            (16_000.0 * (2.0 * std::f64::consts::PI * freq * n as f64 / 37_800.0).sin()).round()
        })
        .collect();
    let mut nearest = Vec::new();
    for i in 0..(src.len() * 7 / 6) {
        nearest.push(src[i * 6 / 7]);
    }
    let nearest: Vec<f64> = nearest.into_iter().skip(400).collect();
    let (_, nearest_db) = fit_tone(&nearest, freq, 44_100.0);
    let (_, zigzag_db) = fit_tone(&convert_tone(freq, 37_800.0, false), freq, 44_100.0);
    assert!(nearest_db > -25.0, "nearest {nearest_db:.1} dB");
    assert!(zigzag_db < nearest_db - 20.0, "zigzag {zigzag_db:.1} dB");
}

#[test]
fn xa_resampler_handles_18900_streams_by_interpolating_to_37800() {
    for freq in [400.0, 1_500.0] {
        let got = convert_tone(freq, 18_900.0, true);
        let (amp, resid_db) = fit_tone(&got, freq, 44_100.0);
        assert!(
            (amp / 16_000.0 - ZIGZAG_GAIN).abs() < 0.03,
            "{freq} Hz amplitude {amp}"
        );
        assert!(resid_db < -30.0, "{freq} Hz residual {resid_db:.1} dB");
    }
}
