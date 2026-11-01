//! Spotify account auth: OAuth (PKCE) on first login, then cached reusable credentials.

use std::fs;

use librespot::{
    core::{authentication::Credentials, cache::Cache, config::SessionConfig},
    oauth::OAuthClientBuilder,
};

use crate::{
    config::Paths,
    error::{CoreError, Result},
};

/// Must match a redirect URI registered for librespot's client id.
const REDIRECT_URI: &str = "http://127.0.0.1:5588/login";

const OAUTH_SCOPES: &[&str] = &[
    "streaming",
    "app-remote-control",
    "playlist-read",
    "playlist-read-private",
    "playlist-read-collaborative",
    "playlist-modify",
    "playlist-modify-private",
    "playlist-modify-public",
    "user-follow-read",
    "user-follow-modify",
    "user-library-read",
    "user-library-modify",
    "user-modify",
    "user-modify-playback-state",
    "user-modify-private",
    "user-personalized",
    "user-read-currently-playing",
    "user-read-email",
    "user-read-play-history",
    "user-read-playback-position",
    "user-read-playback-state",
    "user-read-private",
    "user-read-recently-played",
    "user-top-read",
];

const SUCCESS_PAGE: &str = "<html><body style=\"font-family:sans-serif\">\
    <h2>SpotiPod is logged in.</h2><p>You can close this tab.</p></body></html>";

pub(crate) fn open_cache(paths: &Paths) -> Result<Cache> {
    Ok(Cache::new(
        Some(paths.cache_dir.as_path()),
        None,
        None,
        None,
    )?)
}

/// Reusable credentials stored by a previous successful session.
pub fn cached_credentials(paths: &Paths) -> Option<Credentials> {
    open_cache(paths).ok()?.credentials()
}

/// Runs the browser OAuth flow. The returned token credentials are exchanged for
/// reusable ones (and cached) on the first session connect.
pub async fn login_oauth(open_browser: bool) -> Result<Credentials> {
    let client_id = SessionConfig::default().client_id;
    let mut builder = OAuthClientBuilder::new(&client_id, REDIRECT_URI, OAUTH_SCOPES.to_vec())
        .with_custom_message(SUCCESS_PAGE);
    if open_browser {
        builder = builder.open_in_browser();
    }
    let token = builder.build()?.get_access_token_async().await?;
    Ok(Credentials::with_access_token(token.access_token))
}

/// Forgets cached credentials and the account's library cache.
pub fn logout(paths: &Paths) -> Result<()> {
    for file in [paths.credentials_file(), paths.library_cache_file()] {
        match fs::remove_file(file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(CoreError::Io(e)),
        }
    }
    Ok(())
}
