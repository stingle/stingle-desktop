//! Typed representations of server objects, mirroring the Android `Db/Objects`
//! JSON constructors exactly (field names are the wire names).

use serde::Deserialize;

use crate::de;

/// Set identifiers used throughout the API.
pub mod set {
    pub const GALLERY: i32 = 0;
    pub const TRASH: i32 = 1;
    pub const ALBUM: i32 = 2;
}

/// Delete-event `type` values from `sync/getUpdates`.
pub mod delete_event {
    pub const MAIN: i32 = 1;
    pub const TRASH: i32 = 2;
    pub const DELETE: i32 = 3;
    pub const ALBUM: i32 = 4;
    pub const ALBUM_FILE: i32 = 5;
    pub const CONTACT: i32 = 6;
}

/// A file as returned by `sync/getUpdates` (`StingleDbFile(JSONObject)`).
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteFile {
    #[serde(rename = "file")]
    pub filename: String,
    #[serde(rename = "albumId", default, deserialize_with = "de::nullable_string")]
    pub album_id: String,
    #[serde(default, deserialize_with = "de::opt_i64_flexible")]
    pub version: Option<i64>,
    pub headers: String,
    #[serde(rename = "dateCreated", deserialize_with = "de::i64_flexible")]
    pub date_created: i64,
    #[serde(rename = "dateModified", deserialize_with = "de::i64_flexible")]
    pub date_modified: i64,
}

/// An album as returned by `sync/getUpdates` (`StingleDbAlbum(JSONObject)`).
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteAlbum {
    #[serde(rename = "albumId")]
    pub album_id: String,
    #[serde(rename = "encPrivateKey")]
    pub enc_private_key: String,
    #[serde(rename = "publicKey")]
    pub public_key: String,
    #[serde(default, deserialize_with = "de::nullable_string")]
    pub metadata: String,
    #[serde(rename = "isShared", deserialize_with = "de::int_bool")]
    pub is_shared: bool,
    #[serde(rename = "isHidden", deserialize_with = "de::int_bool")]
    pub is_hidden: bool,
    #[serde(rename = "isOwner", deserialize_with = "de::int_bool")]
    pub is_owner: bool,
    #[serde(default, deserialize_with = "de::nullable_string")]
    pub permissions: String,
    #[serde(rename = "isLocked", deserialize_with = "de::int_bool")]
    pub is_locked: bool,
    #[serde(default, deserialize_with = "de::nullable_string")]
    pub cover: String,
    #[serde(default, deserialize_with = "de::nullable_string")]
    pub members: String, // comma-separated user ids
    #[serde(rename = "dateCreated", deserialize_with = "de::i64_flexible")]
    pub date_created: i64,
    #[serde(rename = "dateModified", deserialize_with = "de::i64_flexible")]
    pub date_modified: i64,
}

/// A contact as returned by `sync/getUpdates` / `sync/getContact`.
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteContact {
    #[serde(rename = "userId", deserialize_with = "de::i64_flexible")]
    pub user_id: i64,
    pub email: String,
    #[serde(rename = "publicKey")]
    pub public_key: String,
    #[serde(rename = "dateUsed", default, deserialize_with = "de::opt_i64_flexible")]
    pub date_used: Option<i64>,
    #[serde(
        rename = "dateModified",
        default,
        deserialize_with = "de::opt_i64_flexible"
    )]
    pub date_modified: Option<i64>,
}

/// A delete event from `sync/getUpdates`.
#[derive(Debug, Clone, Deserialize)]
pub struct DeleteEvent {
    #[serde(rename = "file", default)]
    pub filename: String,
    #[serde(rename = "albumId", default)]
    pub album_id: String,
    #[serde(rename = "type", deserialize_with = "de::i64_flexible")]
    pub event_type: i64,
    #[serde(deserialize_with = "de::i64_flexible")]
    pub date: i64,
}

/// Per-set sync cursors sent to `sync/getUpdates` (milliseconds; start at 0).
#[derive(Debug, Clone, Copy, Default)]
pub struct SyncCursors {
    pub files: i64,
    pub trash: i64,
    pub albums: i64,
    pub album_files: i64,
    pub deletes: i64,
    pub contacts: i64,
}

/// The decoded result of `sync/getUpdates`.
#[derive(Debug, Default)]
pub struct Updates {
    pub files: Vec<RemoteFile>,
    pub trash: Vec<RemoteFile>,
    pub albums: Vec<RemoteAlbum>,
    pub album_files: Vec<RemoteFile>,
    pub contacts: Vec<RemoteContact>,
    pub deletes: Vec<DeleteEvent>,
    pub space_used: Option<i64>,
    pub space_quota: Option<i64>,
}

/// Successful `login/login` response.
#[derive(Debug, Clone)]
pub struct LoginResult {
    pub token: String,
    pub user_id: String,
    pub key_bundle: String,
    pub server_public_key: String,
    pub is_key_backed_up: bool,
    pub home_folder: String,
    pub addons: Vec<String>,
}

/// Result of `login/checkKey` during account recovery.
#[derive(Debug, Clone)]
pub struct CheckKeyResult {
    /// Base64 sealed challenge — decrypt with the recovered keypair; a valid
    /// key yields a plaintext starting with `validkey_`.
    pub challenge: String,
    pub server_pk: String,
    pub is_key_backed_up: bool,
}

/// Storage usage snapshot returned by uploads and `billing/info`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SpaceInfo {
    pub space_used: Option<i64>,
    pub space_quota: Option<i64>,
}
