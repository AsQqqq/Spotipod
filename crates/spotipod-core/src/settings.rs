//! iPod Settings menu (2nd gen): values, defaults, About info.

use serde::{Deserialize, Serialize};

use crate::model::Repeat;

pub const COMPANY: &str = "trade-cloud";
pub const AUTHOR: &str = "AsQqqq";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Shuffle {
    #[default]
    Off,
    Songs,
}

/// Backlight timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backlight {
    #[default]
    Off,
    Sec1,
    Sec2,
    Sec3,
    Sec5,
    AlwaysOn,
}

impl Backlight {
    /// Menu order.
    pub const ALL: [Self; 6] = [
        Self::Off,
        Self::Sec1,
        Self::Sec2,
        Self::Sec3,
        Self::Sec5,
        Self::AlwaysOn,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Sec1 => "1 Second",
            Self::Sec2 => "2 Seconds",
            Self::Sec3 => "3 Seconds",
            Self::Sec5 => "5 Seconds",
            Self::AlwaysOn => "Always On",
        }
    }

    /// Seconds before the backlight turns off; None = never on (Off) or never off (Always On).
    pub fn seconds(self) -> Option<u32> {
        match self {
            Self::Sec1 => Some(1),
            Self::Sec2 => Some(2),
            Self::Sec3 => Some(3),
            Self::Sec5 => Some(5),
            Self::Off | Self::AlwaysOn => None,
        }
    }
}

/// Light / Dark appearance. `Auto` follows the operating system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    #[default]
    Auto,
    Light,
    Dark,
}

/// iPod body / Click Wheel color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Shell {
    #[default]
    Silver,
    Black,
    Blue,
    Pink,
    Green,
    Gold,
}

/// Screen (LCD) tint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Screen {
    #[default]
    Classic,
    Blue,
    Amber,
    Green,
}

/// Selection / progress highlight color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Accent {
    #[default]
    Graphite,
    Blue,
    Red,
    Green,
    Pink,
    Orange,
}

/// Persisted in config.toml under `[settings]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub shuffle: Shuffle,
    /// Off / One (track) / All (context).
    pub repeat: Repeat,
    pub backlight: Backlight,
    /// Click Wheel click sound.
    pub clicker: bool,
    /// Click Wheel click sound volume (0-100%).
    pub clicker_volume: u8,
    /// Appearance > Theme (Light / Dark / Auto).
    pub theme_mode: ThemeMode,
    /// Appearance > Color (iPod body / wheel).
    pub shell: Shell,
    /// Appearance > Screen (LCD tint).
    pub screen: Screen,
    /// Appearance > Accent (selection / progress color).
    pub accent: Accent,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            shuffle: Shuffle::Off,
            repeat: Repeat::Off,
            backlight: Backlight::Off,
            clicker: true,
            clicker_volume: 100,
            theme_mode: ThemeMode::Auto,
            shell: Shell::Silver,
            screen: Screen::Classic,
            accent: Accent::Graphite,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Accent, Screen, Settings, Shell, ThemeMode};

    #[test]
    fn older_settings_default_clicker_volume() {
        let settings: Settings = serde_json::from_str(r#"{"clicker":false}"#).unwrap();

        assert!(!settings.clicker);
        assert_eq!(settings.clicker_volume, 100);
    }

    #[test]
    fn older_settings_default_appearance() {
        // A config saved before the Appearance options existed.
        let settings: Settings = serde_json::from_str(r#"{"clicker":true}"#).unwrap();

        assert_eq!(settings.theme_mode, ThemeMode::Auto);
        assert_eq!(settings.shell, Shell::Silver);
        assert_eq!(settings.screen, Screen::Classic);
        assert_eq!(settings.accent, Accent::Graphite);
    }

    #[test]
    fn appearance_round_trips_through_json() {
        let settings = Settings {
            shell: Shell::Black,
            screen: Screen::Amber,
            accent: Accent::Blue,
            theme_mode: ThemeMode::Dark,
            ..Settings::default()
        };

        let json = serde_json::to_string(&settings).unwrap();
        assert!(json.contains(r#""shell":"black""#));
        assert!(json.contains(r#""screen":"amber""#));
        assert!(json.contains(r#""accent":"blue""#));
        assert!(json.contains(r#""theme_mode":"dark""#));

        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, settings);
    }
}

/// Settings > About.
#[derive(Debug, Clone)]
pub struct About {
    /// Liked songs in the user's library.
    pub songs: u32,
    pub company: &'static str,
    pub author: &'static str,
    pub version: &'static str,
}
