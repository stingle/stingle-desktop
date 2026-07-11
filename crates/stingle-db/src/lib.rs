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
        Self::migrate(&conn)?;
        conn.pragma_update(None, "user_version", schema::SCHEMA_VERSION)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Bring a pre-existing database up to the current schema. The DDL's
    /// `CREATE TABLE IF NOT EXISTS` only shapes *fresh* tables, so column adds
    /// must be applied here. Keyed on column presence (not `user_version`) so
    /// it is idempotent and immune to a version pragma written early.
    fn migrate(conn: &Connection) -> Result<()> {
        // v2: derived `is_video` column (NULL = not derived yet).
        for table in ["files", "trash", "album_files"] {
            let has_col = conn
                .prepare(&format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name='is_video'"))?
                .exists([])?;
            if !has_col {
                conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN is_video INTEGER"))?;
            }
        }
        Ok(())
    }

    pub(crate) fn with_conn<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        let guard = self.conn.lock().map_err(|_| DbError::Lock)?;
        f(&guard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{FileSet, Sort};

    /// A v1 database (no `is_video` column) must be migrated on open, keeping
    /// its rows readable with `is_video = NULL`.
    #[test]
    fn migrates_v1_schema() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE files (
                _id           INTEGER PRIMARY KEY AUTOINCREMENT,
                filename      TEXT NOT NULL UNIQUE,
                is_local      INTEGER NOT NULL DEFAULT 0,
                is_remote     INTEGER NOT NULL DEFAULT 0,
                version       INTEGER NOT NULL DEFAULT 1,
                reupload      INTEGER NOT NULL DEFAULT 0,
                date_created  INTEGER NOT NULL DEFAULT 0,
                date_modified INTEGER NOT NULL DEFAULT 0,
                headers       TEXT
            );
            CREATE TABLE trash (
                _id           INTEGER PRIMARY KEY AUTOINCREMENT,
                filename      TEXT NOT NULL UNIQUE,
                is_local      INTEGER NOT NULL DEFAULT 0,
                is_remote     INTEGER NOT NULL DEFAULT 0,
                version       INTEGER NOT NULL DEFAULT 1,
                reupload      INTEGER NOT NULL DEFAULT 0,
                date_created  INTEGER NOT NULL DEFAULT 0,
                date_modified INTEGER NOT NULL DEFAULT 0,
                headers       TEXT
            );
            CREATE TABLE album_files (
                _id           INTEGER PRIMARY KEY AUTOINCREMENT,
                album_id      TEXT NOT NULL,
                filename      TEXT NOT NULL,
                is_local      INTEGER NOT NULL DEFAULT 0,
                is_remote     INTEGER NOT NULL DEFAULT 0,
                version       INTEGER NOT NULL DEFAULT 1,
                reupload      INTEGER NOT NULL DEFAULT 0,
                headers       TEXT,
                date_created  INTEGER NOT NULL DEFAULT 0,
                date_modified INTEGER NOT NULL DEFAULT 0,
                UNIQUE(album_id, filename)
            );
            INSERT INTO files (filename, headers, date_created) VALUES ('legacy.sp', 'H', 42);
            PRAGMA user_version = 1;
            "#,
        )
        .unwrap();

        let db = Db::init(conn).unwrap();
        let rows = db.list_files(FileSet::Gallery, Sort::Desc, None, 0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].filename, "legacy.sp");
        assert_eq!(rows[0].is_video, None);

        // And the column is writable after migration.
        db.set_is_video_batch(FileSet::Gallery, &[("legacy.sp".to_string(), true)]).unwrap();
        assert_eq!(
            db.get_file(FileSet::Gallery, "legacy.sp").unwrap().unwrap().is_video,
            Some(true)
        );
    }
}
