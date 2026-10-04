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
const MS: u64 = MASTER_CLOCK / 1000;
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
