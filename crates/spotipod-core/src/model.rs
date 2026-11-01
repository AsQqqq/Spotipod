//! UI-facing types, decoupled from librespot.

use std::time::Instant;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Track {
    pub id: Option<String>,
    pub uri: String,
    pub name: String,
    pub artists: Vec<String>,
    pub album: String,
    pub duration_ms: u32,
    pub cover_url: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Playlist {
    pub id: String,
    pub uri: String,
    pub name: String,
    pub owner: String,
    pub track_count: u32,
    pub cover_url: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Album {
    pub id: String,
    pub uri: String,
    pub name: String,
    pub artists: Vec<String>,
    pub cover_url: Option<String>,
}

/// One page of a paginated list.
#[derive(Debug, Clone)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub offset: u32,
    pub total: u32,
}

impl<T> Page<T> {
    pub fn has_more(&self) -> bool {
        self.offset + (self.items.len() as u32) < self.total
    }
}

/// A Spotify Connect device of the account (SpotiPod itself is not listed).
#[derive(Debug, Clone)]
pub struct ConnectDevice {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub is_active: bool,
    pub volume_percent: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Repeat {
    #[default]
    #[serde(rename = "off")]
    Off,
    /// "All": the whole playlist / album.
    #[serde(rename = "all")]
    Context,
    /// "One": the current song.
    #[serde(rename = "one")]
    Track,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    #[default]
    Stopped,
    Loading,
    Playing,
    Paused,
}

/// Snapshot of the controlled Spotify app's playback.
#[derive(Debug, Clone, Default)]
pub struct PlaybackState {
    pub status: Status,
    pub track: Option<Track>,
    /// Position at `position_at`.
    pub position_ms: u32,
    pub position_at: Option<Instant>,
    /// 0..=100
    pub volume: u8,
    pub repeat: Repeat,
    /// Device that plays.
    pub device: Option<String>,
}

impl PlaybackState {
    /// Position extrapolated to now while playing.
    pub fn current_position_ms(&self) -> u32 {
        match (self.status, self.position_at) {
            (Status::Playing, Some(at)) => {
                let pos = self.position_ms as u128 + at.elapsed().as_millis();
                let max = self.track.as_ref().map_or(u32::MAX, |t| t.duration_ms) as u128;
                pos.min(max) as u32
            }
            _ => self.position_ms,
        }
    }
}

/// Events pushed to the UI layer.
#[derive(Debug, Clone)]
pub enum Event {
    /// Playback state changed; read it with `Spotipod::state`.
    StateChanged,
    /// Account devices changed.
    DevicesChanged(Vec<ConnectDevice>),
    /// The local Spotify app is not running and is being started.
    SpotifyLaunching,
    /// A Spotify app to control is available.
    SpotifyReady(String),
    /// No Spotify app to control.
    SpotifyMissing,
    /// A playback command failed.
    CommandFailed(String),
}
