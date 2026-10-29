//! Spotify Connect as a remote: SpotiPod registers a hidden, non-playing Connect member,
//! mirrors the account's devices and the active player from cluster updates, and sends
//! commands to the device that plays. No Web API: the same channel official clients use.

use std::{
    sync::{Arc, RwLock},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use futures_util::StreamExt;
use http::{HeaderMap, HeaderValue, Method, header::CONTENT_TYPE};
use librespot::{
    core::{dealer::protocol::Message, session::Session, spotify_uri::SpotifyUri},
    protocol::{
        connect::{
            Capabilities, Cluster as ProtoCluster, ClusterUpdate, Device, DeviceInfo, MemberType,
            PutStateReason, PutStateRequest,
        },
        devices::DeviceType,
        player::PlayerState,
    },
};
use protobuf::{EnumOrUnknown, Message as _, MessageField};
use tokio::{sync::broadcast, task::JoinHandle};

use crate::{
    error::Result,
    library::resolve_track,
    model::{ConnectDevice, Event, PlaybackState, Repeat, Status, Track},
};

/// Position differences below this are treated as clock jitter.
const JITTER_MS: u32 = 1000;

#[derive(Default)]
struct View {
    /// Set once the first cluster arrived.
    ready: bool,
    devices: Vec<ConnectDevice>,
    /// State of the active device, if any.
    player: Option<PlaybackState>,
}

#[derive(Clone)]
pub(crate) struct Connect {
    view: Arc<RwLock<View>>,
    session: Session,
}

impl Connect {
    /// Must be called before the session connects so the connection id and the first
    /// cluster update are caught.
    pub fn start(
        session: &Session,
        device_name: String,
        events: broadcast::Sender<Event>,
    ) -> Result<(Self, JoinHandle<()>)> {
        let mut connection_ids =
            session
                .dealer()
                .listen_for("hm://pusher/v1/connections/", |msg: Message| {
                    msg.headers
                        .get("Spotify-Connection-Id")
                        .cloned()
                        .ok_or_else(|| librespot::core::Error::invalid_argument("no connection id"))
                })?;
        let mut updates = session.dealer().listen_for(
            "hm://connect-state/v1/cluster",
            Message::from_raw::<ClusterUpdate>,
        )?;
        let this = Self {
            view: Arc::new(RwLock::new(View::default())),
            session: session.clone(),
        };

        let worker = this.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    // Sent on every dealer (re)connect: (re)register and take the full cluster.
                    id = connection_ids.next() => match id {
                        Some(Ok(id)) => match worker.register(&id, &device_name).await {
                            Ok(cluster) => worker.apply(&cluster, &events),
                            Err(e) => log::warn!("connect registration failed: {e}"),
                        },
                        Some(Err(e)) => log::debug!("bad connection id message: {e}"),
                        None => break,
                    },
                    update = updates.next() => match update {
                        Some(Ok(u)) => worker.apply(&u.cluster, &events),
                        Some(Err(e)) => log::debug!("cluster update parse failed: {e}"),
                        None => break,
                    },
                }
            }
        });
        Ok((this, task))
    }

    async fn register(&self, connection_id: &str, name: &str) -> Result<ProtoCluster> {
        self.session.set_connection_id(connection_id);
        let raw = self
            .session
            .spclient()
            .put_connect_state_request(&hello(&self.session, name))
            .await?;
        log::debug!("registered as hidden Connect member");
        Ok(ProtoCluster::parse_from_bytes(&raw).unwrap_or_default())
    }

    /// Removes our Connect member (best effort).
    pub async fn unregister(&self) {
        if let Err(e) = self.session.spclient().delete_connect_state_request().await {
            log::debug!("connect unregister failed: {e}");
        }
    }

    fn apply(&self, c: &ProtoCluster, events: &broadcast::Sender<Event>) {
        let own_id = self.session.device_id();
        let mut devices: Vec<ConnectDevice> = c
            .device
            .values()
            .filter(|d| d.device_id != own_id)
            .map(|d| ConnectDevice {
                id: d.device_id.clone(),
                name: d.name.clone(),
                kind: format!("{:?}", d.device_type.enum_value_or_default()),
                is_active: d.device_id == c.active_device_id,
                volume_percent: Some(d.volume * 100 / u16::MAX as u32),
            })
            .collect();
        devices.sort_by(|a, b| a.name.cmp(&b.name));

        let active = devices.iter().find(|d| d.is_active);
        let (player, resolve) = match active {
            Some(dev) => {
                let prev = self.view.read().unwrap().player.clone();
                let (state, resolve) = player_state(&c.player_state, dev, prev.as_ref());
                (Some(state), resolve)
            }
            None => (None, None),
        };
        *self.view.write().unwrap() = View {
            ready: true,
            devices: devices.clone(),
            player,
        };
        let _ = events.send(Event::DevicesChanged(devices));
        let _ = events.send(Event::StateChanged);
        if let Some(uri) = resolve {
            tokio::spawn(fill_track(
                self.session.clone(),
                self.view.clone(),
                events.clone(),
                uri,
            ));
        }
    }

    /// True once the first cluster arrived.
    pub fn is_ready(&self) -> bool {
        self.view.read().unwrap().ready
    }

    /// Other devices of the account.
    pub fn devices(&self) -> Vec<ConnectDevice> {
        self.view.read().unwrap().devices.clone()
    }

    /// State of the active device, if one is active.
    pub fn player(&self) -> Option<PlaybackState> {
        self.view.read().unwrap().player.clone()
    }

    /// Device to control: the active one, else the Spotify app picked by `preferred` name,
    /// else this computer's app (`computer` name), else the first computer.
    pub fn target(&self, preferred: Option<&str>, computer: Option<&str>) -> Option<ConnectDevice> {
        let v = self.view.read().unwrap();
        let by_name = |name: Option<&str>| {
            name.and_then(|n| v.devices.iter().find(|d| d.name.eq_ignore_ascii_case(n)))
        };
        v.devices
            .iter()
            .find(|d| d.is_active)
            .or_else(|| by_name(preferred))
            .or_else(|| by_name(computer))
            .or_else(|| v.devices.iter().find(|d| d.kind == "COMPUTER"))
            .cloned()
    }

    /// Sends a player command (`{"command": {"endpoint": ...}}`) to a device.
    pub async fn command(&self, to: &str, body: &serde_json::Value) -> Result<()> {
        let endpoint = format!(
            "/connect-state/v1/player/command/from/{}/to/{to}",
            self.session.device_id()
        );
        self.send_json(Method::POST, &endpoint, body).await
    }

    /// Sets a device's volume (0..=65535).
    pub async fn set_volume(&self, to: &str, volume: u16) -> Result<()> {
        let endpoint = format!(
            "/connect-state/v1/connect/volume/from/{}/to/{to}",
            self.session.device_id()
        );
        self.send_json(
            Method::PUT,
            &endpoint,
            &serde_json::json!({ "volume": volume }),
        )
        .await
    }

    async fn send_json(
        &self,
        method: Method,
        endpoint: &str,
        body: &serde_json::Value,
    ) -> Result<()> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let body = body.to_string();
        self.session
            .spclient()
            .request(&method, endpoint, Some(headers), Some(body.as_bytes()))
            .await?;
        Ok(())
    }
}

/// Connect member that observes and sends commands but never plays or shows up as a target.
fn hello(session: &Session, name: &str) -> PutStateRequest {
    let info = DeviceInfo {
        can_play: false,
        name: name.to_owned(),
        device_id: session.device_id().to_owned(),
        device_type: EnumOrUnknown::new(DeviceType::COMPUTER),
        device_software_version: env!("CARGO_PKG_VERSION").into(),
        spirc_version: librespot::core::version::SPOTIFY_SPIRC_VERSION.into(),
        client_id: session.client_id(),
        capabilities: MessageField::some(Capabilities {
            can_be_player: false,
            hidden: true,
            is_observable: true,
            is_controllable: false,
            gaia_eq_connect_id: true,
            needs_full_player_state: true,
            supports_gzip_pushes: true,
            supports_command_request: true,
            volume_steps: 64,
            ..Default::default()
        }),
        ..Default::default()
    };
    PutStateRequest {
        member_type: EnumOrUnknown::new(MemberType::CONNECT_STATE),
        put_state_reason: EnumOrUnknown::new(PutStateReason::NEW_DEVICE),
        device: MessageField::some(Device {
            device_info: MessageField::some(info),
            player_state: MessageField::some(PlayerState {
                session_id: session.session_id(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Builds the active player's state. Returns the track URI to resolve when the track changed.
fn player_state(
    ps: &PlayerState,
    device: &ConnectDevice,
    prev: Option<&PlaybackState>,
) -> (PlaybackState, Option<String>) {
    let uri = ps.track.uri.clone();
    let duration_ms = ps.duration.max(0) as u32;
    let known = prev
        .and_then(|p| p.track.as_ref())
        .filter(|t| !uri.is_empty() && t.uri == uri)
        .cloned();
    let resolve = (known.is_none() && !uri.is_empty()).then(|| uri.clone());
    let track = known.or_else(|| {
        (!uri.is_empty()).then(|| {
            let meta = &ps.track.metadata;
            Track {
                id: uri.rsplit(':').next().map(str::to_owned),
                name: meta.get("title").cloned().unwrap_or_else(|| uri.clone()),
                artists: meta.get("artist_name").cloned().into_iter().collect(),
                album: meta.get("album_title").cloned().unwrap_or_default(),
                duration_ms,
                cover_url: None,
                uri,
            }
        })
    });

    let status = match (ps.is_playing, ps.is_paused, ps.is_buffering) {
        (true, false, true) => Status::Loading,
        (true, false, false) => Status::Playing,
        (true, true, _) => Status::Paused,
        _ => Status::Stopped,
    };
    // Position is reported as of `timestamp`; extrapolate to now while playing.
    let mut position = ps.position_as_of_timestamp.max(0);
    if status == Status::Playing {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        position += (now_ms - ps.timestamp).max(0);
    }
    let max = if duration_ms > 0 {
        duration_ms
    } else {
        u32::MAX
    };
    let mut position_ms = (position.min(u32::MAX as i64) as u32).min(max);
    let mut position_at = Instant::now();
    // Unrelated cluster updates re-report the position with clock jitter: keep the running
    // clock unless the device really moved (seek, stall).
    if let Some(p) = prev.filter(|p| {
        status == Status::Playing
            && p.status == Status::Playing
            && p.track.as_ref().is_some_and(|t| t.uri == ps.track.uri)
            && p.current_position_ms().abs_diff(position_ms) < JITTER_MS
    }) && let Some(at) = p.position_at
    {
        position_ms = p.position_ms;
        position_at = at;
    }

    let options = &ps.options;
    let state = PlaybackState {
        status,
        track,
        position_ms,
        position_at: Some(position_at),
        volume: device.volume_percent.unwrap_or(0).min(100) as u8,
        repeat: match (options.repeating_context, options.repeating_track) {
            (_, true) => Repeat::Track,
            (true, false) => Repeat::Context,
            _ => Repeat::Off,
        },
        device: Some(device.name.clone()),
    };
    (state, resolve)
}

/// Replaces the placeholder track with full metadata, if it is still the current one.
async fn fill_track(
    session: Session,
    view: Arc<RwLock<View>>,
    events: broadcast::Sender<Event>,
    uri: String,
) {
    let Ok(parsed) = SpotifyUri::from_uri(&uri) else {
        return;
    };
    let Some(track) = resolve_track(&session, parsed).await else {
        return;
    };
    {
        let mut v = view.write().unwrap();
        let Some(player) = v.player.as_mut() else {
            return;
        };
        if player.track.as_ref().is_none_or(|t| t.uri != uri) {
            return;
        }
        player.track = Some(track);
    }
    let _ = events.send(Event::StateChanged);
}
