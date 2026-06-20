//! Frozen protocol constants.
//!
//! These values define on-disk (`.sp`) and on-the-wire compatibility with the
//! Stingle Photos Android/web clients and the api.stingle.org server. They must
//! NOT be changed. Sizes are taken from libsodium itself so they always match
//! the linked library.

use libsodium_sys as ffi;

// ---- libsodium primitive sizes (sourced from the linked library) ----
pub const PUBLICKEYBYTES: usize = ffi::crypto_box_PUBLICKEYBYTES as usize; // 32
pub const SECRETKEYBYTES: usize = ffi::crypto_box_SECRETKEYBYTES as usize; // 32
pub const BOX_MACBYTES: usize = ffi::crypto_box_MACBYTES as usize; // 16
pub const BOX_NONCEBYTES: usize = ffi::crypto_box_NONCEBYTES as usize; // 24
pub const SEALBYTES: usize = ffi::crypto_box_SEALBYTES as usize; // 48

pub const SECRETBOX_KEYBYTES: usize = ffi::crypto_secretbox_KEYBYTES as usize; // 32
pub const SECRETBOX_NONCEBYTES: usize = ffi::crypto_secretbox_NONCEBYTES as usize; // 24
pub const SECRETBOX_MACBYTES: usize = ffi::crypto_secretbox_MACBYTES as usize; // 16

pub const PWHASH_SALTBYTES: usize = ffi::crypto_pwhash_SALTBYTES as usize; // 16

pub const KDF_KEYBYTES: usize = ffi::crypto_kdf_KEYBYTES as usize; // 32 (master key)
pub const KDF_CONTEXTBYTES: usize = ffi::crypto_kdf_CONTEXTBYTES as usize; // 8

pub const AEAD_KEYBYTES: usize = ffi::crypto_aead_xchacha20poly1305_ietf_KEYBYTES as usize; // 32
pub const AEAD_NPUBBYTES: usize = ffi::crypto_aead_xchacha20poly1305_ietf_NPUBBYTES as usize; // 24
pub const AEAD_ABYTES: usize = ffi::crypto_aead_xchacha20poly1305_ietf_ABYTES as usize; // 16

pub const SHA256_BYTES: usize = ffi::crypto_hash_sha256_BYTES as usize; // 32

// ---- Argon2id difficulty levels (libsodium standard values) ----
// INTERACTIVE = ops 2 / mem 64 MiB, MODERATE = ops 3 / mem 256 MiB,
// SENSITIVE = ops 4 / mem 1 GiB. Sourced from the linked library.
pub const OPSLIMIT_INTERACTIVE: u64 = ffi::crypto_pwhash_OPSLIMIT_INTERACTIVE as u64;
pub const MEMLIMIT_INTERACTIVE: usize = ffi::crypto_pwhash_MEMLIMIT_INTERACTIVE as usize;
pub const OPSLIMIT_MODERATE: u64 = ffi::crypto_pwhash_OPSLIMIT_MODERATE as u64;
pub const MEMLIMIT_MODERATE: usize = ffi::crypto_pwhash_MEMLIMIT_MODERATE as usize;
pub const OPSLIMIT_SENSITIVE: u64 = ffi::crypto_pwhash_OPSLIMIT_SENSITIVE as u64;
pub const MEMLIMIT_SENSITIVE: usize = ffi::crypto_pwhash_MEMLIMIT_SENSITIVE as usize;
pub const PWHASH_ALG_ARGON2ID13: i32 = ffi::crypto_pwhash_ALG_ARGON2ID13 as i32;

/// Length (in bytes) of the raw login auth hash, hex-encoded before sending.
pub const PWHASH_STORAGE_LEN: usize = 64;

// ---- `.sp` file format ----
pub const FILE_BEGINNING: &[u8; 2] = b"SP";
pub const CURRENT_FILE_VERSION: u8 = 1;
pub const CURRENT_HEADER_VERSION: u8 = 1;
pub const FILE_FILE_ID_LEN: usize = 32;
pub const FILE_HEADER_SIZE_LEN: usize = 4;
/// KDF context used to derive per-chunk file keys (exactly `KDF_CONTEXTBYTES`).
pub const XCHACHA20POLY1305_IETF_CONTEXT: &[u8; 8] = b"__data__";
/// Sanity bound mirroring the Android client.
pub const MAX_BUFFER_LENGTH: usize = 1024 * 1024 * 64;
/// Default plaintext chunk size (1 MiB).
pub const DEFAULT_CHUNK_SIZE: u32 = 1024 * 1024;

pub const FILE_TYPE_GENERAL: u8 = 1;
pub const FILE_TYPE_PHOTO: u8 = 2;
pub const FILE_TYPE_VIDEO: u8 = 3;

// ---- Key bundle (`SPK`) format ----
pub const KEY_FILE_BEGINNING: &[u8; 3] = b"SPK";
pub const CURRENT_KEY_FILE_VERSION: u8 = 1;
pub const KEY_FILE_TYPE_BUNDLE_ENCRYPTED: u8 = 0;
pub const KEY_FILE_TYPE_BUNDLE_PLAIN: u8 = 1;
pub const KEY_FILE_TYPE_PUBLIC_PLAIN: u8 = 2;

// ---- Album metadata ----
pub const CURRENT_ALBUM_METADATA_VERSION: u8 = 1;
