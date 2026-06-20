//! Typed wrappers for every server endpoint (paths from Android `config.xml`).
//!
//! Plain endpoints have fully typed methods. Endpoints whose encrypted-`params`
//! shape is more involved (album sharing, trash/restore, recovery) are reachable
//! today via [`Client::post_encrypted`]/[`Client::post_form`]; typed wrappers are
//! filled in as `stingle-core` pins each exact shape against `SyncManager`.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::client::{Client, ServerCrypto, UploadBlob};
use crate::error::{ApiError, Result};
use crate::models::*;

/// Endpoint path strings (exactly as in Android `res/values/config.xml`).
pub mod paths {
    pub const PRE_LOGIN: &str = "login/preLogin";
    pub const LOGIN: &str = "login/login";
    pub const REGISTER: &str = "register/createAccount";
    pub const LOGOUT: &str = "login/logout";
    pub const CHANGE_PASS: &str = "login/changePass";
    pub const CHANGE_EMAIL: &str = "login/changeEmail";
    pub const DELETE_ACCOUNT: &str = "login/deleteUser";
    pub const CHECK_KEY: &str = "login/checkKey";
    pub const RECOVER_ACCOUNT: &str = "login/recoverAccount";

    pub const UPLOAD_KEY_BUNDLE: &str = "keys/uploadKeyBundle";
    pub const GET_SERVER_PK: &str = "keys/getServerPK";
    pub const REUPLOAD_KEYS: &str = "keys/reuploadKeys";

    pub const GET_UPDATES: &str = "sync/getUpdates";
    pub const UPLOAD: &str = "sync/upload";
    pub const DOWNLOAD: &str = "sync/download";
    pub const GET_DOWNLOAD_URLS: &str = "sync/getDownloadUrls";
    pub const MOVE_FILE: &str = "sync/moveFile";
    pub const TRASH: &str = "sync/trash";
    pub const RESTORE: &str = "sync/restore";
    pub const DELETE: &str = "sync/delete";
    pub const EMPTY_TRASH: &str = "sync/emptyTrash";

    pub const ADD_ALBUM: &str = "sync/addAlbum";
    pub const DELETE_ALBUM: &str = "sync/deleteAlbum";
    pub const RENAME_ALBUM: &str = "sync/renameAlbum";
    pub const CHANGE_ALBUM_COVER: &str = "sync/changeAlbumCover";
    pub const SHARE: &str = "sync/share";
    pub const UNSHARE: &str = "sync/unshareAlbum";
    pub const EDIT_PERMS: &str = "sync/editPerms";
    pub const REMOVE_MEMBER: &str = "sync/removeAlbumMember";
    pub const LEAVE_ALBUM: &str = "sync/leaveAlbum";

    pub const GET_CONTACT: &str = "sync/getContact";

    pub const BILLING_INFO: &str = "billing/info";
    pub const BILLING_DOWNGRADE: &str = "billing/downgrade";
}

fn parse_opt_i64(s: Option<String>) -> Option<i64> {
    s.filter(|x| !x.is_empty()).and_then(|x| x.parse().ok())
}

impl Client {
    // ----------------------------- Auth -----------------------------

    /// `login/preLogin` → the account's password salt (hex).
    pub async fn pre_login(&self, email: &str) -> Result<String> {
        let r = self
            .post_form(paths::PRE_LOGIN, &[("email", email.to_string())])
            .await?;
        r.require("salt")
    }

    /// `login/login`. `password_hash` is the upper-case-hex Argon2id login hash.
    pub async fn login(&self, email: &str, password_hash: &str) -> Result<LoginResult> {
        let r = self
            .post_form(
                paths::LOGIN,
                &[
                    ("email", email.to_string()),
                    ("password", password_hash.to_string()),
                ],
            )
            .await?;
        Ok(LoginResult {
            token: r.require("token")?,
            user_id: r.require("userId")?,
            key_bundle: r.require("keyBundle")?,
            server_public_key: r.get("serverPublicKey").unwrap_or_default(),
            is_key_backed_up: r.get("isKeyBackedUp").as_deref() == Some("1"),
            home_folder: r.require("homeFolder")?,
            addons: r
                .get_array("addons")
                .into_iter()
                .map(|v| match v {
                    Value::String(s) => s,
                    other => other.to_string(),
                })
                .collect(),
        })
    }

    /// `register/createAccount`.
    pub async fn register(
        &self,
        email: &str,
        password_hash: &str,
        salt_hex: &str,
        is_backup: bool,
        key_bundle_b64: &str,
    ) -> Result<()> {
        self.post_form(
            paths::REGISTER,
            &[
                ("email", email.to_string()),
                ("password", password_hash.to_string()),
                ("salt", salt_hex.to_string()),
                ("isBackup", if is_backup { "1" } else { "0" }.to_string()),
                ("keyBundle", key_bundle_b64.to_string()),
            ],
        )
        .await?;
        Ok(())
    }

    /// `login/logout`.
    pub async fn logout(&self, token: &str) -> Result<()> {
        self.post_form(paths::LOGOUT, &[("token", token.to_string())])
            .await?;
        Ok(())
    }

    /// `login/changeEmail` (token + new email, plain).
    pub async fn change_email(&self, token: &str, new_email: &str) -> Result<()> {
        self.post_form(
            paths::CHANGE_EMAIL,
            &[("token", token.to_string()), ("email", new_email.to_string())],
        )
        .await?;
        Ok(())
    }

    /// `login/changePass` (encrypted: newPassword, newSalt, keyBundle) → new token.
    pub async fn change_password(
        &self,
        token: &str,
        new_password_hash: &str,
        new_salt_hex: &str,
        key_bundle_b64: &str,
        sc: ServerCrypto<'_>,
    ) -> Result<String> {
        let mut p = BTreeMap::new();
        p.insert("newPassword".to_string(), new_password_hash.to_string());
        p.insert("newSalt".to_string(), new_salt_hex.to_string());
        p.insert("keyBundle".to_string(), key_bundle_b64.to_string());
        let r = self.post_encrypted(paths::CHANGE_PASS, token, p, sc).await?;
        r.require("token")
    }

    /// `login/deleteUser` (encrypted: password).
    pub async fn delete_account(
        &self,
        token: &str,
        password_hash: &str,
        sc: ServerCrypto<'_>,
    ) -> Result<()> {
        let mut p = BTreeMap::new();
        p.insert("password".to_string(), password_hash.to_string());
        self.post_encrypted(paths::DELETE_ACCOUNT, token, p, sc)
            .await?;
        Ok(())
    }

    /// `login/checkKey` — fetch the recovery challenge + server PK for an email.
    pub async fn check_key(&self, email: &str) -> Result<CheckKeyResult> {
        let r = self
            .post_form(paths::CHECK_KEY, &[("email", email.to_string())])
            .await?;
        Ok(CheckKeyResult {
            challenge: r.require("challenge")?,
            server_pk: r.require("serverPK")?,
            is_key_backed_up: r.get("isKeyBackedUp").as_deref() == Some("1"),
        })
    }

    /// `login/recoverAccount` — `email` plus a caller-encrypted `params` blob
    /// (encrypted with the server PK and the *recovery* private key).
    pub async fn recover_account(&self, email: &str, params_b64: &str) -> Result<()> {
        self.post_form(
            paths::RECOVER_ACCOUNT,
            &[("email", email.to_string()), ("params", params_b64.to_string())],
        )
        .await?;
        Ok(())
    }

    // ----------------------------- Keys -----------------------------

    /// `keys/getServerPK` → base64 server public key.
    pub async fn get_server_pk(&self, token: &str) -> Result<String> {
        let r = self
            .post_form(paths::GET_SERVER_PK, &[("token", token.to_string())])
            .await?;
        r.require("serverPK")
    }

    /// `keys/uploadKeyBundle`.
    pub async fn upload_key_bundle(&self, token: &str, key_bundle_b64: &str) -> Result<()> {
        self.post_form(
            paths::UPLOAD_KEY_BUNDLE,
            &[
                ("token", token.to_string()),
                ("keyBundle", key_bundle_b64.to_string()),
            ],
        )
        .await?;
        Ok(())
    }

    /// `keys/reuploadKeys` (encrypted: keyBundle).
    pub async fn reupload_keys(
        &self,
        token: &str,
        key_bundle_b64: &str,
        sc: ServerCrypto<'_>,
    ) -> Result<()> {
        let mut p = BTreeMap::new();
        p.insert("keyBundle".to_string(), key_bundle_b64.to_string());
        self.post_encrypted(paths::REUPLOAD_KEYS, token, p, sc).await?;
        Ok(())
    }

    // ----------------------------- Sync -----------------------------

    /// `sync/getUpdates` — delta pull with the six per-set cursors.
    pub async fn get_updates(&self, token: &str, c: SyncCursors) -> Result<Updates> {
        let r = self
            .post_form(
                paths::GET_UPDATES,
                &[
                    ("token", token.to_string()),
                    ("filesST", c.files.to_string()),
                    ("trashST", c.trash.to_string()),
                    ("albumsST", c.albums.to_string()),
                    ("albumFilesST", c.album_files.to_string()),
                    ("delST", c.deletes.to_string()),
                    ("cntST", c.contacts.to_string()),
                ],
            )
            .await?;
        Ok(Updates {
            files: r.parse_array("files"),
            trash: r.parse_array("trash"),
            albums: r.parse_array("albums"),
            album_files: r.parse_array("albumFiles"),
            contacts: r.parse_array("contacts"),
            deletes: r.parse_array("deletes"),
            space_used: parse_opt_i64(r.get("spaceUsed")),
            space_quota: parse_opt_i64(r.get("spaceQuota")),
        })
    }

    /// `sync/upload` — multipart original + thumbnail with metadata fields.
    #[allow(clippy::too_many_arguments)]
    pub async fn upload(
        &self,
        token: &str,
        set: i32,
        album_id: &str,
        version: i64,
        date_created: i64,
        date_modified: i64,
        headers: &str,
        filename: &str,
        file_bytes: Vec<u8>,
        thumb_bytes: Vec<u8>,
    ) -> Result<SpaceInfo> {
        let fields = [
            ("token", token.to_string()),
            ("set", set.to_string()),
            ("albumId", album_id.to_string()),
            ("version", version.to_string()),
            ("dateCreated", date_created.to_string()),
            ("dateModified", date_modified.to_string()),
            ("headers", headers.to_string()),
        ];
        let blobs = vec![
            UploadBlob {
                name: "file",
                filename: filename.to_string(),
                bytes: file_bytes,
            },
            UploadBlob {
                name: "thumb",
                filename: filename.to_string(),
                bytes: thumb_bytes,
            },
        ];
        let r = self.post_multipart(paths::UPLOAD, &fields, blobs).await?;
        Ok(SpaceInfo {
            space_used: parse_opt_i64(r.get("spaceUsed")),
            space_quota: parse_opt_i64(r.get("spaceQuota")),
        })
    }

    /// `sync/download` — fetch the encrypted `.sp` bytes for a file or thumbnail.
    pub async fn download(
        &self,
        token: &str,
        filename: &str,
        set: i32,
        is_thumb: bool,
    ) -> Result<Vec<u8>> {
        let mut params = vec![
            ("token", token.to_string()),
            ("file", filename.to_string()),
            ("set", set.to_string()),
        ];
        if is_thumb {
            params.push(("thumb", "1".to_string()));
        }
        self.post_download(paths::DOWNLOAD, &params).await
    }

    /// `sync/delete` — permanently delete files (encrypted: count + filenameN).
    pub async fn delete_files(
        &self,
        token: &str,
        filenames: &[String],
        sc: ServerCrypto<'_>,
    ) -> Result<()> {
        let mut p = BTreeMap::new();
        p.insert("count".to_string(), filenames.len().to_string());
        for (i, f) in filenames.iter().enumerate() {
            p.insert(format!("filename{i}"), f.clone());
        }
        self.post_encrypted(paths::DELETE, token, p, sc).await?;
        Ok(())
    }

    /// `sync/emptyTrash` (encrypted: time).
    pub async fn empty_trash(&self, token: &str, time_ms: i64, sc: ServerCrypto<'_>) -> Result<()> {
        let mut p = BTreeMap::new();
        p.insert("time".to_string(), time_ms.to_string());
        self.post_encrypted(paths::EMPTY_TRASH, token, p, sc).await?;
        Ok(())
    }

    // ----------------------------- Contacts -----------------------------

    /// `sync/getContact` (encrypted: email) → the contact's public key + id.
    pub async fn get_contact(
        &self,
        token: &str,
        email: &str,
        sc: ServerCrypto<'_>,
    ) -> Result<RemoteContact> {
        let mut p = BTreeMap::new();
        p.insert("email".to_string(), email.to_string());
        let r = self.post_encrypted(paths::GET_CONTACT, token, p, sc).await?;
        let val = r
            .parts
            .get("contact")
            .cloned()
            .ok_or(ApiError::MissingField("contact"))?;
        // `contact` may be a JSON object or a JSON-encoded string.
        let obj: Value = match val {
            Value::String(s) => serde_json::from_str(&s)?,
            other => other,
        };
        Ok(serde_json::from_value(obj)?)
    }
}

/// File-move and album operations (all encrypted-`params`), with the exact
/// parameter maps from `SyncManager.java`.
impl Client {
    /// `sync/moveFile` — move/copy files between sets and albums. Trash and
    /// restore are this with `set_to = TRASH` / `set_from = TRASH`.
    /// `files` is `(filename, optional re-sealed headers)`; only remote files
    /// are sent (the server only tracks remote files).
    #[allow(clippy::too_many_arguments)]
    pub async fn move_files(
        &self,
        token: &str,
        set_from: i32,
        set_to: i32,
        album_id_from: &str,
        album_id_to: &str,
        is_moving: bool,
        files: &[(String, Option<String>)],
        sc: ServerCrypto<'_>,
    ) -> Result<()> {
        let mut p = BTreeMap::new();
        p.insert("setFrom".into(), set_from.to_string());
        p.insert("setTo".into(), set_to.to_string());
        p.insert("albumIdFrom".into(), album_id_from.to_string());
        p.insert("albumIdTo".into(), album_id_to.to_string());
        p.insert("isMoving".into(), if is_moving { "1" } else { "0" }.into());
        p.insert("count".into(), files.len().to_string());
        for (i, (filename, headers)) in files.iter().enumerate() {
            p.insert(format!("filename{i}"), filename.clone());
            if let Some(h) = headers {
                p.insert(format!("headers{i}"), h.clone());
            }
        }
        self.post_encrypted(paths::MOVE_FILE, token, p, sc).await?;
        Ok(())
    }

    /// `sync/addAlbum`.
    #[allow(clippy::too_many_arguments)]
    pub async fn add_album(
        &self,
        token: &str,
        album_id: &str,
        enc_private_key: &str,
        public_key: &str,
        metadata: &str,
        date_created: i64,
        date_modified: i64,
        sc: ServerCrypto<'_>,
    ) -> Result<()> {
        let mut p = BTreeMap::new();
        p.insert("albumId".into(), album_id.to_string());
        p.insert("encPrivateKey".into(), enc_private_key.to_string());
        p.insert("publicKey".into(), public_key.to_string());
        p.insert("metadata".into(), metadata.to_string());
        p.insert("dateCreated".into(), date_created.to_string());
        p.insert("dateModified".into(), date_modified.to_string());
        self.post_encrypted(paths::ADD_ALBUM, token, p, sc).await?;
        Ok(())
    }

    /// `sync/deleteAlbum`.
    pub async fn delete_album(&self, token: &str, album_id: &str, sc: ServerCrypto<'_>) -> Result<()> {
        let mut p = BTreeMap::new();
        p.insert("albumId".into(), album_id.to_string());
        self.post_encrypted(paths::DELETE_ALBUM, token, p, sc).await?;
        Ok(())
    }

    /// `sync/renameAlbum` (albumId + new encrypted metadata).
    pub async fn rename_album(
        &self,
        token: &str,
        album_id: &str,
        metadata: &str,
        sc: ServerCrypto<'_>,
    ) -> Result<()> {
        let mut p = BTreeMap::new();
        p.insert("albumId".into(), album_id.to_string());
        p.insert("metadata".into(), metadata.to_string());
        self.post_encrypted(paths::RENAME_ALBUM, token, p, sc).await?;
        Ok(())
    }

    /// `sync/changeAlbumCover`.
    pub async fn change_album_cover(
        &self,
        token: &str,
        album_id: &str,
        cover: &str,
        sc: ServerCrypto<'_>,
    ) -> Result<()> {
        let mut p = BTreeMap::new();
        p.insert("albumId".into(), album_id.to_string());
        p.insert("cover".into(), cover.to_string());
        self.post_encrypted(paths::CHANGE_ALBUM_COVER, token, p, sc)
            .await?;
        Ok(())
    }

    /// `sync/share` — `album` is the album's `toJSON()`, `sharing_keys` maps
    /// userId → album secret key sealed to that member's public key.
    pub async fn share_album(
        &self,
        token: &str,
        album_json: &str,
        sharing_keys_json: &str,
        sc: ServerCrypto<'_>,
    ) -> Result<()> {
        let mut p = BTreeMap::new();
        p.insert("album".into(), album_json.to_string());
        p.insert("sharingKeys".into(), sharing_keys_json.to_string());
        self.post_encrypted(paths::SHARE, token, p, sc).await?;
        Ok(())
    }

    /// `sync/editPerms` — `album` is the album's `toJSON()` with new permissions.
    pub async fn edit_album_perms(
        &self,
        token: &str,
        album_json: &str,
        sc: ServerCrypto<'_>,
    ) -> Result<()> {
        let mut p = BTreeMap::new();
        p.insert("album".into(), album_json.to_string());
        self.post_encrypted(paths::EDIT_PERMS, token, p, sc).await?;
        Ok(())
    }

    /// `sync/removeAlbumMember`.
    pub async fn remove_album_member(
        &self,
        token: &str,
        album_json: &str,
        member_user_id: i64,
        sc: ServerCrypto<'_>,
    ) -> Result<()> {
        let mut p = BTreeMap::new();
        p.insert("album".into(), album_json.to_string());
        p.insert("memberUserId".into(), member_user_id.to_string());
        self.post_encrypted(paths::REMOVE_MEMBER, token, p, sc)
            .await?;
        Ok(())
    }

    /// `sync/unshareAlbum`.
    pub async fn unshare_album(&self, token: &str, album_id: &str, sc: ServerCrypto<'_>) -> Result<()> {
        let mut p = BTreeMap::new();
        p.insert("albumId".into(), album_id.to_string());
        self.post_encrypted(paths::UNSHARE, token, p, sc).await?;
        Ok(())
    }

    /// `sync/leaveAlbum`.
    pub async fn leave_album(&self, token: &str, album_id: &str, sc: ServerCrypto<'_>) -> Result<()> {
        let mut p = BTreeMap::new();
        p.insert("albumId".into(), album_id.to_string());
        self.post_encrypted(paths::LEAVE_ALBUM, token, p, sc).await?;
        Ok(())
    }
}
