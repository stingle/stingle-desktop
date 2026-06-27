//! Import: encrypt local photos/videos into the library (gallery or an album).

use std::io::Cursor;
use std::path::Path;
use std::time::UNIX_EPOCH;

use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64URL};
use base64::Engine;
use stingle_crypto::constants::{FILE_TYPE_PHOTO, FILE_TYPE_VIDEO};
use stingle_crypto::{file, sodium};
use stingle_db::{DbFile, FileSet};

use crate::account::Account;
use crate::error::{CoreError, Result};
use crate::thumbnail;

const VIDEO_EXTS: &[&str] = &["mp4", "mov", "avi", "mkv", "webm", "m4v", "3gp", "wmv", "flv"];
const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff", "heic", "heif",
];

fn ext_lower(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
}

/// Whether a path looks like an importable photo or video.
pub fn is_importable(path: &Path) -> bool {
    let e = ext_lower(path);
    VIDEO_EXTS.contains(&e.as_str()) || IMAGE_EXTS.contains(&e.as_str())
}

fn mtime_ms(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn new_encrypted_filename() -> Result<String> {
    Ok(format!("{}.sp", hex::encode(sodium::random_bytes(16)?)))
}

fn headers_string(sp_file: &[u8], sp_thumb: &[u8]) -> Result<String> {
    let fh = file::extract_header_bytes(&mut Cursor::new(sp_file))?;
    let th = file::extract_header_bytes(&mut Cursor::new(sp_thumb))?;
    Ok(format!("{}*{}", B64URL.encode(fh), B64URL.encode(th)))
}

impl Account {
    /// Import a single file into `set` (Gallery, or Album with `album_id`).
    /// Returns the generated encrypted filename, or `None` if already imported.
    pub async fn import_file(
        &self,
        source: &Path,
        set: FileSet,
        album_id: Option<&str>,
    ) -> Result<Option<String>> {
        // Dedup per destination: the same source file can live in the gallery
        // and also be added to one or more albums.
        let dest = album_id.unwrap_or("gallery");
        let key = format!("{}|{}|{}", source.to_string_lossy(), set.id(), dest);
        let media_id = hex::encode(sodium::sha256(key.as_bytes())?);
        if self.db.is_imported(&media_id)? {
            return Ok(None);
        }

        let data = std::fs::read(source)?;
        let orig_name = source
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();

        let is_video = VIDEO_EXTS.contains(&ext_lower(source).as_str());
        let file_type = if is_video { FILE_TYPE_VIDEO } else { FILE_TYPE_PHOTO };
        let thumb_plain = if is_video {
            thumbnail::video_thumbnail(source).or_else(|_| thumbnail::placeholder_thumbnail())?
        } else {
            thumbnail::image_thumbnail(&data).or_else(|_| thumbnail::placeholder_thumbnail())?
        };

        // The file and thumb MUST share a fileId (server enforces this).
        let file_id = file::new_file_id()?;
        let target_pk = self.target_public_key(set, album_id)?;

        let (sp_file, _) =
            file::encrypt_bytes(&data, &orig_name, file_type, file_id.clone(), 0, &target_pk)?;
        let (sp_thumb, _) =
            file::encrypt_bytes(&thumb_plain, &orig_name, file_type, file_id, 0, &target_pk)?;
        let headers = headers_string(&sp_file, &sp_thumb)?;

        let filename = new_encrypted_filename()?;
        std::fs::write(self.paths.original(&filename), &sp_file)?;
        std::fs::write(self.paths.thumb(&filename), &sp_thumb)?;

        let date = mtime_ms(source);
        let row = DbFile {
            id: 0,
            album_id: album_id.map(|s| s.to_string()),
            filename: filename.clone(),
            is_local: true,
            is_remote: false,
            version: 1,
            reupload: false,
            date_created: date,
            date_modified: date,
            headers,
        };
        if set == FileSet::Album {
            self.db.insert_album_file(&row)?;
        } else {
            self.db.insert_file(set, &row)?;
        }
        self.db.mark_imported(&media_id)?;
        Ok(Some(filename))
    }

    /// Import many files; returns the list of generated filenames.
    pub async fn import_files(
        &self,
        sources: &[std::path::PathBuf],
        set: FileSet,
        album_id: Option<&str>,
    ) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for src in sources {
            if let Some(name) = self.import_file(src, set, album_id).await? {
                out.push(name);
            }
        }
        Ok(out)
    }

    /// Recursively import all importable media under a folder.
    pub async fn import_folder(
        &self,
        dir: &Path,
        set: FileSet,
        album_id: Option<&str>,
    ) -> Result<Vec<String>> {
        let mut files = Vec::new();
        collect_media(dir, &mut files);
        self.import_files(&files, set, album_id).await
    }

    /// Import already-decoded image bytes (e.g. pasted from the clipboard).
    /// Always treated as a photo. Deduped by content hash, so pasting the same
    /// image twice is a no-op. Returns the generated filename, or `None`.
    pub async fn import_bytes(
        &self,
        data: &[u8],
        orig_name: &str,
        set: FileSet,
        album_id: Option<&str>,
    ) -> Result<Option<String>> {
        let dest = album_id.unwrap_or("gallery");
        let content = hex::encode(sodium::sha256(data)?);
        let key = format!("{}|{}|{}", content, set.id(), dest);
        let media_id = hex::encode(sodium::sha256(key.as_bytes())?);
        if self.db.is_imported(&media_id)? {
            return Ok(None);
        }

        let file_type = FILE_TYPE_PHOTO;
        let thumb_plain =
            thumbnail::image_thumbnail(data).or_else(|_| thumbnail::placeholder_thumbnail())?;
        let file_id = file::new_file_id()?;
        let target_pk = self.target_public_key(set, album_id)?;
        let (sp_file, _) =
            file::encrypt_bytes(data, orig_name, file_type, file_id.clone(), 0, &target_pk)?;
        let (sp_thumb, _) =
            file::encrypt_bytes(&thumb_plain, orig_name, file_type, file_id, 0, &target_pk)?;
        let headers = headers_string(&sp_file, &sp_thumb)?;

        let filename = new_encrypted_filename()?;
        std::fs::write(self.paths.original(&filename), &sp_file)?;
        std::fs::write(self.paths.thumb(&filename), &sp_thumb)?;

        let date = crate::util::now_ms();
        let row = DbFile {
            id: 0,
            album_id: album_id.map(|s| s.to_string()),
            filename: filename.clone(),
            is_local: true,
            is_remote: false,
            version: 1,
            reupload: false,
            date_created: date,
            date_modified: date,
            headers,
        };
        if set == FileSet::Album {
            self.db.insert_album_file(&row)?;
        } else {
            self.db.insert_file(set, &row)?;
        }
        self.db.mark_imported(&media_id)?;
        Ok(Some(filename))
    }

    /// Import raw RGBA pixels (an image read from the clipboard) by encoding to
    /// PNG first, then [`import_bytes`].
    pub async fn import_rgba(
        &self,
        rgba: &[u8],
        width: u32,
        height: u32,
        set: FileSet,
        album_id: Option<&str>,
    ) -> Result<Option<String>> {
        let img = image::RgbaImage::from_raw(width, height, rgba.to_vec())
            .ok_or_else(|| CoreError::Other("clipboard image has invalid dimensions".into()))?;
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .map_err(|e| CoreError::Other(format!("encode png: {e}")))?;
        self.import_bytes(&png, "pasted.png", set, album_id).await
    }

    /// The public key files are sealed to: user PK for gallery/trash, album PK
    /// for album imports.
    fn target_public_key(&self, set: FileSet, album_id: Option<&str>) -> Result<Vec<u8>> {
        if set == FileSet::Album {
            let aid = album_id.ok_or(CoreError::Other("album id required".into()))?;
            let a = self
                .db
                .get_album(aid)?
                .ok_or(CoreError::Other("album not found".into()))?;
            Ok(B64.decode(a.public_key.trim())?)
        } else {
            Ok(self.keypair.public_key.clone())
        }
    }

    /// Prove a just-imported gallery original is a faithful, decryptable backup
    /// of its source: decrypt the stored `.sp` **in memory** (never to disk) and
    /// confirm the plaintext exactly matches the source's length and SHA-256.
    ///
    /// This is the gate the watch-folder importer must clear before it is allowed
    /// to delete a user's original file. Returns `Ok(true)` only on an exact
    /// match; `Ok(false)` on any mismatch; `Err` if the blob can't be read or
    /// decrypted at all.
    pub fn verify_local_original(
        &self,
        filename: &str,
        expected_sha256: &[u8],
        expected_len: u64,
    ) -> Result<bool> {
        let sp = std::fs::read(self.paths.original(filename))?;
        let plain = file::decrypt_bytes(
            &sp,
            &self.keypair.public_key,
            &self.keypair.secret_key,
        )?;
        if plain.len() as u64 != expected_len {
            return Ok(false);
        }
        let actual = sodium::sha256(&plain)?;
        Ok(actual.as_slice() == expected_sha256)
    }
}

fn collect_media(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                collect_media(&p, out);
            } else if is_importable(&p) {
                out.push(p);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{Account, AccountInfo};
    use stingle_crypto::keys::KeyPair;

    /// Import a tiny gallery file and confirm `verify_local_original` only
    /// returns true for the exact source bytes — the gate the watch-folder
    /// importer must clear before deleting an original.
    #[tokio::test]
    async fn verify_local_original_roundtrips() {
        let tag = hex::encode(sodium::random_bytes(8).unwrap());
        let base = std::env::temp_dir().join(format!("stingle-verify-test-{tag}"));

        let keypair = KeyPair::generate().unwrap();
        let info = AccountInfo {
            email: "test@example.com".into(),
            user_id: "1".into(),
            home_folder: "home".into(),
            server_url: "https://api.stingle.org".into(),
            server_pk_b64: String::new(),
            key_bundle_b64: String::new(),
            token: "t".into(),
            token_enc: None,
            is_key_backed_up: false,
        };
        let acc = Account::reopen_at(&base, info, keypair, Vec::new()).unwrap();

        // A non-image .jpg is fine: the thumbnailer falls back to a placeholder.
        let src = base.join("photo.jpg");
        let payload = b"hello stingle integrity check payload";
        std::fs::write(&src, payload).unwrap();

        let filename = acc
            .import_file(&src, FileSet::Gallery, None)
            .await
            .unwrap()
            .expect("fresh import returns a filename");

        let sha = sodium::sha256(payload).unwrap();
        // Exact match passes.
        assert!(acc
            .verify_local_original(&filename, &sha, payload.len() as u64)
            .unwrap());
        // Wrong length fails closed.
        assert!(!acc.verify_local_original(&filename, &sha, 1).unwrap());
        // Wrong hash fails closed.
        let wrong = sodium::sha256(b"tampered").unwrap();
        assert!(!acc
            .verify_local_original(&filename, &wrong, payload.len() as u64)
            .unwrap());

        let _ = std::fs::remove_dir_all(&base);
    }
}
