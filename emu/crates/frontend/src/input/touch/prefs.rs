//! The player's touch-controller preferences, kept per device.

/// When the on-screen controller shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OverlayMode {
    /// On a touch device, until a keyboard or gamepad is used.
    Auto,
    /// Always.
    On,
    /// Never.
    Off,
}

impl OverlayMode {
    /// Label for the settings row.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::On => "On",
            Self::Off => "Off",
        }
    }

    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    const fn key(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::On => "on",
            Self::Off => "off",
        }
    }

    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    fn from_key(key: &str) -> Option<Self> {
        [Self::Auto, Self::On, Self::Off]
            .into_iter()
            .find(|mode| mode.key() == key)
    }

    /// The next mode in the settings cycle.
    pub(crate) const fn next(self) -> Self {
        match self {
            Self::Auto => Self::On,
            Self::On => Self::Off,
            Self::Off => Self::Auto,
        }
    }
}

/// Opacity presets, percent of fully opaque.
pub(crate) const OPACITY_PRESETS: [u8; 5] = [30, 45, 60, 75, 90];
/// Size presets, percent of the fitted size.
pub(crate) const SIZE_PRESETS: [u8; 6] = [80, 90, 100, 115, 130, 150];

/// Everything the player can change about the controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TouchPrefs {
    /// When it shows.
    pub(crate) mode: OverlayMode,
    /// Opacity while in use, percent.
    pub(crate) opacity_pct: u8,
    /// Size, percent of the fitted size.
    pub(crate) size_pct: u8,
    /// A vibration tick on each press, where the browser allows it.
    pub(crate) haptics: bool,
    /// Face buttons and right stick on the left, D-pad and left stick on the right.
    pub(crate) left_handed: bool,
}

impl Default for TouchPrefs {
    fn default() -> Self {
        Self {
            mode: OverlayMode::Auto,
            opacity_pct: 60,
            size_pct: 100,
            haptics: true,
            left_handed: false,
        }
    }
}

fn next_preset(presets: &[u8], current: u8) -> u8 {
    presets
        .iter()
        .position(|preset| *preset == current)
        .map_or(presets[0], |index| presets[(index + 1) % presets.len()])
}

impl TouchPrefs {
    /// Step the opacity through its presets.
    pub(crate) fn cycle_opacity(&mut self) {
        self.opacity_pct = next_preset(&OPACITY_PRESETS, self.opacity_pct);
    }

    /// Step the size through its presets.
    pub(crate) fn cycle_size(&mut self) {
        self.size_pct = next_preset(&SIZE_PRESETS, self.size_pct);
    }

    /// `key=value` pairs separated by `;`, for browser storage.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(crate) fn to_text(self) -> String {
        format!(
            "mode={};opacity={};size={};haptics={};left={}",
            self.mode.key(),
            self.opacity_pct,
            self.size_pct,
            u8::from(self.haptics),
            u8::from(self.left_handed),
        )
    }

    /// Parse [`Self::to_text`]; unknown or malformed parts keep the default.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(crate) fn from_text(text: &str) -> Self {
        let mut prefs = Self::default();
        for part in text.split(';') {
            let Some((key, value)) = part.split_once('=') else {
                continue;
            };
            match key.trim() {
                "mode" => {
                    if let Some(mode) = OverlayMode::from_key(value.trim()) {
                        prefs.mode = mode;
                    }
                }
                "opacity" => {
                    if let Ok(v) = value.trim().parse::<u8>() {
                        prefs.opacity_pct = v.clamp(15, 100);
                    }
                }
                "size" => {
                    if let Ok(v) = value.trim().parse::<u8>() {
                        prefs.size_pct = v.clamp(60, 160);
                    }
                }
                "haptics" => prefs.haptics = value.trim() != "0",
                "left" => prefs.left_handed = value.trim() == "1",
                _ => {}
            }
        }
        prefs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferences_round_trip_through_text() {
        let prefs = TouchPrefs {
            mode: OverlayMode::On,
            opacity_pct: 90,
            size_pct: 130,
            haptics: false,
            left_handed: true,
        };
        assert_eq!(TouchPrefs::from_text(&prefs.to_text()), prefs);
    }

    #[test]
    fn garbage_and_missing_parts_keep_defaults() {
        assert_eq!(TouchPrefs::from_text(""), TouchPrefs::default());
        assert_eq!(
            TouchPrefs::from_text("mode=sideways;opacity=x;nonsense;;size=200"),
            TouchPrefs {
                size_pct: 160,
                ..TouchPrefs::default()
            }
        );
    }

    #[test]
    fn presets_cycle_and_recover_from_unknown_values() {
        let mut prefs = TouchPrefs::default();
        let start = prefs.opacity_pct;
        for _ in 0..OPACITY_PRESETS.len() {
            prefs.cycle_opacity();
        }
        assert_eq!(prefs.opacity_pct, start);
        prefs.size_pct = 77;
        prefs.cycle_size();
        assert_eq!(prefs.size_pct, SIZE_PRESETS[0]);
        assert_eq!(OverlayMode::Off.next(), OverlayMode::Auto);
    }
}
