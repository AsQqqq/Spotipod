//! On-disk library cache: playlists, albums, track lists and track metadata, so a launch
//! needs no library requests. Every `REFRESH_EVERY`-th launch re-reads everything from
//! Spotify (picks up what changed while SpotiPod was closed) and drops what is gone.

use std::{collections::HashMap, fs, path::Path};

use serde::{Deserialize, Serialize};

use crate::model::{Album, Playlist, Track};

/// Full re-check every N launches.
pub const REFRESH_EVERY: u32 = 2;
/// Bump when the format changes; older files are ignored.
const FORMAT: u32 = 1;

#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct LibraryCache {
    format: u32,
    /// Account the data belongs to.
    pub username: String,
    /// Launches since the last full refresh.
    pub launches: u32,
    pub playlists: Option<Vec<Playlist>>,
    pub albums: Option<Vec<Album>>,
    /// Track URIs by context URI (playlist / album / liked songs).
    pub lists: HashMap<String, Vec<String>>,
    /// Track metadata by URI.
    pub tracks: HashMap<String, Track>,
}

impl LibraryCache {
    /// Cache for `username`; empty when missing, unreadable, outdated or another account's.
    pub fn load(path: &Path, username: &str) -> Self {
        let cache = fs::read(path)
            .ok()
            .and_then(|raw| serde_json::from_slice::<Self>(&raw).ok())
            .filter(|c| c.format == FORMAT && c.username == username);
        match cache {
            Some(c) => c,
            None => Self {
                format: FORMAT,
                username: username.to_owned(),
                ..Default::default()
            },
        }
    }

    pub fn is_empty(&self) -> bool {
        self.playlists.is_none() && self.lists.is_empty()
    }

    /// Drops track metadata no cached list refers to anymore.
    pub fn prune(&mut self) {
        let used: std::collections::HashSet<&String> = self.lists.values().flatten().collect();
        self.tracks.retain(|uri, _| used.contains(uri));
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let raw = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        // Write-then-rename so a crash never leaves a half-written cache.
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, raw)?;
        fs::rename(tmp, path)
    }
}
