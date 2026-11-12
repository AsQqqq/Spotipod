//! Local music: scan a folder for audio files and play them with rodio.
//!
//! SpotiPod is otherwise a Spotify remote (it never decodes audio itself). This
//! module gives it a real local player so it works without Spotify. rodio's audio
//! stream is `!Send`, so the output lives on a dedicated thread driven by a command
//! channel; the handle (`LocalPlayer`) is cheap to hold and `Send + Sync`.

use std::{
    fs::File,
    io::BufReader,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    time::{Duration, Instant},
};

use walkdir::WalkDir;

use crate::model::{PlaybackState, Repeat, Status, Track};

/// File extensions treated as playable audio.
const AUDIO_EXTS: &[&str] = &["mp3", "flac", "wav", "ogg", "m4a", "aac", "opus"];

/// URI scheme identifying a local file, e.g. `local:/Users/me/Music/song.mp3`.
pub const LOCAL_PREFIX: &str = "local:";

/// How often the audio thread wakes to refresh position / detect track end.
const TICK: Duration = Duration::from_millis(200);

pub fn path_to_uri(path: &Path) -> String {
    format!("{LOCAL_PREFIX}{}", path.display())
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    uri.strip_prefix(LOCAL_PREFIX).map(PathBuf::from)
}

pub fn is_local_uri(uri: &str) -> bool {
    uri.starts_with(LOCAL_PREFIX)
}

fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// The user's default music folder (`~/Music`), if the OS exposes one.
pub fn default_dir() -> Option<PathBuf> {
    directories::UserDirs::new().and_then(|d| d.audio_dir().map(Path::to_path_buf))
}

/// Reads tags into a `Track`, falling back to the file name for the title.
fn read_track(path: &Path) -> Track {
    let (title, artist, album, duration_ms) = read_tags(path);
    let name = title.unwrap_or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Unknown")
            .to_string()
    });
    Track {
        id: None,
        uri: path_to_uri(path),
        name,
        artists: artist.into_iter().collect(),
        album: album.unwrap_or_default(),
        duration_ms,
        cover_url: None,
    }
}

/// (title, artist, album, duration_ms) read best-effort; all optional but duration.
fn read_tags(path: &Path) -> (Option<String>, Option<String>, Option<String>, u32) {
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::tag::Accessor;

    match lofty::read_from_path(path) {
        Ok(tagged) => {
            let duration_ms = tagged.properties().duration().as_millis() as u32;
            let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
            let title = tag.and_then(|t| t.title()).map(|c| c.to_string());
            let artist = tag.and_then(|t| t.artist()).map(|c| c.to_string());
            let album = tag.and_then(|t| t.album()).map(|c| c.to_string());
            (title, artist, album, duration_ms)
        }
        Err(_) => (None, None, None, 0),
    }
}

/// Scans `dir` recursively for audio files, reading tags, sorted by path.
pub fn scan(dir: &Path) -> Vec<Track> {
    let mut paths: Vec<PathBuf> = WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .map(walkdir::DirEntry::into_path)
        .filter(|p| p.is_file() && is_audio(p))
        .collect();
    paths.sort();
    paths.iter().map(|p| read_track(p)).collect()
}

/// A local "playlist": an immediate subfolder of the chosen music folder (or the
/// folder itself for files that sit directly in it).
#[derive(Debug, Clone)]
pub struct LocalPlaylist {
    /// Stable id used in a `local:pl:<id>` context (the group folder name).
    pub id: String,
    pub name: String,
    pub tracks: Vec<Track>,
}

/// Scans `root` and groups tracks into playlists by their immediate subfolder.
/// Files directly in `root` are grouped under the root folder's own name.
/// Playlists are sorted by name; tracks within each by path.
pub fn scan_grouped(root: &Path) -> Vec<LocalPlaylist> {
    use std::collections::BTreeMap;

    let root_name = root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("Local Music")
        .to_string();

    let mut paths: Vec<PathBuf> = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .map(walkdir::DirEntry::into_path)
        .filter(|p| p.is_file() && is_audio(p))
        .collect();
    paths.sort();

    let mut groups: BTreeMap<String, Vec<Track>> = BTreeMap::new();
    for path in &paths {
        let rel = path.strip_prefix(root).unwrap_or(path);
        let mut comps = rel.components();
        let first = comps.next();
        // More than one component => the first is a subfolder; otherwise it is a
        // file sitting directly in root.
        let group = match (first, comps.next()) {
            (Some(dir), Some(_)) => dir.as_os_str().to_string_lossy().into_owned(),
            _ => root_name.clone(),
        };
        groups.entry(group).or_default().push(read_track(path));
    }

    groups
        .into_iter()
        .map(|(name, tracks)| LocalPlaylist {
            id: name.clone(),
            name,
            tracks,
        })
        .collect()
}

// ---- Player ----

enum Cmd {
    Play { tracks: Vec<Track>, index: usize },
    PlayPause,
    Pause,
    Resume,
    Next,
    Prev,
    Seek(u32),
    Volume(u8),
    Stop,
}

#[derive(Default)]
struct Shared {
    status: Status,
    current: Option<Track>,
    /// Position at `since` (ms). While playing, add `since.elapsed()`.
    base_ms: u32,
    since: Option<Instant>,
    volume: u8,
}

impl Shared {
    fn position_ms(&self) -> u32 {
        match (self.status, self.since) {
            (Status::Playing, Some(t)) => self.base_ms.saturating_add(t.elapsed().as_millis() as u32),
            _ => self.base_ms,
        }
    }
}

/// Handle to the local audio engine. Cloneable intent is not needed; one per app.
pub struct LocalPlayer {
    tx: Sender<Cmd>,
    shared: Arc<Mutex<Shared>>,
}

impl LocalPlayer {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Shared {
            volume: 100,
            ..Default::default()
        }));
        let thread_shared = shared.clone();
        std::thread::Builder::new()
            .name("spotipod-local-audio".into())
            .spawn(move || audio_thread(rx, thread_shared))
            .expect("spawn local audio thread");
        Self { tx, shared }
    }

    /// Snapshot shaped like the Spotify remote's, so the UI is source-agnostic.
    pub fn state(&self) -> PlaybackState {
        let s = self.shared.lock().unwrap();
        let cap = s.current.as_ref().map_or(u32::MAX, |t| t.duration_ms);
        PlaybackState {
            status: s.status,
            track: s.current.clone(),
            position_ms: s.position_ms().min(cap),
            position_at: if s.status == Status::Playing {
                Some(Instant::now())
            } else {
                None
            },
            volume: s.volume,
            repeat: Repeat::Off,
            device: Some("Local".into()),
        }
    }

    pub fn is_active(&self) -> bool {
        self.shared.lock().unwrap().current.is_some()
    }

    pub fn play(&self, tracks: Vec<Track>, index: usize) {
        let _ = self.tx.send(Cmd::Play { tracks, index });
    }
    pub fn play_pause(&self) {
        let _ = self.tx.send(Cmd::PlayPause);
    }
    pub fn pause(&self) {
        let _ = self.tx.send(Cmd::Pause);
    }
    pub fn resume(&self) {
        let _ = self.tx.send(Cmd::Resume);
    }
    pub fn next(&self) {
        let _ = self.tx.send(Cmd::Next);
    }
    pub fn prev(&self) {
        let _ = self.tx.send(Cmd::Prev);
    }
    pub fn seek(&self, ms: u32) {
        let _ = self.tx.send(Cmd::Seek(ms));
    }
    pub fn set_volume(&self, percent: u8) {
        let _ = self.tx.send(Cmd::Volume(percent.min(100)));
    }
    pub fn stop(&self) {
        let _ = self.tx.send(Cmd::Stop);
    }
}

impl Default for LocalPlayer {
    fn default() -> Self {
        Self::new()
    }
}

/// Owns the rodio stream + sink (both `!Send`) and mutates the shared snapshot.
struct Audio {
    handle: rodio::OutputStreamHandle,
    sink: Option<rodio::Sink>,
    queue: Vec<Track>,
    index: usize,
    shared: Arc<Mutex<Shared>>,
}

impl Audio {
    fn volume(&self) -> u8 {
        self.shared.lock().unwrap().volume
    }

    /// Starts playing `queue[index]`; returns false if it cannot be opened.
    fn start(&mut self, index: usize) -> bool {
        let Some(track) = self.queue.get(index).cloned() else {
            return false;
        };
        let opened = uri_to_path(&track.uri)
            .and_then(|p| File::open(p).ok())
            .and_then(|f| rodio::Decoder::new(BufReader::new(f)).ok());
        let Some(decoder) = opened else {
            log::warn!("local: cannot open {}", track.uri);
            return false;
        };
        let Ok(sink) = rodio::Sink::try_new(&self.handle) else {
            return false;
        };
        sink.set_volume(self.volume() as f32 / 100.0);
        sink.append(decoder);

        self.index = index;
        self.sink = Some(sink);
        let mut s = self.shared.lock().unwrap();
        s.current = Some(track);
        s.base_ms = 0;
        s.since = Some(Instant::now());
        s.status = Status::Playing;
        true
    }

    fn pause(&mut self) {
        if let Some(sink) = &self.sink {
            sink.pause();
        }
        let mut s = self.shared.lock().unwrap();
        if s.status == Status::Playing {
            s.base_ms = s.position_ms();
            s.since = None;
            s.status = Status::Paused;
        }
    }

    fn resume(&mut self) {
        if let Some(sink) = &self.sink {
            sink.play();
        }
        let mut s = self.shared.lock().unwrap();
        if s.status == Status::Paused {
            s.since = Some(Instant::now());
            s.status = Status::Playing;
        }
    }

    fn play_pause(&mut self) {
        let status = self.shared.lock().unwrap().status;
        match status {
            Status::Playing => self.pause(),
            Status::Paused => self.resume(),
            _ => {}
        }
    }

    fn next(&mut self) {
        let next = self.index + 1;
        if next < self.queue.len() {
            self.start(next);
        } else {
            self.stop();
        }
    }

    fn prev(&mut self) {
        // Restart the current track if we are past its start, else go back one.
        if self.shared.lock().unwrap().position_ms() > 3000 || self.index == 0 {
            self.start(self.index);
        } else {
            self.start(self.index - 1);
        }
    }

    fn seek(&mut self, ms: u32) {
        if let Some(sink) = &self.sink {
            let _ = sink.try_seek(Duration::from_millis(ms as u64));
        }
        let mut s = self.shared.lock().unwrap();
        s.base_ms = ms;
        s.since = (s.status == Status::Playing).then(Instant::now);
    }

    fn set_volume(&mut self, percent: u8) {
        if let Some(sink) = &self.sink {
            sink.set_volume(percent as f32 / 100.0);
        }
        self.shared.lock().unwrap().volume = percent;
    }

    fn stop(&mut self) {
        if let Some(sink) = self.sink.take() {
            sink.stop();
        }
        let mut s = self.shared.lock().unwrap();
        s.current = None;
        s.base_ms = 0;
        s.since = None;
        s.status = Status::Stopped;
    }

    /// Auto-advance when the current track reaches its end.
    fn tick(&mut self) {
        let playing = self.shared.lock().unwrap().status == Status::Playing;
        let ended = self.sink.as_ref().is_some_and(rodio::Sink::empty);
        if playing && ended {
            self.next();
        }
    }
}

fn audio_thread(rx: Receiver<Cmd>, shared: Arc<Mutex<Shared>>) {
    let (_stream, handle) = match rodio::OutputStream::try_default() {
        Ok(v) => v,
        Err(e) => {
            log::warn!("local: no audio output: {e}");
            return;
        }
    };
    let mut audio = Audio {
        handle,
        sink: None,
        queue: Vec::new(),
        index: 0,
        shared,
    };

    loop {
        match rx.recv_timeout(TICK) {
            Ok(Cmd::Play { tracks, index }) => {
                audio.queue = tracks;
                audio.start(index.min(audio.queue.len().saturating_sub(1)));
            }
            Ok(Cmd::PlayPause) => audio.play_pause(),
            Ok(Cmd::Pause) => audio.pause(),
            Ok(Cmd::Resume) => audio.resume(),
            Ok(Cmd::Next) => audio.next(),
            Ok(Cmd::Prev) => audio.prev(),
            Ok(Cmd::Seek(ms)) => audio.seek(ms),
            Ok(Cmd::Volume(v)) => audio.set_volume(v),
            Ok(Cmd::Stop) => audio.stop(),
            Err(RecvTimeoutError::Timeout) => audio.tick(),
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_round_trips() {
        let p = Path::new("/Users/me/Music/song.mp3");
        let uri = path_to_uri(p);
        assert_eq!(uri, "local:/Users/me/Music/song.mp3");
        assert!(is_local_uri(&uri));
        assert_eq!(uri_to_path(&uri), Some(p.to_path_buf()));
        assert!(!is_local_uri("spotify:track:123"));
    }

    #[test]
    fn audio_extension_filter() {
        assert!(is_audio(Path::new("a/b.MP3")));
        assert!(is_audio(Path::new("x.flac")));
        assert!(!is_audio(Path::new("cover.jpg")));
        assert!(!is_audio(Path::new("notes.txt")));
    }
}
