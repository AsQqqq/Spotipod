//! Playback commands for the controlled Spotify app (SpotiPod never plays audio itself).
//! The target is the active device, else the Spotify app on this computer, which is
//! launched when it is not running.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use serde_json::{Value, json};
use tokio::{
    sync::{broadcast, watch},
    task::JoinHandle,
    time::Instant,
};

use crate::{
    app,
    connect::Connect,
    error::{CoreError, Result},
    model::{ConnectDevice, Event, PlaybackState, Repeat, Status},
    settings::{Settings, Shuffle},
};

const VOLUME_STEP: u8 = 5;
/// How long to wait for the first cluster after connecting.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a freshly launched Spotify app may take to show up.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(500);
/// How long a requested seek / volume is shown before the device confirms it.
const PENDING_TTL: Duration = Duration::from_secs(3);
/// A confirmed value is still shown this long, so echoes of earlier requests from the same
/// drag can't make the slider flicker.
const PENDING_SETTLE: Duration = Duration::from_millis(700);
/// Reported position within this distance of the requested one counts as confirmed.
const SEEK_TOLERANCE_MS: u32 = 1500;

/// Requested but not yet confirmed values, shown instead of the device's stale ones.
#[derive(Default)]
struct Pending {
    /// Position, request time, track it applies to.
    seek: Option<(u32, Instant, Option<String>)>,
    volume: Option<(u8, Instant)>,
}

#[derive(Debug, Clone)]
pub(crate) enum PlayRequest {
    Context { uri: String, start: Option<String> },
    Tracks { uris: Vec<String>, index: u32 },
}

impl PlayRequest {
    fn to_command(&self, opts: PlayOptions) -> Value {
        let (context, skip_to) = match self {
            Self::Context { uri, start } => (
                json!({ "uri": uri, "url": format!("context://{uri}"), "metadata": {} }),
                start
                    .as_ref()
                    .map_or(json!({}), |t| json!({ "track_uri": t })),
            ),
            Self::Tracks { uris, index } => (
                json!({
                    "metadata": {},
                    "pages": [{ "tracks": uris.iter().map(|u| json!({ "uri": u })).collect::<Vec<_>>() }],
                }),
                json!({ "track_index": index }),
            ),
        };
        command(
            "play",
            json!({
                "context": context,
                "play_origin": { "feature_identifier": "spotipod" },
                "options": { "skip_to": skip_to, "player_options_override": opts.to_json() },
            }),
        )
    }
}

/// Shuffle / repeat from Settings, applied to every play and on change.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PlayOptions {
    shuffle: bool,
    repeat: Repeat,
}

impl PlayOptions {
    pub fn from_settings(s: &Settings) -> Self {
        Self {
            shuffle: s.shuffle == Shuffle::Songs,
            repeat: s.repeat,
        }
    }

    /// (repeating_context, repeating_track)
    fn repeat_flags(self) -> (bool, bool) {
        match self.repeat {
            Repeat::Off => (false, false),
            Repeat::Context => (true, false),
            Repeat::Track => (false, true),
        }
    }

    fn to_json(self) -> Value {
        let (context, track) = self.repeat_flags();
        json!({
            "shuffling_context": self.shuffle,
            "repeating_context": context,
            "repeating_track": track,
        })
    }
}

/// `{"command": {"endpoint": .., "logging_params": {}, ..fields}}`
fn command(endpoint: &str, fields: Value) -> Value {
    let mut cmd = json!({ "endpoint": endpoint, "logging_params": {} });
    if let (Some(cmd), Value::Object(fields)) = (cmd.as_object_mut(), fields) {
        cmd.extend(fields);
    }
    json!({ "command": cmd })
}

#[derive(Clone)]
pub(crate) struct Router {
    connect: Connect,
    /// Preferred Spotify device name from config.
    preferred: Option<String>,
    launch: bool,
    computer: Option<String>,
    events: broadcast::Sender<Event>,
    pending: Arc<Mutex<Pending>>,
    options: Arc<Mutex<PlayOptions>>,
    // Latest requested value wins: a slider drag sends one request per idle network slot.
    seek_tx: Arc<watch::Sender<Option<u32>>>,
    volume_tx: Arc<watch::Sender<Option<u8>>>,
}

impl Router {
    /// Returns the router and its background senders (abort them on shutdown).
    pub fn new(
        connect: Connect,
        preferred: Option<String>,
        launch: bool,
        events: broadcast::Sender<Event>,
    ) -> (Self, Vec<JoinHandle<()>>) {
        let (seek_tx, seek_rx) = watch::channel(None);
        let (volume_tx, volume_rx) = watch::channel(None);
        let router = Self {
            connect,
            preferred,
            launch,
            computer: app::computer_name(),
            events,
            pending: Arc::default(),
            options: Arc::default(),
            seek_tx: Arc::new(seek_tx),
            volume_tx: Arc::new(volume_tx),
        };
        let tasks = vec![
            router.latest(seek_rx, |r, ms| async move { r.seek(ms).await }),
            router.latest(volume_rx, |r, v| async move { r.set_volume(v).await }),
        ];
        (router, tasks)
    }

    /// Sends the newest value of `rx`, skipping values superseded while a send was running.
    fn latest<T, Fut>(
        &self,
        mut rx: watch::Receiver<Option<T>>,
        send: impl Fn(Router, T) -> Fut + Send + 'static,
    ) -> JoinHandle<()>
    where
        T: Copy + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send,
    {
        let router = self.clone();
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let Some(value) = *rx.borrow_and_update() else {
                    continue;
                };
                if let Err(e) = send(router.clone(), value).await {
                    log::warn!("command failed: {e}");
                    let _ = router.events.send(Event::CommandFailed(e.to_string()));
                }
            }
        })
    }

    /// Seeks without blocking; repeated calls collapse into the latest position.
    pub fn request_seek(&self, position_ms: u32) {
        let track = self.connect.player().and_then(|s| s.track).map(|t| t.uri);
        self.pending.lock().unwrap().seek = Some((position_ms, Instant::now(), track));
        self.seek_tx.send_replace(Some(position_ms));
        let _ = self.events.send(Event::StateChanged);
    }

    /// Sets volume (0..=100) without blocking; repeated calls collapse into the latest.
    pub fn request_volume(&self, percent: u8) {
        let percent = percent.min(100);
        self.pending.lock().unwrap().volume = Some((percent, Instant::now()));
        self.volume_tx.send_replace(Some(percent));
        let _ = self.events.send(Event::StateChanged);
    }

    pub fn request_volume_step(&self, current: u8, up: bool) {
        let v = if up {
            current.saturating_add(VOLUME_STEP)
        } else {
            current.saturating_sub(VOLUME_STEP)
        };
        self.request_volume(v);
    }

    /// Shows requested seek / volume until the device reports them (or they expire),
    /// so sliders don't jump back while the command is in flight.
    pub fn apply_pending(&self, state: &mut PlaybackState) {
        let mut p = self.pending.lock().unwrap();
        if let Some((target, at, track)) = p.seek.clone() {
            let elapsed = if state.status == Status::Playing {
                at.elapsed().as_millis() as u32
            } else {
                0
            };
            let expected = target.saturating_add(elapsed);
            let confirmed = state.current_position_ms().abs_diff(expected) < SEEK_TOLERANCE_MS;
            let same_track = state.track.as_ref().map(|t| &t.uri) == track.as_ref();
            if !same_track
                || (confirmed && at.elapsed() > PENDING_SETTLE)
                || at.elapsed() > PENDING_TTL
            {
                p.seek = None;
            } else {
                state.position_ms = target;
                state.position_at = Some(at.into_std());
            }
        }
        if let Some((volume, at)) = p.volume {
            let confirmed = state.volume == volume;
            if (confirmed && at.elapsed() > PENDING_SETTLE) || at.elapsed() > PENDING_TTL {
                p.volume = None;
            } else {
                state.volume = volume;
            }
        }
    }

    /// Runs a command in the background; failures are reported as `Event::CommandFailed`.
    pub fn spawn<Fut>(&self, f: impl FnOnce(Router) -> Fut)
    where
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let fut = f(self.clone());
        let events = self.events.clone();
        tokio::spawn(async move {
            if let Err(e) = fut.await {
                log::warn!("command failed: {e}");
                let _ = events.send(Event::CommandFailed(e.to_string()));
            }
        });
    }

    /// Device commands go to right now, without launching anything.
    pub fn current_target(&self) -> Option<ConnectDevice> {
        self.connect
            .target(self.preferred.as_deref(), self.computer.as_deref())
    }

    /// Makes sure a Spotify app is reachable, launching the local one if needed.
    pub async fn ensure_app(&self) -> Result<ConnectDevice> {
        let deadline = Instant::now() + REGISTER_TIMEOUT;
        while !self.connect.is_ready() && Instant::now() < deadline {
            tokio::time::sleep(POLL).await;
        }
        if let Some(dev) = self.current_target() {
            let _ = self.events.send(Event::SpotifyReady(dev.name.clone()));
            return Ok(dev);
        }
        if !self.launch {
            let _ = self.events.send(Event::SpotifyMissing);
            return Err(CoreError::Playback("Spotify is not running".into()));
        }

        log::info!("Spotify app not found, launching it");
        let _ = self.events.send(Event::SpotifyLaunching);
        app::launch()?;
        let deadline = Instant::now() + LAUNCH_TIMEOUT;
        while Instant::now() < deadline {
            if let Some(dev) = self.current_target() {
                let _ = self.events.send(Event::SpotifyReady(dev.name.clone()));
                return Ok(dev);
            }
            tokio::time::sleep(POLL).await;
        }
        let _ = self.events.send(Event::SpotifyMissing);
        Err(CoreError::Playback(
            "Spotify did not appear: is it installed and logged in to the same account?".into(),
        ))
    }

    async fn target_id(&self) -> Result<String> {
        match self.current_target() {
            Some(dev) => Ok(dev.id),
            None => Ok(self.ensure_app().await?.id),
        }
    }

    async fn send(&self, body: Value) -> Result<()> {
        let id = self.target_id().await?;
        self.connect.command(&id, &body).await
    }

    // ---- commands ----

    /// A seek belongs to the old track: drop it (shown or still queued) on track change.
    fn cancel_seek(&self) {
        self.pending.lock().unwrap().seek = None;
        self.seek_tx.send_replace(None);
    }

    pub async fn play(&self, req: PlayRequest) -> Result<()> {
        self.cancel_seek();
        let opts = *self.options.lock().unwrap();
        self.send(req.to_command(opts)).await
    }

    pub fn set_options(&self, opts: PlayOptions) {
        *self.options.lock().unwrap() = opts;
    }

    /// Pushes Settings' shuffle / repeat to the device.
    pub async fn apply_options(&self) -> Result<()> {
        let opts = *self.options.lock().unwrap();
        let (context, track) = opts.repeat_flags();
        let id = self.target_id().await?;
        for (endpoint, value) in [
            ("set_shuffling_context", opts.shuffle),
            ("set_repeating_context", context),
            ("set_repeating_track", track),
        ] {
            self.connect
                .command(&id, &command(endpoint, json!({ "value": value })))
                .await?;
        }
        Ok(())
    }

    pub async fn resume(&self) -> Result<()> {
        self.send(command("resume", json!({}))).await
    }

    pub async fn pause(&self) -> Result<()> {
        self.send(command("pause", json!({}))).await
    }

    pub async fn play_pause(&self) -> Result<()> {
        match self.connect.player() {
            Some(s) if s.status == Status::Playing => self.pause().await,
            _ => self.resume().await,
        }
    }

    pub async fn next(&self) -> Result<()> {
        self.cancel_seek();
        self.send(command("skip_next", json!({}))).await
    }

    pub async fn prev(&self) -> Result<()> {
        self.cancel_seek();
        self.send(command("skip_prev", json!({}))).await
    }

    async fn seek(&self, position_ms: u32) -> Result<()> {
        self.send(command("seek_to", json!({ "value": position_ms })))
            .await
    }

    /// 0..=100
    async fn set_volume(&self, percent: u8) -> Result<()> {
        let id = self.target_id().await?;
        let volume = (percent.min(100) as u32 * u16::MAX as u32 / 100) as u16;
        self.connect.set_volume(&id, volume).await
    }
}
