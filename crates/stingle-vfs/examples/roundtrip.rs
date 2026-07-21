//! End-to-end WinFsp mount round-trip, no account required.
//!
//! Mounts a synthetic tree backed by an in-memory mock byte source at a free
//! drive letter, then browses + reads it back through the OS (`std::fs`) and
//! asserts the bytes match. Exercises the real WinFsp adapter: mount, security,
//! open, read_directory, read, get_file_info.
//!
//! Run: `cargo run -p stingle-vfs --example roundtrip --features mount-winfsp`
//! (Windows + WinFsp installed; LIBCLANG_PATH set for the winfsp-sys build.)

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use stingle_vfs::{Entry, Leaf, MediaSource, MountConfig, Section, Tree, Vfs, VfsMount};
use stingle_core::FileSet;

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

fn first_free_drive() -> Option<char> {
    ('E'..='Z').find(|&c| !Path::new(&format!("{c}:\\")).exists())
}

fn main() {
    // Two files in the same 1970-01 bucket; deterministic content.
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

    let drive = first_free_drive().expect("no free drive letter");
    let mount_point = format!("{drive}:");
    println!("Mounting synthetic Stingle tree at {mount_point}\\ ...");

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
    let root = format!("{drive}:\\");
    for _ in 0..50 {
        if Path::new(&root).exists() {
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

    // Directory structure.
    let bucket = format!("{drive}:\\Gallery\\1970\\1970-01");
    let listing: Vec<String> = std::fs::read_dir(&bucket)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    println!("Listing of {bucket}: {listing:?}");
    check("Gallery/1970/1970-01 lists small.bin", listing.iter().any(|n| n == "small.bin"));
    check("Gallery/1970/1970-01 lists big.bin", listing.iter().any(|n| n == "big.bin"));

    // File sizes (get_file_info).
    let small_meta = std::fs::metadata(format!("{bucket}\\small.bin"));
    check(
        "small.bin size == 5000",
        small_meta.as_ref().map(|m| m.len()).unwrap_or(0) == small.len() as u64,
    );

    // Byte-exact reads (open + read + EOF).
    match std::fs::read(format!("{bucket}\\small.bin")) {
        Ok(got) => check("small.bin bytes match", got == small),
        Err(e) => check(&format!("small.bin read error: {e}"), false),
    }
    match std::fs::read(format!("{bucket}\\big.bin")) {
        Ok(got) => check(
            "big.bin bytes match (multi-window read)",
            got.len() == big.len() && got == big,
        ),
        Err(e) => check(&format!("big.bin read error: {e}"), false),
    }

    // Cleanup: drop the mount before exit.
    drop(_mount);
    std::thread::sleep(Duration::from_millis(300));

    if failures == 0 {
        println!("\nALL CHECKS PASSED");
    } else {
        println!("\n{failures} CHECK(S) FAILED");
        std::process::exit(1);
    }
}
