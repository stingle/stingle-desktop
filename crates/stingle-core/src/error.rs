use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error(transparent)]
    Crypto(#[from] stingle_crypto::CryptoError),
    #[error(transparent)]
    Api(#[from] stingle_api::ApiError),
    #[error(transparent)]
    Db(#[from] stingle_db::DbError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("image error: {0}")]
    Image(#[from] image::ImageError),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("base64 error: {0}")]
    Base64(String),
    #[error("hex error: {0}")]
    Hex(String),
    #[error("{0}")]
    Other(String),
}

impl CoreError {
    /// True when the server rejected our token and asked us to log out (the
    /// session expired). Callers should drop the session and re-authenticate
    /// rather than keep hammering the API with a dead token.
    pub fn is_logged_out(&self) -> bool {
        matches!(self, CoreError::Api(stingle_api::ApiError::LoggedOut))
    }
}

impl From<base64::DecodeError> for CoreError {
    fn from(e: base64::DecodeError) -> Self {
        CoreError::Base64(e.to_string())
    }
}
impl From<hex::FromHexError> for CoreError {
    fn from(e: hex::FromHexError) -> Self {
        CoreError::Hex(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, CoreError>;
