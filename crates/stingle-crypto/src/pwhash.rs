//! Password-based key derivation and the server login auth hash.

use zeroize::Zeroizing;

use crate::constants::*;
use crate::error::Result;
use crate::sodium;

/// Argon2id difficulty levels, matching the Android client's
/// `KDF_DIFFICULTY_*` constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KdfDifficulty {
    /// Local private-key encryption (`KDF_DIFFICULTY_NORMAL`) — INTERACTIVE.
    Normal,
    /// Key-bundle export (`KDF_DIFFICULTY_HARD`) — MODERATE.
    Hard,
    /// Recovery import (`KDF_DIFFICULTY_ULTRA`) — SENSITIVE.
    Ultra,
}

impl KdfDifficulty {
    /// `(opslimit, memlimit)` for this level.
    pub fn params(self) -> (u64, usize) {
        match self {
            KdfDifficulty::Normal => (OPSLIMIT_INTERACTIVE, MEMLIMIT_INTERACTIVE),
            KdfDifficulty::Hard => (OPSLIMIT_MODERATE, MEMLIMIT_MODERATE),
            KdfDifficulty::Ultra => (OPSLIMIT_SENSITIVE, MEMLIMIT_SENSITIVE),
        }
    }
}

/// Derive the 32-byte symmetric key from a password and salt at the given
/// difficulty. Equivalent to `Crypto.getKeyFromPassword`.
pub fn derive_key(
    password: &str,
    salt: &[u8],
    difficulty: KdfDifficulty,
) -> Result<Zeroizing<Vec<u8>>> {
    let (ops, mem) = difficulty.params();
    sodium::pwhash(SECRETBOX_KEYBYTES, password.as_bytes(), salt, ops, mem)
}

/// Compute the login auth hash sent to the server: a 64-byte Argon2id hash at
/// MODERATE difficulty, **upper-case** hex-encoded. Equivalent to
/// `Crypto.getPasswordHashForStorage`.
///
/// Upper-case matters: the Android/web clients use upper-case hex, and the
/// server hashes the received string verbatim, so case must match exactly.
pub fn password_hash_for_storage(password: &str, salt: &[u8]) -> Result<String> {
    let hash = sodium::pwhash(
        PWHASH_STORAGE_LEN,
        password.as_bytes(),
        salt,
        OPSLIMIT_MODERATE,
        MEMLIMIT_MODERATE,
    )?;
    Ok(hex::encode_upper(hash.as_slice()))
}

/// Generate a fresh random 16-byte password salt (`crypto_pwhash_SALTBYTES`).
pub fn generate_salt() -> Result<Vec<u8>> {
    sodium::random_bytes(PWHASH_SALTBYTES)
}
