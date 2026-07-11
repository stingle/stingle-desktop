//! Bulk, highly-concurrent thumbnail prefetch.
//!
//! Thumbnails are created and uploaded by whichever client imported the photo;
//! we only ever *download* them. After a sync we pull every missing thumbnail
//! concurrently (thumbnails are small, so a high fan-out finishes quickly).

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::stream::{self, StreamExt};
use stingle_crypto::file::FILE_HEADER_BEGINNING_LEN;
use stingle_db::{FileSet, Sort};

use crate::account::Account;
use crate::error::Result;

/// True if `path` holds no usable encrypted blob and must be (re)downloaded:
/// absent, empty, or smaller than a complete `.sp` outer header
/// (`FILE_HEADER_BEGINNING_LEN` = 39 bytes).
///
/// The size floor is what catches a **poisoned cache**: `sync/download` returns
/// its `{"status":"nok"}` error envelope with HTTP 200, and a build predating the
/// `is_sp_blob` download guard wrote those 16-byte bodies to disk in place of
/// thumbnails. The old `len == 0` check treated them as present, so the bulk
/// prefetch skipped them forever and the tiles stayed permanently broken. A real
/// `.sp` can never be shorter than its own outer header, so anything below the
/// floor is safe to discard and re-fetch. This is a stat-only check (no open) so
/// it stays cheap across a 25k+ item library; the full "SP" magic is validated
/// on the on-demand serving path (`sp_magic_ok` in `sync.rs`).
fn blob_incomplete(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(m) => m.len() < FILE_HEADER_BEGINNING_LEN as u64,
        Err(_) => true,
    }
}

impl Account {
    /// Does this file's encrypted thumbnail still need downloading?
    fn thumb_missing(&self, filename: &str) -> bool {
        blob_incomplete(&self.paths.thumb(filename))
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
                    // Hold a bulk permit so this prefetch never occupies every
                    // download lane — on-demand thumbnail requests stay snappy.
                    let _bulk = match self.bulk_sem.acquire().await {
                        Ok(p) => p,
                        Err(_) => return,
                    };
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
        blob_incomplete(&self.paths.original(filename))
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

        // Fresh start: clear any stale cancellation from a previous toggle.
        self.stop_originals.store(false, Ordering::Relaxed);

        let done = AtomicUsize::new(0);
        stream::iter(items)
            .for_each_concurrent(concurrency.max(1), |(set, _album, filename)| {
                let done = &done;
                async move {
                    // Stop promptly if the user turned "keep originals locally" off
                    // mid-run. Already-queued items just fall through as no-ops.
                    if self.stop_originals.load(Ordering::Relaxed) {
                        return;
                    }
                    // Reserve download lanes for on-demand requests (see bulk_sem).
                    let _bulk = match self.bulk_sem.acquire().await {
                        Ok(p) => p,
                        Err(_) => return,
                    };
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

#[cfg(test)]
mod tests {
    use super::{blob_incomplete, FILE_HEADER_BEGINNING_LEN};

    #[test]
    fn incomplete_detects_absent_empty_and_poisoned_blobs() {
        let dir = std::env::temp_dir().join(format!("sp-prefetch-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Absent file.
        let absent = dir.join("absent.sp");
        assert!(blob_incomplete(&absent));

        // Empty file.
        let empty = dir.join("empty.sp");
        std::fs::write(&empty, b"").unwrap();
        assert!(blob_incomplete(&empty));

        // The exact server error body a pre-guard build cached in place of a
        // thumbnail (16 bytes, HTTP 200) — must be treated as needing re-download.
        let poison = dir.join("poison.sp");
        std::fs::write(&poison, br#"{"status":"nok"}"#).unwrap();
        assert!(blob_incomplete(&poison));

        // A blob at least as large as a full `.sp` outer header is trusted at the
        // planning stage (the on-demand path validates the "SP" magic).
        let ok = dir.join("ok.sp");
        std::fs::write(&ok, vec![0u8; FILE_HEADER_BEGINNING_LEN]).unwrap();
        assert!(!blob_incomplete(&ok));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
