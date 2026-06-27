//! Album sharing: re-seal the album key to each recipient and notify the server.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::json;
use stingle_crypto::album;
use stingle_db::{DbAlbum, DbContact};

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
}
