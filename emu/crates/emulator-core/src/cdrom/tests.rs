use super::*;

fn raw_sector(header: [u8; 4], subheader: [u8; 4], payload_fill: u8) -> Vec<u8> {
    let mut raw = vec![0u8; psx_iso::SECTOR_BYTES];
    raw[12..16].copy_from_slice(&header);
    raw[16..20].copy_from_slice(&subheader);
    raw[20..24].copy_from_slice(&subheader);
    let payload_start = if header[3] == 1 { 16 } else { 24 };
    raw[payload_start..payload_start + 2048].fill(payload_fill);
    raw
}

/// Play `track` and wait out the seek, leaving the drive actually producing
/// audio. `cmd_play` on its own only starts the head moving: see
/// [`super::timing::cdda_seek_cycles`].
fn play_and_arrive(cd: &mut CdRom, track: u8) {
    cd.cmd_play(&[track]);
    let arrives = cd.cdda_seek_done_at.expect("Play arms a seek");
    cd.tick(arrives + 1);
}

fn multitrack_disc_with_pregap() -> Disc {
    Disc::from_tracks(vec![
        psx_iso::Track {
            number: 1,
            track_type: psx_iso::TrackType::Data,
            start_lba: 0,
            sector_count: 10,
            pregap: 0,
            file_pregap: 0,
            source: Box::new(vec![0u8; psx_iso::SECTOR_BYTES * 10]),
        },
        psx_iso::Track {
            number: 2,
            track_type: psx_iso::TrackType::Audio,
            start_lba: 12,
            sector_count: 4,
            pregap: 2,
            file_pregap: 0,
            source: Box::new(vec![0u8; psx_iso::SECTOR_BYTES * 4]),
        },
    ])
}

fn cdda_disc() -> Disc {
    let mut audio = vec![0u8; psx_iso::SECTOR_BYTES * 2];
    for sample in 0..(CDDA_SAMPLES_PER_SECTOR * 2) {
        let left = 1000i16.saturating_add(sample as i16);
        let right = (-1000i16).saturating_sub(sample as i16);
        let off = sample * CDDA_BYTES_PER_SAMPLE;
        audio[off..off + 2].copy_from_slice(&left.to_le_bytes());
        audio[off + 2..off + 4].copy_from_slice(&right.to_le_bytes());
    }

    Disc::from_tracks(vec![
        psx_iso::Track {
            number: 1,
            track_type: psx_iso::TrackType::Data,
            start_lba: 0,
            sector_count: 10,
            pregap: 0,
            file_pregap: 0,
            source: Box::new(vec![0u8; psx_iso::SECTOR_BYTES * 10]),
        },
        psx_iso::Track {
            number: 2,
            track_type: psx_iso::TrackType::Audio,
            start_lba: 12,
            sector_count: 2,
            pregap: 2,
            file_pregap: 0,
            source: Box::new(audio),
        },
    ])
}

#[test]
fn contains_covers_4_bytes() {
    for off in 0..4 {
        assert!(CdRom::contains(BASE + off));
    }
    assert!(!CdRom::contains(BASE - 1));
    assert!(!CdRom::contains(BASE + 4));
}

#[test]
fn index_write_is_masked() {
    let mut cd = CdRom::new();
    cd.write8(BASE, 0xff);
    // The status byte readback has index in low 2 bits.
    assert_eq!(cd.read8(BASE) & 0x3, 3);
}

#[test]
fn parameter_fifo_roundtrips() {
    let mut cd = CdRom::new();
    cd.write8(BASE + 2, 0xab);
    cd.write8(BASE + 2, 0xcd);
    assert_eq!(cd.params.len(), 2);
    // FIFO not-empty bit cleared, not-full bit still set.
    let s = cd.read8(BASE);
    assert_eq!(s & status_bit::PARAM_FIFO_EMPTY, 0);
    assert!(s & status_bit::PARAM_FIFO_NOT_FULL != 0);
}

#[test]
fn response_fifo_pop_returns_pushed_bytes() {
    let mut cd = CdRom::new();
    cd.responses.push_back(0x11);
    cd.responses.push_back(0x22);
    assert_eq!(cd.read8(BASE + 1), 0x11);
    assert_eq!(cd.read8(BASE + 1), 0x22);
    // Empty pop reads as zero.
    assert_eq!(cd.read8(BASE + 1), 0);
}

#[test]
fn data_fifo_read_without_transfer_request_returns_zero_and_keeps_buffered_sector() {
    let mut cd = CdRom::new();
    cd.data_fifo.extend([0x12, 0x34]);
    cd.data_fifo_ready = true;

    assert_ne!(cd.read8(BASE) & status_bit::DATA_FIFO_NOT_EMPTY, 0);
    assert_eq!(cd.read8(BASE + 2), 0);
    assert_eq!(cd.data_fifo_len(), 2);
    assert_eq!(cd.data_fifo.front().copied(), Some(0x12));
    assert_eq!(cd.read8(BASE) & status_bit::DATA_FIFO_NOT_EMPTY, 0);
}

#[test]
fn request_register_bit7_arms_transfer_until_sector_buffer_drains() {
    let mut cd = CdRom::new();
    cd.data_fifo.extend([0x12, 0x34]);
    cd.data_fifo_ready = true;

    cd.write8(BASE + 3, 0x80);
    assert_eq!(cd.read8(BASE + 2), 0x12);
    assert_ne!(cd.read8(BASE) & status_bit::DATA_FIFO_NOT_EMPTY, 0);
    assert_eq!(cd.read8(BASE + 2), 0x34);
    assert_eq!(cd.read8(BASE) & status_bit::DATA_FIFO_NOT_EMPTY, 0);
    assert!(!cd.data_transfer_active);
    assert_eq!(cd.read8(BASE + 2), 0);
}

#[test]
fn irq_mask_write_reports_wake_for_latched_unmasked_irq() {
    let mut cd = CdRom::new();
    cd.irq_flag = IrqType::DataReady as u8;
    cd.irq_mask = 0;
    cd.index = 1;

    assert!(!cd.write8_at(BASE + 2, 0, 0));
    assert!(cd.write8_at(BASE + 2, 1, 0));
}

#[test]
fn new_command_discards_unread_response_and_sets_busy_bit() {
    let mut cd = CdRom::new();
    cd.responses.push_back(0x55);

    cd.queue_command(0x01, 123);

    assert!(
        cd.responses.is_empty(),
        "new command should replace old packet"
    );
    assert_ne!(
        cd.read8(BASE) & status_bit::TRANSMISSION_BUSY,
        0,
        "status bit 7 should latch while the command is pending"
    );
}

#[test]
fn delivered_irq_packet_replaces_stale_bytes_and_clears_busy_bit() {
    let mut cd = CdRom::new();
    cd.command_busy = true;
    cd.responses.push_back(0x11);
    cd.insert_pending_event(PendingEvent {
        command: 0x01,
        deadline: 100,
        irq: IrqType::Acknowledge,
        bytes: vec![0xaa, 0xbb],
        followup: None,
    });

    assert!(cd.tick(101));
    assert_eq!(cd.read8(BASE + 1), 0xaa);
    assert_eq!(cd.read8(BASE + 1), 0xbb);
    assert_eq!(cd.read8(BASE + 1), 0);
    assert_eq!(cd.read8(BASE) & status_bit::TRANSMISSION_BUSY, 0);
}

#[test]
fn irq_ack_clears_only_written_bits() {
    let mut cd = CdRom::new();
    cd.irq_flag = 0x1f;
    // Select index 1 then write-1-to-clear bit 2.
    cd.write8(BASE, 1);
    cd.write8(BASE + 3, 0x04);
    assert_eq!(cd.irq_flag, 0x1b);
}

#[test]
fn cold_boot_is_closed_lid_no_disc() {
    // Cold boot state for "Please insert disc" path: lid closed
    // (SHELL_OPEN cleared) so Init succeeds, motor off until
    // Init runs. No disc (GetID returns the no-disc response).
    let cd = CdRom::new();
    assert_eq!(cd.drive_status & drive_status_bit::SHELL_OPEN, 0);
    assert!(!cd.motor_on);
    assert!(!cd.disc_present);
}

#[test]
fn init_only_queues_ack_and_latches_shell_open() {
    let mut cd = CdRom::new();
    cd.scheduling_cycle = 1_000;

    cd.cmd_init();

    assert_eq!(cd.pending.len(), 1);
    let ack = cd.pending.front().expect("Init ACK pending");
    assert_eq!(ack.irq, IrqType::Acknowledge);
    assert!(ack.followup.is_none(), "Init should not invent INT2");
    assert!(cd.motor_on, "Init should spin the motor up");
    assert_eq!(
        cd.drive_status & drive_status_bit::SHELL_OPEN,
        drive_status_bit::SHELL_OPEN
    );
    assert!(cd.seek_done, "Init resets the seek latch to DONE");
}

#[test]
fn init_ack_bootstraps_lid_rescan_state_machine() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(Disc::from_bin(vec![0u8; psx_iso::SECTOR_BYTES])));
    cd.scheduling_cycle = 1_000;

    cd.cmd_init();
    assert!(cd.lid_bootstrap_pending);
    assert_eq!(cd.drive_state, DriveState::RescanCd);

    let ack_cycle = 1_000 + FIRST_RESPONSE_WITH_MEDIA_CYCLES + 1;
    assert!(cd.tick(ack_cycle));
    assert!(!cd.lid_bootstrap_pending);
    assert_eq!(cd.lid_deadline, Some(ack_cycle + LID_BOOTSTRAP_CYCLES));

    let rescan_cycle = ack_cycle + LID_BOOTSTRAP_CYCLES + 1;
    assert!(!cd.tick(rescan_cycle));
    assert_eq!(cd.drive_state, DriveState::PrepareCd);
    assert_eq!(
        cd.lid_deadline,
        Some(rescan_cycle + LID_PREPARE_SPINUP_CYCLES)
    );

    let prepare_cycle = rescan_cycle + LID_PREPARE_SPINUP_CYCLES + 1;
    assert!(!cd.tick(prepare_cycle));
    assert_eq!(cd.drive_state, DriveState::Standby);
    assert_ne!(cd.drive_status & drive_status_bit::SEEKING, 0);

    let settle_cycle = prepare_cycle + LID_PREPARE_SEEK_CYCLES + 1;
    assert!(!cd.tick(settle_cycle));
    assert_eq!(cd.drive_state, DriveState::Standby);
    assert_eq!(cd.drive_status & drive_status_bit::SEEKING, 0);
}

#[test]
fn getstat_reports_then_clears_shell_open_sticky_bit() {
    let mut cd = CdRom::new();
    cd.drive_status |= drive_status_bit::SHELL_OPEN;
    cd.scheduling_cycle = 1_000;

    cd.cmd_getstat();

    assert_eq!(cd.drive_status & drive_status_bit::SHELL_OPEN, 0);
    assert!(cd.tick(1_000 + FIRST_RESPONSE_WITH_MEDIA_CYCLES + 1));
    let stat = cd.read8(BASE + 1);
    assert_eq!(
        stat & drive_status_bit::SHELL_OPEN,
        drive_status_bit::SHELL_OPEN
    );
    assert_eq!(cd.read8(BASE + 1), 0);
}

#[test]
fn mounted_getstat_fifth_request_crosses_maintenance_sweep() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(Disc::from_bin(vec![0; psx_iso::SECTOR_BYTES])));

    for request in 1..=5 {
        cd.pending.clear();
        cd.scheduling_cycle = request * 100_000;
        cd.cmd_getstat();
        let deadline = cd.pending.front().expect("GetStat response").deadline;
        let extra = if request == 5 {
            GETSTAT_MAINTENANCE_CYCLES
        } else {
            0
        };
        assert_eq!(
            deadline,
            request * 100_000 + FIRST_RESPONSE_WITH_MEDIA_CYCLES + extra
        );
    }
}

#[test]
fn getlocl_without_disc_returns_error() {
    let mut cd = CdRom::new();
    // No disc.
    cd.cmd_get_loc_l();
    // First tick rebases the pending event's relative deadline
    // to absolute; second tick past the deadline delivers it.
    cd.tick(0);
    cd.tick(10_000_000);
    let first = cd.responses.front().copied().unwrap_or(0);
    assert_ne!(first & drive_status_bit::ERROR, 0);
}

#[test]
fn getlocl_without_prior_sector_returns_error_even_with_disc() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(Disc::from_bin(vec![0u8; psx_iso::SECTOR_BYTES])));

    cd.cmd_get_loc_l();
    cd.tick(0);
    cd.tick(10_000_000);

    let first = cd.read8(BASE + 1);
    assert_ne!(first & drive_status_bit::ERROR, 0);
    assert_eq!(
        cd.read8(BASE + 1),
        0x80,
        "psx-spx: GetlocL fails with error code 80h"
    );
}

/// A disc whose sectors carry their own MSF in the header, like a real one.
fn addressed_disc(sectors: u32) -> Disc {
    let bcd = |v: u32| ((v / 10) << 4 | (v % 10)) as u8;
    let mut bin = Vec::new();
    for lba in 0..sectors {
        let at = lba + 150;
        let header = [bcd(at / 4500), bcd(at / 75 % 60), bcd(at % 75), 2];
        bin.extend(raw_sector(header, [0x01, 0x00, 0x08, 0x00], 0));
    }
    Disc::from_bin(bin)
}

/// psx-spx: GetlocL fails with error 80h while the drive is still seeking
/// after a new ReadN, and the caller is expected to retry. Returning the
/// header of the sector read before the seek instead made Gran Turismo 2
/// believe its stream had landed in the wrong place, and it paused, sought
/// and restarted the read forever after skipping the intro.
#[test]
fn getlocl_fails_with_80h_while_a_read_is_still_seeking() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(addressed_disc(64)));
    cd.load_next_sector();
    assert!(cd.last_sector_header_valid);
    cd.scheduling_cycle = 1_000;
    cd.setloc_msf = (0x00, 0x02, 0x40);
    cd.setloc_pending = true;
    cd.cmd_read();
    cd.cmd_get_loc_l();
    let error = cd
        .pending
        .iter()
        .find(|ev| ev.irq == IrqType::Error)
        .expect("GetlocL fails during the seek");
    assert_ne!(error.bytes[0] & drive_status_bit::ERROR, 0);
    assert_eq!(error.bytes[1..], [0x80]);
}

/// SeekL: GetlocL fails during the seek, then reports the target sector's
/// header, as the drive has read it to confirm the seek.
#[test]
fn getlocl_reports_the_seekl_target_once_the_seek_is_over() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(addressed_disc(64)));
    cd.load_next_sector();
    cd.scheduling_cycle = 1_000;
    cd.setloc_msf = (0x00, 0x02, 0x40);
    cd.setloc_pending = true;
    cd.cmd_seek();
    cd.pending.clear();
    cd.cmd_get_loc_l();
    assert!(cd.pending.iter().any(|ev| ev.irq == IrqType::Error));

    cd.pending.clear();
    cd.scheduling_cycle = 1_000 + 100_000_000;
    cd.cmd_get_loc_l();
    let reply = cd
        .pending
        .iter()
        .find(|ev| ev.irq != IrqType::Error)
        .expect("GetlocL answers after the seek");
    assert_eq!(
        reply.bytes,
        [0x00, 0x02, 0x40, 0x02, 0x01, 0x00, 0x08, 0x00]
    );
}

#[test]
fn getlocl_returns_last_sector_header_and_subheader() {
    let mut cd = CdRom::new();
    let header = [0x12, 0x34, 0x56, 0x02];
    let subheader = [0xaa, 0xbb, 0xcc, 0xdd];
    cd.insert_disc(Some(Disc::from_bin(raw_sector(header, subheader, 0x6c))));

    cd.load_next_sector();
    assert_eq!(cd.data_fifo_len(), 2048);
    assert_eq!(cd.data_fifo.front().copied(), Some(0x6c));

    cd.cmd_get_loc_l();
    cd.tick(0);
    cd.tick(10_000_000);

    for expected in header.into_iter().chain(subheader) {
        assert_eq!(cd.read8(BASE + 1), expected);
    }
}

#[test]
fn load_next_sector_whole_sector_mode_returns_2340_bytes() {
    let mut cd = CdRom::new();
    let header = [0x01, 0x02, 0x03, 0x02];
    let subheader = [0x10, 0x20, 0x30, 0x40];
    let raw = raw_sector(header, subheader, 0xab);
    cd.mode = 0x20;
    cd.insert_disc(Some(Disc::from_bin(raw.clone())));

    cd.load_next_sector();

    assert_eq!(cd.data_fifo_len(), 2340);
    let first_bytes: Vec<u8> = cd.data_fifo.iter().take(12).copied().collect();
    assert_eq!(first_bytes, raw[12..24].to_vec());
}

#[test]
fn load_next_sector_whole_sector_mode1_keeps_full_raw_payload() {
    let mut cd = CdRom::new();
    let mut raw = raw_sector([0x00, 0x02, 0x00, 0x01], [0; 4], 0x5d);
    raw[2064..2352].fill(0xe7);
    cd.mode = 0x20;
    cd.insert_disc(Some(Disc::from_bin(raw.clone())));

    cd.load_next_sector();

    assert_eq!(cd.data_fifo_len(), 2340);
    let bytes: Vec<u8> = cd.data_fifo.iter().copied().collect();
    assert_eq!(bytes, raw[12..12 + 2340].to_vec());
}

#[test]
fn load_next_sector_mode1_uses_payload_after_header() {
    let mut cd = CdRom::new();
    let mut raw = raw_sector([0x00, 0x02, 0x00, 0x01], [0; 4], 0x00);
    raw[16..16 + 2048].fill(0x5d);
    raw[24..24 + 2048].fill(0xa7);
    cd.insert_disc(Some(Disc::from_bin(raw)));

    cd.load_next_sector();

    assert_eq!(cd.data_fifo_len(), 2048);
    assert_eq!(cd.data_fifo.front().copied(), Some(0x5d));
}

#[test]
fn load_next_sector_skips_audio_track_during_data_read() {
    // A data read (ReadN/ReadS) that advances into a Red Book audio track
    // delivers no data and raises no DataReady.
    let mut cd = CdRom::new();
    cd.insert_disc(Some(cdda_disc()));

    cd.read_lba = 12; // track 2 (audio) INDEX 01
    let raise = cd.load_next_sector();
    assert!(!raise, "audio-track sector must not raise DataReady");
    assert_eq!(cd.data_fifo_len(), 0, "audio-track sector delivers no data");

    // Control: a data-track sector still delivers its 2048-byte payload.
    cd.read_lba = 0; // track 1 (data)
    let raise = cd.load_next_sector();
    assert!(raise, "data-track sector must raise DataReady");
    assert_eq!(cd.data_fifo_len(), 2048);
}

#[test]
fn load_next_sector_sets_ready_latch_but_leaves_transfer_disarmed() {
    let mut cd = CdRom::new();
    let raw = raw_sector([0x00, 0x02, 0x00, 0x02], [0; 4], 0x6c);
    cd.insert_disc(Some(Disc::from_bin(raw)));

    cd.load_next_sector();

    assert!(cd.data_fifo_ready);
    assert!(!cd.data_transfer_active);
    assert_ne!(cd.read8(BASE) & status_bit::DATA_FIFO_NOT_EMPTY, 0);
    assert_eq!(cd.pop_data_byte(), 0);
    assert_eq!(cd.data_fifo_len(), 2048);
    assert_eq!(cd.data_fifo.front().copied(), Some(0x6c));
}

#[test]
fn sector_read_cycles_match_calibrated_initial_and_nominal_stream_cadence() {
    let mut cd = CdRom::new();
    // Default mode = 0x80 → double-speed. Initial carries the extra half
    // frame of rotational alignment measured on the console (records
    // 0x94/0x95); the steady cadence is the nominal 150 sectors per second.
    assert_eq!(cd.initial_sector_read_cycles(), CD_READ_TIME * 3 / 2);
    assert_eq!(cd.sector_read_cycles(), CD_READ_TIME / 2);
    // Flipping bit 7 off via SetMode gives single-speed (2×).
    cd.cmd_setmode(&[0x00]);
    assert_eq!(cd.initial_sector_read_cycles(), CD_READ_TIME * 3);
    assert_eq!(cd.sector_read_cycles(), CD_READ_TIME);
    // Setting other bits without bit 7 stays single-speed.
    cd.cmd_setmode(&[0x60]);
    assert_eq!(cd.initial_sector_read_cycles(), CD_READ_TIME * 3);
    assert_eq!(cd.sector_read_cycles(), CD_READ_TIME);
    // Back to double-speed when bit 7 returns.
    cd.cmd_setmode(&[0x80]);
    assert_eq!(cd.initial_sector_read_cycles(), CD_READ_TIME * 3 / 2);
    assert_eq!(cd.sector_read_cycles(), CD_READ_TIME / 2);
}

#[test]
fn read_command_uses_initial_delay_then_steady_stream_delay() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(Disc::from_bin(vec![0u8; psx_iso::SECTOR_BYTES * 4])));
    cd.scheduling_cycle = 1_000;

    cd.cmd_read();
    assert_eq!(cd.pending.len(), 1); // command ACK only; first DataReady is chained off it
    assert_eq!(
        cd.pending.front().map(|ev| ev.irq),
        Some(IrqType::Acknowledge)
    );

    assert!(cd.tick(1_000 + FIRST_RESPONSE_WITH_MEDIA_CYCLES + 1));
    let first_data_ready = cd
        .pending
        .iter()
        .find(|ev| ev.irq == IrqType::DataReady)
        .expect("first DataReady scheduled from ACK fire time");
    assert_eq!(
        first_data_ready.deadline,
        1_000 + FIRST_RESPONSE_WITH_MEDIA_CYCLES + 1 + CD_READ_TIME * 3 / 2
    );
    cd.irq_flag = 0;
    cd.responses.clear();

    assert!(cd.tick(1_000 + FIRST_RESPONSE_WITH_MEDIA_CYCLES + 1 + CD_READ_TIME * 3 / 2 + 1));
    let next_data_ready = cd
        .pending
        .iter()
        .find(|ev| ev.irq == IrqType::DataReady)
        .expect("steady DataReady scheduled");
    assert_eq!(
        next_data_ready.deadline,
        1_000 + FIRST_RESPONSE_WITH_MEDIA_CYCLES + 1 + CD_READ_TIME * 3 / 2 + CD_READ_TIME / 2
    );
}

/// A handler that has not run yet does not slow the disc down. The sector is
/// delivered on schedule with the CPU's CDROM interrupt line still pending,
/// because the platter is turning either way.
#[test]
fn dataready_arrives_on_schedule_even_with_the_cpu_irq_still_pending() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(Disc::from_bin(vec![0u8; psx_iso::SECTOR_BYTES * 4])));
    cd.scheduling_cycle = 1_000;

    cd.cmd_read();
    let ack_cycle = 1_000 + FIRST_RESPONSE_WITH_MEDIA_CYCLES + 1;
    assert!(cd.tick(ack_cycle));
    cd.irq_flag = 0;
    cd.responses.clear();

    let first_due = ack_cycle + CD_READ_TIME * 3 / 2 + 1;
    assert!(cd.tick_with_irq_pending(first_due, true));
    assert_eq!(cd.irq_flag, IrqType::DataReady as u8);
}

/// A sector that lands while the CPU still holds the previous INT1 is
/// announced once the CPU acknowledges, instead of never: the controller
/// keeps one pending INT1 and delivers it after the ack (psx-spx "Sector
/// Buffer VS GetlocL Response Tests", where a delayed handler still gets
/// the next INT1; the delay used here is unsourced, see
/// `DEFERRED_DATA_READY_DELAY`). Without this a handler that acked a little
/// late waited a whole sector for the next INT1, and the snap to the newest
/// sector then dropped the one it was never told about (Spider-Man lost
/// sectors this way under both kernels).
#[test]
fn a_sector_landing_during_an_unacked_int1_is_announced_after_the_ack() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(Disc::from_bin(vec![0u8; psx_iso::SECTOR_BYTES * 16])));
    cd.scheduling_cycle = 1_000;
    cd.mode = 0x80;

    cd.cmd_read();
    let ack_cycle = 1_000 + FIRST_RESPONSE_WITH_MEDIA_CYCLES + 1;
    assert!(cd.tick(ack_cycle));
    cd.irq_flag = 0;
    cd.responses.clear();

    // First sector: INT1, which software is slow to acknowledge.
    let first_due = ack_cycle + CD_READ_TIME * 3 / 2 + 1;
    assert!(cd.tick(first_due));
    assert_eq!(cd.irq_flag, IrqType::DataReady as u8);
    // Drain it as a DMA would.
    cd.data_transfer_active = true;
    while cd.data_fifo_len() != 0 {
        cd.pop_data_fifo_byte();
    }
    // Second sector lands while INT1 is still held: no new interrupt yet.
    let second_due = first_due + CD_READ_TIME / 2 + 1;
    assert!(!cd.tick(second_due));
    // Software acknowledges shortly after.
    let ack = second_due + 2_000;
    cd.write8_at(BASE, 1, ack);
    cd.write8_at(BASE + 3, 0x1f, ack);
    assert_eq!(cd.irq_flag, 0);
    assert!(
        cd.tick(ack + 1_000),
        "the held INT1 is delivered after the ack"
    );
    assert_eq!(cd.irq_flag, IrqType::DataReady as u8);
    assert_eq!(cd.dropped_sectors, 0);
}

/// Software that never acknowledges keeps getting sectors read at it, and
/// once the ring is full the oldest are lost. This is the failure the whole
/// model exists to expose: nothing stalls, nothing errors, the stream just
/// quietly develops a hole.
#[test]
fn a_handler_that_never_runs_loses_sectors_rather_than_stalling_the_drive() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(Disc::from_bin(vec![0u8; psx_iso::SECTOR_BYTES * 64])));
    cd.scheduling_cycle = 1_000;

    cd.cmd_read();
    let ack_cycle = 1_000 + FIRST_RESPONSE_WITH_MEDIA_CYCLES + 1;
    assert!(cd.tick(ack_cycle));
    cd.irq_flag = 0;
    cd.responses.clear();

    // Never acknowledge and never read a byte, so the interrupt line stays
    // held and the notifications stop, while the disc keeps turning.
    let mut at = ack_cycle;
    for _ in 0..(SECTOR_BUFFERS * 3) {
        at += CD_READ_TIME + 1;
        cd.tick_with_irq_pending(at, true);
    }

    assert!(cd.reading, "the drive keeps reading regardless");
    assert!(
        cd.dropped_sectors > 0,
        "sectors should have been lost, none were"
    );
    assert!(
        cd.waiting_sectors.len() < SECTOR_BUFFERS,
        "the ring must stay bounded, held {}",
        cd.waiting_sectors.len()
    );
}

#[test]
fn xa_audio_sector_suppresses_dataready_irq_but_keeps_streaming() {
    let mut cd = CdRom::new();
    let xa_sector = raw_sector([0x00, 0x02, 0x00, 0x02], [0x07, 0x02, 0x64, 0x01], 0);
    let data_sector = raw_sector([0x00, 0x02, 0x01, 0x02], [0x07, 0x02, 0x00, 0x00], 0x5a);
    let mut disc = xa_sector;
    disc.extend(data_sector);
    cd.insert_disc(Some(Disc::from_bin(disc)));
    cd.mode = 0xc0; // double-speed + STRSND/XA enable
    cd.setloc_msf = (0x00, 0x02, 0x00); // LBA 0
    cd.scheduling_cycle = 1_000;

    cd.cmd_read();
    assert!(cd.tick(1_000 + FIRST_RESPONSE_WITH_MEDIA_CYCLES + 1));
    cd.irq_flag = 0;
    cd.responses.clear();

    assert!(
        !cd.tick(1_000 + FIRST_RESPONSE_WITH_MEDIA_CYCLES + 1 + CD_READ_TIME * 3 / 2 + 1),
        "DataReady IRQs are suppressed for XA audio sectors while XA-ADPCM is enabled"
    );
    assert_eq!(cd.irq_flag, 0);
    assert!(cd.responses.is_empty());
    assert!(
        cd.cd_audio_queue_len() > 0,
        "XA sector should still feed decoded samples to the SPU"
    );
    assert_eq!(cd.irq_type_counts[IrqType::DataReady as usize], 0);
    let next_data_ready = cd
        .pending
        .iter()
        .find(|ev| ev.irq == IrqType::DataReady)
        .expect("suppressed audio sector should still chain the read stream");
    assert_eq!(
        next_data_ready.deadline,
        1_000 + FIRST_RESPONSE_WITH_MEDIA_CYCLES + 1 + CD_READ_TIME * 3 / 2 + CD_READ_TIME / 2
    );
}

/// An XA audio sector goes to the ADPCM decoder, never to the host-side
/// sector buffer. A data sector that was delivered (INT1 acknowledged) but not
/// yet read must survive the audio sector that lands behind it. STR movies
/// interleave one audio sector every few video sectors, and wiping the pending
/// video sector each time left Tekken 3's intro decoding zeros.
#[test]
fn xa_audio_sector_leaves_the_pending_data_sector_intact() {
    let mut cd = CdRom::new();
    let data_sector = raw_sector([0x00, 0x02, 0x00, 0x02], [0x01, 0x00, 0x48, 0x00], 0x5a);
    let xa_sector = raw_sector([0x00, 0x02, 0x01, 0x02], [0x01, 0x01, 0x64, 0x01], 0);
    let mut disc = data_sector;
    disc.extend(xa_sector);
    cd.insert_disc(Some(Disc::from_bin(disc)));
    cd.mode = 0xc0; // double speed + XA-ADPCM, filter off
    cd.setloc_msf = (0x00, 0x02, 0x00);
    cd.scheduling_cycle = 1_000;

    cd.cmd_read();
    let ack = 1_000 + FIRST_RESPONSE_WITH_MEDIA_CYCLES + 1;
    assert!(cd.tick(ack));
    cd.irq_flag = 0;
    let first_sector = ack + CD_READ_TIME * 3 / 2 + 1;
    assert!(cd.tick(first_sector), "the data sector raises INT1");
    assert_eq!(cd.irq_flag, IrqType::DataReady as u8);
    cd.irq_flag = 0; // acknowledged, but software has not read it yet

    assert!(
        !cd.tick(first_sector + CD_READ_TIME / 2 + 1),
        "audio raises nothing"
    );
    assert!(cd.cd_audio_queue_len() > 0, "the audio sector was decoded");
    assert!(cd.data_fifo_ready, "the unread data sector is still there");
    assert_eq!(cd.data_fifo_len(), 2048);
    assert_eq!(cd.data_fifo.front().copied(), Some(0x5a));
    assert!(
        cd.waiting_sectors.is_empty(),
        "audio never enters the sector ring"
    );
}

/// With the XA filter on, audio sectors of other channels are skipped by the
/// decoder and, like every XA audio sector, are not handed to the CPU. They
/// must not be queued in the sector ring either, where they would overflow it
/// and be counted as lost data.
#[test]
fn filtered_out_xa_audio_sectors_are_not_queued_as_data() {
    let mut cd = CdRom::new();
    let mut disc = Vec::new();
    for i in 0..16u8 {
        disc.extend(raw_sector(
            [0x00, 0x02, i, 0x02],
            [0x01, 0x02 + (i & 1), 0x64, 0x01],
            0,
        ));
    }
    cd.insert_disc(Some(Disc::from_bin(disc)));
    cd.mode = 0xc8; // double speed + XA-ADPCM + filter
    cd.xa_filter_file = 1;
    cd.xa_filter_channel = 2;
    cd.setloc_msf = (0x00, 0x02, 0x00);
    cd.scheduling_cycle = 1_000;

    cd.cmd_read();
    let mut at = 1_000 + FIRST_RESPONSE_WITH_MEDIA_CYCLES + 1;
    assert!(cd.tick(at));
    cd.irq_flag = 0;
    at += CD_READ_TIME * 3 / 2 + 1;
    for _ in 0..16 {
        cd.tick(at);
        at += CD_READ_TIME / 2 + 1;
    }

    assert_eq!(cd.irq_type_counts[IrqType::DataReady as usize], 0);
    assert!(cd.data_fifo.is_empty());
    assert!(cd.waiting_sectors.is_empty());
    assert_eq!(cd.dropped_sectors, 0);
    assert!(
        cd.cd_audio_queue_len() > 0,
        "the matching channel still plays"
    );
}

#[test]
fn pause_on_spun_up_drive_uses_short_followup_delay() {
    let mut cd = CdRom::new();
    cd.motor_on = true;
    cd.reading = true;
    cd.scheduling_cycle = 1_000;
    cd.insert_pending_event(PendingEvent {
        command: 0x06,
        deadline: 50_000,
        irq: IrqType::DataReady,
        bytes: vec![0x20],
        followup: None,
    });

    cd.cmd_pause();
    assert_eq!(
        cd.pending.len(),
        1,
        "Pause should cancel the in-flight read chain"
    );

    let ack_deadline = 1_000 + FIRST_RESPONSE_CYCLES;
    assert!(cd.tick(ack_deadline + 1));
    assert_eq!(
        cd.read8(BASE + 1),
        drive_status_bit::MOTOR_ON | drive_status_bit::READING,
        "the Pause ACK reports the drive as it was: still reading (console 0x22)"
    );
    cd.irq_flag = 0;
    let pause_complete = cd
        .pending
        .iter()
        .find(|ev| ev.irq == IrqType::Complete)
        .expect("pause completion chained off ACK");
    // A read that has delivered no sector yet: the first-sector figure.
    assert_eq!(
        pause_complete.deadline,
        ack_deadline + 1 + pause_from_read_cycles(cd.sector_read_cycles(), 0, false)
    );
    assert_eq!(
        pause_complete.bytes[0],
        drive_status_bit::MOTOR_ON,
        "the second response says stopped (console 0x02)"
    );
}

#[test]
fn seek_command_charges_the_measured_mech_curve() {
    let mut cd = CdRom::new();
    // Even with a prior seek done, silicon answers SeekL along the measured
    // distance curve (record 0x90: ~11 ms for a 1-sector hop, no free path).
    cd.motor_on = true;
    cd.seek_done = true;
    cd.read_lba = 200;
    cd.setloc_msf = (0x00, 0x02, 0x16);
    cd.scheduling_cycle = 1_000;

    cd.cmd_seek();

    let ack_deadline = 1_000 + FIRST_RESPONSE_CYCLES;
    assert!(cd.tick(ack_deadline + 1));
    let seek_complete = cd
        .pending
        .iter()
        .find(|ev| ev.irq == IrqType::Complete)
        .expect("seek completion chained off ACK");
    let target = msf_to_lba(0x00, 0x02, 0x16);
    assert_eq!(
        seek_complete.deadline,
        ack_deadline + 1 + seek_cycles(target.abs_diff(200))
    );
    assert_eq!(cd.read_lba, target, "seek commits the head position");
    assert!(
        !cd.setloc_pending,
        "a following read must not charge the same journey twice"
    );
}

#[test]
fn setloc_far_target_clears_seek_done_and_marks_pending() {
    let mut cd = CdRom::new();
    cd.seek_done = true;
    cd.read_lba = 200;

    cd.cmd_setloc(&[0x00, 0x02, 0x16]);

    assert!(
        !cd.seek_done,
        "far SetLoc should force the next seek slow-path"
    );
    assert!(
        cd.setloc_pending,
        "SetLoc should latch until Read/Play consumes it"
    );
}

#[test]
fn read_command_cancels_old_stream_and_rearms_first_sector() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(Disc::from_bin(vec![0u8; psx_iso::SECTOR_BYTES * 32])));
    cd.reading = true;
    cd.scheduling_cycle = 1_000;
    cd.setloc_msf = (0x00, 0x02, 0x16);
    cd.setloc_pending = true;
    cd.insert_pending_event(PendingEvent {
        command: 0x06,
        deadline: 50_000,
        irq: IrqType::DataReady,
        bytes: vec![0x20],
        followup: None,
    });

    cd.cmd_read();

    assert!(
        cd.pending.iter().all(|ev| ev.irq != IrqType::DataReady),
        "stale DataReady events from the previous stream must be cancelled"
    );
    let ack = cd
        .pending
        .iter()
        .find(|ev| ev.irq == IrqType::Acknowledge)
        .expect("ReadN ACK present");
    let followup = ack.followup.as_ref().expect("first sector chained off ACK");
    assert_eq!(followup.irq, IrqType::DataReady);
    // The pending SetLoc moved the head from LBA 0 to the target, so the
    // first sector carries the measured travel cost on top of the ordinary
    // first-sector delay.
    assert_eq!(
        followup.delay,
        cd.initial_sector_read_cycles() + seek_cycles(msf_to_lba(0x00, 0x02, 0x16))
    );
}

#[test]
fn relocated_read_charges_travel_on_first_sector_then_streams_at_cadence() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(Disc::from_bin(vec![0u8; psx_iso::SECTOR_BYTES * 64])));
    cd.scheduling_cycle = 1_000;
    cd.setloc_msf = (0x00, 0x02, 0x16);
    cd.setloc_pending = true;

    cd.cmd_read();

    let ack_deadline = 1_000 + FIRST_RESPONSE_WITH_MEDIA_CYCLES;
    assert!(cd.tick(ack_deadline + 1));
    cd.irq_flag = 0;

    // Head travel (measured curve) lands on the FIRST sector; once sectors
    // stream, silicon delivers them at the plain frame cadence (records
    // 0x94/0x95), with no long-gap cliff on the second sector.
    let travel = seek_cycles(msf_to_lba(0x00, 0x02, 0x16));
    let first_sector_deadline = ack_deadline + 1 + CD_READ_TIME * 3 / 2 + travel;
    assert!(cd.tick(first_sector_deadline + 1));
    let next_sector = cd
        .pending
        .iter()
        .find(|ev| ev.irq == IrqType::DataReady)
        .expect("read stream should continue after first sector");
    assert_eq!(
        next_sector.deadline,
        first_sector_deadline + CD_READ_TIME / 2
    );
    assert!(
        !cd.location_changed,
        "the travel latch should clear once the stream is running"
    );
}

#[test]
fn streaming_cadence_does_not_accumulate_the_lateness_of_each_tick() {
    // Every sector is serviced a little after its deadline (the CPU only
    // reaches the drive at branch boundaries). The next sector is chained
    // from the deadline, so the stream stays on the disc's own 150 sectors
    // a second however late each tick is.
    let mut cd = CdRom::new();
    cd.insert_disc(Some(Disc::from_bin(vec![0u8; psx_iso::SECTOR_BYTES * 64])));
    cd.mode = 0x80; // double speed, plain data
    cd.scheduling_cycle = 1_000;
    cd.setloc_msf = (0x00, 0x02, 0x00);
    cd.cmd_read();
    let ack_deadline = 1_000 + FIRST_RESPONSE_WITH_MEDIA_CYCLES;
    assert!(cd.tick(ack_deadline + 1));
    cd.irq_flag = 0;
    let mut deadline = ack_deadline + 1 + CD_READ_TIME * 3 / 2;
    for sector in 0..30u64 {
        let lateness = 1 + (sector * 37) % 300;
        cd.tick(deadline + lateness);
        cd.irq_flag = 0;
        deadline += CD_READ_TIME / 2;
        let next = cd
            .pending
            .iter()
            .find(|ev| ev.irq == IrqType::DataReady)
            .expect("the stream continues")
            .deadline;
        assert_eq!(next, deadline, "after sector {sector}");
    }
}

#[test]
fn a_tick_more_than_a_period_late_restarts_the_cadence_from_now() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(Disc::from_bin(vec![0u8; psx_iso::SECTOR_BYTES * 64])));
    cd.mode = 0x80;
    cd.scheduling_cycle = 1_000;
    cd.setloc_msf = (0x00, 0x02, 0x00);
    cd.cmd_read();
    let ack_deadline = 1_000 + FIRST_RESPONSE_WITH_MEDIA_CYCLES;
    assert!(cd.tick(ack_deadline + 1));
    cd.irq_flag = 0;
    let first = ack_deadline + 1 + CD_READ_TIME * 3 / 2;
    let now = first + CD_READ_TIME * 5; // the CPU was away for five frames
    cd.tick(now);
    let next = cd
        .pending
        .iter()
        .find(|ev| ev.irq == IrqType::DataReady)
        .expect("the stream continues")
        .deadline;
    assert_eq!(next, now + CD_READ_TIME / 2);
}

#[test]
fn gettn_single_track_disc_reports_one_to_one() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(Disc::from_bin(vec![0u8; psx_iso::SECTOR_BYTES * 10])));

    cd.cmd_get_tn();
    cd.tick(10_000_000);

    assert_eq!(cd.read8(BASE + 1), cd.stat_byte());
    assert_eq!(cd.read8(BASE + 1), 0x01);
    assert_eq!(cd.read8(BASE + 1), 0x01);
}

#[test]
fn gettd_track_one_reports_data_start() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(Disc::from_bin(vec![0u8; psx_iso::SECTOR_BYTES * 10])));

    cd.cmd_get_td(&[0x01]);
    cd.tick(10_000_000);

    assert_eq!(cd.read8(BASE + 1), cd.stat_byte());
    assert_eq!(cd.read8(BASE + 1), 0x00);
    assert_eq!(cd.read8(BASE + 1), 0x02);
    assert_ne!(cd.read8(BASE) & status_bit::RESPONSE_FIFO_NOT_EMPTY, 0);
    assert_eq!(cd.read8(BASE + 1), 0x00);
    assert_eq!(cd.read8(BASE) & status_bit::RESPONSE_FIFO_NOT_EMPTY, 0);
}

#[test]
fn gettd_track_zero_reports_leadout_minute_second() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(Disc::from_bin(vec![0u8; psx_iso::SECTOR_BYTES * 10])));

    cd.cmd_get_td(&[0x00]);
    cd.tick(10_000_000);

    let _stat = cd.read8(BASE + 1);
    assert_eq!(cd.read8(BASE + 1), 0x00);
    assert_eq!(cd.read8(BASE + 1), 0x02);
    assert_ne!(cd.read8(BASE) & status_bit::RESPONSE_FIFO_NOT_EMPTY, 0);
    assert_eq!(cd.read8(BASE + 1), 0x00);
    assert_eq!(cd.read8(BASE) & status_bit::RESPONSE_FIFO_NOT_EMPTY, 0);
}

/// LibCrypt sectors (from a .sbi file) carry subchannel Q with a bad CRC,
/// which the controller ignores: GetlocP keeps reporting the position of
/// the last good sector (psx-spx "CDROM Protection - LibCrypt").
#[test]
fn getlocp_skips_libcrypt_sectors_with_bad_subchannel_q() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(Disc::from_bin(vec![0u8; psx_iso::SECTOR_BYTES * 16])));
    cd.set_bad_subq_sectors(vec![6, 7]);

    let absolute_frame = |cd: &mut CdRom, lba: u32, at: u64| {
        cd.read_lba = lba;
        cd.cmd_get_loc_p();
        cd.tick(at);
        let reply: Vec<u8> = (0..8).map(|_| cd.read8(BASE + 1)).collect();
        cd.irq_flag = 0;
        reply[7]
    };
    assert_eq!(absolute_frame(&mut cd, 5, 10_000_000), 0x05);
    assert_eq!(
        absolute_frame(&mut cd, 7, 20_000_000),
        0x05,
        "6 and 7 are ignored"
    );
    assert_eq!(absolute_frame(&mut cd, 8, 30_000_000), 0x08);
}

#[test]
fn getlocp_reports_index0_and_index1_for_pregap_tracks() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(multitrack_disc_with_pregap()));

    cd.read_lba = 10;
    cd.cmd_get_loc_p();
    cd.tick(10_000_000);
    assert_eq!(cd.read8(BASE + 1), 0x02);
    assert_eq!(cd.read8(BASE + 1), 0x00);
    assert_eq!(cd.read8(BASE + 1), 0x00);
    assert_eq!(cd.read8(BASE + 1), 0x00);
    assert_eq!(cd.read8(BASE + 1), 0x01);
    assert_eq!(cd.read8(BASE + 1), 0x00);
    assert_eq!(cd.read8(BASE + 1), 0x02);
    assert_eq!(cd.read8(BASE + 1), 0x10);
    cd.irq_flag = 0;

    cd.read_lba = 12;
    cd.cmd_get_loc_p();
    cd.tick(20_000_000);
    assert_eq!(cd.read8(BASE + 1), 0x02);
    assert_eq!(cd.read8(BASE + 1), 0x01);
    assert_eq!(cd.read8(BASE + 1), 0x00);
    assert_eq!(cd.read8(BASE + 1), 0x00);
    assert_eq!(cd.read8(BASE + 1), 0x00);
    assert_eq!(cd.read8(BASE + 1), 0x00);
    assert_eq!(cd.read8(BASE + 1), 0x02);
    assert_eq!(cd.read8(BASE + 1), 0x12);
}

#[test]
fn gettn_reports_last_track_for_multitrack_disc() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(multitrack_disc_with_pregap()));

    cd.cmd_get_tn();
    cd.tick(10_000_000);

    assert_eq!(cd.read8(BASE + 1), cd.stat_byte());
    assert_eq!(cd.read8(BASE + 1), 0x01);
    assert_eq!(cd.read8(BASE + 1), 0x02);
}

#[test]
fn gettd_reports_track_start_and_leadout_for_multitrack_disc() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(multitrack_disc_with_pregap()));

    cd.cmd_get_td(&[0x02]);
    cd.tick(10_000_000);
    assert_eq!(cd.read8(BASE + 1), cd.stat_byte());
    assert_eq!(cd.read8(BASE + 1), 0x00);
    assert_eq!(cd.read8(BASE + 1), 0x02);
    assert_eq!(cd.read8(BASE + 1), 0x00);
    cd.irq_flag = 0;

    cd.cmd_get_td(&[0x00]);
    cd.tick(20_000_000);
    assert_eq!(cd.read8(BASE + 1), cd.stat_byte());
    assert_eq!(cd.read8(BASE + 1), 0x00);
    assert_eq!(cd.read8(BASE + 1), 0x02);
    assert_eq!(cd.read8(BASE + 1), 0x00);
}

#[test]
fn play_track_param_seeks_to_requested_track_start() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(multitrack_disc_with_pregap()));

    play_and_arrive(&mut cd, 0x02);

    assert_eq!(cd.read_lba, 12);
    assert_eq!(
        cd.drive_status & drive_status_bit::PLAYING,
        drive_status_bit::PLAYING
    );
}

#[test]
fn cdda_play_streams_pcm_samples() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(cdda_disc()));

    play_and_arrive(&mut cd, 0x02);
    cd.pump_cdda_samples(2);

    assert_eq!(cd.read_lba, 12);
    assert_eq!(cd.cdda_sample_index, 2);
    assert_eq!(cd.drain_cd_audio(), vec![(1000, -1000), (1001, -1001)]);
}

#[test]
fn cdda_play_advances_one_lba_per_sector_frame() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(cdda_disc()));

    play_and_arrive(&mut cd, 0x02);
    cd.pump_cdda_samples(CDDA_SAMPLES_PER_SECTOR);

    assert_eq!(cd.read_lba, 13);
    assert_eq!(cd.cdda_sample_index, 0);
    assert_eq!(cd.cd_audio_queue_len(), CDDA_SAMPLES_PER_SECTOR);
}

#[test]
fn cdda_play_from_a_pregap_plays_silence_into_index_1() {
    // Tomb Raider seeks into track 2's pregap (index 00), waits for GetlocP
    // to say so, issues Play, then waits for index 01. The drive plays the
    // pregap like any other audio sector (here silence: the pregap is not
    // in the track file) and carries on into the track.
    let mut cd = CdRom::new();
    cd.insert_disc(Some(cdda_disc()));
    cd.setloc_msf = (0x00, 0x02, 0x10); // BCD; LBA 10, the first pregap sector
    cd.setloc_pending = true;
    cd.cmd_play(&[]);
    let arrives = cd.cdda_seek_done_at.expect("Play arms a seek");
    cd.tick(arrives + 1);

    cd.pump_cdda_samples(CDDA_SAMPLES_PER_SECTOR * 2);
    assert_eq!(cd.read_lba, 12);
    assert_ne!(cd.drive_status & drive_status_bit::PLAYING, 0);
    let pregap: Vec<_> = cd.drain_cd_audio();
    assert_eq!(pregap.len(), CDDA_SAMPLES_PER_SECTOR * 2);
    assert!(pregap.iter().all(|&s| s == (0, 0)));

    cd.pump_cdda_samples(1);
    assert_eq!(cd.drain_cd_audio(), vec![(1000, -1000)]);
}

#[test]
fn cdda_play_stops_at_audio_track_end() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(cdda_disc()));

    play_and_arrive(&mut cd, 0x02);
    cd.pump_cdda_samples(CDDA_SAMPLES_PER_SECTOR * 2 + 1);

    assert_eq!(cd.read_lba, 14);
    assert_eq!(cd.cdda_sample_index, 0);
    assert_eq!(cd.drive_status & drive_status_bit::PLAYING, 0);
}

#[test]
fn command_during_cdda_playback_acks_later_than_when_idle() {
    // Baseline: GetStat while the drive is idle acks at the flat first-response
    // delay (what every emulator models, and what makes the bug invisible).
    let mut idle = CdRom::new();
    idle.insert_disc(Some(cdda_disc()));
    idle.queue_command(0x01, 1_000); // GetStat, not playing
    let idle_ack = idle
        .pending
        .iter()
        .find(|ev| ev.command == 0x01 && ev.irq == IrqType::Acknowledge)
        .expect("idle GetStat ack pending");
    assert_eq!(idle_ack.deadline, 1_000 + FIRST_RESPONSE_WITH_MEDIA_CYCLES);

    // During CD-DA playback the same GetStat acks CDDA_BUSY_RESPONSE_CYCLES
    // later -- the busy single controller modelled. This is the latency that
    // makes "poll the drive every frame while music plays" stall on silicon,
    // so the engine must not do it (see the menu CD-DA loop).
    let mut playing = CdRom::new();
    playing.insert_disc(Some(cdda_disc()));
    playing.push_param(0x02);
    // Play track 2. Play only arms a seek; the busy penalty models a
    // controller actually streaming audio, so wait for the head to arrive
    // before measuring it.
    playing.queue_command(0x03, 2_000);
    let arrives = playing.cdda_seek_done_at.expect("Play arms a seek");
    playing.complete_cdda_seek(arrives);
    assert_ne!(playing.drive_status & drive_status_bit::PLAYING, 0);
    // The Play command itself was issued while idle, so its own ack is NOT
    // penalised (only commands during an already-playing drive are).
    let play_ack = playing
        .pending
        .iter()
        .find(|ev| ev.command == 0x03 && ev.irq == IrqType::Acknowledge)
        .expect("play ack pending");
    assert_eq!(play_ack.deadline, 2_000 + FIRST_RESPONSE_WITH_MEDIA_CYCLES);

    playing.queue_command(0x01, arrives + 1_000); // GetStat while playing
    let busy_ack = playing
        .pending
        .iter()
        .find(|ev| ev.command == 0x01 && ev.irq == IrqType::Acknowledge)
        .expect("playing GetStat ack pending");
    assert_eq!(
        busy_ack.deadline,
        arrives + 1_000 + FIRST_RESPONSE_WITH_MEDIA_CYCLES + CDDA_BUSY_RESPONSE_CYCLES
    );
}

#[test]
fn getstat_after_cdda_track_end_is_prompt_and_reports_stopped() {
    // The engine loops a menu track by polling until the drive reports stopped,
    // then re-playing. That only works if, once the track ends, GetStat answers
    // promptly (no busy penalty -- the drive is no longer streaming) AND reports
    // PLAYING clear. This is the counterpart to the during-playback penalty.
    let mut cd = CdRom::new();
    cd.insert_disc(Some(cdda_disc()));
    cd.push_param(0x02);
    cd.queue_command(0x03, 1_000); // Play track 2
    cd.pump_cdda_samples(CDDA_SAMPLES_PER_SECTOR * 2 + 1); // run past track end
    assert_eq!(cd.drive_status & drive_status_bit::PLAYING, 0);

    cd.queue_command(0x01, 2_000); // GetStat after the track ended
    let ack = cd
        .pending
        .iter()
        .find(|ev| ev.command == 0x01 && ev.irq == IrqType::Acknowledge)
        .expect("post-end GetStat ack pending");
    assert_eq!(ack.deadline, 2_000 + FIRST_RESPONSE_WITH_MEDIA_CYCLES);
    assert_eq!(ack.bytes[0] & drive_status_bit::PLAYING, 0);
}

#[test]
fn setfilter_and_getparam_roundtrip_filter_state() {
    let mut cd = CdRom::new();
    cd.mode = 0xe8;

    cd.cmd_set_filter(&[0x12, 0x34]);
    assert_eq!(cd.xa_filter_file, 0x12);
    assert_eq!(cd.xa_filter_channel, 0x34);
    cd.pending.clear();
    cd.responses.clear();
    cd.irq_flag = 0;

    cd.cmd_get_param();
    cd.tick(10_000_000);
    assert_eq!(cd.read8(BASE + 1), cd.stat_byte());
    assert_eq!(cd.read8(BASE + 1), 0xe8);
    assert_eq!(cd.read8(BASE + 1), 0x00);
    assert_eq!(cd.read8(BASE + 1), 0x12);
    assert_eq!(cd.read8(BASE + 1), 0x34);
}

#[test]
fn mute_and_demute_commands_flip_latch() {
    let mut cd = CdRom::new();
    assert!(!cd.muted);

    cd.cmd_mute(true);
    cd.tick(10_000_000);
    assert!(cd.muted);

    cd.pending.clear();
    cd.responses.clear();
    cd.irq_flag = 0;

    cd.cmd_mute(false);
    cd.tick(20_000_000);
    assert!(!cd.muted);
}

#[test]
fn xa_decode_silent_stereo_sector_has_full_frame_count() {
    let mut raw = vec![0u8; psx_iso::SECTOR_BYTES];
    raw[15] = 2;
    raw[18] = 0x24;
    raw[19] = 0x01; // 4-bit stereo, 37.8 kHz

    let mut left = crate::spu::XaDecoderState::new();
    let mut right = crate::spu::XaDecoderState::new();
    let coding = parse_xa_coding(raw[19]).expect("valid XA coding");
    let samples = decode_xa_audio_sector(
        &raw,
        coding,
        &mut left,
        &mut right,
        &mut crate::spu::XaResampler::new(),
    )
    .expect("common 4-bit stereo XA should decode");

    assert_eq!(samples.len(), 2352);
    assert!(samples.iter().all(|&(l, r)| l == 0 && r == 0));
}

#[test]
fn xa_decode_silent_mono_sector_has_full_frame_count() {
    let mut raw = vec![0u8; psx_iso::SECTOR_BYTES];
    raw[15] = 2;
    raw[18] = 0x24;
    raw[19] = 0x00; // 4-bit mono, 37.8 kHz

    let mut left = crate::spu::XaDecoderState::new();
    let mut right = crate::spu::XaDecoderState::new();
    let coding = parse_xa_coding(raw[19]).expect("valid XA coding");
    let samples = decode_xa_audio_sector(
        &raw,
        coding,
        &mut left,
        &mut right,
        &mut crate::spu::XaResampler::new(),
    )
    .expect("4-bit mono XA should decode");

    assert_eq!(samples.len(), 4704);
    assert!(samples.iter().all(|&(l, r)| l == 0 && r == 0));
}

#[test]
fn xa_decode_4bit_stereo_blocks_follow_the_documented_layout() {
    // First 128-byte portion: block 0 is the left channel (low nibble of
    // byte 0 of each word), block 1 the right (high nibble). Headers for
    // blocks 0 and 1 sit at portion offsets 4 and 5: range 12, filter 0.
    let mut raw = vec![0u8; psx_iso::SECTOR_BYTES];
    raw[24 + 4] = 0x0c;
    raw[24 + 5] = 0x0c;
    raw[24 + 16] = 0xf1; // left +1, right -1 (first sample of both)
    raw[24 + 16 + 4] = 0x72; // second word: left +2, right +7
    let mut left = crate::spu::XaDecoderState::new();
    let mut right = crate::spu::XaDecoderState::new();
    let coding = XaCoding {
        stereo: true,
        freq: 37_800,
        nbits: 4,
    };
    let samples = decode_xa_native(&raw, coding, &mut left, &mut right).unwrap();
    assert_eq!(samples[0], (1, -1));
    assert_eq!(samples[1], (2, 7));
}

#[test]
fn xa_decode_8bit_stereo_blocks_use_one_byte_per_block() {
    // 8-bit data: byte 0 of a word is block 0 (left), byte 1 block 1
    // (right). A widened `byte << 8` shifted by range 8 gives the byte.
    let mut raw = vec![0u8; psx_iso::SECTOR_BYTES];
    raw[24 + 4] = 0x08;
    raw[24 + 5] = 0x08;
    raw[24 + 16] = 0x05;
    raw[24 + 16 + 1] = 0xfb; // -5
    let mut left = crate::spu::XaDecoderState::new();
    let mut right = crate::spu::XaDecoderState::new();
    let coding = XaCoding {
        stereo: true,
        freq: 37_800,
        nbits: 8,
    };
    let samples = decode_xa_native(&raw, coding, &mut left, &mut right).unwrap();
    assert_eq!(samples[0], (5, -5));
}

#[test]
fn xa_decode_uses_stream_coding_not_each_sector_byte() {
    let mut raw = vec![0u8; psx_iso::SECTOR_BYTES];
    raw[15] = 2;
    raw[18] = 0x24;
    raw[19] = 0x0c; // invalid if reparsed; the stream coding wins mid-stream.

    let mut left = crate::spu::XaDecoderState::new();
    let mut right = crate::spu::XaDecoderState::new();
    let coding = XaCoding {
        stereo: true,
        freq: 37_800,
        nbits: 4,
    };
    let samples = decode_xa_audio_sector(
        &raw,
        coding,
        &mut left,
        &mut right,
        &mut crate::spu::XaResampler::new(),
    )
    .expect("stream coding should drive decode after the first sector");

    assert_eq!(samples.len(), 2352);
    assert!(samples.iter().all(|&(l, r)| l == 0 && r == 0));
}

#[test]
fn motor_on_command_sets_motor_flag() {
    let mut cd = CdRom::new();
    assert!(!cd.motor_on);

    cd.cmd_motor_on();
    cd.tick(10_000_000);

    assert!(cd.motor_on);
    assert_eq!(
        cd.read8(BASE + 1) & drive_status_bit::MOTOR_ON,
        drive_status_bit::MOTOR_ON
    );
}

#[test]
fn reset_cancels_read_and_completes_with_motor_on() {
    let mut cd = CdRom::new();
    cd.motor_on = true;
    cd.reading = true;
    cd.data_fifo.push_back(0xab);
    cd.pending.push_back(PendingEvent {
        command: 0x06,
        deadline: 123,
        irq: IrqType::DataReady,
        bytes: vec![0x20],
        followup: None,
    });

    cd.cmd_reset();
    assert!(cd.tick(FIRST_RESPONSE_CYCLES + 1));
    assert_eq!(cd.irq_flag, IrqType::Acknowledge as u8);
    cd.write8(BASE, 1);
    cd.write8(BASE + 3, 0x1f);
    assert_eq!(cd.irq_flag, 0);
    assert!(cd.tick(FIRST_RESPONSE_CYCLES + RESET_SECOND_RESPONSE_CYCLES + 2));
    assert_eq!(cd.irq_flag, IrqType::Complete as u8);

    assert!(cd.motor_on);
    assert!(!cd.reading);
    assert!(cd.data_fifo.is_empty());
    assert_eq!(
        cd.pending.len(),
        0,
        "reset ACK and completion should both be delivered"
    );
    assert_eq!(
        cd.read8(BASE + 1) & drive_status_bit::MOTOR_ON,
        drive_status_bit::MOTOR_ON
    );
}

#[test]
fn repeated_reset_before_ack_reschedules_one_long_completion() {
    let mut cd = CdRom::new();
    cd.last_command = 0x0a;
    cd.scheduling_cycle = 100;
    cd.cmd_reset();

    cd.scheduling_cycle = 1_000;
    cd.cmd_reset();
    assert_eq!(
        count_pending_command_irq(&cd, 0x0a, IrqType::Acknowledge),
        1
    );
    assert_eq!(count_pending_command_irq(&cd, 0x0a, IrqType::Complete), 1);
    let ack = cd
        .pending
        .iter()
        .find(|event| event.command == 0x0a && event.irq == IrqType::Acknowledge)
        .expect("reset ACK");
    assert_eq!(ack.deadline, 1_000 + FIRST_RESPONSE_CYCLES);
    assert_eq!(
        ack.followup.as_ref().map(|f| (f.command, f.irq, f.delay)),
        Some((0x0a, IrqType::Complete, RESET_SECOND_RESPONSE_CYCLES))
    );

    assert!(!cd.tick(100 + FIRST_RESPONSE_CYCLES + 1));
    assert!(cd.tick(1_000 + FIRST_RESPONSE_CYCLES + 1));
    cd.irq_flag = 0;
    cd.responses.clear();
    assert_eq!(count_pending_command_irq(&cd, 0x0a, IrqType::Complete), 1);
}

#[test]
fn repeated_reset_while_completion_pending_publishes_completion() {
    let mut cd = CdRom::new();
    cd.last_command = 0x0a;
    cd.scheduling_cycle = 100;
    cd.cmd_reset();

    assert!(cd.tick(100 + FIRST_RESPONSE_CYCLES + 1));
    cd.irq_flag = 0;
    cd.responses.clear();

    cd.scheduling_cycle = 2_000_000;
    cd.cmd_reset();
    assert_eq!(
        count_pending_command_irq(&cd, 0x0a, IrqType::Acknowledge),
        0
    );
    assert_eq!(count_pending_command_irq(&cd, 0x0a, IrqType::Complete), 1);
    let complete = cd
        .pending
        .iter()
        .find(|event| event.command == 0x0a && event.irq == IrqType::Complete)
        .expect("reset completion after pending completion");
    assert_eq!(complete.deadline, 2_000_000 + FIRST_RESPONSE_CYCLES);
    assert!(complete.followup.is_none());
}

#[test]
fn pending_events_are_kept_sorted_by_deadline() {
    let mut cd = CdRom::new();
    cd.insert_pending_event(PendingEvent {
        command: 0x06,
        deadline: 300,
        irq: IrqType::DataReady,
        bytes: vec![0x20],
        followup: None,
    });
    cd.insert_pending_event(PendingEvent {
        command: 0x01,
        deadline: 100,
        irq: IrqType::Acknowledge,
        bytes: vec![0x00],
        followup: None,
    });
    cd.insert_pending_event(PendingEvent {
        command: 0x01,
        deadline: 200,
        irq: IrqType::Complete,
        bytes: vec![0x00],
        followup: None,
    });

    let deadlines = cd.pending.iter().map(|ev| ev.deadline).collect::<Vec<_>>();
    assert_eq!(deadlines, vec![100, 200, 300]);
}

#[test]
fn followup_chains_to_latest_ack_even_with_later_events_present() {
    let mut cd = CdRom::new();
    cd.insert_pending_event(PendingEvent {
        command: 0x06,
        deadline: 300,
        irq: IrqType::DataReady,
        bytes: vec![0x20],
        followup: None,
    });
    cd.insert_pending_event(PendingEvent {
        command: 0x01,
        deadline: 100,
        irq: IrqType::Acknowledge,
        bytes: vec![0x00],
        followup: None,
    });

    cd.chain_followup(IrqType::Complete, vec![0x01], 77);

    let ack = cd
        .pending
        .iter()
        .find(|ev| ev.irq == IrqType::Acknowledge)
        .expect("ack event present");
    let data_ready = cd
        .pending
        .iter()
        .find(|ev| ev.irq == IrqType::DataReady)
        .expect("later event present");
    assert_eq!(
        ack.followup
            .as_ref()
            .map(|f| (f.delay, f.irq, f.bytes.clone())),
        Some((77, IrqType::Complete, vec![0x01]))
    );
    assert!(
        data_ready.followup.is_none(),
        "later event must stay untouched"
    );
}

#[test]
fn disc_region_code_uses_license_sector_text() {
    use psx_iso::{SECTOR_BYTES, SECTOR_USER_DATA_OFFSET};

    let mut bytes = vec![0u8; SECTOR_BYTES * 6];
    let license = b"Licensed by Sony Computer Entertainment Europe for PlayStation";
    let off = 4 * SECTOR_BYTES + SECTOR_USER_DATA_OFFSET;
    bytes[off..off + license.len()].copy_from_slice(license);

    let disc = Disc::from_bin(bytes);
    assert_eq!(disc_region_code(&disc), *b"SCEE");
}

fn count_pending_command_irq(cd: &CdRom, command: u8, irq: IrqType) -> usize {
    cd.pending
        .iter()
        .map(|event| {
            usize::from(event.command == command && event.irq == irq)
                + usize::from(
                    event
                        .followup
                        .as_ref()
                        .is_some_and(|followup| followup.command == command && followup.irq == irq),
                )
        })
        .sum()
}

/// The bug this whole model exists for. A guest that polls GetStat after Play
/// and reads "not playing" as "the track finished" restarts its music for as
/// long as the seek lasts. With Play declaring the drive playing at once, the
/// poll answered correctly by accident and no headless check could show it.
///
/// Costed a real one on the demo disc: menu music at the far end of a 660 MB
/// disc, polled twice a second, restarting forever on hardware while every
/// capture here looked perfect.
#[test]
fn play_reports_seeking_with_no_audio_until_the_head_arrives() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(cdda_disc()));

    cd.cmd_play(&[0x02]);
    let arrives = cd.cdda_seek_done_at.expect("Play arms a seek");

    // On the way: seeking, NOT playing, and producing nothing.
    cd.tick(arrives - 1);
    assert_ne!(cd.drive_status & drive_status_bit::SEEKING, 0);
    assert_eq!(cd.drive_status & drive_status_bit::PLAYING, 0);
    cd.pump_cdda_samples(4);
    assert!(
        cd.drain_cd_audio().is_empty(),
        "no audio before the head lands"
    );

    // Arrived: playing, not seeking, and audible.
    cd.tick(arrives + 1);
    assert_eq!(cd.drive_status & drive_status_bit::SEEKING, 0);
    assert_ne!(cd.drive_status & drive_status_bit::PLAYING, 0);
    cd.pump_cdda_samples(4);
    assert_eq!(cd.drain_cd_audio().len(), 4);
}

/// Stopping a drive that is still on its way has to cancel the journey. Left
/// armed, the abandoned Play would land later and start playing a track the
/// guest already gave up on.
#[test]
fn stopping_mid_seek_cancels_the_journey() {
    for stop in [CdRom::cmd_stop, CdRom::cmd_pause, CdRom::cmd_init] {
        let mut cd = CdRom::new();
        cd.insert_disc(Some(cdda_disc()));
        cd.cmd_play(&[0x02]);
        let arrives = cd.cdda_seek_done_at.expect("Play arms a seek");

        stop(&mut cd);
        assert_eq!(cd.cdda_seek_done_at, None);

        cd.tick(arrives + 1);
        assert_eq!(cd.drive_status & drive_status_bit::PLAYING, 0);
    }
}

/// A source whose sectors arrive when the test says so, like the web build's
/// on-demand image.
struct GatedSource {
    bytes: Vec<u8>,
    open: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl psx_iso::TrackSource for GatedSource {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }
    fn read_at(&self, offset: u64, out: &mut [u8]) -> bool {
        self.ready(offset, out.len() as u64) && self.bytes.read_at(offset, out)
    }
    fn ready(&self, _offset: u64, _len: u64) -> bool {
        self.open.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// A sector delivery whose sector has not arrived waits, event intact, and
/// lands (with the right data) once it has; the frontend's per-frame check
/// reports the wait before the frame runs.
#[test]
fn a_delivery_waits_for_a_sector_the_image_has_not_supplied() {
    let open = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let raw = raw_sector([0x00, 0x02, 0x00, 0x02], [0, 0, 0x08, 0], 0x5a);
    let mut cd = CdRom::new();
    cd.insert_disc(Some(Disc::from_source(Box::new(GatedSource {
        bytes: raw.repeat(4),
        open: open.clone(),
    }))));
    cd.scheduling_cycle = 1_000;
    cd.cmd_read();
    let ack_cycle = 1_000 + FIRST_RESPONSE_WITH_MEDIA_CYCLES + 1;
    assert!(cd.tick(ack_cycle));
    cd.irq_flag = 0;
    cd.responses.clear();
    assert!(cd.prefetch_upcoming());

    open.store(false, std::sync::atomic::Ordering::Relaxed);
    assert!(!cd.prefetch_upcoming());
    let first_due = ack_cycle + CD_READ_TIME * 3 / 2 + 1;
    assert!(!cd.tick(first_due));
    assert!(!cd.tick(first_due + 100));
    assert_eq!(cd.late_sector_counts(), (1, 0), "one wait, counted once");
    assert_eq!(cd.irq_flag, 0);

    open.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(cd.tick(first_due + 200));
    assert_eq!(cd.irq_flag, IrqType::DataReady as u8);
    assert_eq!(cd.data_fifo.front().copied(), Some(0x5a));
}

/// Cycle at which the first event of `irq` for `command` is due, once the
/// acknowledge has been delivered (followups are anchored on it).
fn completion_deadline(cd: &mut CdRom, ack_at: u64, irq: IrqType) -> Option<u64> {
    assert!(cd.tick(ack_at + 1), "acknowledge delivered");
    cd.irq_flag = 0;
    cd.pending
        .iter()
        .find(|ev| ev.irq == irq)
        .map(|ev| ev.deadline)
}

#[test]
fn pause_from_cdda_completes_123_ms_after_the_command() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(cdda_disc()));
    play_and_arrive(&mut cd, 2);
    cd.pending.clear();
    cd.irq_flag = 0;
    cd.scheduling_cycle = 10_000_000;
    cd.queue_command(0x09, 10_000_000);
    let ack_at = 10_000_000 + FIRST_RESPONSE_WITH_MEDIA_CYCLES + CDDA_BUSY_RESPONSE_CYCLES;
    let done = completion_deadline(&mut cd, ack_at, IrqType::Complete).expect("completion");
    // The completion is one tick after the acknowledge's own, plus the rest
    // of the 123.1 ms.
    assert_eq!(
        done,
        ack_at + 1 + PAUSE_FROM_CDDA_CYCLES - (ack_at - 10_000_000)
    );
    assert_eq!(
        (done - 10_000_000) / MS,
        123,
        "complete at the console's median"
    );
}

#[test]
fn pause_of_an_idle_drive_follows_its_acknowledge_by_the_console_gap() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(cdda_disc()));
    cd.queue_command(0x09, 1_000);
    let ack_at = 1_000 + PAUSE_ACK_CYCLES;
    let done = completion_deadline(&mut cd, ack_at, IrqType::Complete).expect("completion");
    assert_eq!(done, ack_at + 1 + PAUSE_COMPLETE_CYCLES_STANDBY);
    // Records 0x350 to 0x352 and 0x364 to 0x366: 37,720 to 156,544 clocks.
    assert!((37_720..=156_544).contains(&(done - 1_000)));
}

#[test]
fn stop_keeps_reporting_the_motor_until_606_ms_then_completes_off() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(cdda_disc()));
    cd.queue_command(0x08, 1_000);
    assert_ne!(
        cd.stat_byte() & drive_status_bit::MOTOR_ON,
        0,
        "still spinning"
    );
    let ack_at = 1_000 + FIRST_RESPONSE_WITH_MEDIA_CYCLES;
    assert!(cd.tick(ack_at + 1));
    assert_eq!(
        cd.read8(BASE + 1) & drive_status_bit::MOTOR_ON,
        drive_status_bit::MOTOR_ON
    );
    cd.irq_flag = 0;
    let done = cd
        .pending
        .iter()
        .find(|ev| ev.irq == IrqType::Complete)
        .expect("stop completion");
    assert_eq!(
        done.bytes[0] & drive_status_bit::MOTOR_ON,
        0,
        "completion says off"
    );
    assert_eq!(done.deadline - 1_000 - 1, STOP_FROM_SPINNING_CYCLES);
    cd.tick(1_000 + STOP_FROM_SPINNING_CYCLES - 1);
    assert_ne!(cd.stat_byte() & drive_status_bit::MOTOR_ON, 0);
    cd.tick(1_000 + STOP_FROM_SPINNING_CYCLES);
    assert_eq!(cd.stat_byte() & drive_status_bit::MOTOR_ON, 0);
}

/// First-sector delay of a ReadN issued at `now` on a one-track data disc.
fn first_sector_delay(cd: &mut CdRom, now: u64) -> u64 {
    cd.queue_command(0x06, now);
    let ack_at = now + FIRST_RESPONSE_WITH_MEDIA_CYCLES;
    completion_deadline(cd, ack_at, IrqType::DataReady).expect("first sector") - (ack_at + 1)
}

#[test]
fn read_after_stop_pays_the_spin_up_and_read_during_spin_down_waits_it_out() {
    let base = {
        let mut cd = CdRom::new();
        cd.insert_disc(Some(Disc::from_bin(vec![0u8; psx_iso::SECTOR_BYTES * 64])));
        first_sector_delay(&mut cd, 1_000)
    };
    // Settled: the motor is off by the time of the read.
    let mut cd = CdRom::new();
    cd.insert_disc(Some(Disc::from_bin(vec![0u8; psx_iso::SECTOR_BYTES * 64])));
    cd.queue_command(0x08, 1_000);
    let later = 1_000 + STOP_FROM_SPINNING_CYCLES + 10;
    cd.tick(later);
    cd.irq_flag = 0;
    assert_eq!(first_sector_delay(&mut cd, later), base + SPIN_UP_CYCLES);
    assert_ne!(
        cd.stat_byte() & drive_status_bit::MOTOR_ON,
        0,
        "spinning again"
    );
    // At once: the Stop was acknowledged, the read follows.
    let mut cd = CdRom::new();
    cd.insert_disc(Some(Disc::from_bin(vec![0u8; psx_iso::SECTOR_BYTES * 64])));
    cd.queue_command(0x08, 1_000);
    let ack_at = 1_000 + FIRST_RESPONSE_WITH_MEDIA_CYCLES;
    cd.tick(ack_at + 1);
    cd.irq_flag = 0;
    let at = ack_at + 100;
    let delay = first_sector_delay(&mut cd, at);
    assert_eq!(
        delay,
        base + (1_000 + STOP_FROM_SPINNING_CYCLES - at)
            + SPIN_UP_CYCLES
            + STOP_ABORT_RESTART_CYCLES
    );
    assert!(
        !cd.pending
            .iter()
            .any(|ev| ev.command == 0x08 && ev.irq == IrqType::Complete),
        "the interrupted Stop never completes"
    );
}

#[test]
fn audio_and_data_pay_a_settle_when_the_head_changes_over() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(cdda_disc()));
    // Data to audio: the settle is on top of the seek curve.
    cd.read_lba = 2;
    cd.queue_command(0x03, 1_000);
    let first = cd.cdda_seek_done_at.expect("play armed");
    assert_eq!(
        first - 1_000,
        seek_cycles(cd.read_lba.abs_diff(2)) + DATA_TO_AUDIO_SETTLE_CYCLES
    );
    // A second Play with the head already on audio pays no settle.
    cd.tick(first + 1);
    cd.queue_command(0x03, first + 10);
    let second = cd.cdda_seek_done_at.expect("play armed");
    assert!(second - (first + 10) < DATA_TO_AUDIO_SETTLE_CYCLES);
    // Audio to data: the first data operation pays, the next does not.
    let mut cd2 = CdRom::new();
    cd2.insert_disc(Some(Disc::from_bin(vec![0u8; psx_iso::SECTOR_BYTES * 64])));
    let plain = first_sector_delay(&mut cd2, 1_000);
    cd2.head_on_audio = true;
    cd2.cmd_pause();
    cd2.pending.clear();
    cd2.irq_flag = 0;
    assert_eq!(
        first_sector_delay(&mut cd2, 5_000_000),
        plain + AUDIO_TO_DATA_SETTLE_CYCLES
    );
    cd2.pending.clear();
    cd2.irq_flag = 0;
    assert_eq!(first_sector_delay(&mut cd2, 9_000_000), plain);
}

/// Reference decoder for a 4-bit stereo XA sector, written from the PSX-SPX
/// "CDROM XA Audio ADPCM Compression" pseudocode (16-bit history, +32
/// rounding, divide by 64) and independent of `decode_xa_native`.
fn spec_decode_4bit_stereo(raw: &[u8]) -> Vec<(i16, i16)> {
    const POS: [i32; 4] = [0, 60, 115, 98];
    const NEG: [i32; 4] = [0, 0, -52, -55];
    let mut hist = [[0i32; 2]; 2]; // [channel][old, older]
    let (mut left, mut right) = (Vec::new(), Vec::new());
    for portion in 0..18 {
        let src = &raw[24 + portion * 128..24 + (portion + 1) * 128];
        for blk in 0..4 {
            for nibble in 0..2 {
                let header = src[4 + blk * 2 + nibble];
                let shift = 12 - i32::from(header & 0x0f);
                let filter = usize::from((header & 0x30) >> 4);
                let ch = nibble;
                for j in 0..28 {
                    let raw4 = i32::from((src[16 + blk + j * 4] >> (nibble * 4)) & 0x0f);
                    let t = if raw4 >= 8 { raw4 - 16 } else { raw4 };
                    let predicted =
                        (hist[ch][0] * POS[filter] + hist[ch][1] * NEG[filter] + 32) >> 6;
                    let s = ((t << shift) + predicted).clamp(-0x8000, 0x7fff);
                    hist[ch][1] = hist[ch][0];
                    hist[ch][0] = s;
                    if ch == 0 {
                        left.push(s as i16);
                    } else {
                        right.push(s as i16);
                    }
                }
            }
        }
    }
    left.into_iter().zip(right).collect()
}

#[test]
fn xa_decode_matches_a_reference_written_from_the_spec() {
    // A pseudo-random first sector with realistic (non-saturating) headers (range 4..=12,
    // every filter): the block layout must match the spec decoder and the
    // numeric model (four fractional history bits, truncation) must stay
    // within a few output steps of the textbook rounding.
    let mut seed = 0x1234_5678u32;
    let mut next = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 16) as u8
    };
    let mut raw = vec![0u8; psx_iso::SECTOR_BYTES];
    for byte in raw[24..24 + 18 * 128].iter_mut() {
        *byte = next();
    }
    for portion in 0..18 {
        let base = 24 + portion * 128;
        for blk in 0..8 {
            let header = ((next() % 4) << 4) | (9 + next() % 4);
            raw[base + 4 + blk] = header;
        }
        // Bytes 0..4 and 12..16 repeat the header bytes 4..8 and 8..12.
        for k in 0..4 {
            raw[base + k] = raw[base + 4 + k];
            raw[base + 12 + k] = raw[base + 8 + k];
        }
    }
    let coding = XaCoding {
        stereo: true,
        freq: 37_800,
        nbits: 4,
    };
    let mut left = crate::spu::XaDecoderState::new();
    let mut right = crate::spu::XaDecoderState::new();
    let got = decode_xa_native(&raw, coding, &mut left, &mut right).unwrap();
    let want = spec_decode_4bit_stereo(&raw);
    assert_eq!((got.len(), want.len()), (2016, 2016));
    let worst = got
        .iter()
        .zip(&want)
        .map(|(g, w)| {
            (i32::from(g.0) - i32::from(w.0))
                .abs()
                .max((i32::from(g.1) - i32::from(w.1)).abs())
        })
        .max()
        .unwrap();
    // Measured 12 on this seed: the model truncates with four fractional
    // history bits where the textbook rounds a 16-bit history.
    assert!(worst <= 16, "deviation from the spec decoder: {worst}");
}

#[test]
fn spindown_and_head_position_survive_a_save_state_round_trip() {
    let mut cd = CdRom::new();
    cd.spindown_until = Some(123_456_789);
    cd.head_on_audio = true;
    let bytes = postcard::to_allocvec(&cd).expect("serialise");
    let back: CdRom = postcard::from_bytes(&bytes).expect("deserialise");
    assert_eq!(back.spindown_until, Some(123_456_789));
    assert!(back.head_on_audio);
}

// --- Pause out of a read (hwtest v2.7, run FB89, records 0x340 to 0x3B8) ---

/// A data disc long enough for a seek that counts as far.
fn long_data_disc() -> Disc {
    Disc::from_bin(vec![0u8; psx_iso::SECTOR_BYTES * 1_200])
}

/// Start a read of `lba` on a drive at `mode`, the way a game does, and
/// deliver `sectors` of it. Returns the cycle after the last delivery.
fn read_and_deliver(cd: &mut CdRom, mode: u8, lba: u32, sectors: u32) -> u64 {
    let mut now = 1_000;
    cd.push_param(mode);
    cd.queue_command(0x0e, now);
    now += 100_000;
    cd.tick(now);
    cd.irq_flag = 0;
    let (m, s, f) = lba_to_msf(lba);
    cd.push_param(bin_to_bcd(m));
    cd.push_param(bin_to_bcd(s));
    cd.push_param(bin_to_bcd(f));
    cd.queue_command(0x02, now);
    now += 100_000;
    cd.tick(now);
    cd.irq_flag = 0;
    cd.queue_command(0x06, now);
    let mut delivered = 0;
    let end = now + 400_000_000;
    while delivered < sectors && now < end {
        now += 1_000;
        if cd.tick(now) {
            if cd.irq_flag == IrqType::DataReady as u8 {
                delivered += 1;
            }
            cd.irq_flag = 0;
        }
    }
    assert_eq!(delivered, sectors, "the read delivered its sectors");
    now
}

/// Cycles from a Pause written at `now` to its second response, with the
/// acknowledge delivered on the way.
fn pause_to_second_response(cd: &mut CdRom, now: u64) -> u64 {
    cd.queue_command(0x09, now);
    let ack_at = now + PAUSE_ACK_CYCLES;
    let done = completion_deadline(cd, ack_at, IrqType::Complete).expect("second response");
    done - now
}

#[test]
fn a_pause_out_of_a_read_completes_inside_the_consoles_range_at_both_speeds() {
    // (mode, first LBA, sectors delivered before the Pause, console min..max
    // in clocks from the Pause, records)
    let cases: [(u8, u32, u32, u64, u64); 6] = [
        // 0x358-0x35A: 1x, first sector, near
        (0x00, 100, 1, 2_423_344, 2_462_384),
        // 0x36C-0x36E: 2x
        (0x80, 100, 1, 1_222_208, 1_244_328),
        // 0x354-0x356: 1x, twelfth sector
        (0x00, 100, 12, 3_797_000, 3_835_000),
        // 0x368-0x36A: 2x (the low end is the trial a period short)
        (0x80, 100, 12, 1_689_176, 1_933_504),
        // 0x360-0x362: 1x, first sector after a far seek
        (0x00, 1_000, 1, 2_790_888, 2_839_032),
        // 0x374-0x376: 2x
        (0x80, 1_000, 1, 1_420_856, 1_436_536),
    ];
    for (mode, lba, sectors, low, high) in cases {
        let mut cd = CdRom::new();
        cd.insert_disc(Some(long_data_disc()));
        // The head starts at the beginning, so LBA 1,000 is a far seek and
        // LBA 100 is not.
        let now = read_and_deliver(&mut cd, mode, lba, sectors) + 4_000;
        let took = pause_to_second_response(&mut cd, now);
        assert!(
            (low..=high).contains(&took),
            "mode {mode:#04x} lba {lba} after {sectors} sectors: {took} not in {low}..={high}"
        );
    }
}

#[test]
fn a_pause_out_of_a_read_scales_with_the_sector_period() {
    for sectors in [0u32, 1, 2, 3, 4, 12, 100] {
        for far in [false, true] {
            let single = pause_from_read_cycles(CD_READ_TIME, sectors, far);
            let double = pause_from_read_cycles(CD_READ_TIME / 2, sectors, far);
            assert!(
                single.abs_diff(double * 2) <= 2,
                "{sectors} sectors, far {far}: {single} vs {double}"
            );
        }
    }
    // Later sectors never shorten it, and it stops growing at the settled figure.
    let mut last = 0;
    for sectors in 0..20 {
        let now = pause_from_read_cycles(CD_READ_TIME, sectors, false);
        assert!(now >= last);
        last = now;
    }
    assert_eq!(last, CD_READ_TIME * PAUSE_SETTLED_PERIODS_X100 / 100);
    assert_eq!(
        pause_from_read_cycles(CD_READ_TIME, 12, true),
        pause_from_read_cycles(CD_READ_TIME, 12, false),
        "the far-seek offset is gone by the twelfth sector"
    );
}

#[test]
fn the_pause_acknowledge_takes_the_consoles_33_thousand_clocks_and_says_reading() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(long_data_disc()));
    let now = read_and_deliver(&mut cd, 0x00, 100, 2) + 4_000;
    cd.queue_command(0x09, now);
    let ack = cd.pending.front().expect("acknowledge queued");
    assert_eq!(ack.irq, IrqType::Acknowledge);
    assert_eq!(ack.deadline, now + PAUSE_ACK_CYCLES);
    assert!((32_832..=33_024).contains(&PAUSE_ACK_CYCLES));
    // 0x22: motor on and reading, as at the Pause's INT3 in every watch trial.
    assert_eq!(
        ack.bytes[0],
        drive_status_bit::MOTOR_ON | drive_status_bit::READING
    );
}

#[test]
fn a_getstat_soon_after_a_pause_still_reads_reading_and_a_later_one_does_not() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(long_data_disc()));
    let now = read_and_deliver(&mut cd, 0x00, 100, 12) + 4_000;
    cd.queue_command(0x09, now);
    cd.tick(now + PAUSE_ACK_CYCLES + 1);
    cd.irq_flag = 0;
    let status_of_getstat = |cd: &mut CdRom, at: u64| {
        cd.responses.clear();
        cd.pending.retain(|ev| ev.command != 0x01);
        cd.queue_command(0x01, at);
        cd.pending
            .iter()
            .find(|ev| ev.command == 0x01)
            .map(|ev| ev.bytes[0])
            .expect("GetStat answers")
    };
    // EARLY trials: sent at the acknowledge, answered at 72,448 and 81,536.
    assert_eq!(
        status_of_getstat(&mut cd, now + 40_000),
        drive_status_bit::MOTOR_ON | drive_status_bit::READING
    );
    // The next one in those trials was answered at 233,088 and 291,968.
    assert_eq!(
        status_of_getstat(&mut cd, now + 233_088),
        drive_status_bit::MOTOR_ON
    );
}

#[test]
fn a_command_between_the_two_responses_cancels_the_second() {
    for command in [0x01u8, 0x0e, 0x19] {
        let mut cd = CdRom::new();
        cd.insert_disc(Some(long_data_disc()));
        let now = read_and_deliver(&mut cd, 0x00, 100, 12) + 4_000;
        cd.queue_command(0x09, now);
        cd.tick(now + PAUSE_ACK_CYCLES + 1);
        cd.irq_flag = 0;
        assert!(cd.pending.iter().any(|ev| ev.irq == IrqType::Complete));
        cd.push_param(0x00);
        cd.queue_command(command, now + 60_000);
        assert!(
            !cd.pending
                .iter()
                .any(|ev| ev.command == 0x09 && ev.irq == IrqType::Complete),
            "command {command:#04x} left the Pause's INT2 pending"
        );
        // And nothing from the Pause ever arrives, however long the wait
        // (the console watched 8,000,000 clocks).
        let mut t = now + 60_000;
        while t < now + 9_000_000 {
            t += 10_000;
            if cd.tick(t) {
                assert_ne!(cd.irq_flag, IrqType::Complete as u8, "INT2 arrived");
                cd.irq_flag = 0;
            }
        }
        // The drive did pause: nothing is reading once the window is over.
        cd.scheduling_cycle = t;
        assert_eq!(cd.stat_byte() & drive_status_bit::READING, 0);
    }
}

#[test]
fn a_command_after_the_second_response_has_nothing_to_cancel() {
    let mut cd = CdRom::new();
    cd.insert_disc(Some(long_data_disc()));
    let now = read_and_deliver(&mut cd, 0x00, 100, 2) + 4_000;
    cd.queue_command(0x09, now);
    let done = now + PAUSE_ACK_CYCLES + 1 + pause_from_read_cycles(CD_READ_TIME, 2, false) + 1;
    let mut t = now;
    let mut seen = false;
    while t < done + 1_000 {
        t += 1_000;
        if cd.tick(t) {
            seen |= cd.irq_flag == IrqType::Complete as u8;
            cd.irq_flag = 0;
        }
    }
    assert!(seen, "INT2 arrives when nothing interferes");
}

#[test]
fn a_pause_during_the_seek_of_a_read_is_refused_and_the_read_goes_on() {
    for mode in [0x00u8, 0x80] {
        let mut cd = CdRom::new();
        cd.insert_disc(Some(long_data_disc()));
        let mut now = 1_000;
        cd.push_param(mode);
        cd.queue_command(0x0e, now);
        now += 100_000;
        cd.tick(now);
        cd.irq_flag = 0;
        let (m, s, f) = lba_to_msf(1_000);
        cd.push_param(bin_to_bcd(m));
        cd.push_param(bin_to_bcd(s));
        cd.push_param(bin_to_bcd(f));
        cd.queue_command(0x02, now);
        now += 100_000;
        cd.tick(now);
        cd.irq_flag = 0;
        cd.queue_command(0x06, now);
        now += FIRST_RESPONSE_WITH_MEDIA_CYCLES + 2;
        assert!(cd.tick(now));
        cd.irq_flag = 0;
        // ReadN is acknowledged and the head is on its way: Pause now.
        cd.queue_command(0x09, now);
        let refusal = cd.pending.front().expect("a refusal").clone();
        assert_eq!(refusal.irq, IrqType::Error, "INT5, not INT3");
        assert_eq!(refusal.deadline, now + PAUSE_ACK_CYCLES);
        // Console: status 0x03, error plus motor on, reading clear (records
        // 0x35C-0x35E, 0x370-0x372; once 0x43 with the seek bit).
        assert_eq!(
            refusal.bytes[0],
            drive_status_bit::ERROR | drive_status_bit::MOTOR_ON
        );
        assert!(
            !cd.pending
                .iter()
                .any(|ev| ev.irq == IrqType::Acknowledge && ev.command == 0x09),
            "no INT3 for it"
        );
        assert!(
            !cd.pending
                .iter()
                .any(|ev| ev.irq == IrqType::Complete && ev.command == 0x09),
            "no INT2 for it"
        );
        // The read carries on: sectors still come, and a Pause after the
        // first one is accepted.
        assert!(cd.reading);
        let mut got = 0;
        let mut t = now;
        while got == 0 && t < now + 400_000_000 {
            t += 1_000;
            if cd.tick(t) {
                if cd.irq_flag == IrqType::DataReady as u8 {
                    got += 1;
                }
                cd.irq_flag = 0;
            }
        }
        assert_eq!(got, 1, "the read delivered its first sector");
        cd.queue_command(0x09, t + 4_000);
        assert!(cd
            .pending
            .iter()
            .any(|ev| ev.irq == IrqType::Acknowledge && ev.command == 0x09));
    }
}

/// A Stop (or Init, Reset, SeekL, Play, a new disc) that follows a Pause out
/// of a read ends the "still reading" window: the drive is then stopping or
/// being reset, not finishing a sector.
#[test]
fn commands_that_move_the_drive_end_the_reading_window() {
    type Action = fn(&mut CdRom);
    let actions: [(&str, Action); 5] = [
        ("Stop", |cd| cd.queue_command(0x08, 1_000_000)),
        ("Init", |cd| cd.queue_command(0x1c, 1_000_000)),
        ("Reset", |cd| cd.queue_command(0x0a, 1_000_000)),
        ("SeekL", |cd| cd.queue_command(0x15, 1_000_000)),
        ("eject", |cd| cd.insert_disc(None)),
    ];
    for (name, action) in actions {
        let mut cd = CdRom::new();
        cd.insert_disc(Some(long_data_disc()));
        let now = read_and_deliver(&mut cd, 0x00, 100, 2) + 4_000;
        cd.queue_command(0x09, now);
        assert_ne!(cd.pause_reading_until, 0, "{name}: window open");
        // Well inside the window.
        cd.scheduling_cycle = now + 10_000;
        action(&mut cd);
        cd.scheduling_cycle = now + 20_000;
        assert_eq!(
            cd.stat_byte() & drive_status_bit::READING,
            0,
            "{name} left the drive reading"
        );
    }
}

#[test]
fn the_pause_window_and_read_counters_survive_a_save_state_round_trip() {
    let mut cd = CdRom::new();
    cd.pause_reading_until = 99_000;
    cd.sectors_since_read = 7;
    cd.read_began_far = true;
    let bytes = postcard::to_allocvec(&cd).expect("serialise");
    let back: CdRom = postcard::from_bytes(&bytes).expect("deserialise");
    assert_eq!(back.pause_reading_until, 99_000);
    assert_eq!(back.sectors_since_read, 7);
    assert!(back.read_began_far);
}
