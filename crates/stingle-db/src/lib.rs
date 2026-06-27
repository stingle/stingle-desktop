//! # stingle-db
//!
//! Local SQLite store mirroring the Android `StingleDbContract` schema (gallery,
//! trash, albums, album files, contacts, imported ids) plus a `kv` table for the
//! per-set sync cursors, cached space, and app settings.
//!
//! [`Db`] wraps a single connection behind a mutex so it can be shared across the
//! Tauri command handlers.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::Connection;

pub mod error;
mod models;
mod queries;
mod schema;

pub use error::{DbError, Result};
pub use models::{DbAlbum, DbContact, DbFile, FileSet, Sort};

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    /// Open (creating if needed) the database at `path` and run migrations.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// Open an in-memory database (used by tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // In WAL mode, NORMAL is crash-safe (no corruption on power loss, only the
        // last few transactions can be lost) but skips the per-commit fsync. This
        // is the single biggest sync-speed win: the cloud→local delta inserts each
        // file in its own auto-commit, so FULL would fsync tens of thousands of
        // times on a large account, taking minutes. NORMAL makes those near memory
        // speed.
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        // 16 MB page cache (negative = KiB) so large gallery scans/sorts stay warm.
        conn.pragma_update(None, "cache_size", -16_000)?;
        // Don't fail instantly when sync writes and UI reads contend on the lock.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(schema::DDL)?;
        conn.pragma_update(None, "user_version", schema::SCHEMA_VERSION)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub(crate) fn with_conn<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        let guard = self.conn.lock().map_err(|_| DbError::Lock)?;
        f(&guard)
    }
}
