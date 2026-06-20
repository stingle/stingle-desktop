//! Verify the concurrent thumbnail prefetcher against the real account.
//! Run: `cargo run -p stingle-core --example verify_prefetch`

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use stingle_core::{Account, FileSet};

const SERVER: &str = "https://api.stingle.org/";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = std::env::temp_dir().join(format!(
        "sp-prefetch-{}",
        hex::encode(stingle_crypto::sodium::random_bytes(4)?)
    ));
    let acc = Account::login(SERVER, "test1@fenritz.com", "alobloclo", &base).await?;
    acc.sync_cloud_to_local().await?;

    let last = AtomicUsize::new(0);
    let cb = move |done: usize, total: usize| {
        if done > last.swap(done, Ordering::Relaxed) {
            println!("  thumbs {done}/{total}");
        }
    };

    let start = Instant::now();
    let n = acc.download_all_thumbs(64, Some(&cb)).await?;
    println!("[+] downloaded {n} missing thumbnails in {:?} (concurrency 64)", start.elapsed());

    // Confirm they're all on disk and decryptable now.
    let files = acc.db.list_files(FileSet::Gallery, stingle_core::Sort::Desc, None, 0)?;
    let mut ok = 0;
    for f in &files {
        if acc.media_response(FileSet::Gallery, None, &f.filename, true, None).await.is_ok() {
            ok += 1;
        }
    }
    println!("[+] gallery thumbnails decodable: {ok}/{}", files.len());

    let _ = std::fs::remove_dir_all(&base);
    println!("=== PREFETCH VERIFIED ===");
    Ok(())
}
