//! Encrypted-cache size management.
//!
//! The on-disk cache is the encrypted `.sp` blobs in `originals/` and `thumbs/`.
//! When a size limit is set and exceeded, the oldest re-downloadable files are
//! evicted — always removing a file AND its thumbnail together. Files that are
//! local-only (imported, not yet uploaded) are never evicted (they're the only
//! copy).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use stingle_db::{FileSet, Sort};

use crate::account::Account;
use crate::error::Result;
use crate::util::now_ms;

/// Grace period before an unreferenced blob is eligible for orphan pruning, so a
/// blob a concurrent import just wrote (but hasn't recorded in the DB yet) is
/// never deleted out from under it.
const ORPHAN_GRACE: Duration = Duration::from_secs(600);

fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

impl Account {
    /// Configured cache limit in bytes (0 = unlimited).
    pub fn cache_limit_bytes(&self) -> i64 {
        self.db.kv_get_i64("cache_limit_bytes").ok().flatten().unwrap_or(0)
    }

    /// Set the cache limit (bytes; 0 = unlimited) and enforce it immediately.
    pub fn set_cache_limit_bytes(&self, bytes: i64) -> Result<()> {
        self.db.kv_set_i64("cache_limit_bytes", bytes.max(0))?;
        self.enforce_cache_limit(true)?;
        Ok(())
    }

    /// Current total size of the encrypted cache (originals + thumbnails).
    pub fn cache_size_bytes(&self) -> u64 {
        dir_size(&self.paths.originals_dir()) + dir_size(&self.paths.thumbs_dir())
    }

    /// Filenames that exist on the server and so can be safely evicted.
    fn remote_filenames(&self) -> Result<HashSet<String>> {
        let mut set = HashSet::new();
        for f in self.db.list_files(FileSet::Gallery, Sort::Desc, None, 0)? {
            if f.is_remote {
                set.insert(f.filename);
            }
        }
        for f in self.db.list_files(FileSet::Trash, Sort::Desc, None, 0)? {
            if f.is_remote {
                set.insert(f.filename);
            }
        }
        for f in self.db.list_all_album_files()? {
            if f.is_remote {
                set.insert(f.filename);
            }
        }
        Ok(set)
    }

    /// Delete all evictable (re-downloadable) cached files and their thumbnails.
    pub fn clear_cache(&self) -> Result<()> {
        for name in self.remote_filenames()? {
            self.remove_local_files(&name);
        }
        Ok(())
    }

    /// Delete cached encrypted blobs (originals + thumbnails) that no DB row
    /// references any more. Album / album-file deletes (and leaving an album)
    /// drop rows but not their on-disk blobs; this reclaims them.
    ///
    /// A blob filename can be shared by several rows (e.g. a gallery file also
    /// copied into an album reuse the same `.sp`), so a file is removed ONLY
    /// when nothing references it. Returns the number of files deleted.
    pub fn prune_orphan_blobs(&self) -> Result<usize> {
        let mut referenced: HashSet<String> = HashSet::new();
        for set in [FileSet::Gallery, FileSet::Trash] {
            for f in self.db.list_files(set, Sort::Desc, None, 0)? {
                referenced.insert(f.filename);
            }
        }
        for f in self.db.list_all_album_files()? {
            referenced.insert(f.filename);
        }

        let mut removed = 0;
        for dir in [self.paths.originals_dir(), self.paths.thumbs_dir()] {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let Ok(meta) = e.metadata() else { continue };
                if !meta.is_file() {
                    continue;
                }
                // Skip a blob young enough that a concurrent import may have just
                // written it but not yet recorded the DB row (avoids a race that
                // would delete a freshly-imported file).
                let too_new = meta
                    .modified()
                    .ok()
                    .and_then(|m| m.elapsed().ok())
                    .map(|age| age < ORPHAN_GRACE)
                    .unwrap_or(true);
                if too_new {
                    continue;
                }
                let name = e.file_name().to_string_lossy().to_string();
                if !referenced.contains(&name) && std::fs::remove_file(e.path()).is_ok() {
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    /// Evict oldest cached files until under the configured limit. `force`
    /// bypasses the 5s throttle (used after sync / when the limit changes).
    pub fn enforce_cache_limit(&self, force: bool) -> Result<()> {
        let limit = self.cache_limit_bytes();
        if limit <= 0 {
            return Ok(()); // unlimited
        }
        let now = now_ms();
        if !force {
            let last = self.last_cache_check_ms.load(Ordering::Relaxed);
            if now - last < 5_000 {
                return Ok(());
            }
        }
        self.last_cache_check_ms.store(now, Ordering::Relaxed);

        let limit = limit as u64;

        // Combine each filename's original + thumbnail into one cache entry,
        // tracking its oldest mtime.
        let mut entries: HashMap<String, (u64, SystemTime)> = HashMap::new();
        for dir in [self.paths.originals_dir(), self.paths.thumbs_dir()] {
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let Ok(meta) = e.metadata() else { continue };
                    if !meta.is_file() {
                        continue;
                    }
                    let name = e.file_name().to_string_lossy().to_string();
                    let t = meta.modified().unwrap_or(UNIX_EPOCH);
                    let ent = entries.entry(name).or_insert((0, t));
                    ent.0 += meta.len();
                    if t < ent.1 {
                        ent.1 = t;
                    }
                }
            }
        }

        let mut total: u64 = entries.values().map(|(s, _)| *s).sum();
        if total <= limit {
            return Ok(());
        }

        let remote = self.remote_filenames()?;
        let mut evictable: Vec<(String, u64, SystemTime)> = entries
            .into_iter()
            .filter(|(name, _)| remote.contains(name))
            .map(|(name, (s, t))| (name, s, t))
            .collect();
        // Oldest first.
        evictable.sort_by(|a, b| a.2.cmp(&b.2));

        for (name, size, _) in evictable {
            if total <= limit {
                break;
            }
            self.remove_local_files(&name); // deletes original AND thumbnail
            total = total.saturating_sub(size);
        }
        Ok(())
    }
}
