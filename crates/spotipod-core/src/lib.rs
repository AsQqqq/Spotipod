//! SpotiPod core. UI-agnostic: any front end (test GUI now, iPod UI later) drives `Spotipod`.
//!
//! SpotiPod is a remote: the official Spotify app plays, SpotiPod browses the library and
//! controls it over Spotify Connect.

mod app;
pub mod auth;
mod cache;
pub mod config;
mod connect;
mod engine;
pub mod error;
pub mod library;
pub mod model;
pub mod output;
mod playback;
mod proto;
pub mod settings;

use librespot::core::authentication::Credentials;
use tokio::sync::broadcast;

pub use config::{Config, Paths};
pub use error::{CoreError, Result};
pub use library::Library;
pub use model::*;
pub use output::OutputDevice;
pub use settings::{About, Backlight, Settings, Shuffle};

use engine::Engine;
use playback::{PlayOptions, PlayRequest, Router};

pub struct Spotipod {
    config: Config,
    paths: Paths,
    events: broadcast::Sender<Event>,
    engine: Option<Engine>,
}

impl Spotipod {
    pub fn new(config: Config, paths: Paths) -> Self {
        let (events, _) = broadcast::channel(64);
        Self {
            config,
            paths,
            events,
            engine: None,
        }
    }

    /// Loads config from platform default paths.
    pub fn from_default_paths() -> Result<Self> {
        let paths = Paths::platform_default()?;
        let config = Config::load(&paths)?;
        Ok(Self::new(config, paths))
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Playback of the active device; when nothing plays, an idle state naming the
    /// device commands would go to.
    pub fn state(&self) -> PlaybackState {
        let Some(engine) = &self.engine else {
            return PlaybackState::default();
        };
        let mut state = engine.connect.player().unwrap_or_else(|| {
            let target = engine.router.current_target();
            PlaybackState {
                volume: target
                    .as_ref()
                    .and_then(|d| d.volume_percent)
                    .unwrap_or(0)
                    .min(100) as u8,
                device: target.map(|d| d.name),
                ..Default::default()
            }
        });
        engine.router.apply_pending(&mut state);
        state
    }

    // ---- auth / lifecycle ----

    /// Credentials are cached, so `start` works without a full login.
    pub fn has_cached_credentials(&self) -> bool {
        auth::cached_credentials(&self.paths).is_some()
    }

    pub fn is_running(&self) -> bool {
        self.engine.as_ref().is_some_and(Engine::is_alive)
    }

    /// OAuth login in the browser, then connects.
    pub async fn login(&mut self, open_browser: bool) -> Result<()> {
        let credentials = auth::login_oauth(open_browser).await?;
        self.start_with(credentials).await
    }

    /// Connects with cached credentials.
    pub async fn start(&mut self) -> Result<()> {
        let credentials = auth::cached_credentials(&self.paths).ok_or(CoreError::NotLoggedIn)?;
        self.start_with(credentials).await
    }

    async fn start_with(&mut self, credentials: Credentials) -> Result<()> {
        self.stop().await;
        let engine =
            Engine::start(&self.config, &self.paths, credentials, self.events.clone()).await?;
        self.engine = Some(engine);
        Ok(())
    }

    /// Reconnects to apply config changes. No-op when stopped.
    pub async fn restart(&mut self) -> Result<()> {
        if self.engine.is_some() {
            self.start().await?;
        }
        Ok(())
    }

    pub async fn stop(&mut self) {
        if let Some(engine) = self.engine.take() {
            engine.shutdown().await;
        }
    }

    pub async fn logout(&mut self) -> Result<()> {
        self.stop().await;
        auth::logout(&self.paths)
    }

    pub fn username(&self) -> Option<String> {
        self.engine.as_ref().map(|e| e.session.username())
    }

    /// Account plan from the session ("premium", "free", ...); arrives shortly after connect.
    pub fn account_type(&self) -> Option<String> {
        self.engine.as_ref()?.session.get_user_attribute("type")
    }

    fn engine(&self) -> Result<&Engine> {
        self.engine.as_ref().ok_or(CoreError::NotRunning)
    }

    /// Playlists, albums and liked songs, cached on disk (see `Library::is_refreshing`).
    /// Cheap to clone; clones share the cache.
    pub fn library(&self) -> Result<Library> {
        Ok(self.engine()?.library.clone())
    }

    /// Spotify devices of the account.
    pub fn devices(&self) -> Result<Vec<ConnectDevice>> {
        Ok(self.engine()?.connect.devices())
    }

    // ---- playback ----
    // Commands run in the background against the controlled Spotify app (launched if
    // needed); failures arrive as `Event::CommandFailed`.

    fn route<Fut>(&self, f: impl FnOnce(Router) -> Fut) -> Result<()>
    where
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.engine()?.router.spawn(f);
        Ok(())
    }

    /// Plays a context (playlist / album / artist / collection), optionally from a given track.
    pub fn play_context(&self, context_uri: &str, start_track_uri: Option<&str>) -> Result<()> {
        let req = PlayRequest::Context {
            uri: context_uri.to_owned(),
            start: start_track_uri.map(str::to_owned),
        };
        self.route(|r| async move { r.play(req).await })
    }

    pub fn play_tracks(&self, track_uris: Vec<String>, start_index: u32) -> Result<()> {
        let req = PlayRequest::Tracks {
            uris: track_uris,
            index: start_index,
        };
        self.route(|r| async move { r.play(req).await })
    }

    /// Context URI of the user's Liked Songs.
    pub fn liked_songs_uri(&self) -> Result<String> {
        Ok(format!(
            "spotify:user:{}:collection",
            self.engine()?.session.username()
        ))
    }

    pub fn play(&self) -> Result<()> {
        self.route(|r| async move { r.resume().await })
    }

    pub fn pause(&self) -> Result<()> {
        self.route(|r| async move { r.pause().await })
    }

    pub fn play_pause(&self) -> Result<()> {
        self.route(|r| async move { r.play_pause().await })
    }

    pub fn next(&self) -> Result<()> {
        self.route(|r| async move { r.next().await })
    }

    pub fn prev(&self) -> Result<()> {
        self.route(|r| async move { r.prev().await })
    }

    /// Shown immediately in `state()`; rapid calls (slider drag) collapse into the latest.
    pub fn seek(&self, position_ms: u32) -> Result<()> {
        self.engine()?.router.request_seek(position_ms);
        Ok(())
    }

    /// 0..=100. Shown immediately in `state()`; rapid calls collapse into the latest.
    pub fn set_volume(&self, percent: u8) -> Result<()> {
        self.engine()?.router.request_volume(percent);
        Ok(())
    }

    pub fn volume_up(&self) -> Result<()> {
        let current = self.state().volume;
        self.engine()?.router.request_volume_step(current, true);
        Ok(())
    }

    pub fn volume_down(&self) -> Result<()> {
        let current = self.state().volume;
        self.engine()?.router.request_volume_step(current, false);
        Ok(())
    }

    // ---- settings (iPod Settings menu) ----

    pub fn settings(&self) -> Settings {
        self.config.settings
    }

    /// Saves settings; shuffle / repeat are pushed to Spotify right away.
    pub fn set_settings(&mut self, settings: Settings) -> Result<()> {
        let old = self.config.settings;
        self.config.settings = settings;
        self.config.save(&self.paths)?;
        if (old.shuffle, old.repeat) != (settings.shuffle, settings.repeat)
            && let Some(engine) = &self.engine
        {
            engine
                .router
                .set_options(PlayOptions::from_settings(&settings));
            engine
                .router
                .spawn(|r| async move { r.apply_options().await });
        }
        Ok(())
    }

    /// Settings > Reset All Settings.
    pub fn reset_settings(&mut self) -> Result<()> {
        self.set_settings(Settings::default())
    }

    /// Settings > About. The returned future is independent of `self`.
    pub fn about(&self) -> Result<impl Future<Output = Result<About>> + Send + use<>> {
        let library = self.library()?;
        Ok(async move {
            Ok(About {
                songs: library.liked_count().await?,
                company: settings::COMPANY,
                author: settings::AUTHOR,
                version: settings::VERSION,
            })
        })
    }

    // ---- audio output ----

    /// System output devices, default first.
    pub fn output_devices(&self) -> Result<Vec<OutputDevice>> {
        output::list()
    }

    /// Switches the system default output; the Spotify app follows it live.
    pub fn set_output_device(&self, id: &str) -> Result<()> {
        output::set_default(id)
    }

    /// Replaces config, persists it and reconnects.
    pub async fn update_config(&mut self, config: Config) -> Result<()> {
        self.config = config;
        self.config.save(&self.paths)?;
        self.restart().await
    }
}
