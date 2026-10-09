//! Host-side audio output -- cpal output stream fed by a lock-free
//! ring buffer whose producer is the emulation thread.
//!
//! Design:
//!
//! - The bus clock produces SPU samples on the emulation thread. The shell
//!   catches up with `Bus::run_spu_to_current_cycle()` and drains the queue
//!   via `Bus::spu.drain_audio()` every frame: `(i16, i16)` at 44.1 kHz stereo.
//! - A cpal output stream runs on an OS-provided audio thread and pulls
//!   samples out of a shared ring buffer on each callback. The callback never
//!   blocks on the producer. Output starts only once [`PREFILL_SAMPLES`] are
//!   queued; if the producer later falls behind, the last sample is faded to
//!   silence over a couple of milliseconds (not cut to zero, which is an
//!   audible click), the callback waits for the queue to refill, and the sound
//!   fades back in. If the producer runs ahead of the backlog cap, the oldest
//!   samples are dropped and the seam is crossfaded rather than jumped.
//! - The ring buffer is a [`std::sync::Mutex<VecDeque<(i16, i16)>>`].
//!   Not lock-free but cheap at audio-block granularity (512-sample
//!   blocks × ~86 blocks/sec = 172 locks/sec -- negligible).
//!
//! We keep the cpal host + stream + config alive inside [`AudioOut`].
//! Dropping the struct stops the stream.
//!
//! On wasm, cpal uses its WebAudio backend (a main-thread ScriptProcessor
//! callback), so the same [`AudioOut`] runs on the web -- single threaded, no
//! SharedArrayBuffer needed. Browsers suspend the audio context until a user
//! gesture, so sound starts once the player first interacts with the page.

#![cfg_attr(target_arch = "wasm32", allow(rustdoc::broken_intra_doc_links))]

use std::sync::{
    atomic::{AtomicU32, AtomicU64, Ordering},
    Arc, Mutex,
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Target sample rate -- PSX SPU native rate. Host may negotiate up
/// (48 kHz is common); cpal handles any rate we ask for or tells us
/// the default.
const TARGET_SAMPLE_RATE: u32 = 44_100;

/// Backlog the output waits for before it starts, and again after an underrun.
/// This is the steady-state latency and also the jitter the host can absorb
/// before the output runs dry: a slow emulation frame (a busy FMV frame, a
/// shader compile, a window drag) must be shorter than this to be inaudible.
///
/// Native has a real audio thread, so it can afford more headroom. The web has
/// no separate audio thread (cpal's callback shares the main thread with the
/// emulator), so there the queue builds up and the numbers stay small.
#[cfg(not(target_arch = "wasm32"))]
const PREFILL_MS: usize = 50;
#[cfg(target_arch = "wasm32")]
const PREFILL_MS: usize = 20;

/// Max audio backlog before the oldest samples are dropped. Bounds how far the
/// sound can lag the action after a burst of catch-up frames.
#[cfg(not(target_arch = "wasm32"))]
const MAX_BACKLOG_MS: usize = 160;
#[cfg(target_arch = "wasm32")]
const MAX_BACKLOG_MS: usize = 64;

const PREFILL_SAMPLES: usize = (TARGET_SAMPLE_RATE as usize) * PREFILL_MS / 1000;
const MAX_BACKLOG_SAMPLES: usize = (TARGET_SAMPLE_RATE as usize) * MAX_BACKLOG_MS / 1000;

/// Length of the fade to or from silence at an underrun, in output frames
/// (about 2 ms). Long enough to carry no audible edge, short enough that an
/// underrun costs almost nothing before the refill wait.
const FADE_FRAMES: u32 = 96;

/// Source samples blended over a dropped-backlog seam (about 1.5 ms).
const SEAM_CROSSFADE_SAMPLES: usize = 64;

/// Shared producer/consumer state. Producer = emulation thread;
/// consumer = cpal callback. `Arc<Mutex<...>>` is overkill for the
/// real-time audio path but cpal's callback lives longer than the
/// main thread's stack and must own its state.
///
/// `queue` stores interleaved stereo; the callback pops `(l, r)` pairs and
/// interleaves them into cpal's output.
pub type SampleQueue = Arc<Mutex<Backlog>>;

/// Queued samples plus the last one the consumer took, which the producer
/// needs to blend a dropped-backlog seam.
#[derive(Default)]
pub struct Backlog {
    queue: std::collections::VecDeque<(i16, i16)>,
    last_consumed: (i16, i16),
}

impl Backlog {
    /// Append `samples`. Past `cap` the oldest are dropped, and the new head of
    /// the queue is crossfaded from the last sample the consumer took, so the
    /// skip is a short blend rather than a step.
    fn push(&mut self, samples: &[(i16, i16)], cap: usize) {
        self.queue.extend(samples.iter().copied());
        let overflow = self.queue.len().saturating_sub(cap);
        if overflow == 0 {
            return;
        }
        self.queue.drain(..overflow);
        let from = self.last_consumed;
        let span = SEAM_CROSSFADE_SAMPLES.min(self.queue.len());
        for (i, sample) in self.queue.iter_mut().take(span).enumerate() {
            let t = (i + 1) as f32 / (span + 1) as f32;
            *sample = (blend(from.0, sample.0, t), blend(from.1, sample.1, t));
        }
    }
}

fn blend(a: i16, b: i16, t: f32) -> i16 {
    (a as f32 + (b as f32 - a as f32) * t).round() as i16
}
type VolumeControl = Arc<AtomicU32>;

/// Live audio output. Owns the cpal stream (which runs on an OS
/// audio thread) and exposes the producer handle to the shell's
/// per-frame SPU drain.
pub struct AudioOut {
    /// Sample producer -- the shell clones this to push drained
    /// SPU samples after each CPU frame.
    queue: SampleQueue,
    /// Kept alive so the cpal stream keeps running. Dropping it
    /// stops the audio thread.
    _stream: cpal::Stream,
    /// Sample rate actually negotiated with the host. Differs from
    /// [`TARGET_SAMPLE_RATE`] when the OS device doesn't accept
    /// 44.1 kHz (e.g. macOS CoreAudio often wants 48 kHz). The
    /// callback linearly resamples the SPU stream to avoid the
    /// zippery crackle nearest-neighbour introduces at 48 kHz.
    host_sample_rate: u32,
    /// Host-output gain. Stored atomically so the UI can adjust it
    /// without locking the CPAL callback.
    volume: VolumeControl,
    /// Output frames the callback had to fill with silence because the
    /// queue was empty (cumulative). Diagnostic for underruns.
    underrun_frames: Arc<AtomicU64>,
    trace: bool,
    trace_stats: Mutex<AudioTraceStats>,
}

#[derive(Default)]
struct AudioTraceStats {
    samples: usize,
    nonzero: usize,
    peak_l: u16,
    peak_r: u16,
}

impl AudioTraceStats {
    fn add(&mut self, samples: &[(i16, i16)]) {
        self.samples += samples.len();
        for &(l, r) in samples {
            self.peak_l = self.peak_l.max(l.unsigned_abs());
            self.peak_r = self.peak_r.max(r.unsigned_abs());
            if l != 0 || r != 0 {
                self.nonzero += 1;
            }
        }
    }

    fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

impl AudioOut {
    /// Spin up the host audio stream. Returns `None` when no output
    /// device is available (headless CI, WSL without PulseAudio).
    /// The shell treats `None` as "audio silenced" -- emulation
    /// still runs, you just don't hear anything.
    pub fn open() -> Option<Self> {
        let host = cpal::default_host();
        let device = host.default_output_device()?;
        // Pick a supported stereo output config; prefer 44.1 kHz to
        // avoid resampling, fall back to whatever the device offers.
        let supported = device.supported_output_configs().ok()?;
        let mut chosen: Option<cpal::SupportedStreamConfig> = None;
        for cfg in supported {
            if cfg.channels() != 2 {
                continue;
            }
            // Only consider formats the stream-building match below actually
            // handles (F32 / I16). Some devices (observed: a Bluetooth
            // headset reporting U8/I16/I32/F32, all at 44.1 kHz, with U8
            // listed *first*) would otherwise have their very first --
            // otherwise-perfectly-matching-rate -- config picked here, land
            // on the `_ => return None` catch-all below, and silently report
            // "no output device available" despite the same device working
            // fine for a format we do support.
            if !matches!(
                cfg.sample_format(),
                cpal::SampleFormat::F32 | cpal::SampleFormat::I16
            ) {
                continue;
            }
            let min = cfg.min_sample_rate().0;
            let max = cfg.max_sample_rate().0;
            if (min..=max).contains(&TARGET_SAMPLE_RATE) {
                chosen = Some(cfg.with_sample_rate(cpal::SampleRate(TARGET_SAMPLE_RATE)));
                break;
            }
            if chosen.is_none() {
                chosen = Some(cfg.with_max_sample_rate());
            }
        }
        let config = chosen?;
        let host_sample_rate = config.sample_rate().0;
        let sample_format = config.sample_format();
        let stream_config: cpal::StreamConfig = config.into();

        let queue: SampleQueue = Arc::new(Mutex::new(Backlog::default()));
        let volume: VolumeControl = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let underrun_frames = Arc::new(AtomicU64::new(0));
        let trace = env_flag("PSOXIDE_AUDIO_TRACE");
        if trace {
            eprintln!("[audio] trace enabled");
        }

        // Ratio between PSX 44.1 kHz and the host's actual rate.
        // E.g. host @ 48 kHz, PSX @ 44.1 kHz → ratio = 44100/48000 ≈ 0.919,
        // so every 1000 host samples advance ~919 source samples.
        let pull_rate = TARGET_SAMPLE_RATE as f32 / host_sample_rate as f32;

        let queue_cb = Arc::clone(&queue);
        let volume_cb = Arc::clone(&volume);
        let underrun_cb = Arc::clone(&underrun_frames);
        let err_fn = |e| eprintln!("[audio] stream error: {e}");

        let stream = match sample_format {
            cpal::SampleFormat::F32 => device
                .build_output_stream(
                    &stream_config,
                    {
                        let mut stage = OutputStage::new();
                        move |out: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                            let mut q = queue_cb.lock().unwrap();
                            for frame in out.chunks_mut(2) {
                                let gain = f32::from_bits(volume_cb.load(Ordering::Relaxed));
                                let (l, r) = stage.next(&mut q, pull_rate);
                                frame[0] = apply_gain_f32(l, gain);
                                if frame.len() > 1 {
                                    frame[1] = apply_gain_f32(r, gain);
                                }
                            }
                            underrun_cb.store(stage.starved, Ordering::Relaxed);
                        }
                    },
                    err_fn,
                    None,
                )
                .ok()?,
            cpal::SampleFormat::I16 => {
                let queue_cb = Arc::clone(&queue);
                let volume_cb = Arc::clone(&volume);
                let underrun_cb = Arc::clone(&underrun_frames);
                device
                    .build_output_stream(
                        &stream_config,
                        {
                            let mut stage = OutputStage::new();
                            move |out: &mut [i16], _info: &cpal::OutputCallbackInfo| {
                                let mut q = queue_cb.lock().unwrap();
                                for frame in out.chunks_mut(2) {
                                    let gain = f32::from_bits(volume_cb.load(Ordering::Relaxed));
                                    let (l, r) = stage.next(&mut q, pull_rate);
                                    frame[0] = apply_gain_i16(l, gain);
                                    if frame.len() > 1 {
                                        frame[1] = apply_gain_i16(r, gain);
                                    }
                                }
                                underrun_cb.store(stage.starved, Ordering::Relaxed);
                            }
                        },
                        err_fn,
                        None,
                    )
                    .ok()?
            }
            // Other formats (U16, etc.) -- not common on modern
            // hosts; gracefully fail and let the shell run silent.
            _ => return None,
        };
        stream.play().ok()?;

        Some(Self {
            queue,
            _stream: stream,
            host_sample_rate,
            volume,
            underrun_frames,
            trace,
            trace_stats: Mutex::new(AudioTraceStats::default()),
        })
    }

    /// Push drained SPU samples into the ring. The shell calls this
    /// after each frame's clock catch-up and queue drain. Past the backlog cap
    /// the oldest samples are dropped (with the seam crossfaded) -- prevents
    /// unbounded growth when the emulator runs faster than real time
    /// (fast-forward, rewind).
    pub fn push_samples(&self, samples: &[(i16, i16)]) {
        let mut q = self.queue.lock().unwrap();
        q.push(samples, MAX_BACKLOG_SAMPLES);
        let queue_len = q.queue.len();
        drop(q);

        if self.trace {
            let mut stats = self.trace_stats.lock().unwrap();
            stats.add(samples);
            if stats.samples >= TARGET_SAMPLE_RATE as usize {
                let stats = stats.take();
                let gain = f32::from_bits(self.volume.load(Ordering::Relaxed));
                eprintln!(
                    "[audio] pushed={} nonzero={} peak=({},{}) queue={} gain={:.2}",
                    stats.samples, stats.nonzero, stats.peak_l, stats.peak_r, queue_len, gain
                );
            }
        }
    }

    /// Host's negotiated sample rate. Diagnostic -- shown in the
    /// HUD so users can confirm audio is actually running.
    pub fn host_sample_rate(&self) -> u32 {
        self.host_sample_rate
    }

    /// Set host-output gain. `1.0` is unity, `0.0` is silence.
    pub fn set_volume(&self, volume: f32) {
        self.volume
            .store(volume.clamp(0.0, 1.5).to_bits(), Ordering::Relaxed);
    }

    /// Output frames filled with silence for lack of samples, since open.
    pub fn underrun_frames(&self) -> u64 {
        self.underrun_frames.load(Ordering::Relaxed)
    }

    /// Current queue depth in stereo samples. Diagnostic -- very
    /// high values mean the CPU is overrunning real-time; very low
    /// means we're starving the callback. Read by the web bench hooks.
    #[cfg(target_arch = "wasm32")]
    pub fn queue_len(&self) -> usize {
        self.queue.lock().map(|q| q.queue.len()).unwrap_or(0)
    }
}

/// The consumer side of the queue: sample-rate conversion plus the underrun
/// policy. Everything the audio thread decides lives here so it can be driven
/// from a test at simulated timings.
struct OutputStage {
    resampler: LinearResampler,
    mode: StageMode,
    /// Last frame handed to the speaker.
    last_audible: (f32, f32),
    /// The frame that was playing when the stage last ran dry, and how much of
    /// it is still audible. It decays to nothing over [`FADE_FRAMES`] whether
    /// or not the queue has refilled, so a short gap is a held-and-faded
    /// sample rather than a cut to zero.
    tail: (f32, f32),
    tail_gain: f32,
    /// Gain on live audio, `0.0..=1.0`. Rises after every start so the stream
    /// fades in over the tail instead of stepping to it.
    gain: f32,
    /// Output frames emitted without live audio since the first start,
    /// cumulative. The initial prefill wait does not count: nothing was
    /// playing yet.
    starved: u64,
    /// Whether the first prefill has completed.
    started: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StageMode {
    /// Waiting for the queue to reach [`PREFILL_SAMPLES`].
    Buffering,
    Playing,
}

impl OutputStage {
    fn new() -> Self {
        Self {
            resampler: LinearResampler::new(),
            mode: StageMode::Buffering,
            last_audible: (0.0, 0.0),
            tail: (0.0, 0.0),
            tail_gain: 0.0,
            gain: 0.0,
            starved: 0,
            started: false,
        }
    }

    fn next(&mut self, backlog: &mut Backlog, pull_rate: f32) -> (f32, f32) {
        let step = 1.0 / FADE_FRAMES as f32;
        if self.mode == StageMode::Buffering && backlog.queue.len() >= PREFILL_SAMPLES {
            self.mode = StageMode::Playing;
            self.started = true;
            self.resampler = LinearResampler::new();
            self.gain = 0.0;
        }
        let live = if self.mode == StageMode::Playing {
            let frame = self.resampler.next(backlog, pull_rate);
            if frame.is_none() {
                // Ran dry mid-stream: keep what was audible, fading, and wait
                // for a refill.
                self.mode = StageMode::Buffering;
                self.tail = self.last_audible;
                self.tail_gain = 1.0;
                self.gain = 0.0;
            }
            frame
        } else {
            None
        };
        let out = match live {
            Some(frame) => {
                self.gain = (self.gain + step).min(1.0);
                (
                    frame.0 * self.gain + self.tail.0 * self.tail_gain,
                    frame.1 * self.gain + self.tail.1 * self.tail_gain,
                )
            }
            None => {
                if self.started {
                    self.starved += 1;
                }
                (self.tail.0 * self.tail_gain, self.tail.1 * self.tail_gain)
            }
        };
        self.tail_gain = (self.tail_gain - step).max(0.0);
        self.last_audible = out;
        out
    }
}

/// Tiny stateful sample-rate converter used by the CPAL callback.
/// It keeps its interpolation phase across callbacks; resetting this
/// state per callback is audible as periodic ticks on hosts whose
/// native rate is 48 kHz.
struct LinearResampler {
    phase: f32,
    prev: (i16, i16),
    next: (i16, i16),
    primed: bool,
}

impl LinearResampler {
    fn new() -> Self {
        Self {
            phase: 0.0,
            prev: (0, 0),
            next: (0, 0),
            primed: false,
        }
    }

    /// The next output frame, or `None` when the queue cannot supply the
    /// source sample the interpolation needs.
    fn next(&mut self, backlog: &mut Backlog, pull_rate: f32) -> Option<(f32, f32)> {
        if !self.primed {
            self.prev = backlog.queue.pop_front()?;
            self.next = backlog.queue.pop_front().unwrap_or(self.prev);
            backlog.last_consumed = self.next;
            self.primed = true;
        }

        let out = lerp_sample(self.prev, self.next, self.phase);
        let mut phase = self.phase + pull_rate;
        while phase >= 1.0 {
            let Some(sample) = backlog.queue.pop_front() else {
                // Keep the phase where it was: nothing is consumed on a miss.
                return None;
            };
            self.prev = self.next;
            self.next = sample;
            backlog.last_consumed = sample;
            phase -= 1.0;
        }
        self.phase = phase;
        Some(out)
    }
}

fn lerp_sample(a: (i16, i16), b: (i16, i16), t: f32) -> (f32, f32) {
    (
        a.0 as f32 + (b.0 as f32 - a.0 as f32) * t,
        a.1 as f32 + (b.1 as f32 - a.1 as f32) * t,
    )
}

fn apply_gain_f32(sample: f32, gain: f32) -> f32 {
    (sample / 32768.0 * gain).clamp(-1.0, 1.0)
}

fn apply_gain_i16(sample: f32, gain: f32) -> i16 {
    (sample * gain)
        .round()
        .clamp(i16::MIN as f32, i16::MAX as f32) as i16
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).ok().is_some_and(|value| {
        !matches!(value.as_str(), "" | "0" | "false" | "FALSE" | "off" | "OFF")
    })
}

// (Web audio uses the same cpal `AudioOut` above, via the WebAudio backend.)

#[cfg(test)]
mod tests {
    //! Drives the real consumer and producer code at simulated timings and
    //! counts audible discontinuities in what the speaker would get. The
    //! `Legacy*` types reproduce the pre-prefill behaviour so every scenario
    //! can be compared before and after: `cargo test -p frontend audio::tests
    //! -- --nocapture` prints the table.

    use super::*;
    use std::collections::VecDeque;

    const HOST_RATE: u32 = 48_000;
    const CALLBACK_FRAMES: usize = 512;
    const EMU_FRAME_S: f64 = 1.0 / 60.0;
    const SAMPLES_PER_EMU_FRAME: usize = 735;

    /// Smooth two-tone source. Its steepest step at 48 kHz is under 1500, so
    /// any output step past `CLICK_STEP` is something the pipeline added.
    fn tone(i: usize) -> (i16, i16) {
        let t = i as f64 / TARGET_SAMPLE_RATE as f64;
        let v = 6000.0 * (std::f64::consts::TAU * 440.0 * t).sin()
            + 6000.0 * (std::f64::consts::TAU * 1320.0 * t).sin();
        (v as i16, (v * 0.8) as i16)
    }
    const CLICK_STEP: f32 = 2500.0;

    /// The old callback: no prefill, silence on an empty queue, oldest samples
    /// dropped without a blend past a 64 ms cap.
    struct LegacyOut {
        queue: VecDeque<(i16, i16)>,
        resampler: LegacyResampler,
    }
    struct LegacyResampler {
        phase: f32,
        prev: (i16, i16),
        next: (i16, i16),
        primed: bool,
    }
    impl LegacyOut {
        fn new() -> Self {
            Self {
                queue: VecDeque::new(),
                resampler: LegacyResampler {
                    phase: 0.0,
                    prev: (0, 0),
                    next: (0, 0),
                    primed: false,
                },
            }
        }
        fn push(&mut self, samples: &[(i16, i16)]) {
            let cap = TARGET_SAMPLE_RATE as usize * 64 / 1000;
            let overflow = (self.queue.len() + samples.len()).saturating_sub(cap);
            for _ in 0..overflow {
                self.queue.pop_front();
            }
            self.queue.extend(samples.iter().copied());
        }
        fn next(&mut self, pull: f32) -> (f32, f32) {
            let r = &mut self.resampler;
            if !r.primed {
                r.prev = self.queue.pop_front().unwrap_or((0, 0));
                r.next = self.queue.pop_front().unwrap_or(r.prev);
                r.primed = true;
            }
            let out = lerp_sample(r.prev, r.next, r.phase);
            r.phase += pull;
            while r.phase >= 1.0 {
                r.prev = r.next;
                r.next = self.queue.pop_front().unwrap_or((0, 0));
                r.phase -= 1.0;
            }
            out
        }
    }

    /// One producer event: at `time_s` the emulation pushes `n` more samples.
    fn push_events(
        seconds: f64,
        mut frame_cost_s: impl FnMut(usize) -> f64,
        hitch: impl Fn(f64) -> f64,
    ) -> Vec<(f64, usize)> {
        // The shell wakes on a 60 Hz redraw, runs the frames owed, and pushes
        // after each. A slow frame delays the next wake, so the following
        // redraw owes (and pushes) more than one frame at once.
        let mut events = Vec::new();
        let mut now = 0.0f64; // wall clock at the start of a redraw
        let mut done = 0usize; // emulation frames delivered
        let mut vsync = 0usize;
        while now < seconds {
            // Frames the guest clock owes at this wall time: the one in
            // progress counts, so a steady 60 Hz redraw runs exactly one.
            let owed = (((now / EMU_FRAME_S).floor() as usize + 1).saturating_sub(done)).min(6);
            for _ in 0..owed {
                now += frame_cost_s(done) + hitch(now);
                done += 1;
                events.push((now, SAMPLES_PER_EMU_FRAME));
            }
            // Next redraw: the following vsync, or at once if the work overran it.
            vsync += 1;
            now = now.max(vsync as f64 * EMU_FRAME_S);
        }
        events
    }

    #[derive(Default, Debug)]
    struct Report {
        clicks: usize,
        underruns: u64,
        max_step: f32,
    }

    fn count_clicks(out: &[f32], report: &mut Report) {
        for pair in out.windows(2) {
            let step = (pair[1] - pair[0]).abs();
            report.max_step = report.max_step.max(step);
            if step > CLICK_STEP {
                report.clicks += 1;
            }
        }
    }

    fn run_new(events: &[(f64, usize)], seconds: f64) -> Report {
        let backlog = std::cell::RefCell::new(Backlog::default());
        let stage = std::cell::RefCell::new(OutputStage::new());
        let pull = TARGET_SAMPLE_RATE as f32 / HOST_RATE as f32;
        let report = run(
            events,
            seconds,
            |samples| backlog.borrow_mut().push(samples, MAX_BACKLOG_SAMPLES),
            |n| {
                (0..n)
                    .map(|_| stage.borrow_mut().next(&mut backlog.borrow_mut(), pull).0)
                    .collect()
            },
        );
        let underruns = stage.borrow().starved;
        Report {
            underruns,
            ..report
        }
    }

    fn run_legacy(events: &[(f64, usize)], seconds: f64) -> Report {
        let out = std::cell::RefCell::new(LegacyOut::new());
        let pull = TARGET_SAMPLE_RATE as f32 / HOST_RATE as f32;
        run(
            events,
            seconds,
            |samples| out.borrow_mut().push(samples),
            |n| (0..n).map(|_| out.borrow_mut().next(pull).0).collect(),
        )
    }

    /// Interleave producer events and fixed-size consumer callbacks in time.
    fn run(
        events: &[(f64, usize)],
        seconds: f64,
        mut push: impl FnMut(&[(i16, i16)]),
        mut pull_block: impl FnMut(usize) -> Vec<f32>,
    ) -> Report {
        let mut report = Report::default();
        let mut produced = 0usize;
        let mut next_event = 0usize;
        let mut samples_out = Vec::new();
        let mut t = 0.0f64;
        let cb_dt = CALLBACK_FRAMES as f64 / HOST_RATE as f64;
        while t < seconds {
            while next_event < events.len() && events[next_event].0 <= t {
                let n = events[next_event].1;
                let chunk: Vec<_> = (produced..produced + n).map(tone).collect();
                produced += n;
                push(&chunk);
                next_event += 1;
            }
            samples_out.extend(pull_block(CALLBACK_FRAMES));
            t += cb_dt;
        }
        // Skip the start-up ramp from silence: it is the stream beginning, not
        // a glitch in it.
        let skip = (HOST_RATE as usize / 4).min(samples_out.len());
        count_clicks(&samples_out[skip..], &mut report);
        report
    }

    struct Rng(u64);
    impl Rng {
        fn unit(&mut self) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) as f64 / (1u64 << 31) as f64
        }
    }

    fn scenario(name: &str, mean_cost_ms: f64, spike_pct: f64, hitch_ms: f64) -> (Report, Report) {
        let seconds = 30.0;
        let mut rng = Rng(7);
        let mut cost = move |_| {
            let c = mean_cost_ms * (0.7 + 0.6 * rng.unit());
            let spike = if rng.unit() < spike_pct / 100.0 {
                4.0
            } else {
                1.0
            };
            c * spike / 1000.0
        };
        let events = push_events(seconds, &mut cost, |now| {
            // One hitch of `hitch_ms` every 2 s, as a window drag or GC would.
            if hitch_ms > 0.0 && now > 1.0 && (now % 2.0) < 0.0167 {
                hitch_ms / 1000.0
            } else {
                0.0
            }
        });
        let legacy = run_legacy(&events, seconds);
        let new = run_new(&events, seconds);
        let worst_gap_ms = events
            .windows(2)
            .map(|w| w[1].0 - w[0].0)
            .fold(0.0f64, f64::max)
            * 1000.0;
        println!("[{name}: longest gap between pushes {worst_gap_ms:.0} ms]");
        println!(
            "{name:34} legacy clicks {:4} (max step {:7.0})  new clicks {:4} (max step {:7.0}, underrun frames {})",
            legacy.clicks, legacy.max_step, new.clicks, new.max_step, new.underruns
        );
        (legacy, new)
    }

    #[test]
    fn steady_pace_has_no_clicks() {
        let (_, new) = scenario("100% speed, light jitter", 6.0, 1.0, 0.0);
        assert_eq!(new.clicks, 0, "{new:?}");
        assert_eq!(new.underruns, 0, "{new:?}");
    }

    #[test]
    fn hitches_do_not_click() {
        // A stall near the prefill length may starve a few frames but must not click; longer ones
        // drain the queue and must degrade to a clean gap.
        let (_, short) = scenario("100%, 25 ms hitch / 2 s", 6.0, 1.0, 25.0);
        assert_eq!(short.clicks, 0, "{short:?}");
        let (_, long) = scenario("100%, 120 ms hitch / 2 s", 6.0, 1.0, 120.0);
        assert_eq!(long.clicks, 0, "{long:?}");
        assert!(long.underruns > 0, "{long:?}");
    }

    #[test]
    fn running_slow_degrades_to_clean_gaps() {
        let (_, new) = scenario("70% speed (24 ms frames)", 24.0, 2.0, 0.0);
        assert_eq!(new.clicks, 0, "{new:?}");
        assert!(new.underruns > 0);
    }

    #[test]
    fn running_ahead_drops_without_a_step() {
        // Frames cost nothing and the pacer does not hold them back, so the
        // producer outruns the consumer and the backlog cap trims it.
        let seconds = 10.0;
        let mut events = Vec::new();
        let mut t = 0.0;
        while t < seconds {
            t += EMU_FRAME_S / 1.3;
            events.push((t, SAMPLES_PER_EMU_FRAME));
        }
        let legacy = run_legacy(&events, seconds);
        let new = run_new(&events, seconds);
        println!(
            "{:34} legacy clicks {:4} (max step {:7.0})  new clicks {:4} (max step {:7.0})",
            "130% speed (backlog trim)", legacy.clicks, legacy.max_step, new.clicks, new.max_step
        );
        assert_eq!(new.clicks, 0, "{new:?}");
    }

    #[test]
    fn output_stage_never_blocks_on_an_empty_queue() {
        let mut backlog = Backlog::default();
        let mut stage = OutputStage::new();
        for _ in 0..10_000 {
            assert_eq!(stage.next(&mut backlog, 0.92), (0.0, 0.0));
        }
        assert_eq!(stage.starved, 0, "start-up wait is not an underrun");
    }
}
