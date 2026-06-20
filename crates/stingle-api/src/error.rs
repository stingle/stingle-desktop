use thiserror::Error;

/// Errors from the Stingle API client.
#[derive(Debug, Error)]
pub enum ApiError {
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("invalid server response: {0}")]
    BadResponse(String),

    /// The server returned `status != "ok"`. Carries any human-readable error
    /// strings the server provided.
    #[error("server returned an error: {}", .errors.join("; "))]
    Server { errors: Vec<String>, infos: Vec<String> },

    /// The server signalled the session is no longer valid (`logout` field set).
    #[error("session expired (server requested logout)")]
    LoggedOut,

    #[error("a required field was missing from the response: {0}")]
    MissingField(&'static str),

    #[error("crypto error: {0}")]
    Crypto(#[from] stingle_crypto::CryptoError),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, ApiError>;
