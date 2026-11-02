//! Owns `Spotipod` on the tokio runtime. The iPod page sends `Cmd`s (JSON over IPC) and
//! receives JSON messages: `state`, `library` and `list`.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

#[cfg(target_os = "macos")]
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};

use serde::Deserialize;
use serde_json::{Value, json};
use spotipod_core::{
    About, Album, Event, Library, OutputDevice, PlaybackState, Playlist, Settings, Spotipod,
    Status, Track,
};
use tokio::sync::mpsc;

/// Tracks resolved per batch; the page asks for the next one while scrolling.
const BATCH: usize = 200;
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const RELEASE_API: &str = "https://api.github.com/repos/AsQqqq/SpotiPOD/releases/latest";
const RELEASE_ASSET_PREFIX: &str = "https://github.com/AsQqqq/SpotiPOD/releases/download/";

#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Cmd {
    /// Page loaded: resend everything.
    Ready,
    Login,
    Logout,
    /// Show a list (playlist / album / liked songs context URI).
    Open {
        context: String,
        name: String,
    },
    /// Load the next batch of a list.
    More {
        context: String,
    },
    /// Play a loaded track of a list.
    Play {
        context: String,
        index: usize,
    },
    PlayPause,
    Next,
    Prev,
    Seek {
        ms: u32,
    },
    Volume {
        value: u8,
    },
    SetSettings {
        settings: Settings,
    },
    SetUiScale {
        scale: f64,
    },
    ResetSettings,
    RefreshOutputs,
    /// System output device id.
    SetOutput {
        id: String,
    },
    CheckForUpdates,
    InstallUpdate {
        version: String,
    },
    /// Window closing: stop cleanly.
    #[serde(skip)]
    Quit,
}

/// Startup order: loading screen -> auth -> preload -> menu.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Phase {
    #[default]
    Starting,
    NeedLogin,
    Authorizing,
    Preparing,
    Ready,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::NeedLogin => "need_login",
            Self::Authorizing => "authorizing",
            Self::Preparing => "preparing",
            Self::Ready => "ready",
        }
    }
}

#[derive(Default)]
struct Data {
    phase: Phase,
    /// Loading screen line.
    progress: String,
    /// Last status / error line.
    status: String,
    playback: PlaybackState,
    outputs: Vec<OutputDevice>,
    settings: Settings,
    ui_scale: f64,
    about: Option<About>,
    playlists: Vec<Playlist>,
    albums: Vec<Album>,
    liked: Option<String>,
    /// Track lists by context URI.
    lists: HashMap<String, TrackList>,
    library: Option<Library>,
    update_version: Option<String>,
    update_url: Option<String>,
    update_digest: Option<String>,
    update_status: String,
    update_message: Option<String>,
}

struct TrackList {
    name: String,
    uris: Vec<String>,
    tracks: Vec<Track>,
    /// URIs before this index are resolved (or skipped as unavailable).
    next: usize,
    loading: bool,
}

impl TrackList {
    fn can_load_more(&self) -> bool {
        !self.loading && self.next < self.uris.len()
    }

    /// `list` message with tracks from `offset`.
    fn message(&self, context: &str, offset: usize) -> Value {
        json!({
            "type": "list",
            "context": context,
            "name": self.name,
            "total": self.uris.len(),
            "offset": offset,
            "done": self.next >= self.uris.len(),
            "tracks": self.tracks[offset.min(self.tracks.len())..]
                .iter()
                .map(track_json)
                .collect::<Vec<_>>(),
        })
    }
}

/// Delivers a JSON message to the page.
pub type Sink = Box<dyn Fn(String) + Send + Sync>;

/// Starts the runtime thread. The returned receiver yields once `Spotipod` has stopped.
pub fn spawn(sink: Sink) -> (mpsc::UnboundedSender<Cmd>, std::sync::mpsc::Receiver<()>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        rt.block_on(run(rx, sink));
        _ = done_tx.send(());
    });
    (tx, done_rx)
}

struct Ctx {
    data: Mutex<Data>,
    sink: Sink,
}

impl Ctx {
    fn send(&self, msg: Value) {
        (self.sink)(msg.to_string());
    }

    /// Mutates the data and pushes the new `state`.
    fn update(&self, f: impl FnOnce(&mut Data)) {
        let msg = {
            let mut d = self.data.lock().unwrap();
            f(&mut d);
            state_json(&d)
        };
        self.send(msg);
    }

    fn status(&self, msg: impl Into<String>) {
        let msg = msg.into();
        log::info!("{msg}");
        self.update(|d| d.status = msg);
    }

    fn report<T, E: std::fmt::Display>(&self, r: Result<T, E>) -> Option<T> {
        r.map_err(|e| self.status(format!("error: {e}"))).ok()
    }

    fn progress(&self, msg: impl Into<String>) {
        let msg = msg.into();
        self.update(|d| d.progress = msg);
    }

    fn send_library(&self) {
        let msg = library_json(&self.data.lock().unwrap());
        self.send(msg);
    }
}

async fn run(mut rx: mpsc::UnboundedReceiver<Cmd>, sink: Sink) {
    let ctx = Arc::new(Ctx {
        data: Mutex::default(),
        sink,
    });
    let mut sp = match Spotipod::from_default_paths() {
        Ok(sp) => sp,
        Err(e) => {
            ctx.status(format!("init failed: {e}"));
            return;
        }
    };
    let mut events = sp.subscribe();
    let settings = sp.settings();
    let ui_scale = sp.ui_scale();
    ctx.update(|d| {
        d.settings = settings;
        d.ui_scale = ui_scale;
    });

    // The loading screen is already up; now auth.
    if sp.has_cached_credentials() {
        ctx.update(|d| {
            d.phase = Phase::Authorizing;
            d.progress = "Connecting".into();
        });
        if ctx.report(sp.start().await).is_some() {
            on_ready(&ctx, &mut sp).await;
        } else {
            need_login(&ctx);
        }
    } else {
        ctx.update(|d| d.phase = Phase::NeedLogin);
    }

    loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                Some(Cmd::Quit) | None => break,
                Some(cmd) => handle(&ctx, &mut sp, cmd).await,
            },
            ev = events.recv() => {
                match ev {
                    Ok(Event::SpotifyLaunching) => ctx.status("Starting Spotify"),
                    Ok(Event::SpotifyReady(dev)) => ctx.status(format!("Spotify on {dev}")),
                    Ok(Event::SpotifyMissing) => ctx.status("error: Spotify app not found"),
                    Ok(Event::CommandFailed(e)) => ctx.status(format!("error: {e}")),
                    _ => {}
                }
                let mut state = sp.state();

                // Если у текущего трека имя неполное или техническое, ищем его в наших локальных списках
                if let Some(ref mut track) = state.track {
                    let uri = track.uri.clone();
                    if track.name.is_empty() || track.name.starts_with("spotify:") || track.name == "Loading..." || track.name.starts_with("Track ") {
                        let d = ctx.data.lock().unwrap();
                        let mut found_track = None;

                        for list in d.lists.values() {
                            if let Some(t) = list.tracks.iter().find(|t| t.uri == uri) {
                                found_track = Some(t.clone());
                                break;
                            }
                        }

                        if let Some(t) = found_track {
                            track.name = t.name;
                            track.artists = t.artists;
                            track.album = t.album;
                            track.cover_url = t.cover_url;
                        }
                    }
                }

                ctx.update(|d| d.playback = state);
            }
        }
    }
    sp.stop().await;
}

/// Auth done: preload behind the loading screen, then show the menu.
async fn on_ready(ctx: &Arc<Ctx>, sp: &mut Spotipod) {
    ctx.update(|d| d.phase = Phase::Preparing);
    refresh_outputs(ctx, sp);
    if let Some(library) = ctx.report(sp.library()) {
        ctx.update(|d| d.library = Some(library.clone()));
        preload(ctx, sp, &library).await;
    }
    let state = sp.state();
    ctx.update(|d| {
        d.playback = state;
        d.phase = Phase::Ready;
    });
}

/// Back to the login item, showing why (the last status, e.g. an error).
fn need_login(ctx: &Ctx) {
    ctx.update(|d| {
        d.phase = Phase::NeedLogin;
        d.progress = d.status.clone();
    });
}

/// All track URIs of a list plus its first batch.
async fn fetch_list(
    library: &Library,
    name: String,
    context: &str,
) -> spotipod_core::Result<TrackList> {
    let uris = library.track_uris(context).await?;
    let next = uris.len().min(BATCH);
    let tracks = library.resolve(&uris[..next]).await;
    Ok(TrackList {
        name,
        uris,
        tracks,
        next,
        loading: false,
    })
}

/// Playlist / album lists, About, and the first batch of Liked Songs and of every playlist
/// and album. Failures are reported and skipped.
async fn preload(ctx: &Ctx, sp: &Spotipod, library: &Library) {
    // Full re-check launch: say so on the loading screen.
    let check = if library.is_refreshing() {
        "Checking "
    } else {
        ""
    };
    ctx.progress(format!("{check}Playlists"));
    if let Some(list) = ctx.report(library.playlists().await) {
        ctx.data.lock().unwrap().playlists = list;
    }
    ctx.progress(format!("{check}Albums"));
    if let Some(list) = ctx.report(library.saved_albums().await) {
        ctx.data.lock().unwrap().albums = list;
    }

    ctx.progress(format!("{check}Songs"));
    if let Some(liked) = ctx.report(sp.liked_songs_uri()) {
        ctx.data.lock().unwrap().liked = Some(liked.clone());
        if let Ok(count) = library.liked_count().await {
            ctx.update(|d| {
                d.about = Some(About {
                    songs: count,
                    company: spotipod_core::settings::COMPANY,
                    author: spotipod_core::settings::AUTHOR,
                    version: spotipod_core::settings::VERSION,
                });
            });
        }
    }

    let lists: Vec<(String, String)> = {
        let d = ctx.data.lock().unwrap();
        d.playlists
            .iter()
            .map(|p| (p.name.clone(), p.uri.clone()))
            .chain(d.albums.iter().map(|a| (a.name.clone(), a.uri.clone())))
            .collect()
    };
    let total = lists.len();
    for (i, (name, uri)) in lists.into_iter().enumerate() {
        ctx.progress(format!("{check}Playlists {}/{total}", i + 1));
        match fetch_list(library, name, &uri).await {
            Ok(list) => _ = ctx.data.lock().unwrap().lists.insert(uri, list),
            Err(e) => log::warn!("preload {uri} failed: {e}"),
        }
    }
    ctx.send_library();
    save_library(library);
}

/// Writes the library cache in the background.
fn save_library(library: &Library) {
    let library = library.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(e) = library.save() {
            log::warn!("library cache save failed: {e}");
        }
    });
}

fn refresh_outputs(ctx: &Ctx, sp: &Spotipod) {
    if let Some(mut list) = ctx.report(sp.output_devices()) {
        ctx.update(|d| {
            preserve_output_order(&mut list, &d.outputs);
            d.outputs = list;
        });
    }
}

fn preserve_output_order(outputs: &mut [OutputDevice], previous: &[OutputDevice]) {
    outputs.sort_by_key(|device| {
        previous
            .iter()
            .position(|old| old.id == device.id)
            .unwrap_or(usize::MAX)
    });
}

async fn handle(ctx: &Arc<Ctx>, sp: &mut Spotipod, cmd: Cmd) {
    match cmd {
        Cmd::Ready => {
            ctx.update(|_| {});
            ctx.send_library();
            check_for_updates(ctx.clone());
        }
        Cmd::Login => {
            ctx.update(|d| {
                d.phase = Phase::Authorizing;
                d.progress = "Log in in your browser".into();
            });
            if ctx.report(sp.login(true).await).is_some() {
                on_ready(ctx, sp).await;
            } else {
                need_login(ctx);
            }
        }
        Cmd::Logout => {
            ctx.report(sp.logout().await);
            ctx.update(|d| {
                *d = Data {
                    phase: Phase::NeedLogin,
                    outputs: std::mem::take(&mut d.outputs),
                    settings: d.settings,
                    ui_scale: d.ui_scale,
                    ..Default::default()
                };
            });
            ctx.send_library();
        }
        Cmd::Open { context, name } => open_list(ctx, name, context),
        Cmd::More { context } => load_more(ctx, context),
        Cmd::Play { context, index } => {
            let uri = {
                let d = ctx.data.lock().unwrap();
                d.lists
                    .get(&context)
                    .and_then(|l| l.tracks.get(index))
                    .map(|t| t.uri.clone())
            };
            if let Some(uri) = uri {
                ctx.report(sp.play_context(&context, Some(&uri)));
            }
        }
        Cmd::PlayPause => _ = ctx.report(sp.play_pause()),
        Cmd::Next => _ = ctx.report(sp.next()),
        Cmd::Prev => _ = ctx.report(sp.prev()),
        Cmd::Seek { ms } => _ = ctx.report(sp.seek(ms)),
        Cmd::Volume { value } => _ = ctx.report(sp.set_volume(value.min(100))),
        Cmd::SetSettings { settings } => {
            ctx.report(sp.set_settings(settings));
            let s = sp.settings();
            ctx.update(|d| d.settings = s);
        }
        Cmd::SetUiScale { scale } => {
            if ctx.report(sp.set_ui_scale(scale)).is_some() {
                ctx.update(|d| d.ui_scale = sp.ui_scale());
            }
        }
        Cmd::ResetSettings => {
            ctx.report(sp.reset_settings());
            let s = sp.settings();
            ctx.update(|d| d.settings = s);
        }
        Cmd::RefreshOutputs => refresh_outputs(ctx, sp),
        Cmd::SetOutput { id } => {
            ctx.report(sp.set_output_device(&id));
            // The system switches asynchronously.
            tokio::time::sleep(Duration::from_millis(200)).await;
            refresh_outputs(ctx, sp);
        }
        Cmd::CheckForUpdates => check_for_updates(ctx.clone()),
        Cmd::InstallUpdate { version } => install_update(ctx.clone(), version),
        Cmd::Quit => {}
    }
}

#[derive(Clone, Debug)]
struct UpdateRelease {
    version: String,
    url: String,
    digest: Option<String>,
}

fn check_for_updates(ctx: Arc<Ctx>) {
    #[cfg(not(target_os = "macos"))]
    {
        ctx.update(|data| {
            data.update_status = "unsupported".into();
            data.update_message =
                Some("Automatic updates are currently supported on macOS only.".into());
        });
        return;
    }

    #[cfg(target_os = "macos")]
    {
        {
            let mut data = ctx.data.lock().unwrap();
            if data.update_status == "checking" || data.update_status == "installing" {
                return;
            }
            data.update_status = "checking".into();
            data.update_message = None;
        }
        ctx.update(|_| {});

        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(latest_release).await;
            match result {
                Ok(Ok(release)) => {
                    let mut data = ctx.data.lock().unwrap();
                    if let Some(release) = release {
                        data.update_version = Some(release.version);
                        data.update_url = Some(release.url);
                        data.update_digest = release.digest;
                        data.update_status = "available".into();
                        data.update_message = None;
                    } else {
                        data.update_version = None;
                        data.update_url = None;
                        data.update_digest = None;
                        data.update_status = "current".into();
                        data.update_message = None;
                    }
                    let state = state_json(&data);
                    drop(data);
                    ctx.send(state);
                }
                Ok(Err(error)) => {
                    log::warn!("update check failed: {error}");
                    ctx.update(|data| {
                        data.update_status = "error".into();
                        data.update_message = Some(error);
                    });
                }
                Err(error) => {
                    log::warn!("update check task failed: {error}");
                    ctx.update(|data| {
                        data.update_status = "error".into();
                        data.update_message = Some(error.to_string());
                    });
                }
            }
        });
    }
}

#[cfg(target_os = "macos")]
fn latest_release() -> Result<Option<UpdateRelease>, String> {
    let response = Command::new("/usr/bin/curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--max-time",
            "20",
            "--user-agent",
            concat!("SpotiPod/", env!("CARGO_PKG_VERSION")),
            "--header",
            "Accept: application/vnd.github+json",
            RELEASE_API,
        ])
        .output()
        .map_err(|error| format!("could not start GitHub request: {error}"))?;
    if !response.status.success() {
        return Err(format!(
            "GitHub API request failed: {}",
            String::from_utf8_lossy(&response.stderr).trim()
        ));
    }
    let release: Value = serde_json::from_slice(&response.stdout)
        .map_err(|error| format!("invalid GitHub release response: {error}"))?;
    parse_release_response(release)
}

fn parse_release_response(release: Value) -> Result<Option<UpdateRelease>, String> {
    let tag = release["tag_name"]
        .as_str()
        .ok_or("GitHub release has no tag name")?;
    let version = tag.strip_prefix('v').unwrap_or(tag);
    if compare_versions(version, CURRENT_VERSION).is_none_or(|ordering| !ordering.is_gt()) {
        return Ok(None);
    }

    let expected_asset = format!("SpotiPod-{version}-macOS-universal.zip");
    let assets = release["assets"]
        .as_array()
        .ok_or("GitHub release has no assets")?;
    let asset = assets
        .iter()
        .find(|asset| asset["name"].as_str() == Some(&expected_asset))
        .ok_or_else(|| format!("GitHub release is missing {expected_asset}"))?;
    let url = asset["browser_download_url"]
        .as_str()
        .ok_or("GitHub release asset has no download URL")?;
    if !url.starts_with(RELEASE_ASSET_PREFIX) {
        return Err("GitHub release asset URL is not trusted".into());
    }

    Ok(Some(UpdateRelease {
        version: version.into(),
        url: url.into(),
        digest: asset["digest"].as_str().map(str::to_owned),
    }))
}

fn compare_versions(left: &str, right: &str) -> Option<std::cmp::Ordering> {
    fn parts(version: &str) -> Option<[u64; 3]> {
        if version.is_empty()
            || version.len() > 64
            || !version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
        {
            return None;
        }
        let stable = version.split(['-', '+']).next()?;
        let mut numbers = stable.split('.').map(str::parse::<u64>);
        let result = [
            numbers.next()?.ok()?,
            numbers.next()?.ok()?,
            numbers.next()?.ok()?,
        ];
        if numbers.next().is_some() {
            return None;
        }
        Some(result)
    }
    Some(parts(left)?.cmp(&parts(right)?))
}

fn install_update(ctx: Arc<Ctx>, version: String) {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = version;
        ctx.update(|data| {
            data.update_status = "unsupported".into();
            data.update_message =
                Some("Automatic updates are currently supported on macOS only.".into());
        });
        return;
    }

    #[cfg(target_os = "macos")]
    {
        let release = {
            let mut data = ctx.data.lock().unwrap();
            if data.update_status == "installing" {
                return;
            }
            if data.update_version.as_deref() != Some(&version) {
                drop(data);
                ctx.update(|data| {
                    data.update_status = "error".into();
                    data.update_message =
                        Some("The selected update is no longer available. Check again.".into());
                });
                return;
            }
            data.update_status = "installing".into();
            data.update_message = Some("Downloading and verifying the update…".into());
            let Some(url) = data.update_url.clone() else {
                drop(data);
                ctx.update(|data| {
                    data.update_status = "error".into();
                    data.update_message =
                        Some("Update download information is missing. Check again.".into());
                });
                return;
            };
            UpdateRelease {
                version,
                url,
                digest: data.update_digest.clone(),
            }
        };
        ctx.update(|_| {});

        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || stage_update(&release)).await;
            match result {
                Ok(Ok(())) => {
                    ctx.send(json!({ "type": "restart_for_update" }));
                }
                Ok(Err(error)) => {
                    log::warn!("update installation failed: {error}");
                    ctx.update(|data| {
                        data.update_status = "error".into();
                        data.update_message = Some(error);
                    });
                }
                Err(error) => {
                    log::warn!("update installation task failed: {error}");
                    ctx.update(|data| {
                        data.update_status = "error".into();
                        data.update_message = Some(error.to_string());
                    });
                }
            }
        });
    }
}

fn stage_update(release: &UpdateRelease) -> Result<(), String> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = release;
        return Err("automatic updates are currently supported on macOS only".into());
    }

    #[cfg(target_os = "macos")]
    {
        let directory = std::env::temp_dir().join(format!(
            "spotipod-update-{}-{}",
            std::process::id(),
            release.version
        ));
        fs::create_dir(&directory).map_err(|error| {
            format!(
                "could not create update directory {}: {error}",
                directory.display()
            )
        })?;

        let result = stage_update_in(&directory, release);
        if result.is_err() {
            if let Err(error) = fs::remove_dir_all(&directory) {
                log::warn!("could not clean up update directory: {error}");
            }
        }
        result
    }
}

#[cfg(target_os = "macos")]
fn stage_update_in(directory: &Path, release: &UpdateRelease) -> Result<(), String> {
    let archive = directory.join("release.zip");
    let response = Command::new("/usr/bin/curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--max-time",
            "300",
            "--user-agent",
            concat!("SpotiPod/", env!("CARGO_PKG_VERSION")),
            "--output",
        ])
        .arg(&archive)
        .arg(&release.url)
        .output()
        .map_err(|error| format!("could not start release download: {error}"))?;
    if !response.status.success() {
        return Err(format!(
            "release download failed: {}",
            String::from_utf8_lossy(&response.stderr).trim()
        ));
    }

    if let Some(expected_digest) = release.digest.as_deref() {
        verify_sha256(&archive, expected_digest)?;
    }

    let listing = Command::new("/usr/bin/unzip")
        .arg("-Z1")
        .arg(&archive)
        .output()
        .map_err(|error| format!("could not inspect release archive: {error}"))?;
    let listing_succeeded = listing.status.success();
    let listing = String::from_utf8_lossy(&listing.stdout);
    if !listing_succeeded || !valid_app_archive_listing(&listing) {
        return Err("release archive does not contain the expected application binary".into());
    }

    let extracted = directory.join("extracted");
    fs::create_dir(&extracted).map_err(|error| format!("could not prepare archive: {error}"))?;
    let extraction = Command::new("/usr/bin/ditto")
        .args(["-x", "-k"])
        .arg(&archive)
        .arg(&extracted)
        .output()
        .map_err(|error| format!("could not extract release archive: {error}"))?;
    if !extraction.status.success() {
        return Err(format!(
            "release extraction failed: {}",
            String::from_utf8_lossy(&extraction.stderr).trim()
        ));
    }

    let replacement = extracted.join("SpotiPod.app/Contents/MacOS/spotipod-gui");
    if !replacement.is_file() {
        return Err("downloaded application binary is missing".into());
    }
    let executable = std::env::current_exe()
        .map_err(|error| format!("could not locate current application: {error}"))?;
    verify_install_location(&executable)?;
    launch_update_helper(directory, &executable, &replacement)
}

fn valid_app_archive_listing(listing: &str) -> bool {
    let entries: Vec<_> = listing.lines().collect();
    entries
        .iter()
        .any(|entry| *entry == "SpotiPod.app/Contents/MacOS/spotipod-gui")
        && entries.iter().all(|entry| {
            let entry = entry.trim_end_matches('/');
            (entry == "SpotiPod.app" || entry.starts_with("SpotiPod.app/"))
                && !entry
                    .split('/')
                    .any(|component| component.is_empty() || component == "." || component == "..")
        })
}

#[cfg(target_os = "macos")]
fn verify_install_location(executable: &Path) -> Result<(), String> {
    let parent = executable
        .parent()
        .ok_or("current application has no parent directory")?;
    let probe = parent.join(format!(
        ".spotipod-update-write-check-{}",
        std::process::id()
    ));
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .map_err(|error| {
            format!(
                "application directory {} is not writable: {error}",
                parent.display()
            )
        })?;
    fs::remove_file(probe).map_err(|error| format!("could not remove update check file: {error}"))
}

#[cfg(target_os = "macos")]
fn verify_sha256(archive: &Path, expected: &str) -> Result<(), String> {
    let expected = expected
        .strip_prefix("sha256:")
        .ok_or("GitHub release asset has an unsupported digest")?;
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("GitHub release asset has an invalid SHA-256 digest".into());
    }
    let output = Command::new("/usr/bin/shasum")
        .args(["-a", "256"])
        .arg(archive)
        .output()
        .map_err(|error| format!("could not verify update checksum: {error}"))?;
    if !output.status.success() {
        return Err("could not calculate update checksum".into());
    }
    let actual = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if actual != expected.to_ascii_lowercase() {
        return Err("downloaded update checksum does not match GitHub".into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn launch_update_helper(
    directory: &Path,
    executable: &Path,
    replacement: &Path,
) -> Result<(), String> {
    let helper = directory.join("install-update.sh");
    let script = r#"#!/bin/sh
set -eu
pid="$1"
target="$2"
replacement="$3"
temporary="${target}.spotipod-update.$$"
while kill -0 "$pid" 2>/dev/null; do sleep 1; done
cp "$replacement" "$temporary"
chmod 755 "$temporary"
mv -f "$temporary" "$target"
"$target" >/dev/null 2>&1 &
"#;
    fs::write(&helper, script).map_err(|error| format!("could not prepare installer: {error}"))?;
    Command::new("/bin/sh")
        .arg(&helper)
        .arg(std::process::id().to_string())
        .arg(executable)
        .arg(replacement)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not start update installer: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        compare_versions, parse_release_response, preserve_output_order, valid_app_archive_listing,
    };
    use spotipod_core::OutputDevice;

    #[test]
    fn output_refresh_keeps_existing_order_and_appends_new_devices() {
        let previous = vec![
            OutputDevice {
                id: "a".into(),
                name: "A".into(),
                is_default: true,
            },
            OutputDevice {
                id: "b".into(),
                name: "B".into(),
                is_default: false,
            },
        ];
        let mut refreshed = vec![
            OutputDevice {
                id: "b".into(),
                name: "B".into(),
                is_default: true,
            },
            OutputDevice {
                id: "c".into(),
                name: "C".into(),
                is_default: false,
            },
            OutputDevice {
                id: "a".into(),
                name: "A".into(),
                is_default: false,
            },
        ];

        preserve_output_order(&mut refreshed, &previous);

        assert_eq!(
            refreshed
                .iter()
                .map(|device| device.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"],
        );
        assert!(refreshed[1].is_default);
    }

    #[test]
    fn update_version_comparison_uses_numeric_components() {
        assert!(compare_versions("1.10.0", "1.9.9").unwrap().is_gt());
        assert!(compare_versions("0.1.0", "0.1.0").unwrap().is_eq());
        assert!(compare_versions("0.1.0", "0.2.0").unwrap().is_lt());
        assert!(compare_versions("../0.2.0", "0.1.0").is_none());
    }

    #[test]
    fn latest_release_selects_only_a_newer_macos_asset() {
        let release = serde_json::json!({
            "tag_name": "v999.0.0",
            "assets": [{
                "name": "SpotiPod-999.0.0-macOS-universal.zip",
                "browser_download_url": "https://github.com/AsQqqq/SpotiPOD/releases/download/v999.0.0/SpotiPod.zip",
                "digest": "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            }]
        });
        let parsed = parse_release_response(release).unwrap().unwrap();
        assert_eq!(parsed.version, "999.0.0");
        assert_eq!(
            parsed.url,
            "https://github.com/AsQqqq/SpotiPOD/releases/download/v999.0.0/SpotiPod.zip"
        );
        assert!(parsed.digest.is_some());
    }

    #[test]
    fn latest_release_ignores_the_current_version() {
        let release = serde_json::json!({
            "tag_name": "v0.1.0",
            "assets": []
        });
        assert!(parse_release_response(release).unwrap().is_none());
    }

    #[test]
    fn latest_release_rejects_untrusted_asset_urls() {
        let release = serde_json::json!({
            "tag_name": "v999.0.0",
            "assets": [{
                "name": "SpotiPod-999.0.0-macOS-universal.zip",
                "browser_download_url": "https://attacker.invalid/SpotiPod.zip"
            }]
        });
        assert!(parse_release_response(release).is_err());
    }

    #[test]
    fn updater_accepts_the_app_bundle_archive_and_rejects_unexpected_paths() {
        assert!(valid_app_archive_listing(
            "SpotiPod.app/\nSpotiPod.app/Contents/\nSpotiPod.app/Contents/MacOS/spotipod-gui\n"
        ));
        assert!(!valid_app_archive_listing(
            "SpotiPod.app/Contents/MacOS/spotipod-gui\n../outside\n"
        ));
        assert!(!valid_app_archive_listing("spotipod-gui\n"));
    }
}

/// Sends a known list, or fetches its first batch (lists added after startup).
fn open_list(ctx: &Arc<Ctx>, name: String, context: String) {
    let library = {
        let d = ctx.data.lock().unwrap();
        if let Some(list) = d.lists.get(&context) {
            let msg = list.message(&context, 0);
            drop(d);
            ctx.send(msg);
            return;
        }
        d.library.clone()
    };
    let Some(library) = library else { return };
    let ctx = ctx.clone();
    tokio::spawn(async move {
        if let Some(list) = ctx.report(fetch_list(&library, name, &context).await) {
            let msg = list.message(&context, 0);
            ctx.data.lock().unwrap().lists.insert(context, list);
            ctx.send(msg);
        }
    });
}

/// Resolves the next batch of a list and sends it.
fn load_more(ctx: &Arc<Ctx>, context: String) {
    let (library, batch) = {
        let mut d = ctx.data.lock().unwrap();
        let Some(library) = d.library.clone() else {
            return;
        };
        let Some(list) = d.lists.get_mut(&context).filter(|l| l.can_load_more()) else {
            return;
        };
        let end = (list.next + BATCH).min(list.uris.len());
        let batch = list.uris[list.next..end].to_vec();
        list.next = end;
        list.loading = true;
        (library, batch)
    };
    let ctx = ctx.clone();
    tokio::spawn(async move {
        let tracks = library.resolve(&batch).await;
        save_library(&library);
        let msg = {
            let mut d = ctx.data.lock().unwrap();
            let Some(list) = d.lists.get_mut(&context) else {
                return;
            };
            let offset = list.tracks.len();
            list.tracks.extend(tracks);
            list.loading = false;
            list.message(&context, offset)
        };
        ctx.send(msg);
    });
}

fn track_json(t: &Track) -> Value {
    let name = if t.name.is_empty() {
        "Unknown Track".to_string()
    } else if t.name.starts_with("spotify:") {
        "Loading...".to_string()
    } else {
        t.name.clone()
    };

    let artist = if t.artists.is_empty() {
        "Spotify".to_string()
    } else {
        t.artists.join(", ")
    };

    json!({
        "uri": t.uri,
        "name": name,
        "artist": artist,
        "album": t.album,
        "duration_ms": t.duration_ms,
    })
}

fn state_json(d: &Data) -> Value {
    let p = &d.playback;
    let status = match p.status {
        Status::Stopped => "stopped",
        Status::Loading => "loading",
        Status::Playing => "playing",
        Status::Paused => "paused",
    };
    json!({
        "type": "state",
        "phase": d.phase.name(),
        "progress": d.progress,
        "status": d.status,
        "update": {
            "current_version": CURRENT_VERSION,
            "version": d.update_version,
            "available": d.update_version.is_some(),
            "status": d.update_status,
            "message": d.update_message,
        },
        "settings": d.settings,
        "ui_scale": d.ui_scale,
        "about": d.about.as_ref().map(|a| json!({
            "songs": a.songs,
            "company": a.company,
            "author": a.author,
            "version": a.version,
        })),
        "outputs": d.outputs.iter().map(|o| json!({
            "id": o.id,
            "name": o.name,
            "is_default": o.is_default,
        })).collect::<Vec<_>>(),
        "playback": {
            "status": status,
            "track": p.track.as_ref().map(track_json),
            "position_ms": p.current_position_ms(),
            "volume": p.volume,
            "device": p.device,
        },
    })
}

fn library_json(d: &Data) -> Value {
    json!({
        "type": "library",
        "liked": d.liked,
        "playlists": d.playlists.iter().map(|p| json!({
            "uri": p.uri,
            "name": p.name,
        })).collect::<Vec<_>>(),
        "albums": d.albums.iter().map(|a| json!({
            "uri": a.uri,
            "name": a.name,
            "artist": a.artists.join(", "),
        })).collect::<Vec<_>>(),
    })
}
