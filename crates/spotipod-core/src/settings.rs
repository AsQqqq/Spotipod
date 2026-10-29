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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            shuffle: Shuffle::Off,
            repeat: Repeat::Off,
            backlight: Backlight::Off,
            clicker: true,
        }
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
