//! Small in-memory LRU of **decrypted** thumbnail bytes.
//!
//! Thumbnails are tiny and immutable. Without this, re-displaying one on every
//! scroll re-reads and re-decrypts its encrypted blob from disk. Caching the
//! plaintext **in memory** (never on disk — see the project security rules)
//! makes scrolling back to already-seen photos instant.
//!
//! Bounded by a total byte budget; the least-recently-used entries are evicted
//! once the budget is exceeded.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

pub struct ThumbCache {
    inner: Mutex<Inner>,
    cap_bytes: usize,
}

struct Inner {
    map: HashMap<String, Vec<u8>>,
    /// Keys in least- → most-recently-used order (front = LRU).
    order: VecDeque<String>,
    bytes: usize,
}

impl ThumbCache {
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

    /// Return a cached copy of the decrypted thumbnail, marking it most-recently
    /// used. `None` on a miss (or if the lock is poisoned).
    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        let mut g = self.inner.lock().ok()?;
        let value = g.map.get(key)?.clone();
        if let Some(pos) = g.order.iter().position(|k| k == key) {
            g.order.remove(pos);
        }
        g.order.push_back(key.to_string());
        Some(value)
    }

    /// Insert (or refresh) a decrypted thumbnail, evicting LRU entries until the
    /// total stays within budget.
    pub fn put(&self, key: String, value: Vec<u8>) {
        // Pointless to cache something that can't fit alongside anything else.
        if value.len() > self.cap_bytes {
            return;
        }
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(old) = g.map.remove(&key) {
            g.bytes -= old.len();
            if let Some(pos) = g.order.iter().position(|k| k == &key) {
                g.order.remove(pos);
            }
        }
        g.bytes += value.len();
        g.map.insert(key.clone(), value);
        g.order.push_back(key);
        while g.bytes > self.cap_bytes {
            match g.order.pop_front() {
                Some(lru) => {
                    if let Some(v) = g.map.remove(&lru) {
                        g.bytes -= v.len();
                    }
                }
                None => break,
            }
        }
    }
}
