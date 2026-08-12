//! End-to-end mount round-trip, no account required. Cross-platform.
//!
//! Mounts a synthetic tree backed by an in-memory mock byte source, then
//! browses + reads it back through the OS (`std::fs`) and asserts the bytes
//! match. Exercises the real driver adapter end to end.
//!
//! Windows: `cargo run -p stingle-vfs --example roundtrip --features mount-winfsp`
//! Linux/macOS: `cargo run -p stingle-vfs --example roundtrip --features mount-fuse`
//! (needs the platform FUSE/WinFsp runtime installed; on Windows also
//! LIBCLANG_PATH for the winfsp-sys build.)

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use stingle_core::FileSet;
use stingle_vfs::{Entry, Leaf, MediaSource, MountConfig, Section, Tree, Vfs, VfsMount};

/// In-memory byte source keyed by encrypted filename.
struct MockSource {
    data: HashMap<String, Vec<u8>>,
    window: usize,
}

impl MediaSource for MockSource {
    fn read_window(&self, leaf: &Leaf, start: u64, end_inclusive: u64) -> io::Result<Vec<u8>> {
        let bytes = self
            .data
            .get(&leaf.enc_filename)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        let start = start as usize;
        let end_excl = ((end_inclusive as usize) + 1)
            .min(bytes.len())
            .min(start + self.window);
        if start >= end_excl {
            return Ok(Vec::new());
        }
        Ok(bytes[start..end_excl].to_vec())
    }
}

fn gallery_entry(enc: &str, name: &str, size: u64, date_ms: i64) -> Entry {
    Entry {
        section: Section::Gallery,
        set: FileSet::Gallery,
        album_id: None,
        enc_filename: enc.to_string(),
        original_name: name.to_string(),
        size,
        date_created_ms: date_ms,
    }
}

/// The mount-point argument for `MountConfig` and the root path to read back.
/// Windows uses a free drive letter; Unix uses a temp directory.
fn pick_mount() -> (String, PathBuf) {
    #[cfg(windows)]
    {
        let d = ('E'..='Z')
            .find(|&c| !std::path::Path::new(&format!("{c}:\\")).exists())
            .expect("no free drive letter");
        (format!("{d}:"), PathBuf::from(format!("{d}:\\")))
    }
    #[cfg(unix)]
    {
        let dir = std::env::temp_dir().join("stingle-vfs-roundtrip");
        (dir.to_string_lossy().into_owned(), dir)
    }
}

fn main() {
    let small: Vec<u8> = (0..5_000u32).map(|i| (i % 251) as u8).collect();
    let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();

    let mut data = HashMap::new();
    data.insert("enc_small".to_string(), small.clone());
    data.insert("enc_big".to_string(), big.clone());
    let source = Arc::new(MockSource { data, window: 64 * 1024 });

    let tree = Tree::build(
        vec![
            gallery_entry("enc_small", "small.bin", small.len() as u64, 0),
            gallery_entry("enc_big", "big.bin", big.len() as u64, 0),
        ],
        0,
    );
    let vfs = Vfs::new(tree, source);

    let (mount_point, root_path) = pick_mount();
    println!("Mounting synthetic Stingle tree at {mount_point} ...");

    let cfg = MountConfig {
        mount_point: mount_point.clone(),
        include_trash: false,
    };
    let _mount = match VfsMount::mount(vfs, &cfg) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("MOUNT FAILED: {e}");
            std::process::exit(1);
        }
    };

    // Give the volume a moment to appear.
    let bucket = root_path.join("Gallery").join("1970").join("1970-01");
    for _ in 0..50 {
        if bucket.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let mut failures = 0;
    let mut check = |label: &str, ok: bool| {
        println!("  [{}] {label}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            failures += 1;
        }
    };

    let listing: Vec<String> = std::fs::read_dir(&bucket)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    println!("Listing of {}: {listing:?}", bucket.display());
    check("Gallery/1970/1970-01 lists small.bin", listing.iter().any(|n| n == "small.bin"));
    check("Gallery/1970/1970-01 lists big.bin", listing.iter().any(|n| n == "big.bin"));

    let small_path = bucket.join("small.bin");
    let big_path = bucket.join("big.bin");

    check(
        "small.bin size == 5000",
        std::fs::metadata(&small_path).map(|m| m.len()).unwrap_or(0) == small.len() as u64,
    );
    match std::fs::read(&small_path) {
        Ok(got) => check("small.bin bytes match", got == small),
        Err(e) => check(&format!("small.bin read error: {e}"), false),
    }
    match std::fs::read(&big_path) {
        Ok(got) => check(
            "big.bin bytes match (multi-window read)",
            got.len() == big.len() && got == big,
        ),
        Err(e) => check(&format!("big.bin read error: {e}"), false),
    }

    drop(_mount);
    std::thread::sleep(Duration::from_millis(300));

    if failures == 0 {
        println!("\nALL CHECKS PASSED");
    } else {
        println!("\n{failures} CHECK(S) FAILED");
        std::process::exit(1);
    }
}
