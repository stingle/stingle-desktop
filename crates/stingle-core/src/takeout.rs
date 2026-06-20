//! Takeout: decrypt the entire library to a plaintext folder.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use stingle_crypto::file;
use stingle_db::{DbFile, FileSet, Sort};

use crate::account::Account;
use crate::error::Result;
use crate::sync::headers_part;

#[derive(Debug, Default, Clone, Copy)]
pub struct TakeoutStats {
    pub written: usize,
    pub errors: usize,
}

impl Account {
    /// Decrypt the whole library into `out_dir`, organized as `gallery/` and
    /// `albums/<album name>/`, restoring original filenames where available.
    pub async fn takeout(&self, out_dir: &Path, include_trash: bool) -> Result<TakeoutStats> {
        let mut stats = TakeoutStats::default();

        let gallery_dir = out_dir.join("gallery");
        std::fs::create_dir_all(&gallery_dir)?;
        for f in self.db.list_files(FileSet::Gallery, Sort::Asc, None, 0)? {
            self.takeout_one(FileSet::Gallery, None, &f, &gallery_dir, &mut stats).await;
        }

        for album in self.db.list_albums(true)? {
            let name = self.album_name(&album).unwrap_or_else(|_| album.album_id.clone());
            let adir = out_dir.join("albums").join(sanitize(&name));
            std::fs::create_dir_all(&adir)?;
            for f in self.db.list_album_files(&album.album_id, Sort::Asc, None, 0)? {
                self.takeout_one(FileSet::Album, Some(&album.album_id), &f, &adir, &mut stats)
                    .await;
            }
        }

        if include_trash {
            let trash_dir = out_dir.join("trash");
            std::fs::create_dir_all(&trash_dir)?;
            for f in self.db.list_files(FileSet::Trash, Sort::Asc, None, 0)? {
                self.takeout_one(FileSet::Trash, None, &f, &trash_dir, &mut stats).await;
            }
        }

        Ok(stats)
    }

    async fn takeout_one(
        &self,
        set: FileSet,
        album_id: Option<&str>,
        f: &DbFile,
        dir: &Path,
        stats: &mut TakeoutStats,
    ) {
        match self.takeout_write(set, album_id, f, dir).await {
            Ok(()) => stats.written += 1,
            Err(_) => stats.errors += 1,
        }
    }

    async fn takeout_write(
        &self,
        set: FileSet,
        album_id: Option<&str>,
        f: &DbFile,
        dir: &Path,
    ) -> Result<()> {
        let plain = self.get_decrypted(set, album_id, &f.filename, false).await?;
        let name = self
            .original_name(set, album_id, &f.filename)
            .unwrap_or_else(|_| f.filename.clone());
        let out_path = unique_path(dir, &name);
        std::fs::write(&out_path, &plain)?;
        Ok(())
    }

    /// Recover the original filename stored in a file's (DB) header.
    pub fn original_name(
        &self,
        set: FileSet,
        album_id: Option<&str>,
        filename: &str,
    ) -> Result<String> {
        let headers = self.headers_for(set, album_id, filename)?;
        let part = headers_part(&headers, false)?;
        let kp = self.keypair_for(set, album_id)?;
        let header = file::read_header(&mut Cursor::new(part), &kp.public_key, &kp.secret_key)?;
        Ok(if header.filename.is_empty() {
            filename.to_string()
        } else {
            header.filename
        })
    }
}

fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if "/\\:*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Avoid clobbering files with duplicate original names.
pub(crate) fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let path = Path::new(name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = path.extension().and_then(|s| s.to_str());
    for i in 1.. {
        let alt = match ext {
            Some(e) => format!("{stem} ({i}).{e}"),
            None => format!("{stem} ({i})"),
        };
        let p = dir.join(alt);
        if !p.exists() {
            return p;
        }
    }
    candidate
}
