//! CD-ROM command-response cycle delays.
//!
//! ## Provenance
//!
//! Every constant here carries one of four evidence tags:
//!
//! - **spec**: arithmetic on documented clocks (33,868,800 Hz, 75 sectors
//!   per second).
//! - **console**: measured on the project's own console by the hardware-test
//!   records named next to it.
//! - **psx-spx**: a figure from the nocash PSX-SPX "CDROM - Response Timings"
//!   tables or command descriptions. Those were measured on a PAL PSone,
//!   whose drive answers measurably slower than the project's console, so
//!   they are only used where nothing better exists.
//! - **pinned**: no external source. The value is what the compat and library
//!   frame hashes require: replacing it with the PSX-SPX figure moves them
//!   (the alternative is listed so a console measurement can replace it).
//!
//! The pinned values are the open items of the CD timing work: each needs a
//! hardware-test probe on the console before it can be justified or changed.
//!
//! Experiment record (2026-10-04, compat hashes of all 20 games plus the 16
//! library boots): swapping every pinned value below for its PSX-SPX
//! alternative changed all 20 compat hash lists (Pause alone changed every
//! one); GetID alone moved Legacy of Kain: Soul Reaver; Stop alone moved
//! WipEout and WipEout 2097. ReadTOC alone changed nothing and uses the
//! PSX-SPX figure.

/// Typical command acknowledgement with no readable media (**psx-spx**
/// order of magnitude, firmware loop rather than a fixed shortcut).
pub(super) const FIRST_RESPONSE_CYCLES: u64 = 15_000;

/// Typical acknowledgement with a disc present. Calibrated 2026-07-31 against
/// the project's own console (hardware-tests record 0x40): GetStat acked in
/// 19,491 cycles minimum including the same 257 cycles of MMIO/poll overhead
/// the earlier PAL-PSone figure carried, yielding 19,234. The PAL PSone that
/// produced the previous 30,029 answers measurably slower; this project's
/// accuracy oracle is the console the discs actually run on.
pub(super) const FIRST_RESPONSE_WITH_MEDIA_CYCLES: u64 = 19_234;

/// The drive firmware periodically performs a media-maintenance sweep before
/// servicing GetStat. The SCPH-9902 envelope exposes an 18,304-cycle outlier
/// once in a five-command status-poll window, while the other acknowledgements
/// stay on the ordinary floor.
pub(super) const GETSTAT_MAINTENANCE_CYCLES: u64 = 18_320;

/// Short second response of a ReadN issued with no disc (INT5). **Unsourced,
/// ungated**: the compat and library gates never reach this path, so any
/// small value holds them.
pub(super) const QUICK_SECOND_RESPONSE_CYCLES: u64 = 0x800;

/// Polling interval when a command response is due but the previous
/// interrupt is still unacknowledged. **Arbitrary granularity**: the gates
/// hold for 128 to 256 cycles and move (Crash Team Racing) at 512.
pub(super) const IRQ_RESCHEDULE_CYCLES: u64 = 0x100;

/// GetID second response. **Pinned**: Soul Reaver's hashes move with the
/// PSX-SPX figure 0x4A00 (18,944 cycles, PAL PSone average).
pub(super) const GETID_SECOND_RESPONSE_CYCLES: u64 = 20_480;

/// Reset / Init completion. **Console**: hardware-test record 0x9A reads
/// ~17 ms longer than the 4,100,000 this value started from. PSX-SPX only
/// says software must wait 1/8 s (0x400000 = 4,194,304) after Reset.
pub(super) const RESET_SECOND_RESPONSE_CYCLES: u64 = 4_790_000;

/// Stop second response. **Pinned**: WipEout and WipEout 2097 hashes move
/// with the PSX-SPX figures (single speed 0xD38ACA, double 0x18A6076, when
/// already stopped 0x1D7B).
pub(super) const STOP_SECOND_RESPONSE_CYCLES: u64 = CD_READ_TIME * 4; // 1,806,336

/// Pause second response with the motor already running. **Pinned**: every
/// compat hash list moves with the PSX-SPX figures (paused: 0x1DF2; pausing
/// a read: 0x21181C single speed, 0x10BD93 double speed).
pub(super) const PAUSE_COMPLETE_CYCLES_STANDBY: u64 = 7_000;

/// Pause second response from a stopped drive (doubled at double speed).
/// **Unsourced**; not distinguished by the gates.
pub(super) const PAUSE_COMPLETE_CYCLES_ACTIVE: u64 = 1_000_000;

/// ReadTOC second response: **psx-spx** ("about 1 second delay"), one second
/// of system clock. Changing it from the earlier 20,321,280 moved no hash.
pub(super) const READ_TOC_SECOND_RESPONSE_CYCLES: u64 = CD_READ_TIME * 75;

/// Lid / rescan sequence after Init or a disc change. **Unsourced**. The
/// first step is not distinguished by the gates (halving it moves nothing);
/// the later steps are unexercised by them.
pub(super) const LID_BOOTSTRAP_CYCLES: u64 = 20_480;
pub(super) const LID_PREPARE_SPINUP_CYCLES: u64 = CD_READ_TIME * 150;
pub(super) const LID_PREPARE_SEEK_CYCLES: u64 = CD_READ_TIME * 26;

/// One CD frame period: system clock / 75 sectors per second, `33_868_800 /
/// 75` (**spec**). Single speed delivers a sector per frame, double speed two.
pub(super) const CD_READ_TIME: u64 = 451_584;

/// Silicon transitions between audio, data and a stopped motor.
///
/// Measured on the project console on 2026-10-08 with hardware tests v1.28
/// (the CD STREAM cases, records `0x2F0` to `0x315`; the report is the
/// `cdstream_cdda` and `cdstream_motor` rows). Every figure below is the
/// median of the console's three repetitions where there are three, and the
/// single sample otherwise; the table in `CHANGELOG.md` has the emulator's
/// reading of each. The model keeps the median and does not add jitter:
/// determinism matters more here (parity suites and headless captures must
/// replay identically). Tolerance used when comparing an emulator capture
/// with the console: inside the console's own min..max, or within 10% of
/// the median where the console took a single sample.
///
/// How the settle terms below were fitted: each is the console median minus
/// the time the same operation takes in the emulator with the term at zero,
/// both read through the same hardware-tests v1.28 harness (the CD cases
/// run headless on the emulator, before and after the change). The
/// harness times with Timer 1 counting HBlanks. Where the guest polls that
/// counter in a tight loop the emulator reads it low: a drive wait that
/// takes 606.0 ms of CPU cycles reads 572 ms, because the counter advances
/// once per 2280 CPU cycles there against 2172 when free-running (a trace
/// of the Stop case; the timers' read-hold model is the likely cause, not
/// investigated further). The stream-based rows below read within 1% of
/// the cycle time, so they are the ones the fits use.
///
/// Pause complete after CD-DA, counted from the Pause command (record
/// `0x301`, `t_pause`): console 120.1 to 125.4 ms, median 123.1 (the
/// median is the middle of three). The drive acknowledges in 0.8 ms and
/// then takes this long to leave the controller idle. In CPU cycles the
/// emulator completes at exactly this figure; the harness reads it 5.6%
/// low (116 ms) for the Timer 1 reason above. A Pause from any other state
/// keeps its older, gate-pinned figures ([`PAUSE_COMPLETE_CYCLES_STANDBY`]):
/// the console has no measurement for Pause from a data read (record
/// `0x300` is 62 ms, but it includes the transport's own latency).
pub(super) const PAUSE_FROM_CDDA_CYCLES: u64 = 1231 * MS / 10;

/// Extra time the first sector of a data operation takes when the head was
/// last on a CD-DA track (record `0x303`: 923 to 1000 ms, median 945, after
/// a Pause; the same read with no Pause at all took 886 ms and with the
/// transport's recovery Pause 1213 ms, one sample each). Added on top of
/// the seek curve and the ordinary first-sector delay, which the harness
/// reads as 356 ms for the same read with this term at zero. The median of
/// the proper hand-off (Pause, wait, read) is the one modelled, because it
/// is what the transport does. 945 - 356 = 589 ms; the first fit at 590 ms
/// left the harness reading 938, so 597 ms is the value that reads 945.
/// The drive does not shorten it with
/// idle time: the proper hand-off waits about a quarter of a second between
/// the Pause and the read and still pays it in full.
pub(super) const AUDIO_TO_DATA_SETTLE_CYCLES: u64 = 597 * MS;

/// Extra time a Play takes to reach PLAYING when the head was last doing
/// data (record `0x305`: Play after SetLoc to the PLAYING bit, 848 to
/// 1076 ms, median 1006 over three resumes). The seek curve alone reads
/// 337 ms for that hop, so the remainder is 1006 - 337 = 669 ms, and 675 ms
/// makes the harness read 1006 (the first fit at 669 ms read 1000).
pub(super) const DATA_TO_AUDIO_SETTLE_CYCLES: u64 = 675 * MS;

/// Stop complete, counted from the Stop command: the motor-off flag in the
/// status byte clears at the same moment (records `0x314`, 606 ms for both;
/// one sample). Stopping an already stopped drive keeps
/// [`STOP_SECOND_RESPONSE_CYCLES`]. Exactly 606 ms in CPU cycles; the
/// harness reads 572 ms (Timer 1 reading low, see above).
pub(super) const STOP_FROM_SPINNING_CYCLES: u64 = 606 * MS;

/// Extra first-sector time of a data read on a drive whose motor is off.
/// The console's settled read (record `0x314`, 4 sectors in 1978 ms; the
/// later three take 27 ms at double speed, as in record `0x315`, so the
/// first arrives at about 1951 ms) against the 202 ms the same read costs
/// on a spinning drive in the emulator (the harness reads 229 ms for four
/// sectors with this term at zero, minus the same 27 ms): 1951 - 202 =
/// 1749 ms.
pub(super) const SPIN_UP_CYCLES: u64 = 1749 * MS;

/// A read issued while the Stop is still spinning the motor down (record
/// `0x315`, first sector 2721 ms after the read command, `0x313`: 2748 ms
/// for 4). The read waits out the rest of the spin-down, then pays the
/// spin-up, then a further 2721 - 605 - 1951 = 165 ms the console does not
/// explain (about one more read latency: the aborted spin-down seems to
/// leave the head needing a fresh seek). One sample. The harness reads
/// 2651 ms with these terms, 2.6% under the console and inside the
/// single-sample tolerance; the shortfall is the Timer 1 effect on the
/// spin-down part of the wait, so the term stays at the console-derived
/// figure rather than being fitted to the biased reading.
pub(super) const STOP_ABORT_RESTART_CYCLES: u64 = 165 * MS;

/// Extra first-response latency for a command issued *while CD-DA audio is
/// playing*, added on top of the applicable first-response delay.
///
/// A PSoXide faithfulness model, not a measurement. On real hardware the CD sub-CPU is a single
/// controller; while it is streaming Red Book audio it services a new command
/// only after attending to the audio it is already decoding, so a command's
/// acknowledge is noticeably delayed. Emulators that ack every command in a
/// flat ~2048 cycles hide this, which is exactly why "poll the drive every
/// frame while music plays" looks free in emulation yet stalls (and, on a
/// missed/late poll, reseeks and kills the audio) on silicon.
///
/// Magnitude is derived from the audio-sector period ([`CD_READ_TIME`], a real
/// PSX spec) rather than a fabricated figure: a command lands within a fraction
/// of one CD frame. It is deliberately well under the cycles a generous polled
/// wait spins for (so intentional blocking handoffs like Pause-before-gameplay
/// still complete) and well over a cheap non-blocking poll's budget (so a
/// frame-rate status poll during playback correctly reads as "couldn't tell"
/// instead of stalling). Tune here if hardware measurement refines it.
pub(super) const CDDA_BUSY_RESPONSE_CYCLES: u64 = CD_READ_TIME / 4; // ≈ 112,896

/// PSX system clock. `CD_READ_TIME * 75`.
const MASTER_CLOCK: u64 = CD_READ_TIME * 75; // 33,868,800
/// One millisecond of system clock.
pub(super) const MS: u64 = MASTER_CLOCK / 1000;
/// Sectors in a full 72-minute sweep, the longest travel a disc can ask for.
const MAX_SLED_LBA: u64 = 72 * 60 * 75; // 324,000

/// Cycles the head needs to reach a track `lba_diff` sectors away. Used for
/// every mech movement: CD-DA Play, SeekL/SeekP completion, and the first
/// sector of a read whose SetLoc moved the head.
///
/// A PSoXide faithfulness model. Acking Play and declaring the drive playing
/// at once makes every "has the track finished?" poll answer correctly
/// by accident. A real drive seeks first, reporting SEEKING with the playing
/// bit CLEAR for the whole journey. Guest code that reads "not playing" as
/// "track over" therefore passes in emulation and restarts its music on
/// silicon.
///
/// Calibrated 2026-07-31 against the project console (hardware-tests timing
/// records 0x90-0x93, command-to-complete): 1 sector -> ~11 ms, 16 -> ~79 ms,
/// 128 -> ~137 ms, 512 -> ~310 ms. The curve is not smooth on silicon (a
/// 16-sector hop costs most of a rotation-resync; a 128-sector lens jump
/// barely more), so this interpolates the measured points instead of fitting
/// a formula, then continues at the old full-sweep sled slope (~700 ms across
/// the whole disc) past the last point.
///
/// Deliberately jitter-free. Varying the time would model a real mech more
/// closely, but determinism is worth more here: the parity suites and every
/// headless capture depend on the same disc giving the same run.
pub(super) fn seek_cycles(lba_diff: u32) -> u64 {
    // Nudged one empirical round above the raw record mins so the suite's
    // own seek probes (which include command overhead this model does not
    // decompose) land on the console values: raw 11/79/137/310 ms read
    // back ~1-25 ms short through the probe.
    const POINTS: [(u64, u64); 4] = [(1, 12), (16, 85), (128, 148), (512, 335)];
    let diff = (lba_diff as u64).min(MAX_SLED_LBA);
    if diff <= POINTS[0].0 {
        return POINTS[0].1 * MS;
    }
    let mut i = 1;
    while i < POINTS.len() {
        let (d0, m0) = POINTS[i - 1];
        let (d1, m1) = POINTS[i];
        if diff <= d1 {
            // Interpolate in cycles, not milliseconds, so every extra
            // sector costs something and the curve stays strictly monotonic.
            return m0 * MS + (m1 - m0) * MS * (diff - d0) / (d1 - d0);
        }
        i += 1;
    }
    // Sled regime past the measured range.
    310 * MS + 700 * MS * (diff - 512) / MAX_SLED_LBA
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The curve has to reproduce the console measurements it was calibrated
    /// from, stay monotonic, and stay inside a real sled's envelope.
    #[test]
    fn seek_time_matches_measured_points_and_grows_with_distance() {
        let ms = |cycles: u64| cycles / MS;
        assert_eq!(ms(seek_cycles(1)), 12);
        assert_eq!(ms(seek_cycles(16)), 85);
        assert_eq!(ms(seek_cycles(128)), 148);
        assert_eq!(ms(seek_cycles(512)), 335);
        assert!(ms(seek_cycles(0)) < 30, "same sector is not a big seek");
        assert!(seek_cycles(8) < seek_cycles(9));
        assert!(seek_cycles(299) < seek_cycles(300));
        assert!(seek_cycles(7_199) < seek_cycles(7_200));
        // A full-disc sweep: around a second, never wildly beyond one.
        let sweep = ms(seek_cycles(MAX_SLED_LBA as u32));
        assert!((900..=1100).contains(&sweep), "{sweep} ms for a full sweep");
        // Past the end of the disc cannot cost more than crossing all of it.
        assert_eq!(seek_cycles(u32::MAX), seek_cycles(MAX_SLED_LBA as u32));
    }

    /// The case the demo disc hit: menu music at the far end of a full disc,
    /// polled twice a second. The seek has to outlast a poll interval or the
    /// emulator cannot show the bug that motivated this model.
    #[test]
    fn a_cross_disc_seek_outlasts_a_two_hertz_poll() {
        assert!(seek_cycles(290_000) > 500 * MS);
    }
}
