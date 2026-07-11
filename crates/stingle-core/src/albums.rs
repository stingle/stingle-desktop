//! Album operations: create, rename, delete, set cover, and name decryption.

use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64URL};
use base64::Engine;
use stingle_crypto::album;
use stingle_crypto::sodium;
use stingle_db::DbAlbum;

use crate::account::Account;
use crate::error::Result;
use crate::util::now_ms;

/// Sentinel `cover` value that hides an album's contents behind a generic
/// placeholder instead of any real photo. Byte-compatible with the Android
/// client's `SetAlbumCoverAsyncTask.ALBUM_COVER_BLANK_TEXT`.
pub const ALBUM_COVER_BLANK: &str = "__b__";

/// 32-byte random album id, base64url-encoded (matches `CryptoHelpers.getRandomString(32)`).
fn new_album_id() -> Result<String> {
    Ok(B64URL.encode(sodium::random_bytes(32)?))
}

impl Account {
    /// Create an album, register it on the server, and store it locally.
    pub async fn create_album(&self, name: &str) -> Result<String> {
        self.create_album_inner(name, false).await
    }

    /// Create an album, optionally hidden. A hidden album is used when sharing
    /// loose files (it holds the shared copies but stays out of the Albums grid),
    /// matching Android's auto-created share album.
    pub(crate) async fn create_album_inner(&self, name: &str, hidden: bool) -> Result<String> {
        let (_album_kp, enc) =
            album::generate_encrypted_album_data(&self.keypair.public_key, name)?;
        let album_id = new_album_id()?;
        let now = now_ms();
        self.client
            .add_album(
                self.token(),
                &album_id,
                &enc.encrypted_private_key,
                &enc.public_key,
                &enc.metadata,
                now,
                now,
                self.server_crypto(),
            )
            .await?;
        self.db.upsert_album(&DbAlbum {
            album_id: album_id.clone(),
            enc_private_key: enc.encrypted_private_key,
            public_key: enc.public_key,
            metadata: enc.metadata,
            is_shared: false,
            is_hidden: hidden,
            is_owner: true,
            members: String::new(),
            permissions: String::new(),
            sync_local: false,
            is_locked: false,
            cover: String::new(),
            date_created: now,
            date_modified: now,
        })?;
        Ok(album_id)
    }

    /// Rename an album (re-encrypts the metadata to the album public key).
    pub async fn rename_album(&self, album_id: &str, new_name: &str) -> Result<()> {
        let a = self
            .db
            .get_album(album_id)?
            .ok_or(crate::error::CoreError::Other("album not found".into()))?;
        let album_pk = B64.decode(a.public_key.trim())?;
        let meta = album::encrypt_album_metadata(new_name, &album_pk)?;
        let meta_b64 = B64.encode(&meta);
        self.client
            .rename_album(self.token(), album_id, &meta_b64, self.server_crypto())
            .await?;
        let mut updated = a;
        updated.metadata = meta_b64;
        updated.date_modified = now_ms();
        self.db.upsert_album(&updated)?;
        Ok(())
    }

    /// Delete an album (server + local, including its file rows).
    pub async fn delete_album(&self, album_id: &str) -> Result<()> {
        self.client
            .delete_album(self.token(), album_id, self.server_crypto())
            .await?;
        self.db.delete_all_files_in_album(album_id)?;
        self.db.delete_album(album_id)?;
        // Reclaim the album's now-unreferenced encrypted blobs.
        let _ = self.prune_orphan_blobs();
        Ok(())
    }

    /// Set an album's cover to a file already in the album.
    pub async fn set_album_cover(&self, album_id: &str, cover_filename: &str) -> Result<()> {
        self.client
            .change_album_cover(self.token(), album_id, cover_filename, self.server_crypto())
            .await?;
        if let Some(mut a) = self.db.get_album(album_id)? {
            a.cover = cover_filename.to_string();
            a.date_modified = now_ms();
            self.db.upsert_album(&a)?;
        }
        Ok(())
    }

    /// Set a blank album cover: hides the album's contents behind a generic
    /// placeholder by storing the `__b__` sentinel ([`ALBUM_COVER_BLANK`]).
    /// Mirrors the Android "Set a blank album cover" action.
    pub async fn set_album_blank_cover(&self, album_id: &str) -> Result<()> {
        self.set_album_cover(album_id, ALBUM_COVER_BLANK).await
    }

    /// Decrypt an album's display name from its metadata.
    pub fn album_name(&self, a: &DbAlbum) -> Result<String> {
        let kp = self.album_keypair(&a.album_id)?;
        let album_pk = B64.decode(a.public_key.trim())?;
        let meta = B64.decode(a.metadata.trim())?;
        Ok(album::decrypt_album_metadata(&meta, &album_pk, &kp.secret_key)?)
    }

    /// List albums together with their decrypted names.
    pub fn list_albums_with_names(&self, include_hidden: bool) -> Result<Vec<(DbAlbum, String)>> {
        let mut out = Vec::new();
        for a in self.db.list_albums(include_hidden)? {
            let name = self.album_name(&a).unwrap_or_else(|_| a.album_id.clone());
            out.push((a, name));
        }
        Ok(out)
    }
}
