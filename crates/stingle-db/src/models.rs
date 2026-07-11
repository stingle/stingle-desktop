//! Row types mirroring the Android `Db/Objects` classes.

/// The three logical file sets. Gallery/Trash map to their own tables;
/// Album maps to `album_files` (rows additionally carry an `album_id`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileSet {
    Gallery,
    Trash,
    Album,
}

impl FileSet {
    pub fn table(self) -> &'static str {
        match self {
            FileSet::Gallery => "files",
            FileSet::Trash => "trash",
            FileSet::Album => "album_files",
        }
    }

    /// The integer id used by the server/API (`GALLERY=0, TRASH=1, ALBUM=2`).
    pub fn id(self) -> i32 {
        match self {
            FileSet::Gallery => 0,
            FileSet::Trash => 1,
            FileSet::Album => 2,
        }
    }

    pub fn from_id(id: i32) -> Option<Self> {
        match id {
            0 => Some(FileSet::Gallery),
            1 => Some(FileSet::Trash),
            2 => Some(FileSet::Album),
            _ => None,
        }
    }
}

/// A file row (gallery, trash, or album file).
#[derive(Debug, Clone)]
pub struct DbFile {
    pub id: i64,
    /// Present only for album files.
    pub album_id: Option<String>,
    pub filename: String,
    pub is_local: bool,
    pub is_remote: bool,
    pub version: i64,
    pub reupload: bool,
    pub date_created: i64,
    pub date_modified: i64,
    pub headers: String,
    /// Derived from the encrypted header once (at ingest or lazy backfill);
    /// `None` = not derived yet, so callers must fall back to decoding.
    pub is_video: Option<bool>,
}

/// An album row.
#[derive(Debug, Clone)]
pub struct DbAlbum {
    pub album_id: String,
    pub enc_private_key: String,
    pub public_key: String,
    pub metadata: String,
    pub is_shared: bool,
    pub is_hidden: bool,
    pub is_owner: bool,
    pub members: String,
    pub permissions: String,
    pub sync_local: bool,
    pub is_locked: bool,
    pub cover: String,
    pub date_created: i64,
    pub date_modified: i64,
}

/// A contact row.
#[derive(Debug, Clone)]
pub struct DbContact {
    pub user_id: i64,
    pub email: String,
    pub public_key: String,
    pub date_used: i64,
    pub date_modified: i64,
}

/// Sort direction for file listings.
#[derive(Debug, Clone, Copy)]
pub enum Sort {
    Asc,
    Desc,
}

impl Sort {
    pub(crate) fn sql(self) -> &'static str {
        match self {
            Sort::Asc => "ASC",
            Sort::Desc => "DESC",
        }
    }
}
