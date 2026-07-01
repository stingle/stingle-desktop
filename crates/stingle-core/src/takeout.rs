//! Takeout: decrypt the entire library to a plaintext folder.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

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
    ///
    /// `progress(done, total)` is called as each file is written so the UI can
    /// show a live count. The pass stops promptly if [`request_stop_takeout`]
    /// is called mid-run, returning the partial stats gathered so far.
    ///
    /// [`request_stop_takeout`]: Account::request_stop_takeout
    pub async fn takeout(
        &self,
        out_dir: &Path,
        include_trash: bool,
        progress: Option<&(dyn Fn(usize, usize) + Send + Sync)>,
    ) -> Result<TakeoutStats> {
        let mut stats = TakeoutStats::default();

        // Fresh start: clear any stale cancellation from a previous run.
        self.stop_takeout.store(false, Ordering::Relaxed);

        // Build the full work list up front so `total` is known for progress.
        // Each item carries its destination directory (created lazily below).
        let mut items: Vec<(FileSet, Option<String>, DbFile, PathBuf)> = Vec::new();

        let gallery_dir = out_dir.join("gallery");
        for f in self.db.list_files(FileSet::Gallery, Sort::Asc, None, 0)? {
            items.push((FileSet::Gallery, None, f, gallery_dir.clone()));
        }

        for album in self.db.list_albums(true)? {
            let name = self.album_name(&album).unwrap_or_else(|_| album.album_id.clone());
            let adir = out_dir.join("albums").join(sanitize(&name));
            for f in self.db.list_album_files(&album.album_id, Sort::Asc, None, 0)? {
                items.push((FileSet::Album, Some(album.album_id.clone()), f, adir.clone()));
            }
        }

        if include_trash {
            let trash_dir = out_dir.join("trash");
            for f in self.db.list_files(FileSet::Trash, Sort::Asc, None, 0)? {
                items.push((FileSet::Trash, None, f, trash_dir.clone()));
            }
        }

        let total = items.len();
        if let Some(cb) = progress {
            cb(0, total);
        }

        let mut done = 0usize;
        for (set, album_id, f, dir) in &items {
            if self.stop_takeout.load(Ordering::Relaxed) {
                break;
            }
            std::fs::create_dir_all(dir)?;
            self.takeout_one(*set, album_id.as_deref(), f, dir, &mut stats).await;
            done += 1;
            if let Some(cb) = progress {
                cb(done, total);
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
    // `.`/`..` are valid filenames but, used as a path component, would point at
    // the current/parent directory — never let them through as a name.
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        "untitled".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Reduce an untrusted, header-derived filename to a safe bare filename for use
/// as the final component of an output path.
///
/// The original filename inside a `.sp` header is sealed with an *anonymous*
/// `crypto_box_seal`, so anyone who knows the recipient public key (the server,
/// a MITM, or a malicious album sharer) can fabricate a header with any
/// filename — including one containing `../`, a leading `/`, or `C:\…`. Writing
/// decrypted bytes to `dir.join(that)` would escape `dir` and could drop a file
/// into e.g. the Startup folder. We therefore keep only the final path
/// component and neutralize separators / illegal characters.
pub fn safe_filename(name: &str) -> String {
    // Take the last component, splitting on BOTH separators (a Windows header
    // opened on Linux keeps `\` as a literal char, and vice-versa).
    let last = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned = sanitize(last);
    if cleaned == "." || cleaned == ".." {
        "untitled".to_string()
    } else {
        cleaned
    }
}

/// Avoid clobbering files with duplicate original names. The name is first run
/// through [`safe_filename`], so an untrusted/header-derived `name` can never
/// escape `dir`.
pub(crate) fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let name = &safe_filename(name);
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

#[cfg(test)]
mod tests {
    use super::safe_filename;
    use std::path::Path;

    #[test]
    fn reduces_to_a_safe_basename() {
        assert_eq!(safe_filename("normal.jpg"), "normal.jpg");
        assert_eq!(safe_filename("../../etc/passwd"), "passwd");
        assert_eq!(safe_filename("..\\..\\evil.exe"), "evil.exe");
        assert_eq!(safe_filename("/abs/x.jpg"), "x.jpg");
        assert_eq!(safe_filename("C:\\Users\\a\\Startup\\x.exe"), "x.exe");
        assert_eq!(safe_filename(".."), "untitled");
        assert_eq!(safe_filename(""), "untitled");
    }

    #[test]
    fn result_never_escapes_dir() {
        let base = Path::new("/base/out");
        for evil in ["../../e", "..\\..\\e", "/etc/p", "C:\\x\\y"] {
            let joined = base.join(safe_filename(evil));
            assert!(joined.starts_with(base), "{evil} escaped to {joined:?}");
        }
    }
}
