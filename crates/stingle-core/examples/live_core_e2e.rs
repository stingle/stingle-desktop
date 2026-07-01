//! Full live core test against api.stingle.org. Validates the whole engine:
//! import → sync(upload) → second-device sync(download) → decrypt → albums →
//! takeout, then cleans up.
//!
//! Run: `cargo run -p stingle-core --example live_core_e2e`

use std::io::Cursor;
use std::path::PathBuf;

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

fn rand_hex(n: usize) -> String {
    hex::encode(sodium::random_bytes(n).unwrap())
}

fn make_png(seed: u8) -> Vec<u8> {
    let mut img = RgbImage::new(96, 64);
    for (x, y, p) in img.enumerate_pixels_mut() {
        *p = image::Rgb([(x as u8).wrapping_add(seed), (y as u8).wrapping_mul(3), seed]);
    }
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut out, ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = std::env::temp_dir().join(format!("sp-core-{}", rand_hex(4)));
    let dev1 = tmp.join("dev1");
    let dev2 = tmp.join("dev2");
    let out_dir = tmp.join("takeout");
    std::fs::create_dir_all(&tmp)?;

    // --- account (reuse or register) ---
    let creds: Creds = match std::fs::read_to_string(CRED_PATH).ok().and_then(|s| serde_json::from_str(&s).ok()) {
        Some(c) => {
            println!("[*] reusing {}", { let c: &Creds = &c; c.email.clone() });
            c
        }
        None => {
            let c = Creds {
                email: format!("spcore-{}@fenritz.com", rand_hex(8)),
                password: format!("Pw-{}", rand_hex(12)),
            };
            println!("[*] registering {}", c.email);
            Account::register(SERVER, &c.email, &c.password, &dev1, true).await?;
            std::fs::write(CRED_PATH, serde_json::to_string_pretty(&c)?)?;
            c
        }
    };

    // --- device 1: login, import a photo, sync up ---
    let dev1_acc = Account::login(SERVER, &creds.email, &creds.password, &dev1).await?;
    println!("[+] dev1 logged in (userId={})", dev1_acc.info.user_id);

    let png = make_png(7);
    let src = tmp.join(format!("photo-{}.png", rand_hex(4)));
    std::fs::write(&src, &png)?;

    let filename = dev1_acc
        .import_file(&src, FileSet::Gallery, None)
        .await?
        .expect("imported");
    println!("[+] dev1 imported -> {filename}");

    dev1_acc.full_sync().await?;
    println!("[+] dev1 full_sync done (uploaded); space={:?}", dev1_acc.space());

    // --- device 2: fresh dir, login, sync down, decrypt, verify ---
    let dev2_acc = Account::login(SERVER, &creds.email, &creds.password, &dev2).await?;
    dev2_acc.sync_cloud_to_local().await?;
    let count = dev2_acc.db.count_files(FileSet::Gallery)?;
    println!("[+] dev2 synced; gallery has {count} file(s)");
    assert!(count >= 1);

    let dec = dev2_acc
        .get_decrypted(FileSet::Gallery, None, &filename, false)
        .await?;
    assert_eq!(dec, png, "dev2 decrypted file must match original PNG");
    println!("[+] dev2 downloaded + decrypted FILE matches original ({} bytes)", dec.len());

    let dec_thumb = dev2_acc
        .get_decrypted(FileSet::Gallery, None, &filename, true)
        .await?;
    println!("[+] dev2 decrypted THUMB ok ({} bytes)", dec_thumb.len());

    let orig_name = dev2_acc.original_name(FileSet::Gallery, None, &filename)?;
    println!("[+] recovered original filename: {orig_name}");

    // --- albums: create on dev1, add a photo, sync; dev2 sees it ---
    let album_id = dev1_acc.create_album("Live Test Album").await?;
    let src2 = tmp.join(format!("album-{}.png", rand_hex(4)));
    std::fs::write(&src2, make_png(99))?;
    let afile = dev1_acc
        .import_file(&src2, FileSet::Album, Some(&album_id))
        .await?
        .expect("album import");
    dev1_acc.full_sync().await?;
    println!("[+] dev1 created album + uploaded album file");

    dev2_acc.sync_cloud_to_local().await?;
    let albums = dev2_acc.list_albums_with_names(true)?;
    println!("[+] dev2 sees {} album(s): {:?}", albums.len(), albums.iter().map(|(_, n)| n).collect::<Vec<_>>());
    assert!(albums.iter().any(|(_, n)| n == "Live Test Album"));
    let adec = dev2_acc
        .get_decrypted(FileSet::Album, Some(&album_id), &afile, false)
        .await?;
    assert_eq!(adec, make_png(99), "album file must decrypt for dev2");
    println!("[+] dev2 decrypted ALBUM file ({} bytes)", adec.len());

    // --- takeout on dev2 ---
    let stats = dev2_acc.takeout(&out_dir, false, None).await?;
    println!("[+] takeout wrote {} file(s), {} error(s)", stats.written, stats.errors);
    assert!(stats.written >= 2 && stats.errors == 0);

    // --- cleanup: trash + delete the gallery file, delete album ---
    dev1_acc.trash(&[filename.clone()]).await?;
    dev1_acc.full_sync().await?;
    // It is now in trash; permanently delete.
    dev1_acc.sync_cloud_to_local().await?;
    dev1_acc.delete_permanently(&[filename.clone()]).await?;
    dev1_acc.delete_album(&album_id).await?;
    println!("[+] cleaned up (trashed+deleted file, deleted album)");

    // tidy temp
    let _ = std::fs::remove_dir_all(&tmp);

    println!("\n=== LIVE CORE E2E PASSED ===");
    Ok(())
}
