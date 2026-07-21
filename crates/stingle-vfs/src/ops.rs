//! Filesystem read operations over a [`Tree`](crate::tree::Tree).
//!
//! `getattr` / `lookup` / `readdir` are pure tree lookups (see
//! [`Tree`](crate::tree::Tree)); this module adds the byte-serving `read` path.
//! Reads are pulled from a [`MediaSource`] in bounded windows so a large video
//! never buffers whole. The production source ([`AccountSource`]) reuses
//! `Account::media_response`, which decrypts each window in memory via
//! `file::decrypt_range` and persists nothing.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::{Arc, Mutex};

use stingle_core::{Account, MediaStream};

use crate::tree::{Leaf, Tree};

/// Max bytes decrypted per source window (bounds memory; the read loop stitches
/// larger requests from several windows).
const MAX_WINDOW: u64 = 4 * 1024 * 1024;

/// How many prepared per-file streams to keep cached at once.
const STREAM_CACHE_CAP: usize = 256;

/// Serves decrypted byte windows for a file leaf. Abstracted so the read
/// assembly loop can be unit-tested without keys or a real encrypted blob.
pub trait MediaSource: Send + Sync {
    /// Return decrypted bytes for the inclusive range `[start, end_inclusive]`
    /// of `leaf`. The implementation MAY return fewer bytes than requested
    /// (e.g. capped to its own window size); an empty result means EOF/none.
    fn read_window(&self, leaf: &Leaf, start: u64, end_inclusive: u64) -> io::Result<Vec<u8>>;
}

/// Production [`MediaSource`]: serves decrypted windows from the local
/// encrypted cache. The expensive per-file setup (DB lookup, header seal-open,
/// blob download, outer-header scan) runs once via `Account::open_media_stream`
/// and the resulting [`MediaStream`] is cached, so repeated windows of the same
/// file — an image preview, a video scrub, sequential viewer reads — are just a
/// file open + `decrypt_range`. Everything stays in memory; nothing persisted.
pub struct AccountSource {
    acc: Arc<Account>,
    handle: tokio::runtime::Handle,
    /// Per-file prepared streams, keyed by encrypted filename (FIFO-bounded).
    streams: Mutex<StreamCache>,
}

struct StreamCache {
    map: HashMap<String, Arc<MediaStream>>,
    order: VecDeque<String>,
}

impl AccountSource {
    /// `handle` must be a Tokio runtime handle; `read_window` is called from the
    /// driver's own OS threads (never a runtime worker), so `block_on` is safe.
    pub fn new(acc: Arc<Account>, handle: tokio::runtime::Handle) -> Self {
        Self {
            acc,
            handle,
            streams: Mutex::new(StreamCache {
                map: HashMap::new(),
                order: VecDeque::new(),
            }),
        }
    }

    /// Get, or prepare-and-cache, the stream for `leaf`. The first call may
    /// block to download the encrypted blob once; later calls are a map hit.
    fn stream_for(&self, leaf: &Leaf) -> io::Result<Arc<MediaStream>> {
        if let Some(s) = self.streams.lock().unwrap().map.get(&leaf.enc_filename) {
            return Ok(s.clone());
        }
        let acc = self.acc.clone();
        let set = leaf.set;
        let album = leaf.album_id.clone();
        let name = leaf.enc_filename.clone();
        let stream = self
            .handle
            .block_on(async move { acc.open_media_stream(set, album.as_deref(), &name).await })
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let stream = Arc::new(stream);

        let mut cache = self.streams.lock().unwrap();
        // A concurrent reader may have inserted the same file meanwhile.
        if let Some(existing) = cache.map.get(&leaf.enc_filename) {
            return Ok(existing.clone());
        }
        if cache.order.len() >= STREAM_CACHE_CAP {
            if let Some(evicted) = cache.order.pop_front() {
                cache.map.remove(&evicted);
            }
        }
        cache.map.insert(leaf.enc_filename.clone(), stream.clone());
        cache.order.push_back(leaf.enc_filename.clone());
        Ok(stream)
    }
}

impl MediaSource for AccountSource {
    fn read_window(&self, leaf: &Leaf, start: u64, end_inclusive: u64) -> io::Result<Vec<u8>> {
        let stream = self.stream_for(leaf)?;
        // Cap the window so one request can't decrypt the whole file at once.
        let end = end_inclusive.min(start.saturating_add(MAX_WINDOW - 1));
        stream
            .read(start, end)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
    }
}

/// The mounted, read-only view: an immutable [`Tree`] plus a byte source.
pub struct Vfs {
    pub tree: Tree,
    source: Arc<dyn MediaSource>,
}

impl Vfs {
    /// Compose a view from a prebuilt tree and any byte source (tests).
    pub fn new(tree: Tree, source: Arc<dyn MediaSource>) -> Self {
        Self { tree, source }
    }

    /// Build the whole view from an unlocked account: enumerate the library
    /// into a tree and wire up an [`AccountSource`] for reads. `now_ms` is the
    /// mtime reported for synthesized directories.
    pub fn from_account(
        acc: Arc<Account>,
        handle: tokio::runtime::Handle,
        now_ms: i64,
        include_trash: bool,
    ) -> Self {
        let entries = crate::collect_entries(&acc, include_trash);
        let tree = Tree::build(entries, now_ms);
        Self {
            tree,
            source: Arc::new(AccountSource::new(acc, handle)),
        }
    }

    /// Read up to `size` bytes of file `ino` starting at `offset`.
    ///
    /// Clamps to the file's real size (so a picker reading past EOF gets a short
    /// read, not an error) and stitches together as many source windows as
    /// needed — the source caps each window (≤ 4 MiB for `AccountSource`), so a
    /// large `size` loops rather than buffering the whole file at once.
    pub fn read(&self, ino: u64, offset: u64, size: u32) -> io::Result<Vec<u8>> {
        let leaf = self
            .tree
            .leaf(ino)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;

        let total = leaf.size;
        if offset >= total {
            return Ok(Vec::new());
        }
        let end_excl = offset.saturating_add(size as u64).min(total);

        let mut out = Vec::with_capacity((end_excl - offset) as usize);
        let mut pos = offset;
        while pos < end_excl {
            let window = self.source.read_window(leaf, pos, end_excl - 1)?;
            if window.is_empty() {
                break; // source reported EOF earlier than the header claimed
            }
            pos += window.len() as u64;
            out.extend_from_slice(&window);
            if pos >= end_excl {
                break;
            }
        }
        // A source that over-delivered on the final window can't overshoot the
        // caller's request.
        out.truncate((end_excl - offset) as usize);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Entry, Section};
    use stingle_core::FileSet;

    /// A source that serves slices of an in-memory buffer, capped to `window`
    /// bytes per call — exercises the multi-window stitching without crypto.
    struct MockSource {
        data: Vec<u8>,
        window: usize,
    }

    impl MediaSource for MockSource {
        fn read_window(&self, _leaf: &Leaf, start: u64, end_inclusive: u64) -> io::Result<Vec<u8>> {
            let start = start as usize;
            let end_excl = ((end_inclusive as usize) + 1)
                .min(self.data.len())
                .min(start + self.window);
            if start >= end_excl {
                return Ok(Vec::new());
            }
            Ok(self.data[start..end_excl].to_vec())
        }
    }

    fn vfs_with(data: Vec<u8>, window: usize) -> (Vfs, u64) {
        let size = data.len() as u64;
        let tree = Tree::build(
            vec![Entry {
                section: Section::Gallery,
                set: FileSet::Gallery,
                album_id: None,
                enc_filename: "enc1".to_string(),
                original_name: "test.bin".to_string(),
                size,
                date_created_ms: 0, // 1970-01
            }],
            0,
        );
        let ino = tree.resolve("Gallery/1970/1970-01/test.bin").unwrap();
        let vfs = Vfs::new(tree, Arc::new(MockSource { data, window }));
        (vfs, ino)
    }

    #[test]
    fn full_read_reassembles_across_windows() {
        let data: Vec<u8> = (0..250u32).map(|i| (i % 256) as u8).collect();
        let (vfs, ino) = vfs_with(data.clone(), 100); // 3 windows: 100+100+50
        let got = vfs.read(ino, 0, 250).unwrap();
        assert_eq!(got, data);
    }

    #[test]
    fn partial_read_in_the_middle() {
        let data: Vec<u8> = (0..250u32).map(|i| (i % 256) as u8).collect();
        let (vfs, ino) = vfs_with(data.clone(), 100);
        // Straddles a window boundary at 100.
        let got = vfs.read(ino, 80, 60).unwrap();
        assert_eq!(got, data[80..140]);
    }

    #[test]
    fn read_clamps_at_eof() {
        let data: Vec<u8> = (0..10u32).map(|i| i as u8).collect();
        let (vfs, ino) = vfs_with(data.clone(), 4);
        // Asking well past the end yields only the remaining bytes.
        let got = vfs.read(ino, 8, 100).unwrap();
        assert_eq!(got, data[8..10]);
        // Starting at/after EOF yields empty.
        assert!(vfs.read(ino, 10, 100).unwrap().is_empty());
        assert!(vfs.read(ino, 50, 100).unwrap().is_empty());
    }

    #[test]
    fn zero_size_read_is_empty() {
        let (vfs, ino) = vfs_with(vec![1, 2, 3], 4);
        assert!(vfs.read(ino, 0, 0).unwrap().is_empty());
    }

    #[test]
    fn read_on_a_directory_is_not_found() {
        let (vfs, _) = vfs_with(vec![1, 2, 3], 4);
        let gino = vfs.tree.resolve("Gallery").unwrap();
        assert_eq!(
            vfs.read(gino, 0, 10).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }
}
