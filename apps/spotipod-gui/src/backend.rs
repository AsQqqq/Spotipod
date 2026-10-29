//! Owns `Spotipod` on the tokio runtime. The iPod page sends `Cmd`s (JSON over IPC) and
//! receives JSON messages: `state`, `library` and `list`.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
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
    ResetSettings,
    RefreshOutputs,
    /// System output device id.
    SetOutput {
        id: String,
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
    about: Option<About>,
    playlists: Vec<Playlist>,
    albums: Vec<Album>,
    liked: Option<String>,
    /// Track lists by context URI.
    lists: HashMap<String, TrackList>,
    library: Option<Library>,
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
    ctx.update(|d| d.settings = settings);

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
                let state = sp.state();
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
        if let Some(list) = ctx.report(fetch_list(library, "Songs".into(), &liked).await) {
            ctx.update(|d| {
                d.about = Some(About {
                    songs: list.uris.len() as u32,
                    company: spotipod_core::settings::COMPANY,
                    author: spotipod_core::settings::AUTHOR,
                    version: spotipod_core::settings::VERSION,
                });
                d.lists.insert(liked, list);
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
    if let Some(list) = ctx.report(sp.output_devices()) {
        ctx.update(|d| d.outputs = list);
    }
}

async fn handle(ctx: &Arc<Ctx>, sp: &mut Spotipod, cmd: Cmd) {
    match cmd {
        Cmd::Ready => {
            ctx.update(|_| {});
            ctx.send_library();
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
        Cmd::Quit => {}
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
    json!({
        "uri": t.uri,
        "name": t.name,
        "artist": t.artists.join(", "),
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
        "settings": d.settings,
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
