//! Small in-memory LRU of **decrypted** media bytes (+ content type).
//!
//! Two instances hang off [`crate::account::Account`]: one for thumbnails
//! (re-displaying on scroll must not re-read + re-decrypt the blob) and one for
//! full-resolution images (re-opening a recently viewed photo must not re-read,
//! re-decrypt and — for HEIC/TIFF — re-transcode it). Plaintext lives in memory
//! only, never on disk — see the project security rules.
//!
//! Bounded by a total byte budget; the least-recently-used entries are evicted
//! once the budget is exceeded.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

pub struct MediaCache {
    inner: Mutex<Inner>,
    cap_bytes: usize,
}

struct Inner {
    map: HashMap<String, (String, Vec<u8>)>,
    /// Keys in least- → most-recently-used order (front = LRU).
    order: VecDeque<String>,
    bytes: usize,
}

impl MediaCache {
    pub fn new(cap_bytes: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                order: VecDeque::new(),
                bytes: 0,
            }),
            cap_bytes,
        }
    }

    /// Return a cached copy of the decrypted media as `(content_type, bytes)`,
    /// marking it most-recently used. `None` on a miss (or a poisoned lock).
    pub fn get(&self, key: &str) -> Option<(String, Vec<u8>)> {
        let mut g = self.inner.lock().ok()?;
        let value = g.map.get(key)?.clone();
        if let Some(pos) = g.order.iter().position(|k| k == key) {
            g.order.remove(pos);
        }
        g.order.push_back(key.to_string());
        Some(value)
    }

    /// Insert (or refresh) a decrypted media entry, evicting LRU entries until
    /// the total stays within budget.
    pub fn put(&self, key: String, content_type: String, value: Vec<u8>) {
        // Pointless to cache something that can't fit alongside anything else.
        if value.len() > self.cap_bytes {
            return;
        }
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some((_, old)) = g.map.remove(&key) {
            g.bytes -= old.len();
            if let Some(pos) = g.order.iter().position(|k| k == &key) {
                g.order.remove(pos);
            }
        }
        g.bytes += value.len();
        g.map.insert(key.clone(), (content_type, value));
        g.order.push_back(key);
        while g.bytes > self.cap_bytes {
            match g.order.pop_front() {
                Some(lru) => {
                    if let Some((_, v)) = g.map.remove(&lru) {
                        g.bytes -= v.len();
                    }
                }
                None => break,
            }
        }
    }

    /// Drop one entry (e.g. after the underlying file is deleted or replaced).
    pub fn remove(&self, key: &str) {
        let Ok(mut g) = self.inner.lock() else { return };
        if let Some((_, old)) = g.map.remove(key) {
            g.bytes -= old.len();
            if let Some(pos) = g.order.iter().position(|k| k == key) {
                g.order.remove(pos);
            }
        }
    }
}
