//! Platform-agnostic directory index for the virtual filesystem.
//!
//! A [`Tree`] is an immutable snapshot of the library laid out as browsable
//! folders:
//!
//! ```text
//! /Gallery/2024/2024-06/IMG_0001.jpg
//! /Albums/<album name>/2023/2023-11/VID_0002.mp4
//! /Trash/...                         (only when include_trash)
//! ```
//!
//! Leaves are date-bucketed (`YYYY/YYYY-MM`, UTC) so no single directory holds
//! the whole 25K-item gallery — file dialogs `stat` and thumbnail every visible
//! entry, and a flat mega-directory makes them crawl.
//!
//! The tree carries no decrypted content and no keys — only the mapping from a
//! display path to the `(set, album_id, enc_filename)` needed to fetch bytes
//! on read (see [`ops`](crate::ops)). It is rebuilt after each sync pass.

use std::collections::BTreeMap;

use stingle_core::{safe_filename, FileSet};

/// Which top-level section a file belongs to. `Album` carries the already
/// sanitized + de-duplicated folder name (see [`crate::collect_entries`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Section {
    Gallery,
    Album(String),
    Trash,
}

/// One library item, flattened for [`Tree::build`]. Produced by
/// [`crate::collect_entries`]; also the unit tests' input.
#[derive(Debug, Clone)]
pub struct Entry {
    pub section: Section,
    pub set: FileSet,
    /// Album id for `FileSet::Album`; `None` for gallery/trash.
    pub album_id: Option<String>,
    /// The encrypted (storage) filename — the item's stable identity.
    pub enc_filename: String,
    /// The original filename from the header (used for the display name).
    pub original_name: String,
    /// Plaintext size in bytes.
    pub size: u64,
    /// Creation time, epoch milliseconds (drives the date bucket + mtime).
    pub date_created_ms: i64,
}

/// A file leaf: everything needed to fetch its bytes, and its size for `stat`.
#[derive(Debug, Clone)]
pub struct Leaf {
    pub set: FileSet,
    pub album_id: Option<String>,
    pub enc_filename: String,
    pub size: u64,
}

#[derive(Debug)]
enum NodeKind {
    Dir(BTreeMap<String, u64>),
    File(Leaf),
}

#[derive(Debug)]
struct Node {
    parent: u64,
    mtime_ms: i64,
    kind: NodeKind,
}

/// Attributes for a `getattr`/`lookup` reply (driver-agnostic).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attr {
    pub ino: u64,
    /// 0 for directories.
    pub size: u64,
    pub is_dir: bool,
    pub mtime_ms: i64,
    /// Hard-link count (`2 + #subdirs` for dirs, `1` for files).
    pub nlink: u32,
}

/// One entry in a `readdir` reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirent {
    pub name: String,
    pub ino: u64,
    pub is_dir: bool,
}

/// The inode of the filesystem root (matches FUSE's `FUSE_ROOT_ID`).
pub const ROOT_INO: u64 = 1;

/// An immutable directory index. Inodes are 1-based; `nodes[ino - 1]` is the
/// node, `ROOT_INO` (1) is the root directory.
#[derive(Debug)]
pub struct Tree {
    nodes: Vec<Node>,
}

impl Tree {
    /// Assemble a tree from flattened [`Entry`]s. Pure and deterministic:
    /// entries are processed in a stable order (by encrypted filename), so the
    /// ` (n)` suffixes handed to colliding original names don't shift between
    /// rebuilds. `now_ms` is the mtime reported for synthesized directories.
    pub fn build(mut entries: Vec<Entry>, now_ms: i64) -> Tree {
        let mut nodes: Vec<Node> = vec![Node {
            parent: ROOT_INO,
            mtime_ms: now_ms,
            kind: NodeKind::Dir(BTreeMap::new()),
        }];

        // Stable order → stable dedup suffixes across rebuilds.
        entries.sort_by(|a, b| a.enc_filename.cmp(&b.enc_filename));

        for e in entries {
            let (y, m, _) = civil_from_unix_ms(e.date_created_ms);
            let year = format!("{y:04}");
            let month = format!("{y:04}-{m:02}");
            let path: Vec<String> = match &e.section {
                Section::Gallery => vec!["Gallery".to_string(), year, month],
                Section::Album(name) => {
                    vec!["Albums".to_string(), name.clone(), year, month]
                }
                Section::Trash => vec!["Trash".to_string(), year, month],
            };

            let mut dir = ROOT_INO;
            for comp in &path {
                dir = ensure_dir(&mut nodes, dir, comp, now_ms);
            }

            let leaf = Leaf {
                set: e.set,
                album_id: e.album_id,
                enc_filename: e.enc_filename,
                size: e.size,
            };
            add_file(&mut nodes, dir, &safe_filename(&e.original_name), leaf, e.date_created_ms);
        }

        Tree { nodes }
    }

    fn node(&self, ino: u64) -> Option<&Node> {
        let idx = ino.checked_sub(1)? as usize;
        self.nodes.get(idx)
    }

    /// Total node count (root + dirs + files). Mostly for tests/diagnostics.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Attributes for `ino`, or `None` if it doesn't exist.
    pub fn attr(&self, ino: u64) -> Option<Attr> {
        let n = self.node(ino)?;
        let (size, is_dir, nlink) = match &n.kind {
            NodeKind::File(l) => (l.size, false, 1),
            NodeKind::Dir(children) => {
                let subdirs = children
                    .values()
                    .filter(|&&c| matches!(self.node(c).map(|n| &n.kind), Some(NodeKind::Dir(_))))
                    .count() as u32;
                (0, true, 2 + subdirs)
            }
        };
        Some(Attr { ino, size, is_dir, mtime_ms: n.mtime_ms, nlink })
    }

    /// Resolve `name` within directory `parent`.
    pub fn lookup(&self, parent: u64, name: &str) -> Option<Attr> {
        match &self.node(parent)?.kind {
            NodeKind::Dir(children) => self.attr(*children.get(name)?),
            NodeKind::File(_) => None,
        }
    }

    /// List the children of directory `ino` (sorted by name). `None` if `ino`
    /// is missing or is a file.
    pub fn children(&self, ino: u64) -> Option<Vec<Dirent>> {
        match &self.node(ino)?.kind {
            NodeKind::Dir(children) => Some(
                children
                    .iter()
                    .map(|(name, &cino)| Dirent {
                        name: name.clone(),
                        ino: cino,
                        is_dir: matches!(
                            self.node(cino).map(|n| &n.kind),
                            Some(NodeKind::Dir(_))
                        ),
                    })
                    .collect(),
            ),
            NodeKind::File(_) => None,
        }
    }

    /// The file leaf at `ino`, or `None` if `ino` is a directory / missing.
    pub fn leaf(&self, ino: u64) -> Option<&Leaf> {
        match &self.node(ino)?.kind {
            NodeKind::File(l) => Some(l),
            NodeKind::Dir(_) => None,
        }
    }

    /// Parent inode of `ino` (root's parent is itself).
    pub fn parent(&self, ino: u64) -> u64 {
        self.node(ino).map(|n| n.parent).unwrap_or(ROOT_INO)
    }

    /// Resolve a `/`-separated path from the root. For tests and debugging.
    pub fn resolve(&self, path: &str) -> Option<u64> {
        let mut ino = ROOT_INO;
        for comp in path.split('/').filter(|s| !s.is_empty()) {
            match &self.node(ino)?.kind {
                NodeKind::Dir(children) => ino = *children.get(comp)?,
                NodeKind::File(_) => return None,
            }
        }
        Some(ino)
    }
}

/// Get or create a child directory `name` under `parent`, returning its inode.
fn ensure_dir(nodes: &mut Vec<Node>, parent: u64, name: &str, now_ms: i64) -> u64 {
    if let NodeKind::Dir(children) = &nodes[(parent - 1) as usize].kind {
        if let Some(&ino) = children.get(name) {
            return ino;
        }
    }
    let ino = nodes.len() as u64 + 1;
    nodes.push(Node {
        parent,
        mtime_ms: now_ms,
        kind: NodeKind::Dir(BTreeMap::new()),
    });
    if let NodeKind::Dir(children) = &mut nodes[(parent - 1) as usize].kind {
        children.insert(name.to_string(), ino);
    }
    ino
}

/// Add a file leaf under `parent`, de-duplicating `name` against existing
/// children with the ` (n)` scheme (matching `takeout`'s `unique_path`).
fn add_file(nodes: &mut Vec<Node>, parent: u64, name: &str, leaf: Leaf, mtime_ms: i64) {
    let unique = {
        let children = match &nodes[(parent - 1) as usize].kind {
            NodeKind::Dir(c) => c,
            NodeKind::File(_) => return,
        };
        unique_name(children, name)
    };
    let ino = nodes.len() as u64 + 1;
    nodes.push(Node {
        parent,
        mtime_ms,
        kind: NodeKind::File(leaf),
    });
    if let NodeKind::Dir(children) = &mut nodes[(parent - 1) as usize].kind {
        children.insert(unique, ino);
    }
}

/// Pick a name not already present in `children`, appending ` (1)`, ` (2)`, …
/// before the extension on collision.
fn unique_name(children: &BTreeMap<String, u64>, name: &str) -> String {
    if !children.contains_key(name) {
        return name.to_string();
    }
    let (stem, ext) = split_ext(name);
    for i in 1.. {
        let candidate = match ext {
            Some(e) => format!("{stem} ({i}).{e}"),
            None => format!("{stem} ({i})"),
        };
        if !children.contains_key(&candidate) {
            return candidate;
        }
    }
    unreachable!("i32 range exhausted while de-duplicating filenames")
}

/// Split a filename into `(stem, Some(ext))`, or `(name, None)` when there's no
/// usable extension. A leading dot (`.gitignore`) is treated as a stem, not an
/// extension, matching `Path::extension`.
fn split_ext(name: &str) -> (&str, Option<&str>) {
    match name.rfind('.') {
        Some(dot) if dot > 0 && dot < name.len() - 1 => (&name[..dot], Some(&name[dot + 1..])),
        _ => (name, None),
    }
}

/// Convert an epoch-millisecond timestamp to a UTC `(year, month, day)`.
///
/// Howard Hinnant's `civil_from_days` algorithm — no dependency, deterministic,
/// and correct for the full range. UTC is used (not local time) so a file's
/// bucket never shifts across a DST change or a machine timezone change.
fn civil_from_unix_ms(ms: i64) -> (i64, u32, u32) {
    let days = ms.div_euclid(86_400_000);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2024-06-15T00:00:00Z and 2023-11-02T00:00:00Z in epoch ms.
    const T_2024_06: i64 = 1_718_409_600_000;
    const T_2023_11: i64 = 1_698_883_200_000;

    fn gallery(enc: &str, orig: &str, size: u64, date: i64) -> Entry {
        Entry {
            section: Section::Gallery,
            set: FileSet::Gallery,
            album_id: None,
            enc_filename: enc.to_string(),
            original_name: orig.to_string(),
            size,
            date_created_ms: date,
        }
    }

    #[test]
    fn civil_conversion_is_correct() {
        assert_eq!(civil_from_unix_ms(0), (1970, 1, 1));
        assert_eq!(civil_from_unix_ms(T_2024_06), (2024, 6, 15));
        assert_eq!(civil_from_unix_ms(T_2023_11), (2023, 11, 2));
        // A pre-epoch timestamp must still bucket correctly.
        assert_eq!(civil_from_unix_ms(-1), (1969, 12, 31));
    }

    #[test]
    fn builds_date_bucketed_gallery_path() {
        let tree = Tree::build(vec![gallery("enc1", "IMG_1.jpg", 10, T_2024_06)], 0);
        let ino = tree.resolve("Gallery/2024/2024-06/IMG_1.jpg").expect("path exists");
        let attr = tree.attr(ino).unwrap();
        assert!(!attr.is_dir);
        assert_eq!(attr.size, 10);
        let leaf = tree.leaf(ino).unwrap();
        assert_eq!(leaf.enc_filename, "enc1");
        assert_eq!(leaf.set, FileSet::Gallery);
    }

    #[test]
    fn dedups_colliding_original_names_in_same_bucket() {
        let tree = Tree::build(
            vec![
                gallery("encA", "IMG.jpg", 1, T_2024_06),
                gallery("encB", "IMG.jpg", 2, T_2024_06),
                gallery("encC", "IMG.jpg", 3, T_2024_06),
            ],
            0,
        );
        // All three land in the same bucket under distinct display names.
        let a = tree.resolve("Gallery/2024/2024-06/IMG.jpg").unwrap();
        let b = tree.resolve("Gallery/2024/2024-06/IMG (1).jpg").unwrap();
        let c = tree.resolve("Gallery/2024/2024-06/IMG (2).jpg").unwrap();
        // Distinct inodes mapping to distinct encrypted files.
        let mut encs = [a, b, c]
            .iter()
            .map(|&i| tree.leaf(i).unwrap().enc_filename.clone())
            .collect::<Vec<_>>();
        encs.sort();
        assert_eq!(encs, vec!["encA", "encB", "encC"]);
    }

    #[test]
    fn dedup_is_stable_across_rebuilds() {
        let entries = || {
            vec![
                gallery("encB", "IMG.jpg", 2, T_2024_06),
                gallery("encA", "IMG.jpg", 1, T_2024_06),
            ]
        };
        let t1 = Tree::build(entries(), 0);
        let t2 = Tree::build(entries(), 999); // different now_ms, same inputs
        // encA sorts first → always gets the unsuffixed name.
        let name_of = |t: &Tree, enc: &str| {
            for cand in ["IMG.jpg", "IMG (1).jpg"] {
                if let Some(i) = t.resolve(&format!("Gallery/2024/2024-06/{cand}")) {
                    if t.leaf(i).unwrap().enc_filename == enc {
                        return cand.to_string();
                    }
                }
            }
            panic!("not found");
        };
        assert_eq!(name_of(&t1, "encA"), "IMG.jpg");
        assert_eq!(name_of(&t2, "encA"), "IMG.jpg");
        assert_eq!(name_of(&t1, "encB"), "IMG (1).jpg");
    }

    #[test]
    fn sanitizes_untrusted_original_names() {
        // A header-derived name with traversal must be reduced to a basename.
        let tree = Tree::build(vec![gallery("enc1", "../../etc/passwd", 1, T_2024_06)], 0);
        assert!(tree.resolve("Gallery/2024/2024-06/passwd").is_some());
        // No escape: the traversal path must not exist.
        assert!(tree.resolve("etc/passwd").is_none());
    }

    #[test]
    fn albums_and_trash_sections() {
        let tree = Tree::build(
            vec![
                gallery("g1", "g.jpg", 1, T_2024_06),
                Entry {
                    section: Section::Album("Trip".to_string()),
                    set: FileSet::Album,
                    album_id: Some("alb1".to_string()),
                    enc_filename: "a1".to_string(),
                    original_name: "p.jpg".to_string(),
                    size: 5,
                    date_created_ms: T_2023_11,
                },
                Entry {
                    section: Section::Trash,
                    set: FileSet::Trash,
                    album_id: None,
                    enc_filename: "t1".to_string(),
                    original_name: "old.jpg".to_string(),
                    size: 7,
                    date_created_ms: T_2024_06,
                },
            ],
            0,
        );
        let a = tree.resolve("Albums/Trip/2023/2023-11/p.jpg").unwrap();
        assert_eq!(tree.leaf(a).unwrap().album_id.as_deref(), Some("alb1"));
        assert_eq!(tree.leaf(a).unwrap().set, FileSet::Album);
        let t = tree.resolve("Trash/2024/2024-06/old.jpg").unwrap();
        assert_eq!(tree.leaf(t).unwrap().set, FileSet::Trash);
        assert!(tree.resolve("Gallery/2024/2024-06/g.jpg").is_some());
    }

    #[test]
    fn readdir_and_lookup_agree() {
        let tree = Tree::build(vec![gallery("enc1", "a.jpg", 1, T_2024_06)], 0);
        let root_kids = tree.children(ROOT_INO).unwrap();
        assert!(root_kids.iter().any(|d| d.name == "Gallery" && d.is_dir));
        let gino = tree.lookup(ROOT_INO, "Gallery").unwrap().ino;
        assert!(tree.attr(gino).unwrap().is_dir);
        // Looking up a missing name yields None.
        assert!(tree.lookup(ROOT_INO, "Nope").is_none());
    }
}
