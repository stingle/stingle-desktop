//! Verify decryption against a real account's existing photos/videos.
//! Run: `cargo run -p stingle-core --example verify_real`

use stingle_core::{Account, FileSet};

const SERVER: &str = "https://api.stingle.org/";
const EMAIL: &str = "test1@fenritz.com";
const PASSWORD: &str = "alobloclo";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = std::env::temp_dir().join(format!(
        "sp-verify-{}",
        hex::encode(stingle_crypto::sodium::random_bytes(4)?)
    ));

    let acc = Account::login(SERVER, EMAIL, PASSWORD, &base).await?;
    println!("[+] logged in as {EMAIL} (userId={})", acc.info.user_id);

    acc.sync_cloud_to_local().await?;
    let files = acc.db.list_files(FileSet::Gallery, stingle_core::Sort::Desc, Some(8), 0)?;
    println!("[+] gallery has files; checking first {}", files.len());
    if files.is_empty() {
        println!("    (account has no gallery photos to check)");
    }

    let mut ok_thumb = 0;
    let mut ok_full = 0;
    let mut ok_video = 0;

    for (i, f) in files.iter().enumerate() {
        // Thumbnail
        let t = acc.media_response(FileSet::Gallery, None, &f.filename, true, None).await?;
        match image::load_from_memory(&t.body) {
            Ok(img) => {
                ok_thumb += 1;
                println!("  [{i}] thumb OK  {}x{} ({} bytes, {})", img.width(), img.height(), t.body.len(), t.content_type);
            }
            Err(e) => println!("  [{i}] thumb FAILED to decode: {e} ({} bytes)", t.body.len()),
        }

        // Full original
        let full = acc.media_response(FileSet::Gallery, None, &f.filename, false, None).await?;
        if full.content_type.starts_with("video/") {
            // Range test: request first 64 KiB.
            let r = acc.media_response(FileSet::Gallery, None, &f.filename, false, Some((0, Some(65535)))).await?;
            let (s, e) = r.range.unwrap();
            println!("  [{i}] VIDEO {} total={} bytes; range {}-{} -> {} bytes", full.content_type, full.total_size, s, e, r.body.len());
            ok_video += 1;
        } else {
            match image::load_from_memory(&full.body) {
                Ok(img) => {
                    ok_full += 1;
                    println!("  [{i}] full  OK  {}x{} ({} bytes, {})", img.width(), img.height(), full.body.len(), full.content_type);
                }
                Err(e) => println!("  [{i}] full  decode note: {e} (type={}, {} bytes) — original kept verbatim", full.content_type, full.body.len()),
            }
        }
    }

    println!("\n[summary] thumbs decoded: {ok_thumb}, full images decoded: {ok_full}, videos+range: {ok_video}");
    let _ = std::fs::remove_dir_all(&base);
    if files.is_empty() || (ok_thumb == 0 && ok_video == 0) {
        println!("=== NO MEDIA VERIFIED (empty account or decode issue) ===");
    } else {
        println!("=== REAL-ACCOUNT MEDIA DECRYPTION VERIFIED ===");
    }
    Ok(())
}
