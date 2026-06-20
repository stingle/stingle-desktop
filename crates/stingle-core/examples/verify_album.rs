//! Verify album media decryption on the real account.
//! Run: `cargo run -p stingle-core --example verify_album`

use stingle_core::{Account, FileSet};

const SERVER: &str = "https://api.stingle.org/";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = std::env::temp_dir().join(format!(
        "sp-album-{}",
        hex::encode(stingle_crypto::sodium::random_bytes(4)?)
    ));
    let acc = Account::login(SERVER, "test1@fenritz.com", "alobloclo", &base).await?;
    acc.sync_cloud_to_local().await?;

    let albums = acc.list_albums_with_names(true)?;
    println!("[+] {} album(s)", albums.len());
    for (a, name) in &albums {
        let files = acc.db.list_album_files(&a.album_id, stingle_core::Sort::Desc, None, 0)?;
        println!("  album \"{name}\" (id={}) — {} file(s)", a.album_id, files.len());
        for f in files.iter().take(3) {
            let t = acc.media_response(FileSet::Album, Some(&a.album_id), &f.filename, true, None).await;
            let full = acc.media_response(FileSet::Album, Some(&a.album_id), &f.filename, false, None).await;
            match (&t, &full) {
                (Ok(t), Ok(fu)) => println!("    {} thumb {}B / full {}B ({})", f.filename, t.body.len(), fu.body.len(), fu.content_type),
                _ => println!("    {} FAILED thumb={:?} full={:?}", f.filename, t.err().map(|e| e.to_string()), full.err().map(|e| e.to_string())),
            }
        }
    }

    let _ = std::fs::remove_dir_all(&base);
    println!("=== ALBUM VERIFY DONE ===");
    Ok(())
}
