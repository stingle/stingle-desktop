use thiserror::Error;

/// Errors produced by the Stingle crypto core.
#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("libsodium initialization failed")]
    InitFailed,

    #[error("libsodium operation failed: {0}")]
    Sodium(&'static str),

    #[error("decryption/authentication failed: {0}")]
    Decryption(&'static str),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("malformed file: {0}")]
    MalformedFile(&'static str),

    #[error("unsupported version: {0}")]
    UnsupportedVersion(String),

    #[error("invalid mnemonic: {0}")]
    InvalidMnemonic(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, CryptoError>;
