//! File operations: trash, restore, permanent delete, empty trash, save
//! (decrypt-and-export), and move-to-album.

use std::io::Cursor;
use std::path::Path;

use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64URL};
use base64::Engine;
use stingle_crypto::file;
use stingle_db::{DbFile, FileSet, Sort};

use crate::account::Account;
use crate::error::{CoreError, Result};
use crate::sync::headers_part;
use crate::takeout::unique_path;
use crate::util::now_ms;

impl Account {
    /// Move gallery files to trash.
    pub async fn trash(&self, filenames: &[String]) -> Result<()> {
        self.move_between(FileSet::Gallery, FileSet::Trash, filenames).await
    }

    /// Restore files from trash to the gallery.
    pub async fn restore(&self, filenames: &[String]) -> Result<()> {
        self.move_between(FileSet::Trash, FileSet::Gallery, filenames).await
    }

    async fn move_between(&self, from: FileSet, to: FileSet, filenames: &[String]) -> Result<()> {
        let mut rows = Vec::new();
        let mut remote: Vec<(String, Option<String>)> = Vec::new();
        for name in filenames {
            if let Some(row) = self.db.get_file(from, name)? {
                if row.is_remote {
                    remote.push((name.clone(), None));
                }
                rows.push(row);
            }
        }
        if !remote.is_empty() {
            self.client
                .move_files(
                    self.token(),
                    from.id(),
                    to.id(),
                    "",
                    "",
                    true,
                    &remote,
                    self.server_crypto(),
                )
                .await?;
        }
        for row in rows {
            let mut moved = row.clone();
            moved.id = 0;
            self.db.insert_file(to, &moved)?;
            self.db.delete_file(from, &row.filename)?;
        }
        Ok(())
    }

    /// Permanently delete files from trash (server + local blob + db row).
    pub async fn delete_permanently(&self, filenames: &[String]) -> Result<()> {
        let remote: Vec<String> = filenames
            .iter()
            .filter_map(|n| {
                self.db
                    .get_file(FileSet::Trash, n)
                    .ok()
                    .flatten()
                    .filter(|r| r.is_remote)
                    .map(|_| n.clone())
            })
            .collect();
        if !remote.is_empty() {
            self.client
                .delete_files(self.token(), &remote, self.server_crypto())
                .await?;
        }
        for name in filenames {
            self.db.delete_file(FileSet::Trash, name)?;
            self.remove_local_files(name);
        }
        Ok(())
    }

    /// Empty the trash entirely.
    pub async fn empty_trash(&self) -> Result<()> {
        self.client
            .empty_trash(self.token(), now_ms(), self.server_crypto())
            .await?;
        for f in self.db.list_files(FileSet::Trash, Sort::Asc, None, 0)? {
            self.db.delete_file(FileSet::Trash, &f.filename)?;
            self.remove_local_files(&f.filename);
        }
        Ok(())
    }

    /// Decrypt and export files to a user-chosen folder (the explicit
    /// "decrypt & save"). Original filenames are restored. Returns the count.
    pub async fn save_files(
        &self,
        set: FileSet,
        album_id: Option<&str>,
        filenames: &[String],
        dest_dir: &Path,
    ) -> Result<usize> {
        std::fs::create_dir_all(dest_dir)?;
        let mut n = 0;
        for name in filenames {
            let plain = self.get_decrypted(set, album_id, name, false).await?;
            let orig = self
                .original_name(set, album_id, name)
                .unwrap_or_else(|_| name.clone());
            let out = unique_path(dest_dir, &orig);
            std::fs::write(&out, &plain)?;
            n += 1;
        }
        Ok(n)
    }

    /// Move or copy files into an album (re-sealing their headers to the album
    /// key). `from_album` is set only when moving between albums. When
    /// `is_moving` is false the source copy is kept (copy instead of move).
    pub async fn move_to_album(
        &self,
        from_set: FileSet,
        from_album: Option<&str>,
        filenames: &[String],
        to_album: &str,
        is_moving: bool,
    ) -> Result<()> {
        let to = self
            .db
            .get_album(to_album)?
            .ok_or(CoreError::Other("target album not found".into()))?;
        let to_pk = B64.decode(to.public_key.trim())?;

        let mut server_files: Vec<(String, Option<String>)> = Vec::new();
        let mut rows: Vec<(DbFile, String)> = Vec::new();
        for name in filenames {
            let row = match from_set {
                FileSet::Album => self.db.get_album_file(from_album.unwrap_or(""), name)?,
                _ => self.db.get_file(from_set, name)?,
            };
            let Some(row) = row else { continue };
            let new_headers = self.reseal_headers(&row.headers, from_set, from_album, &to_pk)?;
            if row.is_remote {
                server_files.push((name.clone(), Some(new_headers.clone())));
            }
            rows.push((row, new_headers));
        }

        self.client
            .move_files(
                self.token(),
                from_set.id(),
                FileSet::Album.id(),
                from_album.unwrap_or(""),
                to_album,
                is_moving,
                &server_files,
                self.server_crypto(),
            )
            .await?;

        for (row, new_headers) in rows {
            self.db.insert_album_file(&DbFile {
                id: 0,
                album_id: Some(to_album.to_string()),
                filename: row.filename.clone(),
                is_local: row.is_local,
                is_remote: row.is_remote,
                version: row.version,
                reupload: row.reupload,
                headers: new_headers,
                date_created: row.date_created,
                date_modified: row.date_modified,
                // Re-sealing changes the header's recipient, not the content type.
                is_video: row.is_video,
            })?;
            // Copy keeps the source; move removes it.
            if is_moving {
                match from_set {
                    FileSet::Album => self.db.delete_album_file(from_album.unwrap_or(""), &row.filename)?,
                    _ => self.db.delete_file(from_set, &row.filename)?,
                }
            }
        }
        Ok(())
    }

    /// Move or copy album files back to the gallery (re-sealing headers to the
    /// user key). When `is_moving` is false the album copy is kept.
    pub async fn move_to_gallery(&self, from_album: &str, filenames: &[String], is_moving: bool) -> Result<()> {
        let user_pk = self.keypair.public_key.clone();
        let mut server_files: Vec<(String, Option<String>)> = Vec::new();
        let mut rows: Vec<(DbFile, String)> = Vec::new();
        for name in filenames {
            if let Some(row) = self.db.get_album_file(from_album, name)? {
                let new_headers =
                    self.reseal_headers(&row.headers, FileSet::Album, Some(from_album), &user_pk)?;
                if row.is_remote {
                    server_files.push((name.clone(), Some(new_headers.clone())));
                }
                rows.push((row, new_headers));
            }
        }
        self.client
            .move_files(
                self.token(),
                FileSet::Album.id(),
                FileSet::Gallery.id(),
                from_album,
                "",
                is_moving,
                &server_files,
                self.server_crypto(),
            )
            .await?;
        for (row, new_headers) in rows {
            self.db.insert_file(
                FileSet::Gallery,
                &DbFile {
                    id: 0,
                    album_id: None,
                    filename: row.filename.clone(),
                    is_local: row.is_local,
                    is_remote: row.is_remote,
                    version: row.version,
                    reupload: row.reupload,
                    headers: new_headers,
                    date_created: row.date_created,
                    date_modified: row.date_modified,
                    is_video: row.is_video,
                },
            )?;
            // Copy keeps the album entry; move removes it.
            if is_moving {
                self.db.delete_album_file(from_album, &row.filename)?;
            }
        }
        Ok(())
    }

    /// Move files to trash from any context. Gallery/trash files keep their
    /// (user-sealed) headers; album files are re-sealed back to the user key.
    pub async fn trash_from(
        &self,
        from_set: FileSet,
        from_album: Option<&str>,
        filenames: &[String],
    ) -> Result<()> {
        if from_set != FileSet::Album {
            return self.trash(filenames).await;
        }
        let album = from_album.unwrap_or("");
        let user_pk = self.keypair.public_key.clone();
        let mut server_files: Vec<(String, Option<String>)> = Vec::new();
        let mut rows: Vec<(DbFile, String)> = Vec::new();
        for name in filenames {
            if let Some(row) = self.db.get_album_file(album, name)? {
                let new_headers =
                    self.reseal_headers(&row.headers, FileSet::Album, Some(album), &user_pk)?;
                if row.is_remote {
                    server_files.push((name.clone(), Some(new_headers.clone())));
                }
                rows.push((row, new_headers));
            }
        }
        self.client
            .move_files(
                self.token(),
                FileSet::Album.id(),
                FileSet::Trash.id(),
                album,
                "",
                true,
                &server_files,
                self.server_crypto(),
            )
            .await?;
        for (row, new_headers) in rows {
            self.db.insert_file(
                FileSet::Trash,
                &DbFile {
                    id: 0,
                    album_id: None,
                    filename: row.filename.clone(),
                    is_local: row.is_local,
                    is_remote: row.is_remote,
                    version: row.version,
                    reupload: row.reupload,
                    headers: new_headers,
                    date_created: row.date_created,
                    date_modified: row.date_modified,
                    is_video: row.is_video,
                },
            )?;
            self.db.delete_album_file(album, &row.filename)?;
        }
        Ok(())
    }

    /// Re-seal a file's `headers` (both file and thumb parts) from the source
    /// keypair to a new recipient public key. The data stays encrypted with the
    /// same symmetric key, so only the (small) headers change.
    fn reseal_headers(
        &self,
        headers: &str,
        from_set: FileSet,
        from_album: Option<&str>,
        to_pk: &[u8],
    ) -> Result<String> {
        let kp = self.keypair_for(from_set, from_album)?;
        let file_part = headers_part(headers, false)?;
        let thumb_part = headers_part(headers, true)?;
        let fh = file::read_header(&mut Cursor::new(&file_part), &kp.public_key, &kp.secret_key)?;
        let th = file::read_header(&mut Cursor::new(&thumb_part), &kp.public_key, &kp.secret_key)?;
        let f_new = fh.serialize(to_pk)?;
        let t_new = th.serialize(to_pk)?;
        Ok(format!("{}*{}", B64URL.encode(f_new), B64URL.encode(t_new)))
    }
}
