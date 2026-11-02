//! Library via Spotify's internal API (the librespot session), like
//! official clients. Only the user's account is needed: no developer app, no Web API.

use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use bytes::Bytes;
use futures_util::{StreamExt, stream};
use librespot::{
    core::{error::ErrorKind, session::Session, spotify_uri::SpotifyUri},
    metadata::{Album as LsAlbum, Metadata, Track as LsTrack, image::Images},
    protocol::{
        context_page::ContextPage,
        extended_metadata::{BatchedEntityRequest, EntityRequest, ExtensionQuery},
        extension_kind::ExtensionKind,
        metadata::Track as TrackProto,
        playlist4_external::SelectedListContent,
    },
};
use protobuf::{EnumOrUnknown, Message};

use crate::{
    cache::{LibraryCache, REFRESH_EVERY},
    error::{CoreError, Result},
    model::{Album, Page, Playlist, Track},
    proto::collection2v2::{PageRequest, PageResponse},
    telemetry,
};

/// Parallel metadata requests.
const CONCURRENCY: usize = 16;
/// Tracks per extended-metadata request.
const METADATA_BATCH: usize = 100;
/// Parallel extended-metadata requests.
const METADATA_BATCH_CONCURRENCY: usize = 4;
/// Collection set holding liked tracks and saved albums.
const COLLECTION_SET: &str = "collection";
const COLLECTION_PAGE: i32 = 300;
/// Extra attempts per track metadata request (backoff 1 s, 2 s, 4 s).
const METADATA_RETRIES: u32 = 3;
/// Playlist items per request.
const PLAYLIST_PAGE: usize = 500;

/// Library access with the on-disk cache in front. Clones share the cache.
#[derive(Clone)]
pub struct Library {
    session: Session,
    cache: Arc<Mutex<LibraryCache>>,
    path: PathBuf,
    /// This launch re-reads everything from Spotify instead of trusting the cache.
    refresh: bool,
}

impl Library {
    /// Loads the cache and decides whether this launch is a full refresh.
    pub(crate) fn open(session: Session, path: PathBuf) -> Self {
        let mut cache = LibraryCache::load(&path, &session.username());
        cache.launches += 1;
        let refresh = cache.is_empty() || cache.launches >= REFRESH_EVERY;
        if refresh {
            // Lists are re-read; track metadata is kept and pruned on save.
            cache.launches = 0;
            cache.playlists = None;
            cache.albums = None;
            cache.lists.clear();
        }
        if let Err(e) = cache.save(&path) {
            log::warn!("library cache save failed: {e}");
        }
        log::info!(
            "library cache: {}",
            if refresh {
                "full refresh"
            } else {
                "using cache"
            }
        );
        Self {
            session,
            cache: Arc::new(Mutex::new(cache)),
            path,
            refresh,
        }
    }

    /// True when this launch re-checks the whole library (first launch, every 2nd launch).
    pub fn is_refreshing(&self) -> bool {
        self.refresh
    }

    /// Writes the cache to disk, dropping metadata of tracks no list contains anymore.
    pub fn save(&self) -> Result<()> {
        let mut cache = self.cache.lock().unwrap();
        cache.prune();
        cache.save(&self.path)?;
        Ok(())
    }

    /// User's playlists (rootlist), folders flattened.
    pub async fn playlists(&self) -> Result<Vec<Playlist>> {
        if let Some(list) = self.cache.lock().unwrap().playlists.clone() {
            return Ok(list);
        }
        let list = self.fetch_playlists().await?;
        self.cache.lock().unwrap().playlists = Some(list.clone());
        Ok(list)
    }

    async fn fetch_playlists(&self) -> Result<Vec<Playlist>> {
        let raw = telemetry::span("rootlist").result(
            self.session.spclient().get_rootlist(0, Some(500)).await,
            |b| (b.len(), 0),
        )?;
        let msg = SelectedListContent::parse_from_bytes(&raw)
            .map_err(|e| CoreError::Library(e.to_string()))?;

        let user = self.session.username();
        let items = &msg.contents.items;
        let metas = &msg.contents.meta_items;
        Ok(items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.uri().starts_with("spotify:playlist:"))
            .map(|(i, item)| {
                let uri = item.uri().to_owned();
                let meta = metas.get(i);
                let owner = meta
                    .map(|m| m.owner_username().to_owned())
                    .unwrap_or_default();
                Playlist {
                    id: uri.rsplit(':').next().unwrap_or_default().to_owned(),
                    name: meta
                        .map(|m| m.attributes.name().to_owned())
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| uri.clone()),
                    owner: if owner == user { "you".into() } else { owner },
                    track_count: meta.map_or(0, |m| m.length().max(0) as u32),
                    cover_url: None,
                    uri,
                }
            })
            .collect())
    }

    /// All track URIs of a playlist, album or Liked Songs (`spotify:user:<u>:collection`),
    /// in order. Cheap compared to `resolve`: pair them for progressive loading.
    pub async fn track_uris(&self, context_uri: &str) -> Result<Vec<String>> {
        if let Some(uris) = self.cache.lock().unwrap().lists.get(context_uri).cloned() {
            return Ok(uris);
        }
        let uris = self.fetch_track_uris(context_uri).await?;
        self.cache
            .lock()
            .unwrap()
            .lists
            .insert(context_uri.to_owned(), uris.clone());
        Ok(uris)
    }

    async fn fetch_track_uris(&self, context_uri: &str) -> Result<Vec<String>> {
        // `spotify:user:<u>:collection` is a context, not a parsable item URI.
        if context_uri.ends_with(":collection") {
            let uris = self.context_uris(context_uri).await?;
            return Ok(uris.iter().filter_map(|u| u.to_uri().ok()).collect());
        }
        let uris: Vec<SpotifyUri> = match parse_uri(context_uri)? {
            uri @ SpotifyUri::Playlist { .. } => self.playlist_uris(&uri).await?,
            uri @ SpotifyUri::Album { .. } => telemetry::span("metadata/album")
                .result(LsAlbum::get(&self.session, &uri).await, |a| {
                    (0, a.tracks().count())
                })?
                .tracks()
                .cloned()
                .collect(),
            _ => self.context_uris(context_uri).await?,
        };
        Ok(uris.iter().filter_map(|u| u.to_uri().ok()).collect())
    }

    /// Tracks for `uris`, in order; episodes, local files and unavailable items are skipped.
    pub async fn resolve(&self, uris: &[String]) -> Vec<Track> {
        let uris: Vec<SpotifyUri> = uris
            .iter()
            .filter_map(|u| SpotifyUri::from_uri(u).ok())
            .collect();
        self.resolve_uris(uris).await
    }

    pub async fn playlist_tracks(
        &self,
        playlist_uri: &str,
        offset: u32,
        limit: u32,
    ) -> Result<Page<Track>> {
        self.page_of(playlist_uri, offset, limit).await
    }

    pub async fn album_tracks(
        &self,
        album_uri: &str,
        offset: u32,
        limit: u32,
    ) -> Result<Page<Track>> {
        self.page_of(album_uri, offset, limit).await
    }

    /// Liked Songs.
    pub async fn liked_tracks(&self, offset: u32, limit: u32) -> Result<Page<Track>> {
        self.page_of(&self.liked_uri(), offset, limit).await
    }

    /// Number of liked songs.
    pub async fn liked_count(&self) -> Result<u32> {
        Ok(self.track_uris(&self.liked_uri()).await?.len() as u32)
    }

    fn liked_uri(&self) -> String {
        format!("spotify:user:{}:collection", self.session.username())
    }

    /// All items of a playlist. The playlist service returns a window of the list per
    /// request, so large playlists are read range by range.
    async fn playlist_uris(&self, uri: &SpotifyUri) -> Result<Vec<SpotifyUri>> {
        let id = uri
            .to_id()
            .map_err(|e| CoreError::InvalidArgument(e.to_string()))?;
        let mut out = Vec::new();
        let mut pos = 0usize;
        // Guards against a server ignoring `from` (same window again).
        let mut last_first: Option<String> = None;
        loop {
            let endpoint = format!("/playlist/v2/playlist/{id}?from={pos}&length={PLAYLIST_PAGE}");
            let span = telemetry::span("playlist/v2 window");
            let raw = match self
                .session
                .spclient()
                .request(&http::Method::GET, &endpoint, None, None)
                .await
            {
                Ok(raw) => raw,
                Err(e) => {
                    span.fail(&e);
                    return Err(e.into());
                }
            };
            let msg = SelectedListContent::parse_from_bytes(&raw)
                .map_err(|e| CoreError::Library(e.to_string()))?;
            let items = &msg.contents.items;
            span.ok(raw.len(), items.len());
            // The window may be smaller than asked and `length` may be absent: read until an
            // empty window, stopping early only on a known total.
            let first = items.first().map(|i| i.uri().to_owned());
            if items.is_empty() || first == last_first {
                break;
            }
            last_first = first;
            pos += items.len();
            out.extend(
                items
                    .iter()
                    .filter_map(|i| SpotifyUri::from_uri(i.uri()).ok()),
            );
            let known_total = msg.length().max(0) as usize;
            log::debug!("playlist {id}: {pos} items (total {known_total})");
            if known_total > 0 && pos >= known_total {
                break;
            }
        }
        Ok(out)
    }

    /// Every track of a context, following all its pages in order.
    async fn context_uris(&self, context_uri: &str) -> Result<Vec<SpotifyUri>> {
        let ctx = telemetry::span("context").result(
            self.session.spclient().get_context(context_uri).await,
            |c| (0, c.pages.iter().map(|p| p.tracks.len()).sum()),
        )?;
        let mut uris = Vec::new();
        let mut queue = VecDeque::new();
        for page in &ctx.pages {
            take_page(page, &mut uris, &mut queue);
        }
        while let Some(url) = queue.pop_front() {
            let raw = telemetry::span("context page")
                .result(self.session.spclient().get_next_page(&url).await, |b| {
                    (b.len(), 0)
                })?;
            let json = String::from_utf8_lossy(&raw);
            let page: ContextPage = protobuf_json_mapping::parse_from_str_with_options(
                &json,
                &protobuf_json_mapping::ParseOptions {
                    ignore_unknown_fields: true,
                    ..Default::default()
                },
            )
            .map_err(|e| CoreError::Library(e.to_string()))?;
            take_page(&page, &mut uris, &mut queue);
        }
        Ok(uris)
    }

    async fn page_of(&self, context_uri: &str, offset: u32, limit: u32) -> Result<Page<Track>> {
        let uris = self.track_uris(context_uri).await?;
        let total = uris.len() as u32;
        let end = uris.len().min((offset + limit) as usize);
        let window = uris.get(offset as usize..end).unwrap_or_default();
        Ok(Page {
            items: self.resolve(window).await,
            offset,
            total,
        })
    }

    /// Saved albums, newest first.
    pub async fn saved_albums(&self) -> Result<Vec<Album>> {
        if let Some(list) = self.cache.lock().unwrap().albums.clone() {
            return Ok(list);
        }
        let list = self.fetch_saved_albums().await?;
        self.cache.lock().unwrap().albums = Some(list.clone());
        Ok(list)
    }

    async fn fetch_saved_albums(&self) -> Result<Vec<Album>> {
        let uris: Vec<SpotifyUri> = self
            .collection_uris()
            .await?
            .into_iter()
            .filter(|u| matches!(u, SpotifyUri::Album { .. }))
            .collect();
        Ok(stream::iter(uris)
            .map(|uri| {
                let session = self.session.clone();
                async move { resolve_album(&session, uri).await }
            })
            .buffered(CONCURRENCY)
            .filter_map(|a| async move { a })
            .collect()
            .await)
    }

    /// All URIs in the collection set (tracks and albums mixed).
    async fn collection_uris(&self) -> Result<Vec<SpotifyUri>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut req = PageRequest::new();
            req.username = self.session.username();
            req.set = COLLECTION_SET.into();
            if let Some(ref t) = token {
                req.pagination_token = t.clone();
            }
            req.limit = COLLECTION_PAGE;

            let raw = self
                .collection_request("/collection/v2/paging", &req)
                .await?;
            let page = PageResponse::parse_from_bytes(&raw)
                .map_err(|e| CoreError::Library(e.to_string()))?;
            out.extend(
                page.items
                    .iter()
                    .filter(|i| !i.is_removed)
                    .filter_map(|i| SpotifyUri::from_uri(&i.uri).ok()),
            );
            if page.next_page_token.is_empty() {
                break;
            }
            token = Some(page.next_page_token);
        }
        Ok(out)
    }

    /// POST a collection protobuf. Built by hand: spclient's default requests append
    /// `product` / `country` / `salt` query params, a likely cause of its 400 responses.
    async fn collection_request<M: Message>(&self, endpoint: &str, msg: &M) -> Result<Bytes> {
        use http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
        use http_body_util::BodyExt;
        // The collection service only takes its own media type (400 for x-protobuf).
        const MEDIA_TYPE: &str = "application/vnd.collection-v2.spotify.proto";

        let spclient = self.session.spclient();
        let url = format!("{}{endpoint}", spclient.base_url().await?);
        let body = msg
            .write_to_bytes()
            .map_err(|e| CoreError::Library(e.to_string()))?;
        let token = self.session.login5().auth_token().await?;
        let mut req = http::Request::post(&url)
            .header(CONTENT_TYPE, MEDIA_TYPE)
            .header(ACCEPT, MEDIA_TYPE)
            .header(
                AUTHORIZATION,
                format!("{} {}", token.token_type, token.access_token),
            )
            .body(Bytes::from(body))
            .map_err(|e| CoreError::Library(e.to_string()))?;
        if let Ok(ct) = spclient.client_token().await
            && let Ok(v) = http::HeaderValue::from_str(&ct)
        {
            req.headers_mut().insert("client-token", v);
        }

        let span = telemetry::span(endpoint.trim_start_matches('/'));
        let res = match self.session.http_client().request_fut(req) {
            Ok(fut) => fut.await.map_err(|e| CoreError::Library(e.to_string())),
            Err(e) => Err(e.into()),
        };
        let res = match res {
            Ok(res) => res,
            Err(e) => {
                span.fail(&e);
                return Err(e);
            }
        };
        let status = res.status();
        let bytes = res
            .into_body()
            .collect()
            .await
            .map(|b| b.to_bytes())
            .unwrap_or_default();
        if !status.is_success() {
            // The body says why (field errors), keep it in the log.
            let why = String::from_utf8_lossy(&bytes[..bytes.len().min(300)]).into_owned();
            let err = CoreError::Library(format!("{endpoint}: {status}: {why}"));
            log::error!("{err}");
            span.fail(&err);
            return Err(err);
        }
        span.ok(bytes.len(), 0);
        Ok(bytes)
    }

    /// Cache first; the rest via batched extended-metadata requests, then one by one for
    /// whatever a batch did not return.
    async fn resolve_uris(&self, uris: Vec<SpotifyUri>) -> Vec<Track> {
        let started = Instant::now();
        let keys: Vec<Option<String>> = uris.iter().map(|u| u.to_uri().ok()).collect();
        let missing: Vec<SpotifyUri> = {
            let cache = self.cache.lock().unwrap();
            uris.iter()
                .zip(&keys)
                .filter(|(u, k)| {
                    matches!(u, SpotifyUri::Track { .. })
                        && k.as_ref().is_some_and(|k| !cache.tracks.contains_key(k))
                })
                .map(|(u, _)| u.clone())
                .collect()
        };
        let cached = uris.len() - missing.len();

        let chunks: Vec<Vec<SpotifyUri>> =
            missing.chunks(METADATA_BATCH).map(<[_]>::to_vec).collect();
        let batches: Vec<HashMap<String, Track>> = stream::iter(chunks)
            .map(|chunk| {
                let session = self.session.clone();
                async move { fetch_track_batch(&session, &chunk).await }
            })
            .buffer_unordered(METADATA_BATCH_CONCURRENCY)
            .collect()
            .await;
        let mut fetched: HashMap<String, Track> = batches.into_iter().flatten().collect();

        // Not in a batch answer: single requests with retries.
        let rest: Vec<SpotifyUri> = missing
            .iter()
            .filter(|u| u.to_uri().is_ok_and(|k| !fetched.contains_key(&k)))
            .cloned()
            .collect();
        let singles: Vec<Track> = stream::iter(rest)
            .map(|uri| {
                let session = self.session.clone();
                async move { resolve_track(&session, uri).await }
            })
            .buffer_unordered(CONCURRENCY)
            .filter_map(|t| async move { t })
            .collect()
            .await;
        fetched.extend(singles.into_iter().map(|t| (t.uri.clone(), t)));

        let mut cache = self.cache.lock().unwrap();
        for (k, t) in &fetched {
            cache.tracks.insert(k.clone(), t.clone());
        }
        let out: Vec<Track> = keys
            .iter()
            .flatten()
            .filter_map(|k| cache.tracks.get(k).cloned())
            .collect();
        telemetry::emit(telemetry::Record::Resolve {
            requested: uris.len(),
            cached,
            fetched: fetched.len(),
            failed: missing.len().saturating_sub(fetched.len()),
            ms: started.elapsed().as_millis() as u64,
        });
        out
    }
}

fn parse_uri(s: &str) -> Result<SpotifyUri> {
    SpotifyUri::from_uri(s).map_err(|e| CoreError::InvalidArgument(e.to_string()))
}

/// Collects a page's tracks; queues its lazy body (`page_url`) and the next page.
fn take_page(page: &ContextPage, out: &mut Vec<SpotifyUri>, queue: &mut VecDeque<String>) {
    if page.tracks.is_empty() && !page.page_url().is_empty() {
        queue.push_back(page.page_url().to_owned());
    }
    collect_uris(page, out);
    if !page.next_page_url().is_empty() {
        queue.push_back(page.next_page_url().to_owned());
    }
}

fn collect_uris(page: &ContextPage, out: &mut Vec<SpotifyUri>) {
    out.extend(
        page.tracks
            .iter()
            .filter_map(|t| SpotifyUri::from_uri(t.uri()).ok()),
    );
}

/// Cover art CDN url for the largest image.
fn cover_url(images: &Images) -> Option<String> {
    images
        .iter()
        .max_by_key(|i| i.width)
        .and_then(|i| i.id.to_base16().ok())
        .map(|hex| format!("https://i.scdn.co/image/{hex}"))
}

async fn resolve_album(session: &Session, uri: SpotifyUri) -> Option<Album> {
    let span = crate::telemetry::span("metadata/album");
    match span.result(LsAlbum::get(session, &uri).await, |a| {
        (0, a.tracks().count())
    }) {
        Ok(a) => Some(Album {
            id: uri.to_id().ok()?,
            uri: uri.to_uri().ok()?,
            name: a.name,
            artists: a.artists.iter().map(|x| x.name.clone()).collect(),
            cover_url: cover_url(&a.covers),
        }),
        Err(e) => {
            log::warn!("album metadata failed for {uri:?}: {e}");
            None
        }
    }
}

/// Track metadata for up to `METADATA_BATCH` URIs in one extended-metadata request.
/// Missing or broken entries are left out (the caller falls back to single requests).
async fn fetch_track_batch(session: &Session, uris: &[SpotifyUri]) -> HashMap<String, Track> {
    let req = BatchedEntityRequest {
        entity_request: uris
            .iter()
            .filter_map(|u| u.to_uri().ok())
            .map(|entity_uri| EntityRequest {
                entity_uri,
                query: vec![ExtensionQuery {
                    extension_kind: EnumOrUnknown::new(ExtensionKind::TRACK_V4),
                    ..Default::default()
                }],
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    let span = telemetry::span("extended-metadata batch");
    let res = match session.spclient().get_extended_metadata(req).await {
        Ok(res) => res,
        Err(e) => {
            log::warn!("metadata batch of {} failed: {e}", uris.len());
            span.fail(&e);
            return HashMap::new();
        }
    };
    let mut out = HashMap::new();
    let mut bytes = 0;
    for data in res.extended_metadata.iter().flat_map(|a| &a.extension_data) {
        let Some(any) = data.extension_data.as_ref() else {
            continue;
        };
        bytes += any.value.len();
        let Ok(uri) = SpotifyUri::from_uri(&data.entity_uri) else {
            continue;
        };
        let parsed = TrackProto::parse_from_bytes(&any.value)
            .map_err(librespot::core::Error::from)
            .and_then(|msg| LsTrack::parse(&msg, &uri));
        match parsed {
            Ok(t) => {
                if let Some(track) = to_track(&uri, t) {
                    out.insert(track.uri.clone(), track);
                }
            }
            Err(e) => log::debug!("metadata batch entry {} unreadable: {e}", data.entity_uri),
        }
    }
    span.ok(bytes, out.len());
    out
}

fn to_track(uri: &SpotifyUri, t: LsTrack) -> Option<Track> {
    Some(Track {
        id: uri.to_id().ok(),
        uri: uri.to_uri().ok()?,
        name: t.name,
        artists: t.artists.iter().map(|a| a.name.clone()).collect(),
        album: t.album.name,
        duration_ms: t.duration.max(0) as u32,
        cover_url: cover_url(&t.album.covers),
    })
}

/// None for episodes, local files and unavailable items.
pub(crate) async fn resolve_track(session: &Session, uri: SpotifyUri) -> Option<Track> {
    if !matches!(uri, SpotifyUri::Track { .. }) {
        return None;
    }
    // Large libraries hit rate limits: retry with backoff instead of dropping the track.
    let mut attempt = 0;
    let t = loop {
        let span = telemetry::span("metadata/track");
        match LsTrack::get(session, &uri).await {
            Ok(t) => {
                span.ok(0, 1);
                break t;
            }
            Err(e)
                if attempt < METADATA_RETRIES
                    && !matches!(
                        e.kind,
                        ErrorKind::NotFound
                            | ErrorKind::InvalidArgument
                            | ErrorKind::PermissionDenied
                    ) =>
            {
                span.fail(&e);
                attempt += 1;
                log::debug!("track metadata failed for {uri:?} (retry {attempt}): {e}");
                tokio::time::sleep(Duration::from_millis(500 * (1 << attempt))).await;
            }
            Err(e) => {
                span.fail(&e);
                log::warn!("track metadata failed for {uri:?}: {e}");
                return None;
            }
        }
    };
    to_track(&uri, t)
}
