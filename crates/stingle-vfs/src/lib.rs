//! # stingle-vfs
//!
//! A **read-only, in-memory** virtual filesystem over an unlocked Stingle
//! library. It exists so that any OS file-open dialog (or file manager) can
//! browse into `Gallery` / `Albums` and pick a photo/video to hand to another
//! app (e.g. a website's upload field) — without ever materializing decrypted
//! bytes on disk.
//!
//! - [`tree`] is the platform-agnostic directory index (folders + `stat` data),
//!   built once per mount from the local DB. It holds no keys and no plaintext.
//! - [`ops`] adds the byte-serving `read` path, which pulls decrypted windows
//!   from `Account::media_response` in memory and persists nothing.
//!
//! Platform driver adapters (WinFsp on Windows, macFUSE/FUSE on unix) wrap a
//! [`Vfs`] and forward `getattr`/`lookup`/`readdir`/`read` to it. They live
//! behind the `mount-winfsp` / `mount-fuse` features (added in later
//! milestones); the default build is just this platform-agnostic core, which
//! compiles and tests on every platform.
//!
//! ## Security
//!
//! This crate keeps the project rule that *we* never write plaintext to disk:
//! decryption happens in ≤ 4 MiB windows in RAM on each `read`. Making the
//! library browsable does, however, expose it to OS thumbnailers, indexers, AV,
//! and the consuming app — side channels outside this crate's control. The
//! feature is therefore opt-in and gated by a warning in the app shell, not
//! here.

mod ops;
mod tree;

#[cfg(all(windows, feature = "mount-winfsp"))]
mod winfsp;

#[cfg(all(unix, feature = "mount-fuse"))]
mod fuse;

pub use ops::{AccountSource, MediaSource, Vfs};
pub use tree::{Attr, Dirent, Entry, Leaf, Section, Tree, ROOT_INO};

use std::collections::BTreeSet;

use stingle_core::{safe_filename, Account, FileSet, Sort};

/// How to mount the virtual filesystem.
#[derive(Debug, Clone)]
pub struct MountConfig {
    /// The mount point: a drive spec like `"S:"` on Windows, a directory path
    /// on unix.
    pub mount_point: String,
    /// Whether to expose the Trash section (off by default).
    pub include_trash: bool,
}

/// Why a mount attempt failed.
#[derive(Debug)]
pub enum MountError {
    /// No filesystem driver is built into this binary for the current platform
    /// (or the `mount-winfsp` / future `mount-fuse` feature is off).
    Unsupported,
    /// The platform driver rejected the mount — WinFsp not installed, the drive
    /// letter/mount point is busy, permission denied, etc.
    Driver(String),
}

impl std::fmt::Display for MountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MountError::Unsupported => {
                write!(f, "no virtual-filesystem driver is available in this build")
            }
            MountError::Driver(msg) => write!(f, "virtual-filesystem driver error: {msg}"),
        }
    }
}

impl std::error::Error for MountError {}

/// A live mount of the virtual filesystem. Dropping it unmounts and tears down
/// the driver dispatcher. Cross-platform wrapper over the per-OS adapters.
pub struct VfsMount {
    #[cfg(all(windows, feature = "mount-winfsp"))]
    _inner: winfsp::WinFspMount,
    #[cfg(all(unix, feature = "mount-fuse"))]
    _inner: fuse::FuseMount,
}

impl VfsMount {
    /// Mount `vfs` read-only per `cfg`. Returns [`MountError::Unsupported`] when
    /// this build has no driver for the current platform. Exactly one of the
    /// three cfg arms below is compiled for any target/feature combination.
    pub fn mount(vfs: Vfs, cfg: &MountConfig) -> Result<Self, MountError> {
        #[cfg(all(windows, feature = "mount-winfsp"))]
        {
            winfsp::WinFspMount::mount(vfs, &cfg.mount_point)
                .map(|inner| VfsMount { _inner: inner })
                .map_err(|e| MountError::Driver(format!("{e:?}")))
        }
        #[cfg(all(unix, feature = "mount-fuse"))]
        {
            fuse::FuseMount::mount(vfs, &cfg.mount_point)
                .map(|inner| VfsMount { _inner: inner })
                .map_err(|e| MountError::Driver(e.to_string()))
        }
        #[cfg(not(any(
            all(windows, feature = "mount-winfsp"),
            all(unix, feature = "mount-fuse")
        )))]
        {
            let _ = (vfs, cfg);
            Err(MountError::Unsupported)
        }
    }
}

/// Enumerate the unlocked library into flat [`Entry`]s for [`Tree::build`].
///
/// Walks the same sets as `Account::takeout` — gallery, every album, and
/// (optionally) trash — decoding each row's header in memory via
/// `Account::row_header_meta` for the display name and size. Album folder names
/// are decoded, sanitized, and de-duplicated here so two distinct albums that
/// share a display name still get distinct folders. Rows whose header can't be
/// decoded are skipped (they couldn't be served anyway).
pub fn collect_entries(acc: &Account, include_trash: bool) -> Vec<Entry> {
    let mut entries = Vec::new();

    // Gallery.
    if let Ok(files) = acc.db.list_files(FileSet::Gallery, Sort::Asc, None, 0) {
        for f in files {
            if let Ok(meta) = acc.row_header_meta(FileSet::Gallery, None, &f.headers) {
                let name = display_name(meta.original_filename, &f.filename);
                entries.push(Entry {
                    section: Section::Gallery,
                    set: FileSet::Gallery,
                    album_id: None,
                    enc_filename: f.filename,
                    original_name: name,
                    size: meta.data_size,
                    date_created_ms: f.date_created,
                });
            }
        }
    }

    // Albums. `include_hidden = true` mirrors takeout (this is the user's own
    // data, already behind the feature's warning); revisit if hidden albums
    // should stay hidden here.
    let mut used_album_dirs = BTreeSet::new();
    if let Ok(albums) = acc.db.list_albums(true) {
        for a in albums {
            let raw = acc.album_name(&a).unwrap_or_else(|_| a.album_id.clone());
            let dir = dedup_dir_name(&mut used_album_dirs, &safe_filename(&raw));
            if let Ok(files) = acc.db.list_album_files(&a.album_id, Sort::Asc, None, 0) {
                for f in files {
                    if let Ok(meta) = acc.row_header_meta(FileSet::Album, Some(&a.album_id), &f.headers)
                    {
                        let name = display_name(meta.original_filename, &f.filename);
                        entries.push(Entry {
                            section: Section::Album(dir.clone()),
                            set: FileSet::Album,
                            album_id: Some(a.album_id.clone()),
                            enc_filename: f.filename,
                            original_name: name,
                            size: meta.data_size,
                            date_created_ms: f.date_created,
                        });
                    }
                }
            }
        }
    }

    // Trash (opt-in).
    if include_trash {
        if let Ok(files) = acc.db.list_files(FileSet::Trash, Sort::Asc, None, 0) {
            for f in files {
                if let Ok(meta) = acc.row_header_meta(FileSet::Trash, None, &f.headers) {
                    let name = display_name(meta.original_filename, &f.filename);
                    entries.push(Entry {
                        section: Section::Trash,
                        set: FileSet::Trash,
                        album_id: None,
                        enc_filename: f.filename,
                        original_name: name,
                        size: meta.data_size,
                        date_created_ms: f.date_created,
                    });
                }
            }
        }
    }

    entries
}

/// The header's original filename, falling back to the encrypted name when the
/// header stored none. Final sanitization happens in [`Tree::build`].
fn display_name(original: String, enc: &str) -> String {
    if original.is_empty() {
        enc.to_string()
    } else {
        original
    }
}

/// De-duplicate a directory name within `used`, appending ` (1)`, ` (2)`, … on
/// collision. Album folders have no extension, so the suffix goes on the end.
fn dedup_dir_name(used: &mut BTreeSet<String>, name: &str) -> String {
    if used.insert(name.to_string()) {
        return name.to_string();
    }
    for i in 1.. {
        let candidate = format!("{name} ({i})");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("i32 range exhausted while de-duplicating album folders")
}
