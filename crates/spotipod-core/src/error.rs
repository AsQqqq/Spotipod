use thiserror::Error;

pub type Result<T, E = CoreError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("not logged in: run login first")]
    NotLoggedIn,

    #[error("not connected")]
    NotRunning,

    #[error("playback: {0}")]
    Playback(String),

    #[error("library: {0}")]
    Library(String),

    #[error("oauth: {0}")]
    OAuth(String),

    #[error("audio output: {0}")]
    Audio(String),

    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    #[error("librespot: {0}")]
    Librespot(#[from] librespot::core::Error),

    #[error("config: {0}")]
    Config(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl From<librespot::oauth::OAuthError> for CoreError {
    fn from(e: librespot::oauth::OAuthError) -> Self {
        Self::OAuth(e.to_string())
    }
}
