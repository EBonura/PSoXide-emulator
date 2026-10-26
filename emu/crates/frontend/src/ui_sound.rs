//! Quiet sounds for the shell's menus, synthesised here (no samples): a few sine and triangle
//! notes with a soft attack and an exponential decay, 20 to 200 ms long, at about a tenth of full
//! scale. The shell asks for a sound by name; the audio pump mixes the voices into the output
//! while a menu is on screen.
//!
//! Nothing here touches the audio device: [`UiSynth::render_add`] adds the current voices into a
//! block of stereo samples, so the same code runs on the web and on a desktop and is tested
//! without a sound card.

use std::f32::consts::TAU;

/// Output sample rate of the emulator's audio queue.
pub(crate) const RATE: f32 = 44_100.0;
/// Loudest a single voice gets, of full scale: sounds stay under the game's own level.
const PEAK: f32 = 0.11;
/// The same sound asked for again within this many seconds is dropped (a held key, a slider drag).
const MIN_GAP: f32 = 0.022;
/// Attack and release ramps, seconds: no clicks at the edges.
const RAMP: f32 = 0.003;

/// What the shell can say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UiSound {
    /// The cursor moved to another case, row or tab-bar item.
    Move,
    /// A choice was confirmed.
    Confirm,
    /// Back, close or cancel.
    Back,
    /// Another page or section.
    Tab,
    /// A switch went on.
    On,
    /// A switch went off.
    Off,
    /// A slider or stepper changed by one step.
    Step,
    /// The shell or a sheet opened.
    Open,
    /// The shell or a sheet closed.
    Close,
    /// The answer is no: a save that cannot load, an action that is refused.
    Refused,
}

impl UiSound {
    pub(crate) const ALL: [Self; 10] = [
        Self::Move,
        Self::Confirm,
        Self::Back,
        Self::Tab,
        Self::On,
        Self::Off,
        Self::Step,
        Self::Open,
        Self::Close,
        Self::Refused,
    ];

    /// The name the browser journeys read.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Move => "move",
            Self::Confirm => "confirm",
            Self::Back => "back",
            Self::Tab => "tab",
            Self::On => "on",
            Self::Off => "off",
            Self::Step => "step",
            Self::Open => "open",
            Self::Close => "close",
            Self::Refused => "refused",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }

    /// The notes of the sound.
    fn notes(self) -> &'static [Note] {
        use Wave::{Sine, Triangle};
        // Struct literals of constants are promoted to statics; a function call would not be.
        macro_rules! n {
            ($start:expr, $from:expr, $to:expr, $len:expr, $amp:expr, $wave:expr) => {
                Note {
                    start: $start,
                    from: $from,
                    to: $to,
                    len: $len,
                    amp: $amp,
                    wave: $wave,
                }
            };
        }
        match self {
            Self::Move => &[n!(0.0, 1250.0, 1250.0, 0.032, 0.7, Sine)],
            Self::Tab => &[n!(0.0, 960.0, 1180.0, 0.07, 0.8, Sine)],
            Self::Confirm => &[
                n!(0.0, 784.0, 784.0, 0.075, 0.9, Sine),
                n!(0.055, 1175.0, 1175.0, 0.13, 1.0, Sine),
            ],
            Self::Back => &[n!(0.0, 760.0, 520.0, 0.11, 0.9, Sine)],
            Self::On => &[
                n!(0.0, 880.0, 880.0, 0.06, 0.8, Sine),
                n!(0.05, 1320.0, 1320.0, 0.1, 0.9, Sine),
            ],
            Self::Off => &[
                n!(0.0, 1320.0, 1320.0, 0.06, 0.8, Sine),
                n!(0.05, 880.0, 880.0, 0.1, 0.8, Sine),
            ],
            Self::Step => &[n!(0.0, 1800.0, 1800.0, 0.016, 0.55, Sine)],
            Self::Open => &[n!(0.0, 520.0, 920.0, 0.15, 0.85, Sine)],
            Self::Close => &[n!(0.0, 920.0, 520.0, 0.13, 0.8, Sine)],
            Self::Refused => &[
                n!(0.0, 196.0, 190.0, 0.1, 1.0, Triangle),
                n!(0.115, 185.0, 175.0, 0.16, 1.0, Triangle),
            ],
        }
    }

    /// Seconds from the start to the end of the last note.
    pub(crate) fn length(self) -> f32 {
        self.notes()
            .iter()
            .map(|n| n.start + n.len)
            .fold(0.0, f32::max)
    }
}

#[derive(Clone, Copy, Debug)]
enum Wave {
    Sine,
    Triangle,
}

/// One tone: when it starts, its pitch glide, how long it lasts and how loud (0 to 1).
#[derive(Clone, Copy, Debug)]
struct Note {
    start: f32,
    from: f32,
    to: f32,
    len: f32,
    amp: f32,
    wave: Wave,
}

impl Note {
    /// The note's value `t` seconds after the sound started.
    fn at(&self, t: f32) -> f32 {
        let local = t - self.start;
        if !(0.0..self.len).contains(&local) {
            return 0.0;
        }
        let k = local / self.len;
        // The pitch glides linearly; the phase is the integral of the frequency.
        let phase = TAU * (self.from * local + 0.5 * (self.to - self.from) * local * k);
        let tone = match self.wave {
            Wave::Sine => phase.sin(),
            Wave::Triangle => (2.0 / std::f32::consts::PI) * phase.sin().asin(),
        };
        let attack = (local / RAMP).min(1.0);
        let release = ((self.len - local) / RAMP).min(1.0);
        // Decay: down to about 2% by the end of the note.
        let decay = (-4.0 * k).exp();
        self.amp * tone * attack * release * decay
    }
}

#[derive(Clone, Copy, Debug)]
struct Voice {
    sound: UiSound,
    /// Samples played so far.
    age: u32,
    /// Pitch multiplier (a slider step rises with the value).
    pitch: f32,
}

/// The menu sounds: what is playing now and what has been asked for.
#[derive(Debug)]
pub(crate) struct UiSynth {
    voices: Vec<Voice>,
    /// The Menu sounds setting.
    enabled: bool,
    /// A person has used the page (browsers play no sound before that).
    armed: bool,
    /// Sounds that were allowed to play, by kind, since start (for the browser journeys).
    fired: [u32; 10],
    /// The last sound allowed to play.
    last: Option<UiSound>,
    /// Sounds dropped because the setting is off or nobody has used the page yet.
    held_back: u32,
}

impl Default for UiSynth {
    fn default() -> Self {
        Self {
            voices: Vec::new(),
            enabled: true,
            armed: false,
            fired: [0; 10],
            last: None,
            held_back: 0,
        }
    }
}

impl UiSynth {
    /// The Menu sounds setting.
    pub(crate) fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        if !on {
            self.voices.clear();
        }
    }

    /// A key, a click or a touch happened: sound may play from now on.
    pub(crate) fn arm(&mut self) {
        self.armed = true;
    }

    /// Ask for a sound. `pitch` is 1 for the written pitch.
    pub(crate) fn trigger(&mut self, sound: UiSound, pitch: f32) {
        if !self.enabled || !self.armed {
            self.held_back += 1;
            return;
        }
        let gap = (MIN_GAP * RATE) as u32;
        if self.voices.iter().any(|v| v.sound == sound && v.age < gap) {
            return;
        }
        self.fired[sound.index()] += 1;
        self.last = Some(sound);
        self.voices.push(Voice {
            sound,
            age: 0,
            pitch: pitch.clamp(0.5, 2.0),
        });
    }

    /// Drop everything that is sounding.
    pub(crate) fn silence(&mut self) {
        self.voices.clear();
    }

    /// The state the browser journeys read: what was heard, by kind.
    pub(crate) fn probe_json(&self) -> String {
        let counts = UiSound::ALL
            .iter()
            .map(|s| format!("\"{}\":{}", s.name(), self.fired(*s)))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{{\"enabled\":{},\"armed\":{},\"total\":{},\"held\":{},\"last\":\"{}\",\"counts\":{{{counts}}}}}",
            self.enabled,
            self.armed,
            self.fired_total(),
            self.held_back,
            self.last.map_or("", UiSound::name)
        )
    }

    /// Whether anything is still sounding.
    pub(crate) fn active(&self) -> bool {
        !self.voices.is_empty()
    }

    /// How many times `sound` was allowed to play, and the total.
    pub(crate) fn fired(&self, sound: UiSound) -> u32 {
        self.fired[sound.index()]
    }

    pub(crate) fn fired_total(&self) -> u32 {
        self.fired.iter().sum()
    }

    #[cfg(test)]
    pub(crate) fn last(&self) -> Option<UiSound> {
        self.last
    }

    #[cfg(test)]
    pub(crate) fn held_back(&self) -> u32 {
        self.held_back
    }

    /// Add the voices into `out` (stereo, [`RATE`]) and move them on by `out.len()` samples.
    /// `gain` is the player's volume with mute applied.
    pub(crate) fn render_add(&mut self, out: &mut [(i16, i16)], gain: f32) {
        if self.voices.is_empty() {
            return;
        }
        let gain = gain.clamp(0.0, 1.5) * PEAK * f32::from(i16::MAX);
        for voice in &mut self.voices {
            let notes = voice.sound.notes();
            for sample in out.iter_mut() {
                let t = voice.age as f32 / RATE * voice.pitch;
                voice.age += 1;
                let v: f32 = notes.iter().map(|n| n.at(t)).sum::<f32>() * gain;
                let v = v.round() as i32;
                sample.0 = (i32::from(sample.0) + v).clamp(-32_768, 32_767) as i16;
                sample.1 = (i32::from(sample.1) + v).clamp(-32_768, 32_767) as i16;
            }
        }
        self.voices
            .retain(|v| (v.age as f32 / RATE * v.pitch) < v.sound.length() + 0.005);
    }

    /// `frames` samples of just the voices.
    pub(crate) fn render(&mut self, frames: usize, gain: f32) -> Vec<(i16, i16)> {
        let mut out = vec![(0, 0); frames];
        self.render_add(&mut out, gain);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn armed() -> UiSynth {
        let mut s = UiSynth::default();
        s.arm();
        s
    }

    fn play(sound: UiSound) -> Vec<(i16, i16)> {
        let mut s = armed();
        s.trigger(sound, 1.0);
        s.render((RATE * 0.5) as usize, 1.0)
    }

    #[test]
    fn every_sound_is_short_quiet_and_ends_in_silence() {
        for sound in UiSound::ALL {
            assert!(sound.length() > 0.012 && sound.length() < 0.3, "{sound:?}");
            let out = play(sound);
            let peak = out.iter().map(|s| s.0.unsigned_abs()).max().unwrap_or(0);
            assert!(peak > 300, "{sound:?} is audible, peak {peak}");
            assert!(peak < 4_500, "{sound:?} stays quiet, peak {peak}");
            // The tail is silent and the edges start and end at zero (no click).
            assert!(out[(RATE * 0.4) as usize..].iter().all(|s| *s == (0, 0)));
            assert!(out[0].0.abs() < 200, "{sound:?} starts from silence");
        }
    }

    #[test]
    fn the_sounds_differ() {
        let all: Vec<Vec<(i16, i16)>> = UiSound::ALL.iter().map(|s| play(*s)).collect();
        for i in 0..all.len() {
            for j in i + 1..all.len() {
                assert_ne!(
                    all[i],
                    all[j],
                    "{:?} and {:?} are the same",
                    UiSound::ALL[i],
                    UiSound::ALL[j]
                );
            }
        }
    }

    #[test]
    fn nothing_plays_before_the_page_was_used_or_with_the_setting_off() {
        let mut s = UiSynth::default();
        s.trigger(UiSound::Move, 1.0);
        assert!(!s.active() && s.fired_total() == 0 && s.held_back() == 1);
        s.arm();
        s.set_enabled(false);
        s.trigger(UiSound::Move, 1.0);
        assert!(!s.active() && s.fired_total() == 0 && s.held_back() == 2);
        s.set_enabled(true);
        s.trigger(UiSound::Move, 1.0);
        assert!(s.active());
        assert_eq!(s.fired(UiSound::Move), 1);
        assert_eq!(s.last(), Some(UiSound::Move));
    }

    #[test]
    fn muting_or_volume_zero_is_silent_and_the_volume_scales_it() {
        let mut s = armed();
        s.trigger(UiSound::Confirm, 1.0);
        let quiet = s.render(2000, 0.0);
        assert!(quiet.iter().all(|x| *x == (0, 0)));
        let mut s = armed();
        s.trigger(UiSound::Confirm, 1.0);
        let full = s.render(2000, 1.0);
        let mut s = armed();
        s.trigger(UiSound::Confirm, 1.0);
        let half = s.render(2000, 0.5);
        let peak = |v: &[(i16, i16)]| v.iter().map(|x| x.0.unsigned_abs()).max().unwrap_or(0);
        assert!(peak(&half) * 2 > peak(&full) - 8 && peak(&half) * 2 < peak(&full) + 8);
    }

    #[test]
    fn a_held_key_does_not_stack_the_same_sound() {
        let mut s = armed();
        s.trigger(UiSound::Move, 1.0);
        s.trigger(UiSound::Move, 1.0);
        assert_eq!(s.fired(UiSound::Move), 1);
        s.render((RATE * 0.03) as usize, 1.0);
        s.trigger(UiSound::Move, 1.0);
        assert_eq!(s.fired(UiSound::Move), 2);
    }

    #[test]
    fn mixing_adds_to_what_is_there_and_never_wraps() {
        let mut s = armed();
        s.trigger(UiSound::Open, 1.0);
        let mut block = vec![(32_000_i16, -32_000_i16); 4000];
        s.render_add(&mut block, 1.5);
        // The loud block stays loud on its own side: no wrap-around to the other sign.
        assert!(block.iter().all(|x| x.0 > 20_000 && x.1 < -20_000));
        let mut s = armed();
        s.trigger(UiSound::Open, 1.0);
        let mut quiet = vec![(100_i16, 100_i16); 4000];
        s.render_add(&mut quiet, 1.0);
        assert!(
            quiet.iter().any(|x| x.0 != 100),
            "the voice is added to the block"
        );
    }

    #[test]
    fn voices_are_dropped_when_done() {
        let mut s = armed();
        s.trigger(UiSound::Refused, 1.0);
        assert!(s.active());
        s.render((RATE * 0.5) as usize, 1.0);
        assert!(!s.active());
    }
}
