//! Thin, safe wrappers over the libsodium primitives used by Stingle.
//!
//! Every function here maps 1:1 onto a libsodium call with identical
//! parameters to the Android client, so outputs are byte-for-byte compatible.

use std::sync::Once;

use libsodium_sys as ffi;
use zeroize::Zeroizing;

use crate::constants::*;
use crate::error::{CryptoError, Result};

static INIT: Once = Once::new();
static mut INIT_OK: bool = false;

/// Initialize libsodium. Safe to call repeatedly; runs the underlying
/// `sodium_init()` exactly once. Called automatically by every wrapper.
pub fn init() -> Result<()> {
    // SAFETY: guarded by `Once`; sodium_init is the documented init entry point.
    INIT.call_once(|| unsafe {
        INIT_OK = ffi::sodium_init() >= 0;
    });
    if unsafe { INIT_OK } {
        Ok(())
    } else {
        Err(CryptoError::InitFailed)
    }
}

#[inline]
fn ensure_init() -> Result<()> {
    init()
}

/// Fill `buf` with cryptographically secure random bytes.
pub fn random_into(buf: &mut [u8]) -> Result<()> {
    ensure_init()?;
    // SAFETY: writing exactly buf.len() bytes into a valid mutable slice.
    unsafe { ffi::randombytes_buf(buf.as_mut_ptr() as *mut _, buf.len()) };
    Ok(())
}

/// Return `len` cryptographically secure random bytes.
pub fn random_bytes(len: usize) -> Result<Vec<u8>> {
    let mut v = vec![0u8; len];
    random_into(&mut v)?;
    Ok(v)
}

/// Argon2id (v1.3) password hashing — `crypto_pwhash`.
pub fn pwhash(
    out_len: usize,
    password: &[u8],
    salt: &[u8],
    opslimit: u64,
    memlimit: usize,
) -> Result<Zeroizing<Vec<u8>>> {
    ensure_init()?;
    if salt.len() != PWHASH_SALTBYTES {
        return Err(CryptoError::InvalidInput(format!(
            "salt must be {PWHASH_SALTBYTES} bytes, got {}",
            salt.len()
        )));
    }
    let mut out = Zeroizing::new(vec![0u8; out_len]);
    // SAFETY: pointers/lengths are valid; alg is the Argon2id13 constant.
    let rc = unsafe {
        ffi::crypto_pwhash(
            out.as_mut_ptr(),
            out_len as u64,
            password.as_ptr() as *const _,
            password.len() as u64,
            salt.as_ptr(),
            opslimit,
            memlimit,
            PWHASH_ALG_ARGON2ID13,
        )
    };
    if rc != 0 {
        return Err(CryptoError::Sodium("crypto_pwhash (out of memory?)"));
    }
    Ok(out)
}

/// Generate a Curve25519 box keypair. Returns `(public_key, secret_key)`.
pub fn box_keypair() -> Result<(Vec<u8>, Zeroizing<Vec<u8>>)> {
    ensure_init()?;
    let mut pk = vec![0u8; PUBLICKEYBYTES];
    let mut sk = Zeroizing::new(vec![0u8; SECRETKEYBYTES]);
    // SAFETY: output buffers sized per libsodium contract.
    let rc = unsafe { ffi::crypto_box_keypair(pk.as_mut_ptr(), sk.as_mut_ptr()) };
    if rc != 0 {
        return Err(CryptoError::Sodium("crypto_box_keypair"));
    }
    Ok((pk, sk))
}

/// Derive the Curve25519 public key from a secret key (`crypto_scalarmult_base`).
pub fn scalarmult_base(secret_key: &[u8]) -> Result<Vec<u8>> {
    ensure_init()?;
    if secret_key.len() != SECRETKEYBYTES {
        return Err(CryptoError::InvalidInput("bad secret key length".into()));
    }
    let mut pk = vec![0u8; PUBLICKEYBYTES];
    // SAFETY: input/output sized correctly.
    let rc = unsafe { ffi::crypto_scalarmult_base(pk.as_mut_ptr(), secret_key.as_ptr()) };
    if rc != 0 {
        return Err(CryptoError::Sodium("crypto_scalarmult_base"));
    }
    Ok(pk)
}

/// Authenticated symmetric encryption — `crypto_secretbox_easy`.
/// Output is `MAC || ciphertext` per libsodium (combined mode), length = data + 16.
pub fn secretbox_easy(key: &[u8], nonce: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    ensure_init()?;
    check_len("secretbox key", key, SECRETBOX_KEYBYTES)?;
    check_len("secretbox nonce", nonce, SECRETBOX_NONCEBYTES)?;
    let mut out = vec![0u8; data.len() + SECRETBOX_MACBYTES];
    // SAFETY: out sized data+MAC; key/nonce validated above.
    let rc = unsafe {
        ffi::crypto_secretbox_easy(
            out.as_mut_ptr(),
            data.as_ptr(),
            data.len() as u64,
            nonce.as_ptr(),
            key.as_ptr(),
        )
    };
    if rc != 0 {
        return Err(CryptoError::Sodium("crypto_secretbox_easy"));
    }
    Ok(out)
}

/// Authenticated symmetric decryption — `crypto_secretbox_open_easy`.
pub fn secretbox_open_easy(key: &[u8], nonce: &[u8], data: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    ensure_init()?;
    check_len("secretbox key", key, SECRETBOX_KEYBYTES)?;
    check_len("secretbox nonce", nonce, SECRETBOX_NONCEBYTES)?;
    if data.len() < SECRETBOX_MACBYTES {
        return Err(CryptoError::Decryption("secretbox ciphertext too short"));
    }
    let mut out = Zeroizing::new(vec![0u8; data.len() - SECRETBOX_MACBYTES]);
    // SAFETY: out sized data-MAC; key/nonce validated.
    let rc = unsafe {
        ffi::crypto_secretbox_open_easy(
            out.as_mut_ptr(),
            data.as_ptr(),
            data.len() as u64,
            nonce.as_ptr(),
            key.as_ptr(),
        )
    };
    if rc != 0 {
        return Err(CryptoError::Decryption("crypto_secretbox_open_easy"));
    }
    Ok(out)
}

/// Authenticated public-key encryption — `crypto_box_easy`.
/// Returns `nonce(24) || (MAC || ciphertext)`, matching `Crypto.encryptCryptoBox`.
pub fn box_easy(message: &[u8], recipient_pk: &[u8], sender_sk: &[u8]) -> Result<Vec<u8>> {
    ensure_init()?;
    check_len("box public key", recipient_pk, PUBLICKEYBYTES)?;
    check_len("box secret key", sender_sk, SECRETKEYBYTES)?;
    let mut nonce = vec![0u8; BOX_NONCEBYTES];
    random_into(&mut nonce)?;
    let mut cipher = vec![0u8; message.len() + BOX_MACBYTES];
    // SAFETY: buffers sized per contract; keys/nonce validated.
    let rc = unsafe {
        ffi::crypto_box_easy(
            cipher.as_mut_ptr(),
            message.as_ptr(),
            message.len() as u64,
            nonce.as_ptr(),
            recipient_pk.as_ptr(),
            sender_sk.as_ptr(),
        )
    };
    if rc != 0 {
        return Err(CryptoError::Sodium("crypto_box_easy"));
    }
    let mut out = Vec::with_capacity(nonce.len() + cipher.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&cipher);
    Ok(out)
}

/// Open an authenticated public-key box produced by [`box_easy`]. Input is
/// `nonce(24) || (MAC || ciphertext)`; `sender_pk`/`recipient_sk` are the
/// complementary keys to those used for encryption.
pub fn box_open_easy(combined: &[u8], sender_pk: &[u8], recipient_sk: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    ensure_init()?;
    check_len("box public key", sender_pk, PUBLICKEYBYTES)?;
    check_len("box secret key", recipient_sk, SECRETKEYBYTES)?;
    if combined.len() < BOX_NONCEBYTES + BOX_MACBYTES {
        return Err(CryptoError::Decryption("box ciphertext too short"));
    }
    let (nonce, cipher) = combined.split_at(BOX_NONCEBYTES);
    let mut out = Zeroizing::new(vec![0u8; cipher.len() - BOX_MACBYTES]);
    // SAFETY: out sized cipher-MAC; nonce/keys validated.
    let rc = unsafe {
        ffi::crypto_box_open_easy(
            out.as_mut_ptr(),
            cipher.as_ptr(),
            cipher.len() as u64,
            nonce.as_ptr(),
            sender_pk.as_ptr(),
            recipient_sk.as_ptr(),
        )
    };
    if rc != 0 {
        return Err(CryptoError::Decryption("crypto_box_open_easy"));
    }
    Ok(out)
}

/// Anonymous sealed box — `crypto_box_seal`. Output length = message + 48.
pub fn box_seal(message: &[u8], recipient_pk: &[u8]) -> Result<Vec<u8>> {
    ensure_init()?;
    check_len("seal public key", recipient_pk, PUBLICKEYBYTES)?;
    let mut out = vec![0u8; message.len() + SEALBYTES];
    // SAFETY: out sized message+SEALBYTES; pk validated.
    let rc = unsafe {
        ffi::crypto_box_seal(
            out.as_mut_ptr(),
            message.as_ptr(),
            message.len() as u64,
            recipient_pk.as_ptr(),
        )
    };
    if rc != 0 {
        return Err(CryptoError::Sodium("crypto_box_seal"));
    }
    Ok(out)
}

/// Open an anonymous sealed box — `crypto_box_seal_open`.
pub fn box_seal_open(enc: &[u8], pk: &[u8], sk: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    ensure_init()?;
    check_len("seal public key", pk, PUBLICKEYBYTES)?;
    check_len("seal secret key", sk, SECRETKEYBYTES)?;
    if enc.len() < SEALBYTES {
        return Err(CryptoError::Decryption("sealed box too short"));
    }
    let mut out = Zeroizing::new(vec![0u8; enc.len() - SEALBYTES]);
    // SAFETY: out sized enc-SEALBYTES; keys validated.
    let rc = unsafe {
        ffi::crypto_box_seal_open(
            out.as_mut_ptr(),
            enc.as_ptr(),
            enc.len() as u64,
            pk.as_ptr(),
            sk.as_ptr(),
        )
    };
    if rc != 0 {
        return Err(CryptoError::Decryption("crypto_box_seal_open"));
    }
    Ok(out)
}

/// Generate a random 32-byte KDF master key — `crypto_kdf_keygen`.
pub fn kdf_keygen() -> Result<Zeroizing<Vec<u8>>> {
    ensure_init()?;
    let mut key = Zeroizing::new(vec![0u8; KDF_KEYBYTES]);
    // SAFETY: key sized KDF_KEYBYTES.
    unsafe { ffi::crypto_kdf_keygen(key.as_mut_ptr()) };
    Ok(key)
}

/// Derive a subkey — `crypto_kdf_derive_from_key` (BLAKE2b). `context` must be
/// exactly `KDF_CONTEXTBYTES` (8) bytes.
pub fn kdf_derive_from_key(
    subkey_len: usize,
    subkey_id: u64,
    context: &[u8],
    master_key: &[u8],
) -> Result<Zeroizing<Vec<u8>>> {
    ensure_init()?;
    check_len("kdf context", context, KDF_CONTEXTBYTES)?;
    check_len("kdf master key", master_key, KDF_KEYBYTES)?;
    let mut subkey = Zeroizing::new(vec![0u8; subkey_len]);
    // SAFETY: subkey sized; context/master validated.
    let rc = unsafe {
        ffi::crypto_kdf_derive_from_key(
            subkey.as_mut_ptr(),
            subkey_len,
            subkey_id,
            context.as_ptr() as *const _,
            master_key.as_ptr(),
        )
    };
    if rc != 0 {
        return Err(CryptoError::Sodium("crypto_kdf_derive_from_key"));
    }
    Ok(subkey)
}

/// AEAD encryption — `crypto_aead_xchacha20poly1305_ietf_encrypt` (no AD/nsec).
/// Output length = plaintext + 16.
pub fn aead_encrypt(plaintext: &[u8], nonce: &[u8], key: &[u8]) -> Result<Vec<u8>> {
    ensure_init()?;
    check_len("aead nonce", nonce, AEAD_NPUBBYTES)?;
    check_len("aead key", key, AEAD_KEYBYTES)?;
    let mut out = vec![0u8; plaintext.len() + AEAD_ABYTES];
    let mut out_len: u64 = 0;
    // SAFETY: out sized plaintext+ABYTES; nonce/key validated; no AD or nsec.
    let rc = unsafe {
        ffi::crypto_aead_xchacha20poly1305_ietf_encrypt(
            out.as_mut_ptr(),
            &mut out_len,
            plaintext.as_ptr(),
            plaintext.len() as u64,
            std::ptr::null(),
            0,
            std::ptr::null(),
            nonce.as_ptr(),
            key.as_ptr(),
        )
    };
    if rc != 0 {
        return Err(CryptoError::Sodium("aead encrypt"));
    }
    out.truncate(out_len as usize);
    Ok(out)
}

/// AEAD decryption — `crypto_aead_xchacha20poly1305_ietf_decrypt` (no AD/nsec).
pub fn aead_decrypt(ciphertext: &[u8], nonce: &[u8], key: &[u8]) -> Result<Vec<u8>> {
    ensure_init()?;
    check_len("aead nonce", nonce, AEAD_NPUBBYTES)?;
    check_len("aead key", key, AEAD_KEYBYTES)?;
    if ciphertext.len() < AEAD_ABYTES {
        return Err(CryptoError::Decryption("aead ciphertext too short"));
    }
    let mut out = vec![0u8; ciphertext.len() - AEAD_ABYTES];
    let mut out_len: u64 = 0;
    // SAFETY: out sized ciphertext-ABYTES; nonce/key validated.
    let rc = unsafe {
        ffi::crypto_aead_xchacha20poly1305_ietf_decrypt(
            out.as_mut_ptr(),
            &mut out_len,
            std::ptr::null_mut(),
            ciphertext.as_ptr(),
            ciphertext.len() as u64,
            std::ptr::null(),
            0,
            nonce.as_ptr(),
            key.as_ptr(),
        )
    };
    if rc != 0 {
        return Err(CryptoError::Decryption("aead decrypt"));
    }
    out.truncate(out_len as usize);
    Ok(out)
}

/// SHA-256 — `crypto_hash_sha256`.
pub fn sha256(data: &[u8]) -> Result<[u8; SHA256_BYTES]> {
    ensure_init()?;
    let mut out = [0u8; SHA256_BYTES];
    // SAFETY: out sized SHA256_BYTES.
    let rc = unsafe { ffi::crypto_hash_sha256(out.as_mut_ptr(), data.as_ptr(), data.len() as u64) };
    if rc != 0 {
        return Err(CryptoError::Sodium("crypto_hash_sha256"));
    }
    Ok(out)
}

#[inline]
fn check_len(what: &'static str, buf: &[u8], expected: usize) -> Result<()> {
    if buf.len() != expected {
        Err(CryptoError::InvalidInput(format!(
            "{what} must be {expected} bytes, got {}",
            buf.len()
        )))
    } else {
        Ok(())
    }
}
