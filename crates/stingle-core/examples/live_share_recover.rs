//! Live test of album sharing (two accounts) and account recovery (mnemonic).
//! Run: `cargo run -p stingle-core --example live_share_recover`

use std::io::Cursor;

use image::{ImageFormat, RgbImage};
use serde::{Deserialize, Serialize};

use stingle_core::{Account, FileSet};
use stingle_crypto::sodium;

const CRED_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/.live_account.json");
const SERVER: &str = "https://api.stingle.org/";

#[derive(Serialize, Deserialize)]
struct Creds {
    email: String,
    password: String,
}

fn rh(n: usize) -> String {
    hex::encode(sodium::random_bytes(n).unwrap())
}

fn make_png(seed: u8) -> Vec<u8> {
    let mut img = RgbImage::new(80, 60);
    for (x, y, p) in img.enumerate_pixels_mut() {
        *p = image::Rgb([x as u8, y as u8, seed]);
    }
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img).write_to(&mut out, ImageFormat::Png).unwrap();
    out.into_inner()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = std::env::temp_dir().join(format!("sp-sr-{}", rh(4)));
    std::fs::create_dir_all(&tmp)?;

    // Account A: reuse cached owner account.
    let a: Creds = serde_json::from_str(&std::fs::read_to_string(CRED_PATH)?)?;
    println!("[*] owner A = {}", a.email);
    let a_acc = Account::login(SERVER, &a.email, &a.password, &tmp.join("A")).await?;

    // Account B: fresh recipient.
    let b = Creds { email: format!("spshare-{}@fenritz.com", rh(8)), password: format!("Pw-{}", rh(12)) };
    println!("[*] recipient B = {}", b.email);
    Account::register(SERVER, &b.email, &b.password, &tmp.join("Breg"), true).await?;
    let b_acc = Account::login(SERVER, &b.email, &b.password, &tmp.join("B")).await?;

    // A creates an album, adds a known photo, shares with B.
    let album_id = a_acc.create_album(&format!("Shared {}", rh(2))).await?;
    let png = make_png(42);
    let src = tmp.join("shared.png");
    std::fs::write(&src, &png)?;
    a_acc.import_file(&src, FileSet::Album, Some(&album_id)).await?;
    a_acc.full_sync().await?;
    a_acc.share_album(&album_id, &[b.email.clone()], true, true, true).await?;
    println!("[+] A shared album {album_id} with B");

    // B syncs and sees the shared album + decrypts the file.
    b_acc.sync_cloud_to_local().await?;
    let albums = b_acc.list_albums_with_names(true)?;
    let shared = albums.iter().find(|(al, _)| al.album_id == album_id);
    assert!(shared.is_some(), "B should see the shared album");
    let (al, name) = shared.unwrap();
    assert!(!al.is_owner, "B is not the owner");
    println!("[+] B sees shared album: \"{name}\" (owner={})", al.is_owner);

    let files = b_acc.db.list_album_files(&album_id, stingle_core::Sort::Desc, None, 0)?;
    assert!(!files.is_empty(), "B should see the album file");
    let dec = b_acc.get_decrypted(FileSet::Album, Some(&album_id), &files[0].filename, false).await?;
    assert_eq!(dec, png, "B must decrypt the shared file to the original bytes");
    println!("[+] B decrypted the shared file ({} bytes) — sharing works", dec.len());

    // Cleanup sharing.
    a_acc.unshare_album(&album_id).await?;
    a_acc.delete_album(&album_id).await?;
    println!("[+] A unshared + deleted album");

    // ---- Recovery: throwaway account C ----
    let c = Creds { email: format!("sprec-{}@fenritz.com", rh(8)), password: format!("Pw-{}", rh(12)) };
    println!("[*] recovery account C = {}", c.email);
    let c_acc = Account::register(SERVER, &c.email, &c.password, &tmp.join("Creg"), true).await?;
    let phrase = c_acc.recovery_phrase()?;
    println!("[+] C recovery phrase has {} words", phrase.split(' ').count());
    drop(c_acc);

    let new_password = format!("New-{}", rh(12));
    let recovered = Account::recover(SERVER, &c.email, &phrase, &new_password, &tmp.join("Crec")).await?;
    println!("[+] recovered C with new password; userId={}", recovered.info.user_id);

    // Confirm the new password actually works via a fresh login + sync.
    let relog = Account::login(SERVER, &c.email, &new_password, &tmp.join("Crelog")).await?;
    relog.sync_cloud_to_local().await?;
    println!("[+] logged in with the NEW password — recovery works");

    let _ = std::fs::remove_dir_all(&tmp);
    println!("\n=== LIVE SHARE + RECOVER PASSED ===");
    Ok(())
}
