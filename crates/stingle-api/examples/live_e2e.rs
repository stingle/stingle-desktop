//! Live end-to-end test against api.stingle.org.
//!
//! Registers (or reuses) a test account, then exercises the full crypto+API
//! path: login → key-bundle unlock → getServerPK → getUpdates → upload a
//! file+thumbnail → getUpdates (sees it) → download → decrypt → verify bytes.
//!
//! Run: `cargo run -p stingle-api --example live_e2e`
//! Credentials are cached in `crates/stingle-api/.live_account.json` (gitignored)
//! so reruns reuse the same account instead of registering new ones.

use std::io::Cursor;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64URL};
use base64::Engine;
use serde::{Deserialize, Serialize};

use stingle_api::{Client, ServerCrypto};
use stingle_crypto::constants::FILE_TYPE_PHOTO;
use stingle_crypto::file;
use stingle_crypto::keys::{KeyBundle, KeyPair};
use stingle_crypto::pwhash;
use stingle_crypto::sodium;

const CRED_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/.live_account.json");

#[derive(Serialize, Deserialize)]
struct Creds {
    email: String,
    password: String,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn load_creds() -> Option<Creds> {
    std::fs::read_to_string(CRED_PATH)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

fn save_creds(c: &Creds) {
    let _ = std::fs::write(CRED_PATH, serde_json::to_string_pretty(c).unwrap());
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new(None)?;

    // --- account: reuse cached creds or register a fresh one ---
    let (creds, keypair) = match load_creds() {
        Some(c) => {
            println!("[*] reusing cached account {}", c.email);
            // We'll obtain the keypair by unlocking the server key bundle at login.
            (c, None)
        }
        None => {
            let rand = hex::encode(sodium::random_bytes(8)?);
            let email = format!("spdesk-{rand}@fenritz.com");
            let password = format!("Pw-{}", hex::encode(sodium::random_bytes(12)?));
            println!("[*] registering new account {email}");

            let kp = KeyPair::generate()?;
            let bundle = KeyBundle::create(&password, &kp)?;
            let salt = pwhash::generate_salt()?;
            let salt_hex = hex::encode_upper(&salt);
            let login_hash = pwhash::password_hash_for_storage(&password, &salt)?;

            client
                .register(&email, &login_hash, &salt_hex, true, &bundle.to_base64())
                .await?;
            println!("[+] registered");
            let c = Creds { email, password };
            save_creds(&c);
            (c, Some(kp))
        }
    };

    // --- login ---
    let salt_hex = client.pre_login(&creds.email).await?;
    let salt = hex::decode(salt_hex.trim())?;
    let login_hash = pwhash::password_hash_for_storage(&creds.password, &salt)?;
    let login = client.login(&creds.email, &login_hash).await?;
    println!(
        "[+] logged in: userId={} keyBackedUp={} home={}",
        login.user_id, login.is_key_backed_up, login.home_folder
    );

    // Unlock keypair from the server-provided bundle (or use the freshly generated one).
    let keypair = match keypair {
        Some(kp) => kp,
        None => KeyBundle::parse_base64(&login.key_bundle)?.unlock(&creds.password)?,
    };
    println!("[+] key bundle unlocked, pk matches");

    // --- server public key ---
    let server_pk_b64 = client.get_server_pk(&login.token).await?;
    let server_pk = B64.decode(server_pk_b64.trim())?;
    println!("[+] got server PK ({} bytes)", server_pk.len());
    let _sc = ServerCrypto {
        server_pk: &server_pk,
        user_sk: &keypair.secret_key,
    };

    // --- initial getUpdates ---
    let updates = client
        .get_updates(&login.token, Default::default())
        .await?;
    println!(
        "[+] getUpdates: {} gallery files, spaceUsed={:?} quota={:?}",
        updates.files.len(),
        updates.space_used,
        updates.space_quota
    );

    // --- build an encrypted file + thumbnail and upload ---
    let original: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
    let thumb_plain: Vec<u8> = (0..3_000u32).map(|i| (i % 113) as u8).collect();

    let file_id = file::new_file_id()?;
    let (sp_file, _h) = file::encrypt_bytes(
        &original,
        "vacation.jpg",
        FILE_TYPE_PHOTO,
        file_id.clone(),
        0,
        &keypair.public_key,
    )?;
    // The thumbnail must share the file's fileId — the server reads the fileId
    // from the plaintext outer header of both and requires them to match.
    let (sp_thumb, _th) = file::encrypt_bytes(
        &thumb_plain,
        "vacation.jpg",
        FILE_TYPE_PHOTO,
        file_id.clone(),
        0,
        &keypair.public_key,
    )?;

    // Combined `headers` string = base64url(fileHeader)*base64url(thumbHeader).
    let file_header_bytes = file::extract_header_bytes(&mut Cursor::new(&sp_file))?;
    let thumb_header_bytes = file::extract_header_bytes(&mut Cursor::new(&sp_thumb))?;
    let headers = format!(
        "{}*{}",
        B64URL.encode(&file_header_bytes),
        B64URL.encode(&thumb_header_bytes)
    );

    let filename = format!("{}.sp", hex::encode(sodium::random_bytes(16)?));
    let ts = now_ms();
    let space = client
        .upload(
            &login.token,
            stingle_api::models::set::GALLERY,
            "",
            1,
            ts,
            ts,
            &headers,
            &filename,
            sp_file.clone(),
            sp_thumb.clone(),
        )
        .await?;
    println!("[+] uploaded {filename}; spaceUsed={:?}", space.space_used);

    // --- getUpdates should now include the file ---
    let updates2 = client
        .get_updates(&login.token, Default::default())
        .await?;
    let found = updates2.files.iter().find(|f| f.filename == filename);
    assert!(found.is_some(), "uploaded file not present in getUpdates");
    println!("[+] getUpdates now lists the uploaded file");

    // --- download + decrypt + verify ---
    let dl_file = client
        .download(&login.token, &filename, stingle_api::models::set::GALLERY, false)
        .await?;
    let dec_file = file::decrypt_bytes(&dl_file, &keypair.public_key, &keypair.secret_key)?;
    assert_eq!(dec_file, original, "downloaded file did not match original");
    println!("[+] downloaded + decrypted FILE matches original ({} bytes)", dec_file.len());

    let dl_thumb = client
        .download(&login.token, &filename, stingle_api::models::set::GALLERY, true)
        .await?;
    let dec_thumb = file::decrypt_bytes(&dl_thumb, &keypair.public_key, &keypair.secret_key)?;
    assert_eq!(dec_thumb, thumb_plain, "downloaded thumb did not match original");
    println!("[+] downloaded + decrypted THUMB matches original ({} bytes)", dec_thumb.len());

    println!("\n=== LIVE E2E PASSED ===");
    Ok(())
}
