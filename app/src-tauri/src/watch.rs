//! Watch-folder auto-import.
//!
//! A polling watcher (driven by the loop in `lib.rs`) scans each configured
//! folder for new media and imports it into the gallery. If a folder has
//! `delete_originals` set, the original file is **permanently deleted — but only
//! after the encrypted import is proven successful** by a strict, multi-step
//! gate (see [`deletion_is_safe`]). Nothing decrypted is ever written to disk;
//! the integrity check decrypts in memory only.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use stingle_core::import::is_importable;
use stingle_core::{Account, FileSet};
use tauri::{AppHandle, Emitter};

use crate::config::WatchFolder;

/// A file's `(size, mtime_ms)` — the fingerprint used both for the "still being
/// copied" stability gate and the "unchanged since import" delete guard.
type Stamp = (u64, i64);

fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `(size, mtime_ms)` for `path`, or `None` if it can't be stat'd.
fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), mtime_ms(&meta)))
}

fn nonzero_file(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false)
}

fn label(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string()
}

/// Max directory depth walked when scanning a watch folder — a guard against a
/// pathologically deep tree blowing the stack.
const MAX_WALK_DEPTH: u32 = 64;

fn collect_media(dir: &Path, out: &mut Vec<PathBuf>, depth: u32) {
    if depth >= MAX_WALK_DEPTH {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            // Use the dir-entry type (does NOT follow symlinks) and skip symlinks
            // entirely, so a symlink loop can't recurse forever.
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            let p = e.path();
            if ft.is_dir() {
                collect_media(&p, out, depth + 1);
            } else if ft.is_file() && is_importable(&p) {
                out.push(p);
            }
        }
    }
}

/// One polling pass over every watched folder. `stable` persists across passes
/// so we can require a file to be unchanged for two consecutive passes before
/// touching it (it may still be mid-copy).
/// Returns the number of files newly imported this pass, so the caller can kick
/// off an upload when there's something fresh to push to the cloud.
pub(crate) async fn scan_folders(
    app: &AppHandle,
    acc: &Account,
    folders: &[WatchFolder],
    stable: &mut HashMap<PathBuf, Stamp>,
) -> usize {
    // Drop fingerprints for files that have gone away, so the map can't grow
    // without bound.
    stable.retain(|p, _| p.exists());

    let mut imported = 0;
    for folder in folders {
        let root = PathBuf::from(&folder.path);
        if !root.is_dir() {
            continue;
        }
        let mut files = Vec::new();
        collect_media(&root, &mut files, 0);
        for path in files {
            if process_one(app, acc, &path, folder.delete_originals, stable).await {
                imported += 1;
            }
        }
    }
    imported
}

/// Returns `true` when a new file was imported into the gallery this call.
async fn process_one(
    app: &AppHandle,
    acc: &Account,
    path: &Path,
    delete_originals: bool,
    stable: &mut HashMap<PathBuf, Stamp>,
) -> bool {
    let Some((size, mtime)) = stamp(path) else {
        return false;
    };

    // Stability gate: only proceed once the file looks identical to the previous
    // pass. A file still being copied changes size/mtime between passes and is
    // held back until it settles — we never import a partial file.
    match stable.get(path) {
        Some(&prev) if prev == (size, mtime) => {}
        _ => {
            stable.insert(path.to_path_buf(), (size, mtime));
            return false;
        }
    }

    // Hash the source up front; this is what the post-import integrity check
    // compares the decrypted blob against.
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(_) => return false, // locked / vanished; retry a later pass
    };
    let source_sha = match stingle_crypto::sodium::sha256(&data) {
        Ok(h) => h,
        Err(_) => return false,
    };
    let source_len = data.len() as u64;
    drop(data); // free the bytes before import_file reads the file again

    match acc.import_file(path, FileSet::Gallery, None).await {
        Ok(Some(filename)) => {
            let _ = app.emit("watch-import-progress", label(path));
            if delete_originals {
                match deletion_is_safe(acc, &filename, &source_sha, source_len, path, (size, mtime))
                {
                    Ok(true) => match std::fs::remove_file(path) {
                        Ok(()) => {
                            stable.remove(path);
                        }
                        Err(err) => {
                            let _ = app.emit(
                                "watch-import-error",
                                format!(
                                    "Imported {} but could not delete the original: {err}",
                                    label(path)
                                ),
                            );
                        }
                    },
                    Ok(false) => {
                        let _ = app.emit(
                            "watch-import-error",
                            format!(
                                "Import of {} could not be verified — original kept.",
                                label(path)
                            ),
                        );
                    }
                    Err(err) => {
                        let _ = app.emit(
                            "watch-import-error",
                            format!("Verifying {} failed: {err} — original kept.", label(path)),
                        );
                    }
                }
            }
            true
        }
        // Path-dedup duplicate: already imported earlier. We have no filename to
        // verify against, so we deliberately never delete on `None`.
        Ok(None) => false,
        Err(err) => {
            let _ = app.emit(
                "watch-import-error",
                format!("Failed to import {}: {err}", label(path)),
            );
            false
        }
    }
}

/// The deletion gate. Returns `Ok(true)` **only** when every independent check
/// confirming a complete, faithful, decryptable encrypted copy passes. Any
/// doubt → `Ok(false)` (keep the original).
fn deletion_is_safe(
    acc: &Account,
    filename: &str,
    source_sha: &[u8],
    source_len: u64,
    path: &Path,
    pre_import: Stamp,
) -> stingle_core::Result<bool> {
    // 1. Both encrypted blobs are on disk and non-empty.
    if !nonzero_file(&acc.paths.original(filename)) || !nonzero_file(&acc.paths.thumb(filename)) {
        return Ok(false);
    }
    // 2. The DB row exists and is marked local.
    match acc.db.get_file(FileSet::Gallery, filename)? {
        Some(f) if f.is_local => {}
        _ => return Ok(false),
    }
    // 3. The stored blob decrypts (in memory) back to the exact source bytes.
    if !acc.verify_local_original(filename, source_sha, source_len)? {
        return Ok(false);
    }
    // 4. The source file has not changed since we hashed it (guards against a
    //    file that was edited/replaced mid-import).
    match stamp(path) {
        Some(now) if now == pre_import => Ok(true),
        _ => Ok(false),
    }
}
