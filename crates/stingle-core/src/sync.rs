//! The sync engine: cloud→local delta processing, local→cloud upload, and
//! on-demand download + decryption. Mirrors `SyncManager` / `SyncSteps`.

use std::path::PathBuf;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use stingle_api::models::{self, DeleteEvent, RemoteAlbum, RemoteFile, SyncCursors, Updates};
use stingle_crypto::file;
use stingle_crypto::keys::KeyPair;
use stingle_db::{DbAlbum, DbContact, DbFile, FileSet, Sort};

use crate::account::Account;
use crate::error::{CoreError, Result};

/// Snapshot of cached storage usage (MB), as reported by the server.
#[derive(Debug, Clone, Copy, Default)]
pub struct Space {
    pub used: i64,
    pub quota: i64,
}

/// Does this look like a real `.sp` blob rather than a server JSON error
/// envelope (which `sync/download` returns with HTTP 200 on a bad token / rate
/// limit)? Guards the on-disk cache against being poisoned with error bodies.
fn is_sp_blob(bytes: &[u8]) -> bool {
    bytes.len() >= stingle_crypto::file::FILE_HEADER_BEGINNING_LEN
        && bytes.starts_with(stingle_crypto::constants::FILE_BEGINNING)
}

/// Cheap on-disk validity check: read just the 2-byte "SP" prefix. Lets us
/// detect (and re-download) an empty or poisoned cached blob without reading a
/// whole multi-GB original.
fn sp_magic_ok(path: &std::path::Path) -> bool {
    use std::io::Read;
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut buf = [0u8; 2];
    f.read_exact(&mut buf).is_ok() && &buf == stingle_crypto::constants::FILE_BEGINNING
}

impl Account {
    // ----------------------------- cursors -----------------------------

    fn load_cursors(&self) -> SyncCursors {
        let g = |k: &str| self.db.kv_get_i64(k).ok().flatten().unwrap_or(0);
        SyncCursors {
            files: g("filesST"),
            trash: g("trashST"),
            albums: g("albumsST"),
            album_files: g("albumFilesST"),
            deletes: g("delST"),
            contacts: g("cntST"),
        }
    }

    fn save_cursors(&self, c: &SyncCursors) -> Result<()> {
        self.db.kv_set_i64("filesST", c.files)?;
        self.db.kv_set_i64("trashST", c.trash)?;
        self.db.kv_set_i64("albumsST", c.albums)?;
        self.db.kv_set_i64("albumFilesST", c.album_files)?;
        self.db.kv_set_i64("delST", c.deletes)?;
        self.db.kv_set_i64("cntST", c.contacts)?;
        Ok(())
    }

    pub fn space(&self) -> Space {
        Space {
            used: self.db.kv_get_i64("spaceUsed").ok().flatten().unwrap_or(0),
            quota: self.db.kv_get_i64("spaceQuota").ok().flatten().unwrap_or(0),
        }
    }

    fn update_space(&self, used: Option<i64>, quota: Option<i64>) -> Result<()> {
        if let Some(u) = used {
            self.db.kv_set_i64("spaceUsed", u)?;
        }
        if let Some(q) = quota {
            self.db.kv_set_i64("spaceQuota", q)?;
        }
        Ok(())
    }

    // ----------------------------- full sync -----------------------------

    /// Pull server changes then push local changes.
    pub async fn full_sync(&self) -> Result<()> {
        self.sync_cloud_to_local().await?;
        self.upload_to_cloud().await?;
        Ok(())
    }

    // ------------------------- cloud → local -------------------------

    pub async fn sync_cloud_to_local(&self) -> Result<()> {
        let mut cur = self.load_cursors();
        let updates: Updates = self.client.get_updates(self.token(), cur).await?;

        for rf in &updates.files {
            self.process_file(FileSet::Gallery, rf)?;
            cur.files = cur.files.max(rf.date_modified);
        }
        for rf in &updates.trash {
            self.process_file(FileSet::Trash, rf)?;
            cur.trash = cur.trash.max(rf.date_modified);
        }
        for ra in &updates.albums {
            self.process_album(ra)?;
            cur.albums = cur.albums.max(ra.date_modified);
        }
        for rf in &updates.album_files {
            self.process_album_file(rf)?;
            cur.album_files = cur.album_files.max(rf.date_modified);
        }
        for rc in &updates.contacts {
            self.db.upsert_contact(&DbContact {
                user_id: rc.user_id,
                email: rc.email.clone(),
                public_key: rc.public_key.clone(),
                date_used: rc.date_used.unwrap_or(0),
                date_modified: rc.date_modified.unwrap_or(0),
            })?;
            cur.contacts = cur.contacts.max(rc.date_modified.unwrap_or(0));
        }
        let had_deletes = !updates.deletes.is_empty();
        for de in &updates.deletes {
            self.process_delete(de)?;
            cur.deletes = cur.deletes.max(de.date);
        }

        self.update_space(updates.space_used, updates.space_quota)?;
        self.save_cursors(&cur)?;
        // Server deletes (album / album-file removals) drop DB rows but not the
        // on-disk blobs; reclaim any that are now unreferenced.
        if had_deletes {
            let _ = self.prune_orphan_blobs();
        }
        Ok(())
    }

    fn process_file(&self, set: FileSet, rf: &RemoteFile) -> Result<()> {
        // The server is untrusted: refuse a filename that isn't a plain single
        // component, so a crafted name can never be used to build a cache path
        // that escapes the account directory. Skip (don't fail the whole sync).
        if !crate::paths::is_safe_component(&rf.filename) {
            tracing::warn!("skipping file with unsafe name from server: {:?}", rf.filename);
            return Ok(());
        }
        let existing = self.db.get_file(set, &rf.filename)?;
        let is_local =
            existing.as_ref().map(|e| e.is_local).unwrap_or(false) || self.paths.original(&rf.filename).exists();
        self.db.insert_file(
            set,
            &DbFile {
                id: 0,
                album_id: None,
                filename: rf.filename.clone(),
                is_local,
                is_remote: true,
                version: rf.version.unwrap_or(1),
                reupload: false,
                date_created: rf.date_created,
                date_modified: rf.date_modified,
                headers: rf.headers.clone(),
            },
        )?;
        Ok(())
    }

    fn process_album_file(&self, rf: &RemoteFile) -> Result<()> {
        if !crate::paths::is_safe_component(&rf.filename) {
            tracing::warn!("skipping album file with unsafe name from server: {:?}", rf.filename);
            return Ok(());
        }
        let is_local = self
            .db
            .get_album_file(&rf.album_id, &rf.filename)?
            .map(|e| e.is_local)
            .unwrap_or(false)
            || self.paths.original(&rf.filename).exists();
        self.db.insert_album_file(&DbFile {
            id: 0,
            album_id: Some(rf.album_id.clone()),
            filename: rf.filename.clone(),
            is_local,
            is_remote: true,
            version: rf.version.unwrap_or(1),
            reupload: false,
            date_created: rf.date_created,
            date_modified: rf.date_modified,
            headers: rf.headers.clone(),
        })?;
        Ok(())
    }

    fn process_album(&self, ra: &RemoteAlbum) -> Result<()> {
        let sync_local = self
            .db
            .get_album(&ra.album_id)?
            .map(|a| a.sync_local)
            .unwrap_or(false);
        self.db.upsert_album(&DbAlbum {
            album_id: ra.album_id.clone(),
            enc_private_key: ra.enc_private_key.clone(),
            public_key: ra.public_key.clone(),
            metadata: ra.metadata.clone(),
            is_shared: ra.is_shared,
            is_hidden: ra.is_hidden,
            is_owner: ra.is_owner,
            members: ra.members.clone(),
            permissions: ra.permissions.clone(),
            sync_local,
            is_locked: ra.is_locked,
            cover: ra.cover.clone(),
            date_created: ra.date_created,
            date_modified: ra.date_modified,
        })?;
        Ok(())
    }

    fn process_delete(&self, de: &DeleteEvent) -> Result<()> {
        use models::delete_event as dt;
        match de.event_type as i32 {
            dt::MAIN => {
                if self.file_older(FileSet::Gallery, &de.filename, de.date)? {
                    self.db.delete_file(FileSet::Gallery, &de.filename)?;
                    self.remove_local_files(&de.filename);
                }
            }
            dt::TRASH => {
                if self.file_older(FileSet::Trash, &de.filename, de.date)? {
                    self.db.delete_file(FileSet::Trash, &de.filename)?;
                }
            }
            dt::DELETE => {
                if self.file_older(FileSet::Trash, &de.filename, de.date)? {
                    self.db.delete_file(FileSet::Trash, &de.filename)?;
                    self.remove_local_files(&de.filename);
                }
            }
            dt::ALBUM => {
                if let Some(a) = self.db.get_album(&de.album_id)? {
                    if a.date_modified < de.date {
                        self.db.delete_all_files_in_album(&de.album_id)?;
                        self.db.delete_album(&de.album_id)?;
                    }
                }
            }
            dt::ALBUM_FILE => {
                if let Some(f) = self.db.get_album_file(&de.album_id, &de.filename)? {
                    if f.date_modified < de.date {
                        self.db.delete_album_file(&de.album_id, &de.filename)?;
                    }
                }
            }
            dt::CONTACT => {
                if let Ok(uid) = de.filename.parse::<i64>() {
                    self.db.delete_contact(uid)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn file_older(&self, set: FileSet, filename: &str, date: i64) -> Result<bool> {
        Ok(self
            .db
            .get_file(set, filename)?
            .map(|f| f.date_modified < date)
            .unwrap_or(false))
    }

    pub(crate) fn remove_local_files(&self, filename: &str) {
        // Guard against a server-supplied delete event with a traversal name
        // (`..\..\victim`) turning into deletion of an arbitrary file.
        if !crate::paths::is_safe_component(filename) {
            tracing::warn!("refusing to delete local files for unsafe name: {filename:?}");
            return;
        }
        let _ = std::fs::remove_file(self.paths.original(filename));
        let _ = std::fs::remove_file(self.paths.thumb(filename));
    }

    // ------------------------- local → cloud -------------------------

    pub async fn upload_to_cloud(&self) -> Result<()> {
        for set in [FileSet::Gallery, FileSet::Trash] {
            let mut todo = self.db.list_only_local(set, Sort::Desc)?;
            todo.extend(self.db.list_reupload(set)?);
            for f in todo {
                self.upload_one(set, &f).await?;
            }
        }
        for f in self.db.list_album_files_only_local()? {
            self.upload_one(FileSet::Album, &f).await?;
        }
        Ok(())
    }

    async fn upload_one(&self, set: FileSet, f: &DbFile) -> Result<()> {
        let orig = std::fs::read(self.paths.original(&f.filename))?;
        let thumb = std::fs::read(self.paths.thumb(&f.filename))?;
        let album_id = f.album_id.clone().unwrap_or_default();
        let space = self
            .client
            .upload(
                self.token(),
                set.id(),
                &album_id,
                f.version,
                f.date_created,
                f.date_modified,
                &f.headers,
                &f.filename,
                orig,
                thumb,
            )
            .await?;
        self.update_space(space.space_used, space.space_quota)?;
        if set == FileSet::Album {
            self.db.mark_album_file_remote(&album_id, &f.filename)?;
        } else {
            self.db.mark_remote(set, &f.filename)?;
        }
        Ok(())
    }

    // ------------------------- download + decrypt -------------------------

    /// Ensure the encrypted `.sp` for a file/thumb is on disk, downloading it if
    /// missing. Returns the local path.
    pub async fn ensure_encrypted(
        &self,
        set: FileSet,
        filename: &str,
        is_thumb: bool,
    ) -> Result<PathBuf> {
        // Defense in depth: unsafe names are already rejected at sync ingest, but
        // never build a cache path from one even if one slips through.
        if !crate::paths::is_safe_component(filename) {
            return Err(CoreError::Other("unsafe storage filename".into()));
        }
        let path = if is_thumb {
            self.paths.thumb(filename)
        } else {
            self.paths.original(filename)
        };
        // A non-empty file is only trusted if it actually starts with the `.sp`
        // magic. This both validates the cache and self-heals blobs poisoned by
        // an older build that cached a server JSON error body (HTTP 200) here.
        if path.exists() && sp_magic_ok(&path) {
            return Ok(path);
        }
        let _ = std::fs::remove_file(&path); // drop any empty/poisoned blob
        let bytes = self.download_limited(filename, set.id(), is_thumb).await?;
        std::fs::write(&path, &bytes)?;
        let _ = self.enforce_cache_limit(false);
        // The original being present locally means is_local should reflect that.
        if !is_thumb && set != FileSet::Album {
            let _ = self.db.set_local(set, filename, true);
        }
        Ok(path)
    }

    /// Download a file/thumbnail blob, bounded by the account's concurrency
    /// semaphore, with one retry on transient network failure.
    pub(crate) async fn download_limited(
        &self,
        filename: &str,
        set_id: i32,
        is_thumb: bool,
    ) -> Result<Vec<u8>> {
        let _permit = self
            .download_sem
            .acquire()
            .await
            .map_err(|_| CoreError::Other("download semaphore closed".into()))?;
        // Two attempts, to ride out a transient network error or a one-off server
        // error body. CRITICAL: `sync/download` returns its JSON error envelope
        // with HTTP 200 (bad token, rate limit, …), so a successful HTTP response
        // is NOT proof of an `.sp` blob — validate the magic and never return /
        // cache a body that would poison the on-disk cache.
        let mut last: Option<CoreError> = None;
        for _ in 0..2 {
            match self.client.download(self.token(), filename, set_id, is_thumb).await {
                Ok(b) if is_sp_blob(&b) => return Ok(b),
                Ok(_) => {
                    last = Some(CoreError::Other(
                        "download did not return an SP file (server error body)".into(),
                    ))
                }
                Err(e) => last = Some(e.into()),
            }
        }
        Err(last.unwrap_or_else(|| CoreError::Other("download failed".into())))
    }

    /// Download (if needed) and decrypt a file or thumbnail to plaintext bytes.
    /// Uses the DB `headers` field (sealed to the user or album key) — correct
    /// for both owned and shared/album files.
    pub async fn get_decrypted(
        &self,
        set: FileSet,
        album_id: Option<&str>,
        filename: &str,
        is_thumb: bool,
    ) -> Result<Vec<u8>> {
        let headers = self.headers_for(set, album_id, filename)?;
        let part = headers_part(&headers, is_thumb)?;
        let kp = self.keypair_for(set, album_id)?;
        let path = self.ensure_encrypted(set, filename, is_thumb).await?;
        let blob = std::fs::read(path)?;
        Ok(file::decrypt_with_external_header(
            &part,
            &blob,
            &kp.public_key,
            &kp.secret_key,
        )?)
    }

    /// The stored `headers` string for a file in the given set.
    pub(crate) fn headers_for(
        &self,
        set: FileSet,
        album_id: Option<&str>,
        filename: &str,
    ) -> Result<String> {
        let row = match set {
            FileSet::Album => {
                let aid = album_id.ok_or(CoreError::Other("album id required".into()))?;
                self.db.get_album_file(aid, filename)?
            }
            _ => self.db.get_file(set, filename)?,
        };
        row.map(|r| r.headers)
            .ok_or(CoreError::Other("file not found in db".into()))
    }

    /// The keypair that a file's header is sealed to.
    pub(crate) fn keypair_for(&self, set: FileSet, album_id: Option<&str>) -> Result<KeyPair> {
        if set == FileSet::Album {
            let aid = album_id.ok_or(CoreError::Other("album id required".into()))?;
            self.album_keypair(aid)
        } else {
            // Cheap clone of the user keypair (small fixed-size buffers).
            KeyPair::from_secret_key(&self.keypair.secret_key).map_err(Into::into)
        }
    }

    /// Decrypt an album's secret key with the user keypair and rebuild it.
    pub fn album_keypair(&self, album_id: &str) -> Result<KeyPair> {
        let a = self
            .db
            .get_album(album_id)?
            .ok_or(CoreError::Other("album not found".into()))?;
        let enc_sk = B64.decode(a.enc_private_key.trim())?;
        let album_sk = stingle_crypto::album::decrypt_album_sk(
            &enc_sk,
            &self.keypair.public_key,
            &self.keypair.secret_key,
        )?;
        Ok(KeyPair::from_secret_key(&album_sk)?)
    }
}

/// Pick and base64-decode the file or thumbnail part of a `headers` string.
/// Tolerant of padding and alphabet: different Stingle clients encode the
/// header with or without `=` padding (and URL-safe vs standard alphabet).
pub(crate) fn headers_part(headers: &str, is_thumb: bool) -> Result<Vec<u8>> {
    let mut it = headers.split('*');
    let file_part = it.next().unwrap_or("");
    let thumb_part = it.next().unwrap_or("");
    let chosen = if is_thumb { thumb_part } else { file_part };
    decode_b64_flexible(chosen.trim())
}

/// Decode base64 accepting URL-safe or standard alphabet, with or without padding.
pub(crate) fn decode_b64_flexible(s: &str) -> Result<Vec<u8>> {
    use base64::alphabet;
    use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
    let cfg = GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent);
    let url = GeneralPurpose::new(&alphabet::URL_SAFE, cfg);
    if let Ok(b) = url.decode(s) {
        return Ok(b);
    }
    let std = GeneralPurpose::new(&alphabet::STANDARD, cfg);
    Ok(std.decode(s)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_sp_download_bodies() {
        // A real `.sp` blob: "SP" magic + version + 32-byte id + 4-byte size.
        let mut good = Vec::new();
        good.extend_from_slice(stingle_crypto::constants::FILE_BEGINNING);
        good.push(stingle_crypto::constants::CURRENT_FILE_VERSION);
        good.extend_from_slice(&[0u8; 32]);
        good.extend_from_slice(&[0u8; 4]);
        assert!(is_sp_blob(&good));

        // The server's HTTP-200 JSON error envelope must be rejected.
        assert!(!is_sp_blob(br#"{"status":"error","parts":{}}"#));
        // Empty / truncated bodies too.
        assert!(!is_sp_blob(b""));
        assert!(!is_sp_blob(b"SP"));
    }
}
