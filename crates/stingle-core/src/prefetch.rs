//! Bulk, highly-concurrent thumbnail prefetch.
//!
//! Thumbnails are created and uploaded by whichever client imported the photo;
//! we only ever *download* them. After a sync we pull every missing thumbnail
//! concurrently (thumbnails are small, so a high fan-out finishes quickly).

use std::sync::atomic::{AtomicUsize, Ordering};

use futures::stream::{self, StreamExt};
use stingle_db::{FileSet, Sort};

use crate::account::Account;
use crate::error::Result;

impl Account {
    /// Does this file's encrypted thumbnail still need downloading?
    fn thumb_missing(&self, filename: &str) -> bool {
        let p = self.paths.thumb(filename);
        !p.exists() || std::fs::metadata(&p).map(|m| m.len() == 0).unwrap_or(true)
    }

    /// Download one encrypted thumbnail blob to disk (no decryption).
    async fn fetch_thumb_blob(&self, set: FileSet, filename: &str) -> Result<()> {
        let path = self.paths.thumb(filename);
        let bytes = self.download_limited(filename, set.id(), true).await?;
        std::fs::write(&path, &bytes)?;
        Ok(())
    }

    /// Download every missing thumbnail (gallery, trash, all albums) with up to
    /// `concurrency` requests in flight. `progress(done, total)` is called as it
    /// goes. Returns the number of thumbnails that needed downloading.
    pub async fn download_all_thumbs(
        &self,
        concurrency: usize,
        progress: Option<&(dyn Fn(usize, usize) + Send + Sync)>,
    ) -> Result<usize> {
        let mut items: Vec<(FileSet, String)> = Vec::new();
        for set in [FileSet::Gallery, FileSet::Trash] {
            for f in self.db.list_files(set, Sort::Desc, None, 0)? {
                if f.is_remote && self.thumb_missing(&f.filename) {
                    items.push((set, f.filename));
                }
            }
        }
        for f in self.db.list_all_album_files()? {
            if f.is_remote && self.thumb_missing(&f.filename) {
                items.push((FileSet::Album, f.filename));
            }
        }

        let total = items.len();
        if let Some(cb) = progress {
            cb(0, total);
        }
        if total == 0 {
            return Ok(0);
        }

        let done = AtomicUsize::new(0);
        stream::iter(items)
            .for_each_concurrent(concurrency.max(1), |(set, filename)| {
                let done = &done;
                async move {
                    // Best-effort: a single failed thumbnail shouldn't abort the batch.
                    let _ = self.fetch_thumb_blob(set, &filename).await;
                    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                    if let Some(cb) = progress {
                        if n % 16 == 0 || n == total {
                            cb(n, total);
                        }
                    }
                }
            })
            .await;

        let _ = self.enforce_cache_limit(true);
        Ok(total)
    }

    /// Is this file's encrypted original still missing from disk?
    fn original_missing(&self, filename: &str) -> bool {
        let p = self.paths.original(filename);
        !p.exists() || std::fs::metadata(&p).map(|m| m.len() == 0).unwrap_or(true)
    }

    /// Download every missing **original** (gallery, trash, all albums) with up
    /// to `concurrency` requests in flight, marking each local. Drives the
    /// "sync everything locally" option. `progress(done, total)` is called as it
    /// goes. Returns the number of originals that needed downloading.
    ///
    /// Unlike thumbnails, originals can be large, so callers should use a modest
    /// concurrency. Cache-limit enforcement is intentionally NOT run here — the
    /// whole point of this mode is to keep everything local.
    pub async fn download_all_originals(
        &self,
        concurrency: usize,
        progress: Option<&(dyn Fn(usize, usize) + Send + Sync)>,
    ) -> Result<usize> {
        let mut items: Vec<(FileSet, Option<String>, String)> = Vec::new();
        for set in [FileSet::Gallery, FileSet::Trash] {
            for f in self.db.list_files(set, Sort::Desc, None, 0)? {
                if f.is_remote && self.original_missing(&f.filename) {
                    items.push((set, None, f.filename));
                }
            }
        }
        for f in self.db.list_all_album_files()? {
            if f.is_remote && self.original_missing(&f.filename) {
                items.push((FileSet::Album, f.album_id.clone(), f.filename));
            }
        }

        let total = items.len();
        if let Some(cb) = progress {
            cb(0, total);
        }
        if total == 0 {
            return Ok(0);
        }

        let done = AtomicUsize::new(0);
        stream::iter(items)
            .for_each_concurrent(concurrency.max(1), |(set, _album, filename)| {
                let done = &done;
                async move {
                    // Best-effort: a single failed original shouldn't abort the batch.
                    let _ = self.ensure_encrypted(set, &filename, false).await;
                    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                    if let Some(cb) = progress {
                        if n % 4 == 0 || n == total {
                            cb(n, total);
                        }
                    }
                }
            })
            .await;

        Ok(total)
    }
}
