//! Album cryptography: each album has its own Curve25519 keypair. The album's
//! secret key is sealed to a user's public key; album metadata is sealed to the
//! album's own public key; files in the album have headers sealed to the album
//! public key.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use zeroize::Zeroizing;

use crate::constants::*;
use crate::error::{CryptoError, Result};
use crate::keys::KeyPair;
use crate::sodium;

/// Base64 (standard) encoded album material for `sync/addAlbum`.
pub struct AlbumEncData {
    pub public_key: String,
    pub encrypted_private_key: String,
    pub metadata: String,
}

/// Generate a new album keypair plus its encrypted material, sealing the album
/// secret key to `user_pk` and the metadata (name) to the album public key.
/// Equivalent to `Crypto.generateEncryptedAlbumData`.
pub fn generate_encrypted_album_data(user_pk: &[u8], name: &str) -> Result<(KeyPair, AlbumEncData)> {
    let album = KeyPair::generate()?;
    let enc_metadata = encrypt_album_metadata(name, &album.public_key)?;
    let enc_sk = encrypt_album_sk(&album.secret_key, user_pk)?;
    let data = AlbumEncData {
        public_key: B64.encode(&album.public_key),
        encrypted_private_key: B64.encode(&enc_sk),
        metadata: B64.encode(&enc_metadata),
    };
    Ok((album, data))
}

/// Seal an album secret key to a user's public key.
pub fn encrypt_album_sk(album_sk: &[u8], user_pk: &[u8]) -> Result<Vec<u8>> {
    sodium::box_seal(album_sk, user_pk)
}

/// Open an album secret key sealed to the user's keypair.
pub fn decrypt_album_sk(enc_album_sk: &[u8], user_pk: &[u8], user_sk: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    sodium::box_seal_open(enc_album_sk, user_pk, user_sk)
}

/// Encode and seal album metadata (currently just the name) to the album PK.
pub fn encrypt_album_metadata(name: &str, album_pk: &[u8]) -> Result<Vec<u8>> {
    let mut meta = Vec::new();
    meta.push(CURRENT_ALBUM_METADATA_VERSION);
    let name_bytes = name.as_bytes();
    meta.extend_from_slice(&(name_bytes.len() as u32).to_be_bytes());
    meta.extend_from_slice(name_bytes);
    sodium::box_seal(&meta, album_pk)
}

/// Decrypt album metadata, returning the album name. Requires the album keypair.
pub fn decrypt_album_metadata(enc_metadata: &[u8], album_pk: &[u8], album_sk: &[u8]) -> Result<String> {
    let meta = sodium::box_seal_open(enc_metadata, album_pk, album_sk)?;
    let mut c = std::io::Cursor::new(meta.as_slice());
    use std::io::Read;
    let mut version = [0u8; 1];
    c.read_exact(&mut version)?;
    if version[0] != CURRENT_ALBUM_METADATA_VERSION {
        return Err(CryptoError::UnsupportedVersion(format!(
            "album metadata version {}",
            version[0]
        )));
    }
    let mut len_bytes = [0u8; 4];
    c.read_exact(&mut len_bytes)?;
    let name_len = u32::from_be_bytes(len_bytes) as usize;
    if name_len == 0 {
        return Ok(String::new());
    }
    if name_len > MAX_BUFFER_LENGTH {
        return Err(CryptoError::MalformedFile("invalid album name length"));
    }
    let mut name_bytes = vec![0u8; name_len];
    c.read_exact(&mut name_bytes)?;
    Ok(String::from_utf8_lossy(&name_bytes).into_owned())
}
