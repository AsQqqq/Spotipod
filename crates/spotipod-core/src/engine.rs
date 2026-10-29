//! Logged-in session + hidden Connect member + command router.

use librespot::core::{authentication::Credentials, config::SessionConfig, session::Session};
use tokio::{sync::broadcast, task::JoinHandle};

use crate::{
    auth,
    config::{Config, Paths},
    connect::Connect,
    error::Result,
    library::Library,
    model::Event,
    playback::{PlayOptions, Router},
};

pub(crate) struct Engine {
    pub session: Session,
    pub connect: Connect,
    pub router: Router,
    pub library: Library,
    connect_task: JoinHandle<()>,
    app_task: JoinHandle<()>,
    router_tasks: Vec<JoinHandle<()>>,
}

impl Engine {
    pub async fn start(
        config: &Config,
        paths: &Paths,
        credentials: Credentials,
        events: broadcast::Sender<Event>,
    ) -> Result<Self> {
        let session = Session::new(SessionConfig::default(), Some(auth::open_cache(paths)?));
        let (connect, connect_task) =
            Connect::start(&session, config.device_name.clone(), events.clone())?;

        let connected = async {
            session.connect(credentials, true).await?;
            session.dealer().start().await?;
            Ok::<_, librespot::core::Error>(())
        };
        if let Err(e) = connected.await {
            connect_task.abort();
            session.shutdown();
            return Err(e.into());
        }

        let (router, router_tasks) = Router::new(
            connect.clone(),
            config.spotify_device.clone(),
            config.launch_spotify,
            events,
        );
        router.set_options(PlayOptions::from_settings(&config.settings));
        // Launch the Spotify app right away if it is not running, then push Settings'
        // shuffle / repeat to it; errors go out as events.
        let app_router = router.clone();
        let app_task = tokio::spawn(async move {
            let ready = async {
                app_router.ensure_app().await?;
                app_router.apply_options().await
            };
            if let Err(e) = ready.await {
                log::warn!("{e}");
            }
        });

        let library = Library::open(session.clone(), paths.library_cache_file());

        Ok(Self {
            library,
            session,
            connect,
            router,
            connect_task,
            app_task,
            router_tasks,
        })
    }

    pub fn is_alive(&self) -> bool {
        !self.connect_task.is_finished() && !self.session.is_invalid()
    }

    pub async fn shutdown(self) {
        if let Err(e) = self.library.save() {
            log::warn!("library cache save failed: {e}");
        }
        self.app_task.abort();
        self.router_tasks.iter().for_each(JoinHandle::abort);
        self.connect.unregister().await;
        self.session.dealer().close().await;
        self.session.shutdown();
        self.connect_task.abort();
    }
}
