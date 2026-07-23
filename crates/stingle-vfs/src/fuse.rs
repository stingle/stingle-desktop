//! FUSE adapter (Linux + macOS via macFUSE/FUSE-T): presents a [`Vfs`] as a
//! read-only mount at a directory path.
//!
//! Implements `fuser::Filesystem` over the platform-agnostic [`Vfs`] core — the
//! same tree, sizes, and windowed decryption the Windows adapter uses. Mounts
//! read-only; reads are served in-memory and nothing is persisted.
//!
//! Pinned to the fuser 0.14 API. `fuser` links libfuse at build time
//! (pkg-config), so building this needs libfuse dev files; running a mount needs
//! the FUSE runtime (Linux `fuse3`, macOS macFUSE/FUSE-T).

use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    BackgroundSession, FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData,
    ReplyDirectory, ReplyEntry, ReplyOpen, ReplyStatfs, Request,
};

use crate::ops::Vfs;
use crate::tree::Attr;

/// Attribute/entry cache lifetime handed to the kernel. Matches the Windows
/// adapter's ~1s info timeout: long enough to avoid re-`stat` storms while a
/// picker scrolls, short enough to pick up a post-sync tree rebuild.
const TTL: Duration = Duration::from_secs(1);

const BLOCK_SIZE: u32 = 4096;

/// Convert epoch-milliseconds to a `SystemTime` (handles pre-1970 dates).
fn to_systime(ms: i64) -> SystemTime {
    if ms >= 0 {
        UNIX_EPOCH + Duration::from_millis(ms as u64)
    } else {
        UNIX_EPOCH - Duration::from_millis((-ms) as u64)
    }
}

/// The FUSE filesystem: the read-only [`Vfs`] plus the mounting user's ids.
struct StingleFuse {
    vfs: Vfs,
    uid: u32,
    gid: u32,
}

impl StingleFuse {
    fn to_file_attr(&self, a: Attr) -> FileAttr {
        let t = to_systime(a.mtime_ms);
        FileAttr {
            ino: a.ino,
            size: a.size,
            blocks: a.size.div_ceil(512),
            atime: t,
            mtime: t,
            ctime: t,
            crtime: t,
            kind: if a.is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if a.is_dir { 0o555 } else { 0o444 },
            nlink: a.nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: BLOCK_SIZE,
            flags: 0,
        }
    }
}

impl Filesystem for StingleFuse {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name.to_str() else {
            reply.error(libc::ENOENT);
            return;
        };
        match self.vfs.tree.lookup(parent, name) {
            Some(attr) => reply.entry(&TTL, &self.to_file_attr(attr), 0),
            None => reply.error(libc::ENOENT),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
        match self.vfs.tree.attr(ino) {
            Some(attr) => reply.attr(&TTL, &self.to_file_attr(attr)),
            None => reply.error(libc::ENOENT),
        }
    }

    fn open(&mut self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
        reply.opened(0, 0);
    }

    fn opendir(&mut self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
        reply.opened(0, 0);
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        match self.vfs.read(ino, offset.max(0) as u64, size) {
            Ok(bytes) => reply.data(&bytes),
            Err(e) => {
                let errno = match e.kind() {
                    // `read` on a directory / non-leaf inode.
                    std::io::ErrorKind::NotFound => libc::EISDIR,
                    _ => libc::EIO,
                };
                reply.error(errno);
            }
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let Some(attr) = self.vfs.tree.attr(ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        if !attr.is_dir {
            reply.error(libc::ENOTDIR);
            return;
        }

        // "." and ".." then the sorted children.
        let parent = self.vfs.tree.parent(ino);
        let mut entries: Vec<(u64, FileType, String)> = vec![
            (ino, FileType::Directory, ".".to_string()),
            (parent, FileType::Directory, "..".to_string()),
        ];
        if let Some(children) = self.vfs.tree.children(ino) {
            for d in children {
                let kind = if d.is_dir {
                    FileType::Directory
                } else {
                    FileType::RegularFile
                };
                entries.push((d.ino, kind, d.name));
            }
        }

        // Resume after `offset`; the cookie we hand back is the next index.
        for (i, (e_ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(e_ino, (i + 1) as i64, kind, name) {
                break; // reply buffer is full
            }
        }
        reply.ok();
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        // Virtual, read-only: advertise a large capacity, no free space.
        reply.statfs(1 << 30, 0, 0, 1 << 20, 0, BLOCK_SIZE, 255, BLOCK_SIZE);
    }
}

/// A live FUSE mount. Dropping it unmounts and removes the mount directory if we
/// created it.
pub struct FuseMount {
    session: Option<BackgroundSession>,
    mountpoint: PathBuf,
    created_dir: bool,
}

impl FuseMount {
    /// Mount `vfs` read-only at the directory `mountpoint` (created if missing),
    /// serving on a background thread.
    pub fn mount(vfs: Vfs, mountpoint: &str) -> std::io::Result<Self> {
        let path = PathBuf::from(mountpoint);
        let created_dir = if path.exists() {
            false
        } else {
            std::fs::create_dir_all(&path)?;
            true
        };

        let uid = unsafe { libc::getuid() } as u32;
        let gid = unsafe { libc::getgid() } as u32;
        let fs = StingleFuse { vfs, uid, gid };

        let mut options = vec![
            MountOption::RO,
            MountOption::FSName("Stingle".to_string()),
            // Enforce our 0444/0555 modes in the kernel.
            MountOption::DefaultPermissions,
            // Unmount if this process dies unexpectedly.
            MountOption::AutoUnmount,
        ];
        // macFUSE shows this as the Finder volume name.
        #[cfg(target_os = "macos")]
        options.push(MountOption::CUSTOM("volname=Stingle".to_string()));

        let session = fuser::spawn_mount2(fs, &path, &options)?;
        Ok(FuseMount {
            session: Some(session),
            mountpoint: path,
            created_dir,
        })
    }
}

impl Drop for FuseMount {
    fn drop(&mut self) {
        // Dropping the session unmounts (and joins the serving thread).
        self.session.take();
        if self.created_dir {
            let _ = std::fs::remove_dir(&self.mountpoint);
        }
    }
}
