//! Verify move-to-album (header re-seal), save (decrypt-export), and cache
//! eviction on a throwaway account.
//! Run: `cargo run -p stingle-core --example verify_move`

use std::io::Cursor;

use image::{ImageFormat, RgbImage};
use stingle_core::{Account, FileSet};

const SERVER: &str = "https://api.stingle.org/";

fn rh(n: usize) -> String { hex::encode(stingle_crypto::sodium::random_bytes(n).unwrap()) }

fn make_png(seed: u8) -> Vec<u8> {
    let mut img = RgbImage::new(64, 48);
    for (x, y, p) in img.enumerate_pixels_mut() { *p = image::Rgb([x as u8, y as u8, seed]); }
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img).write_to(&mut out, ImageFormat::Png).unwrap();
    out.into_inner()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = std::env::temp_dir().join(format!("sp-move-{}", rh(4)));
    std::fs::create_dir_all(&tmp)?;
    let email = format!("spmove-{}@fenritz.com", rh(8));
    let pass = format!("Pw-{}", rh(12));
    let acc = Account::register(SERVER, &email, &pass, &tmp.join("acc"), true).await?;
    println!("[*] account {email}");

    // Import two photos to the gallery.
    let png1 = make_png(11);
    let png2 = make_png(22);
    let s1 = tmp.join("a.png"); std::fs::write(&s1, &png1)?;
    let s2 = tmp.join("b.png"); std::fs::write(&s2, &png2)?;
    let fn1 = acc.import_file(&s1, FileSet::Gallery, None).await?.unwrap();
    let fn2 = acc.import_file(&s2, FileSet::Gallery, None).await?.unwrap();
    acc.full_sync().await?;
    println!("[+] imported 2 photos, uploaded");

    // Create an album and MOVE photo 1 into it (re-seals headers to album key).
    let album = acc.create_album("Move Target").await?;
    acc.move_to_album(FileSet::Gallery, None, &[fn1.clone()], &album).await?;
    println!("[+] moved {fn1} into album {album}");

    let gcount = acc.db.count_files(FileSet::Gallery)?;
    let acount = acc.db.count_album_files(&album)?;
    assert_eq!(gcount, 1, "gallery should have 1 left");
    assert_eq!(acount, 1, "album should have 1");

    // The moved album file must decrypt to the original bytes (proves re-seal).
    let dec = acc.get_decrypted(FileSet::Album, Some(&album), &fn1, false).await?;
    assert_eq!(dec, png1, "moved album file must decrypt to original PNG");
    println!("[+] moved album file decrypts correctly (re-seal works)");

    // Move it back to the gallery (reverse re-seal).
    acc.move_to_gallery(&album, &[fn1.clone()]).await?;
    assert_eq!(acc.db.count_files(FileSet::Gallery)?, 2, "gallery back to 2");
    assert_eq!(acc.db.count_album_files(&album)?, 0, "album empty");
    let back = acc.get_decrypted(FileSet::Gallery, None, &fn1, false).await?;
    assert_eq!(back, png1, "moved-back file decrypts in gallery");
    println!("[+] move-to-gallery decrypts correctly");

    // Save (decrypt + export) a gallery photo to a folder.
    let out = tmp.join("saved");
    let n = acc.save_files(FileSet::Gallery, None, &[fn2.clone()], &out).await?;
    assert_eq!(n, 1);
    let saved: Vec<_> = std::fs::read_dir(&out)?.flatten().collect();
    assert_eq!(saved.len(), 1, "one file exported");
    let bytes = std::fs::read(saved[0].path())?;
    assert_eq!(bytes, png2, "exported file must equal original");
    println!("[+] save (decrypt+export) produced the original bytes");

    // Cache eviction: set a tiny limit and confirm evictable blobs are removed.
    let before = acc.cache_size_bytes();
    acc.set_cache_limit_bytes(1)?; // 1 byte -> evict everything re-downloadable
    let after = acc.cache_size_bytes();
    println!("[+] cache eviction: {before} -> {after} bytes (limit 1)");
    assert!(after < before, "cache should shrink after enforcing a tiny limit");

    let _ = std::fs::remove_dir_all(&tmp);
    println!("\n=== MOVE + SAVE + CACHE VERIFIED ===");
    Ok(())
}
