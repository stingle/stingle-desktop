use thiserror::Error;

#[derive(Debug, Error)]
pub enum DbError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("db lock poisoned")]
    Lock,
}

pub type Result<T> = std::result::Result<T, DbError>;
