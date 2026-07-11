//! Album sharing: re-seal the album key to each recipient and notify the server.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::json;
use stingle_crypto::album;
use stingle_db::{DbAlbum, DbContact, FileSet};

use crate::account::Account;
use crate::error::{CoreError, Result};
use crate::util::now_ms;

fn b(x: bool) -> &'static str {
    if x {
        "1"
    } else {
        "0"
    }
}

/// Serialize an album exactly as `StingleDbAlbum.toJSON()` (string-valued flags).
pub fn album_to_json(a: &DbAlbum) -> String {
    json!({
        "albumId": a.album_id,
        "encPrivateKey": a.enc_private_key,
        "publicKey": a.public_key,
        "metadata": a.metadata,
        "isShared": b(a.is_shared),
        "isHidden": b(a.is_hidden),
        "isOwner": b(a.is_owner),
        "permissions": a.permissions,
        "members": a.members,
        "syncLocal": b(a.sync_local),
        "isLocked": b(a.is_locked),
        "cover": a.cover,
        "dateCreated": a.date_created.to_string(),
        "dateModified": a.date_modified.to_string(),
    })
    .to_string()
}

impl Account {
    /// Share an album with one or more contacts (by email), granting the given
    /// permissions. The album secret key is re-sealed to each recipient's public
    /// key; files are never re-encrypted.
    pub async fn share_album(
        &self,
        album_id: &str,
        emails: &[String],
        allow_add: bool,
        allow_share: bool,
        allow_copy: bool,
    ) -> Result<()> {
        let mut album = self
            .db
            .get_album(album_id)?
            .ok_or(CoreError::Other("album not found".into()))?;
        let kp = self.album_keypair(album_id)?;

        let mut sharing = serde_json::Map::new();
        let mut members: Vec<String> = if album.members.is_empty() {
            vec![]
        } else {
            album.members.split(',').map(|s| s.to_string()).collect()
        };

        for email in emails {
            let contact = self
                .client
                .get_contact(self.token(), email, self.server_crypto())
                .await?;
            let pk = B64.decode(contact.public_key.trim())?;
            let enc = album::encrypt_album_sk(&kp.secret_key, &pk)?;
            sharing.insert(contact.user_id.to_string(), json!(B64.encode(enc)));

            let id = contact.user_id.to_string();
            if !members.contains(&id) {
                members.push(id);
            }
            self.db.upsert_contact(&DbContact {
                user_id: contact.user_id,
                email: contact.email.clone(),
                public_key: contact.public_key.clone(),
                date_used: now_ms(),
                date_modified: now_ms(),
            })?;
        }

        album.is_shared = true;
        album.members = members.join(",");
        album.permissions = format!("1{}{}{}", b(allow_add), b(allow_share), b(allow_copy));
        album.date_modified = now_ms();

        let album_json = album_to_json(&album);
        let sharing_json = serde_json::to_string(&sharing)?;
        self.client
            .share_album(self.token(), &album_json, &sharing_json, self.server_crypto())
            .await?;
        self.db.upsert_album(&album)?;
        Ok(())
    }

    /// Share loose files by auto-creating a hidden album, copying the files into
    /// it (headers re-sealed to the album key), and sharing it with the given
    /// recipients. The source files stay where they are. Returns the new album id.
    /// Mirrors Android's `ShareAlbumAsyncTask` loose-files path.
    #[allow(clippy::too_many_arguments)]
    pub async fn share_new_album(
        &self,
        from_set: FileSet,
        from_album: Option<&str>,
        filenames: &[String],
        name: &str,
        emails: &[String],
        allow_add: bool,
        allow_share: bool,
        allow_copy: bool,
    ) -> Result<String> {
        // The server can only share files it already holds. If any selected file
        // hasn't finished uploading, refuse rather than create a half-broken share
        // (matches Android's "wait for upload" gate).
        for name in filenames {
            let row = match from_set {
                FileSet::Album => self.db.get_album_file(from_album.unwrap_or(""), name)?,
                _ => self.db.get_file(from_set, name)?,
            };
            if !row.map(|r| r.is_remote).unwrap_or(false) {
                return Err(CoreError::Other(
                    "Wait for upload to finish before sharing.".into(),
                ));
            }
        }
        let album_id = self.create_album_inner(name, true).await?;
        // Roll the album back if copying or sharing fails: the server only learns
        // the album is hidden via the share call, so a half-finished album would
        // otherwise re-appear (un-hidden) in the Albums grid on the next sync.
        let result = async {
            // Copy (not move) so the originals remain in the gallery/source album.
            self.move_to_album(from_set, from_album, filenames, &album_id, false)
                .await?;
            self.share_album(&album_id, emails, allow_add, allow_share, allow_copy)
                .await?;
            Ok::<(), CoreError>(())
        }
        .await;
        if let Err(err) = result {
            let _ = self.delete_album(&album_id).await;
            return Err(err);
        }
        Ok(album_id)
    }

    /// Stop sharing an album you own.
    pub async fn unshare_album(&self, album_id: &str) -> Result<()> {
        self.client
            .unshare_album(self.token(), album_id, self.server_crypto())
            .await?;
        if let Some(mut a) = self.db.get_album(album_id)? {
            a.is_shared = false;
            a.members = String::new();
            a.date_modified = now_ms();
            self.db.upsert_album(&a)?;
        }
        Ok(())
    }

    /// Leave an album shared with you.
    pub async fn leave_album(&self, album_id: &str) -> Result<()> {
        self.client
            .leave_album(self.token(), album_id, self.server_crypto())
            .await?;
        self.db.delete_all_files_in_album(album_id)?;
        self.db.delete_album(album_id)?;
        // Reclaim the left album's now-unreferenced encrypted blobs.
        let _ = self.prune_orphan_blobs();
        Ok(())
    }

    /// All known sharing contacts.
    pub fn contacts(&self) -> Result<Vec<DbContact>> {
        Ok(self.db.list_contacts()?)
    }

    /// Change an owned album's member permissions. Keys are untouched — only the
    /// 4-char permission string (`"1"+add+share+copy`) changes.
    pub async fn edit_album_perms(
        &self,
        album_id: &str,
        allow_add: bool,
        allow_share: bool,
        allow_copy: bool,
    ) -> Result<()> {
        let mut album = self
            .db
            .get_album(album_id)?
            .ok_or(CoreError::Other("album not found".into()))?;
        album.permissions = format!("1{}{}{}", b(allow_add), b(allow_share), b(allow_copy));
        album.date_modified = now_ms();
        self.client
            .edit_album_perms(self.token(), &album_to_json(&album), self.server_crypto())
            .await?;
        self.db.upsert_album(&album)?;
        Ok(())
    }

    /// Remove one member from an owned shared album. Drops the user-id from the
    /// members CSV and tells the server to revoke that member's access.
    pub async fn remove_album_member(&self, album_id: &str, member_user_id: i64) -> Result<()> {
        let mut album = self
            .db
            .get_album(album_id)?
            .ok_or(CoreError::Other("album not found".into()))?;
        let target = member_user_id.to_string();
        let members: Vec<String> = album
            .members
            .split(',')
            .filter(|s| !s.is_empty() && *s != target)
            .map(|s| s.to_string())
            .collect();
        album.members = members.join(",");
        album.date_modified = now_ms();
        self.client
            .remove_album_member(
                self.token(),
                &album_to_json(&album),
                member_user_id,
                self.server_crypto(),
            )
            .await?;
        self.db.upsert_album(&album)?;
        Ok(())
    }

    /// Resolve an album's members CSV to `(user_id, email)` pairs. `email` is
    /// `None` for members not present in the local contacts table.
    pub fn album_members(&self, album_id: &str) -> Result<Vec<(i64, Option<String>)>> {
        let album = self
            .db
            .get_album(album_id)?
            .ok_or(CoreError::Other("album not found".into()))?;
        let mut out = Vec::new();
        for part in album.members.split(',') {
            if part.is_empty() {
                continue;
            }
            let Ok(uid) = part.parse::<i64>() else {
                continue;
            };
            let email = self.db.get_contact_by_user_id(uid)?.map(|c| c.email);
            out.push((uid, email));
        }
        Ok(out)
    }
}
