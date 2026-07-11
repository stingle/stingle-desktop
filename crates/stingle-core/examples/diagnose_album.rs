//! Verify every album thumbnail decodes to something the webview can render,
//! exercising the exact path the UI uses (`media_response`, which now transcodes
//! HEIC/TIFF thumbnails and falls back to a blob's embedded header when the DB
//! header is stale). Read-only, offline.
//!
//! Run: SP_PW='yourpassword' cargo run -p stingle-core --example diagnose_album

use stingle_core::{Account, FileSet};

fn renderable(b: &[u8]) -> Result<&'static str, String> {
    let s = |sig: &[u8]| b.len() >= sig.len() && &b[..sig.len()] == sig;
    if s(&[0xff, 0xd8, 0xff]) {
        Ok("jpeg")
    } else if s(b"\x89PNG") {
        Ok("png")
    } else if s(b"GIF8") {
        Ok("gif")
    } else if s(b"RIFF") && b.len() >= 12 && &b[8..12] == b"WEBP" {
        Ok("webp")
    } else if s(b"BM") {
        Ok("bmp")
    } else if b.len() >= 12 && &b[4..8] == b"ftyp" && matches!(&b[8..12], b"avif" | b"avis") {
        Ok("avif")
    } else if b.len() >= 12 && &b[4..8] == b"ftyp" {
        Err(format!("still-unrenderable ftyp:{}", String::from_utf8_lossy(&b[8..12])))
    } else {
        Err(format!("still-unrenderable {:02x?}", &b[..b.len().min(4)]))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = std::env::var("SP_BASE").unwrap_or_else(|_| r"D:\Stingle".to_string());
    let key = std::env::var("SP_KEY").unwrap_or_else(|_| {
        "b893d199d257194c712b5e20e9fa76396a872a96f221131f44fc17b388366cfe".to_string()
    });
    let pw = std::env::var("SP_PW").expect("set SP_PW to the account password");
    let acc = Account::resume(std::path::Path::new(&base), &key, &pw)?;

    let (mut ok, mut broken) = (0usize, 0usize);
    for (a, name) in acc.list_albums_with_names(true)? {
        for f in acc.db.list_album_files(&a.album_id, stingle_core::Sort::Desc, None, 0)? {
            match acc
                .media_response(FileSet::Album, Some(&a.album_id), &f.filename, true, None)
                .await
            {
                Ok(m) => match renderable(&m.body) {
                    Ok(_) => ok += 1,
                    Err(why) => {
                        broken += 1;
                        println!("STILL BROKEN \"{name}\" {} -> {why}", f.filename);
                    }
                },
                Err(e) => {
                    broken += 1;
                    println!("STILL BROKEN \"{name}\" {} -> decrypt err: {e}", f.filename);
                }
            }
        }
    }
    println!("\n=== renderable: {ok}, still-broken: {broken} ===");
    Ok(())
}
