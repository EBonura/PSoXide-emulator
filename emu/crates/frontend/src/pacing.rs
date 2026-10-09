// SPDX-License-Identifier: GPL-2.0-or-later
//! How many guest frames one host redraw runs.
//!
//! One mechanism, two policies. The policy is chosen per workspace by
//! [`FramePacing::for_workspace`]:
//!
//! - [`FramePacing::CatchUp`] is the standalone emulator's. A redraw runs every
//!   frame it owes, up to [`MAX_CATCHUP_FRAMES`], so a late redraw is made up
//!   for. With `video.smooth_slow_host` it runs only what fits in one paint
//!   (see [`HostPace`]) and forgives the rest, so a slow host slows the game
//!   instead of the picture.
//! - [`FramePacing::HardCap`] is editor Play's. One guest frame per redraw,
//!   two on a display slower than the guest, and the unpaid backlog is
//!   dropped. Emulating a frame takes most of a 60 Hz redraw, and the editor
//!   UI draws beside it, so bursting several frames made the next redraw late
//!   too (NitroXide on a loaded host: 70 ms redraws, one in ten presented late).

use crate::ui::profiler::FrameProfileSample;

/// Most guest frames a catching-up redraw runs.
pub(crate) const MAX_CATCHUP_FRAMES: u32 = 4;
/// Guest frames a hard-capped redraw may run.
const HARD_CAP_FRAMES: u32 = 1;
/// The same on a display slower than the guest (a 50 Hz panel), or the game
/// would run slow on it.
const HARD_CAP_FRAMES_SLOW_DISPLAY: u32 = 2;

/// The frame-pacing policy of one workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FramePacing {
    /// Make up for late redraws; `smooth` is `video.smooth_slow_host`.
    CatchUp {
        /// Run only what fits in one paint and forgive the rest.
        smooth: bool,
    },
    /// One frame per redraw, backlog dropped.
    HardCap,
}

/// What a redraw decided to run, and the accumulator it carries forward.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FramePlan {
    /// Guest frames to run this redraw.
    pub(crate) frames: u32,
    /// Seconds of owed guest time to carry before the run is subtracted.
    pub(crate) accum: f32,
}

impl FramePacing {
    /// The policy for the workspace being shown. Play beside the editor is
    /// hard-capped; the emulator workspace (and the standalone emulator)
    /// catches up.
    pub(crate) fn for_workspace(editor_workspace: bool, smooth_slow_host: bool) -> Self {
        if editor_workspace {
            Self::HardCap
        } else {
            Self::CatchUp {
                smooth: smooth_slow_host,
            }
        }
    }

    /// Frames to run now. `accum` is the owed guest time in seconds, `dt` the
    /// guest frame period. `slow_display` is asked only when a backlog makes
    /// the answer matter, so the steady state never queries the monitor.
    pub(crate) fn plan(
        self,
        accum: f32,
        dt: f32,
        pace: &HostPace,
        slow_display: impl FnOnce() -> bool,
    ) -> FramePlan {
        let owed = (accum / dt) as u32;
        match self {
            Self::CatchUp { smooth } => {
                let owed = owed.min(MAX_CATCHUP_FRAMES);
                if !smooth {
                    return FramePlan {
                        frames: owed,
                        accum,
                    };
                }
                // Run only what fits in one paint and forgive the rest: the
                // game slows down, the picture does not.
                let fit = pace.frames_per_paint(dt);
                FramePlan {
                    frames: owed.min(fit),
                    accum: if owed > fit { fit as f32 * dt } else { accum },
                }
            }
            Self::HardCap => {
                let cap = if owed > 1 && slow_display() {
                    HARD_CAP_FRAMES_SLOW_DISPLAY
                } else {
                    HARD_CAP_FRAMES
                };
                FramePlan {
                    frames: owed.min(cap),
                    accum,
                }
            }
        }
    }

    /// Pay for `frames_run` frames. A hard cap also drops whole frames of
    /// backlog it left unpaid: the game loses that time rather than bursting
    /// through it on the next redraws. The fraction stays, so the wake-up
    /// phase (`schedule_next_redraw`) is unchanged.
    pub(crate) fn settle(self, accum: f32, dt: f32, frames_run: u32) -> f32 {
        let accum = accum - frames_run as f32 * dt;
        if self == Self::HardCap && accum >= dt {
            accum % dt
        } else {
            accum
        }
    }
}

/// Measured redraw costs, for pacing a host too slow to keep up (see
/// `video.smooth_slow_host`).
#[derive(Clone, Copy, Default)]
pub(crate) struct HostPace {
    /// Emulation + audio per guest frame, ms.
    pub(crate) frame_ms: f32,
    /// Everything else in a redraw (rendering, UI), ms.
    pub(crate) other_ms: f32,
}

impl HostPace {
    /// Fold one redraw's profile into the running averages.
    pub(crate) fn note(&mut self, profile: &FrameProfileSample) {
        fn ewma(avg: &mut f32, sample: f32) {
            *avg += (sample - *avg) * 0.1;
        }
        let emulation = profile.emu_ms + profile.audio_ms;
        if profile.frames_run > 0.0 {
            ewma(&mut self.frame_ms, emulation / profile.frames_run);
        }
        ewma(&mut self.other_ms, (profile.total_ms - emulation).max(0.0));
    }

    /// Guest frames whose emulation fits in one guest frame period next to
    /// the rest of a redraw, at least one. Measured against the guest
    /// period rather than the redraw interval: on a slow host the interval
    /// is the overrun itself.
    pub(crate) fn frames_per_paint(&self, frame_dt: f32) -> u32 {
        if self.frame_ms <= 0.0 {
            return MAX_CATCHUP_FRAMES;
        }
        let room = (frame_dt * 1000.0 - self.other_ms).max(0.0);
        ((room / self.frame_ms) as u32).clamp(1, MAX_CATCHUP_FRAMES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f32 = 1.0 / 60.0;

    #[test]
    fn slow_host_pacing_runs_what_fits_in_a_frame_period() {
        let unmeasured = HostPace::default();
        assert_eq!(unmeasured.frames_per_paint(DT), MAX_CATCHUP_FRAMES);
        let fast = HostPace {
            frame_ms: 5.0,
            other_ms: 1.0,
        };
        assert_eq!(fast.frames_per_paint(DT), 3);
        let slow = HostPace {
            frame_ms: 25.0,
            other_ms: 3.0,
        };
        assert_eq!(slow.frames_per_paint(DT), 1, "always at least one frame");
    }

    #[test]
    fn the_editor_workspace_hard_caps_and_the_emulator_catches_up() {
        assert_eq!(
            FramePacing::for_workspace(true, false),
            FramePacing::HardCap
        );
        assert_eq!(FramePacing::for_workspace(true, true), FramePacing::HardCap);
        assert_eq!(
            FramePacing::for_workspace(false, true),
            FramePacing::CatchUp { smooth: true }
        );
    }

    #[test]
    fn catch_up_runs_every_owed_frame_up_to_the_limit() {
        let pace = HostPace::default();
        let policy = FramePacing::CatchUp { smooth: false };
        assert_eq!(policy.plan(DT * 0.9, DT, &pace, || false).frames, 0);
        assert_eq!(policy.plan(DT * 1.1, DT, &pace, || false).frames, 1);
        assert_eq!(policy.plan(DT * 3.5, DT, &pace, || false).frames, 3);
        let burst = policy.plan(DT * 9.0, DT, &pace, || false);
        assert_eq!(burst.frames, MAX_CATCHUP_FRAMES);
        assert_eq!(burst.accum, DT * 9.0, "catch-up forgives nothing");
    }

    #[test]
    fn smooth_catch_up_forgives_what_does_not_fit_in_one_paint() {
        let slow = HostPace {
            frame_ms: 12.0,
            other_ms: 3.0,
        };
        let policy = FramePacing::CatchUp { smooth: true };
        let plan = policy.plan(DT * 3.2, DT, &slow, || false);
        assert_eq!(plan.frames, 1);
        assert_eq!(plan.accum, DT, "the unpaid backlog is forgiven");
        // Nothing to forgive when everything owed fits.
        let plan = policy.plan(DT * 1.2, DT, &slow, || false);
        assert_eq!(plan.frames, 1);
        assert_eq!(plan.accum, DT * 1.2);
    }

    #[test]
    fn hard_cap_runs_one_frame_or_two_on_a_slow_display() {
        let pace = HostPace::default();
        let policy = FramePacing::HardCap;
        assert_eq!(policy.plan(DT * 0.5, DT, &pace, || true).frames, 0);
        assert_eq!(policy.plan(DT * 1.5, DT, &pace, || true).frames, 1);
        assert_eq!(policy.plan(DT * 3.0, DT, &pace, || false).frames, 1);
        assert_eq!(policy.plan(DT * 3.0, DT, &pace, || true).frames, 2);
        let mut asked = false;
        policy.plan(DT * 1.5, DT, &pace, || {
            asked = true;
            true
        });
        assert!(!asked, "the monitor is only asked when a backlog exists");
    }

    #[test]
    fn hard_cap_drops_whole_frames_of_backlog_but_keeps_the_fraction() {
        let left = FramePacing::HardCap.settle(DT * 3.25, DT, 1);
        assert!((left - DT * 0.25).abs() < 1e-6, "left {left}");
        let catch_up = FramePacing::CatchUp { smooth: false }.settle(DT * 3.25, DT, 1);
        assert!((catch_up - DT * 2.25).abs() < 1e-6, "left {catch_up}");
    }
}
