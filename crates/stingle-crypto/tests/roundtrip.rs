//! Internal round-trip and structural tests for the crypto core.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

use stingle_crypto::album;
use stingle_crypto::constants::*;
use stingle_crypto::file;
use stingle_crypto::keys::{encrypt_params_for_server, KeyBundle, KeyPair};
use stingle_crypto::mnemonic;
use stingle_crypto::pwhash::{self, KdfDifficulty};
use stingle_crypto::sodium;

#[test]
fn argon2id_is_deterministic_and_storage_hash_is_upper_hex() {
    let salt = vec![7u8; PWHASH_SALTBYTES];
    let a = pwhash::derive_key("correct horse", &salt, KdfDifficulty::Normal).unwrap();
    let b = pwhash::derive_key("correct horse", &salt, KdfDifficulty::Normal).unwrap();
    assert_eq!(a.as_slice(), b.as_slice());
    assert_eq!(a.len(), SECRETBOX_KEYBYTES);

    let hash = pwhash::password_hash_for_storage("correct horse", &salt).unwrap();
    assert_eq!(hash.len(), PWHASH_STORAGE_LEN * 2); // 128 hex chars
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()));
    // Stable across calls.
    assert_eq!(hash, pwhash::password_hash_for_storage("correct horse", &salt).unwrap());
}

#[test]
fn keypair_public_derives_from_secret() {
    let kp = KeyPair::generate().unwrap();
    let kp2 = KeyPair::from_secret_key(&kp.secret_key).unwrap();
    assert_eq!(kp.public_key, kp2.public_key);
}

#[test]
fn key_bundle_roundtrip_and_wrong_password() {
    let kp = KeyPair::generate().unwrap();
    let bundle = KeyBundle::create("hunter2", &kp).unwrap();

    // Serialize → base64 → parse → unlock.
    let b64 = bundle.to_base64();
    let parsed = KeyBundle::parse_base64(&b64).unwrap();
    let unlocked = parsed.unlock("hunter2").unwrap();
    assert_eq!(unlocked.public_key, kp.public_key);
    assert_eq!(unlocked.secret_key.as_slice(), kp.secret_key.as_slice());

    // Structural sizes.
    let raw = bundle.serialize();
    let expected_len = 3 + 1 + 1 + PUBLICKEYBYTES + (SECRETKEYBYTES + SECRETBOX_MACBYTES)
        + PWHASH_SALTBYTES + SECRETBOX_NONCEBYTES;
    assert_eq!(raw.len(), expected_len);
    assert_eq!(&raw[0..3], KEY_FILE_BEGINNING);

    // Wrong password must fail authentication.
    assert!(parsed.unlock("wrong").is_err());
}

#[test]
fn mnemonic_recovers_private_key_and_rebuilds_keypair() {
    let kp = KeyPair::generate().unwrap();
    let phrase = mnemonic::entropy_to_mnemonic(&kp.secret_key).unwrap();
    assert_eq!(phrase.split(' ').count(), 24);

    let recovered = mnemonic::mnemonic_to_entropy(&phrase).unwrap();
    assert_eq!(recovered.as_slice(), kp.secret_key.as_slice());

    let rebuilt = KeyPair::from_secret_key(&recovered).unwrap();
    assert_eq!(rebuilt.public_key, kp.public_key);
}

#[test]
fn file_roundtrip_various_sizes() {
    let kp = KeyPair::generate().unwrap();
    let other = KeyPair::generate().unwrap();

    // Includes 0, sub-chunk, exact boundaries, and multi-chunk.
    let chunk = DEFAULT_CHUNK_SIZE as usize;
    let sizes = [0usize, 1, 100, chunk - 1, chunk, chunk + 1, 2 * chunk + 12345];
    for &size in &sizes {
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let file_id = file::new_file_id().unwrap();
        let (sp, header) = file::encrypt_bytes(
            &data,
            "vacation.jpg",
            FILE_TYPE_PHOTO,
            file_id.clone(),
            0,
            &kp.public_key,
        )
        .unwrap();

        // Header metadata survives.
        assert_eq!(header.data_size, size as u64);
        assert_eq!(header.file_id, file_id);
        assert_eq!(&sp[0..2], FILE_BEGINNING);

        // Correct keypair decrypts to the original bytes.
        let out = file::decrypt_bytes(&sp, &kp.public_key, &kp.secret_key).unwrap();
        assert_eq!(out, data, "size {size} failed round trip");

        // Reading the header back recovers the filename.
        let mut cur = std::io::Cursor::new(&sp);
        let parsed = file::read_header(&mut cur, &kp.public_key, &kp.secret_key).unwrap();
        assert_eq!(parsed.filename, "vacation.jpg");
        assert_eq!(parsed.file_type, FILE_TYPE_PHOTO);

        // A different keypair cannot decrypt the sealed header.
        assert!(file::decrypt_bytes(&sp, &other.public_key, &other.secret_key).is_err());
    }
}

#[test]
fn extract_header_bytes_matches_serialized_prefix() {
    let kp = KeyPair::generate().unwrap();
    let (sp, _) = file::encrypt_bytes(b"hello", "a.txt", FILE_TYPE_GENERAL, file::new_file_id().unwrap(), 0, &kp.public_key).unwrap();
    let mut cur = std::io::Cursor::new(&sp);
    let header_bytes = file::extract_header_bytes(&mut cur).unwrap();
    // The extracted header is a prefix of the full file.
    assert_eq!(&sp[..header_bytes.len()], header_bytes.as_slice());
    // And it parses back to a valid header.
    let mut hc = std::io::Cursor::new(&header_bytes);
    let h = file::read_header(&mut hc, &kp.public_key, &kp.secret_key).unwrap();
    assert_eq!(h.filename, "a.txt");
}

#[test]
fn album_data_roundtrip() {
    let user = KeyPair::generate().unwrap();
    let (album_kp, enc) = album::generate_encrypted_album_data(&user.public_key, "Summer 2026").unwrap();

    // Recover the album secret key via the user's keypair.
    let enc_sk = B64.decode(&enc.encrypted_private_key).unwrap();
    let album_sk = album::decrypt_album_sk(&enc_sk, &user.public_key, &user.secret_key).unwrap();
    assert_eq!(album_sk.as_slice(), album_kp.secret_key.as_slice());

    // Decrypt metadata using the album keypair.
    let album_pk = B64.decode(&enc.public_key).unwrap();
    assert_eq!(album_pk, album_kp.public_key);
    let enc_meta = B64.decode(&enc.metadata).unwrap();
    let name = album::decrypt_album_metadata(&enc_meta, &album_pk, &album_sk).unwrap();
    assert_eq!(name, "Summer 2026");

    // An album-file header sealed to the album PK is readable with the album keypair.
    let (sp, _) = file::encrypt_bytes(b"x", "p.jpg", FILE_TYPE_PHOTO, file::new_file_id().unwrap(), 0, &album_kp.public_key).unwrap();
    let out = file::decrypt_bytes(&sp, &album_kp.public_key, &album_sk).unwrap();
    assert_eq!(out, b"x");
}

#[test]
fn server_param_encryption_roundtrip() {
    let server = KeyPair::generate().unwrap();
    let user = KeyPair::generate().unwrap();

    let json = br#"{"albumId":"abc","count":"2"}"#;
    let b64 = encrypt_params_for_server(json, &server.public_key, &user.secret_key).unwrap();

    // Server decrypts with its secret key and the user's public key.
    let combined = B64.decode(&b64).unwrap();
    let recovered = sodium::box_open_easy(&combined, &user.public_key, &server.secret_key).unwrap();
    assert_eq!(recovered.as_slice(), json.as_slice());
}

#[test]
fn decrypt_range_matches_plaintext_slices() {
    let kp = KeyPair::generate().unwrap();
    // Multi-chunk file (~2.5 MiB over 1 MiB chunks).
    let size = 2 * (DEFAULT_CHUNK_SIZE as usize) + 500_000;
    let data: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
    let (sp, header) = file::encrypt_bytes(&data, "v.mp4", FILE_TYPE_GENERAL, file::new_file_id().unwrap(), 0, &kp.public_key).unwrap();

    let outer_len = file::outer_header_len(&mut std::io::Cursor::new(&sp)).unwrap();

    // Ranges spanning chunk boundaries, mid-chunk, and the tail.
    let cs = DEFAULT_CHUNK_SIZE as u64;
    let ranges = [
        (0u64, 10u64),
        (cs - 5, cs + 5),            // across chunk 0/1 boundary
        (cs, cs),                    // single byte at boundary
        (2 * cs - 1, 2 * cs + 100),  // across chunk 1/2 boundary
        (size as u64 - 10, size as u64 - 1), // tail
    ];
    for (s, e) in ranges {
        let got = file::decrypt_range(
            &mut std::io::Cursor::new(&sp),
            outer_len,
            &header.symmetric_key,
            header.chunk_size,
            s,
            e,
        )
        .unwrap();
        assert_eq!(got, &data[s as usize..=e as usize], "range {s}..={e}");
    }
}
