//! Schema DDL, mirroring `StingleDbContract` plus a `kv` table for sync cursors,
//! cached space, and app settings.

pub const SCHEMA_VERSION: i32 = 1;

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
    headers       TEXT
);
CREATE INDEX IF NOT EXISTS idx_files_localremote ON files(is_local, is_remote);

CREATE TABLE IF NOT EXISTS trash (
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
CREATE INDEX IF NOT EXISTS idx_trash_localremote ON trash(is_local, is_remote);

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
    UNIQUE(album_id, filename)
);

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
