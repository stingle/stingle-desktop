//! Schema DDL, mirroring `StingleDbContract` plus a `kv` table for sync cursors,
//! cached space, and app settings.

pub const SCHEMA_VERSION: i32 = 2;

// v2: `is_video` on files/trash/album_files — derived once from the encrypted
// header (at sync/import ingest, or lazily backfilled) so listings never have
// to seal-open every row's header. NULL = not derived yet (legacy rows).
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS files (
    _id           INTEGER PRIMARY KEY AUTOINCREMENT,
    filename      TEXT NOT NULL UNIQUE,
    is_local      INTEGER NOT NULL DEFAULT 0,
    is_remote     INTEGER NOT NULL DEFAULT 0,
    version       INTEGER NOT NULL DEFAULT 1,
    reupload      INTEGER NOT NULL DEFAULT 0,
    date_created  INTEGER NOT NULL DEFAULT 0,
    date_modified INTEGER NOT NULL DEFAULT 0,
    headers       TEXT,
    is_video      INTEGER
);
CREATE INDEX IF NOT EXISTS idx_files_localremote ON files(is_local, is_remote);
-- Composite so paginated `ORDER BY date_created, _id` (both directions) is served
-- entirely from the index — no temp-B-tree sort. The `_id` tiebreaker is what
-- makes LIMIT/OFFSET pagination stable. Supersedes the old date-only index.
DROP INDEX IF EXISTS idx_files_date;
CREATE INDEX IF NOT EXISTS idx_files_date_id ON files(date_created DESC, _id DESC);

CREATE TABLE IF NOT EXISTS trash (
    _id           INTEGER PRIMARY KEY AUTOINCREMENT,
    filename      TEXT NOT NULL UNIQUE,
    is_local      INTEGER NOT NULL DEFAULT 0,
    is_remote     INTEGER NOT NULL DEFAULT 0,
    version       INTEGER NOT NULL DEFAULT 1,
    reupload      INTEGER NOT NULL DEFAULT 0,
    date_created  INTEGER NOT NULL DEFAULT 0,
    date_modified INTEGER NOT NULL DEFAULT 0,
    headers       TEXT,
    is_video      INTEGER
);
CREATE INDEX IF NOT EXISTS idx_trash_localremote ON trash(is_local, is_remote);
DROP INDEX IF EXISTS idx_trash_date;
CREATE INDEX IF NOT EXISTS idx_trash_date_id ON trash(date_created DESC, _id DESC);

CREATE TABLE IF NOT EXISTS albums (
    _id           INTEGER PRIMARY KEY AUTOINCREMENT,
    album_id      TEXT NOT NULL UNIQUE,
    album_sk      TEXT NOT NULL,
    album_pk      TEXT NOT NULL,
    metadata      TEXT,
    is_shared     INTEGER NOT NULL DEFAULT 0,
    is_hidden     INTEGER NOT NULL DEFAULT 0,
    is_owner      INTEGER NOT NULL DEFAULT 1,
    members       TEXT,
    permissions   TEXT,
    sync_local    INTEGER NOT NULL DEFAULT 0,
    is_locked     INTEGER NOT NULL DEFAULT 0,
    cover         TEXT,
    date_created  INTEGER NOT NULL DEFAULT 0,
    date_modified INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS album_files (
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
    is_video      INTEGER,
    UNIQUE(album_id, filename)
);
CREATE INDEX IF NOT EXISTS idx_album_files_album ON album_files(album_id, date_created DESC);

CREATE TABLE IF NOT EXISTS contacts (
    _id           INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id       INTEGER NOT NULL UNIQUE,
    email         TEXT NOT NULL,
    pk            TEXT NOT NULL,
    date_used     INTEGER NOT NULL DEFAULT 0,
    date_modified INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS imported_ids (
    _id       INTEGER PRIMARY KEY AUTOINCREMENT,
    media_id  TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS kv (
    k TEXT PRIMARY KEY,
    v TEXT NOT NULL
);
"#;
