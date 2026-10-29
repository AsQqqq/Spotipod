use std::{fs, path::PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::{
    error::{CoreError, Result},
    settings::Settings,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Name of SpotiPod's (hidden) Connect member.
    pub device_name: String,
    /// Spotify device (name) to control when nothing is active; None = this computer's app.
    pub spotify_device: Option<String>,
    /// Start the Spotify app when it is not running.
    pub launch_spotify: bool,
    /// iPod Settings menu.
    pub settings: Settings,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device_name: "SpotiPod".into(),
            spotify_device: None,
            launch_spotify: true,
            settings: Settings::default(),
        }
    }
}

/// On-disk locations used by the core.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_file: PathBuf,
    /// librespot credentials.
    pub cache_dir: PathBuf,
}

impl Paths {
    /// Platform-specific default dirs.
    pub fn platform_default() -> Result<Self> {
        let dirs = ProjectDirs::from("org", "spotipod", "SpotiPod")
            .ok_or_else(|| CoreError::Config("cannot resolve home directory".into()))?;
        Ok(Self::with_roots(
            dirs.config_dir().into(),
            dirs.cache_dir().into(),
        ))
    }

    pub fn with_roots(config_dir: PathBuf, cache_dir: PathBuf) -> Self {
        Self {
            config_file: config_dir.join("config.toml"),
            cache_dir,
        }
    }

    pub fn library_cache_file(&self) -> PathBuf {
        self.cache_dir.join("library.json")
    }

    pub fn credentials_file(&self) -> PathBuf {
        self.cache_dir.join("credentials.json")
    }
}

impl Config {
    /// Loads config, falling back to defaults when the file is missing.
    pub fn load(paths: &Paths) -> Result<Self> {
        match fs::read_to_string(&paths.config_file) {
            Ok(raw) => toml::from_str(&raw).map_err(|e| CoreError::Config(e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        if let Some(dir) = paths.config_file.parent() {
            fs::create_dir_all(dir)?;
        }
        let raw = toml::to_string_pretty(self).map_err(|e| CoreError::Config(e.to_string()))?;
        fs::write(&paths.config_file, raw)?;
        Ok(())
    }
}
