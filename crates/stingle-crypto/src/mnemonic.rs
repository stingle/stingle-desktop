//! BIP39-style mnemonic encoding of the raw private key.
//!
//! NOTE: This is NOT BIP39 *seed* derivation. As in the Android client
//! (`MnemonicUtils`), the words directly encode the entropy (the 32-byte
//! Curve25519 private key) plus an SHA-256 checksum. A 32-byte key therefore
//! produces 24 words, and decoding returns the private key bytes verbatim.

use std::sync::OnceLock;

use crate::error::{CryptoError, Result};
use crate::sodium;

static WORDS: OnceLock<Vec<&'static str>> = OnceLock::new();

/// The embedded 2048-word English wordlist (identical bytes to the Android
/// app's `res/raw/dictionary.txt`).
fn words() -> &'static [&'static str] {
    WORDS.get_or_init(|| {
        let list: Vec<&'static str> = include_str!("wordlist.txt").lines().collect();
        debug_assert_eq!(list.len(), 2048, "wordlist must contain exactly 2048 words");
        list
    })
}

fn validate_entropy(entropy: &[u8]) -> Result<()> {
    let ent = entropy.len() * 8;
    if ent < 128 || ent > 256 || ent % 32 != 0 {
        return Err(CryptoError::InvalidMnemonic(
            "entropy must be 128-256 bits in multiples of 32".into(),
        ));
    }
    Ok(())
}

/// First `ent/32` bits of SHA-256(entropy), as a single masked byte (matching
/// `MnemonicUtils.calculateChecksum`).
fn checksum_byte(entropy: &[u8]) -> Result<u8> {
    let ent = entropy.len() * 8;
    let shift = 8 - ent / 32;
    let mask: u8 = 0xffu8.wrapping_shl(shift as u32);
    let hash = sodium::sha256(entropy)?;
    Ok(hash[0] & mask)
}

/// Encode entropy (e.g. a 32-byte private key) as a mnemonic phrase.
pub fn entropy_to_mnemonic(entropy: &[u8]) -> Result<String> {
    validate_entropy(entropy)?;
    let words = words();

    let ent = entropy.len() * 8;
    let checksum_length = ent / 32;
    let checksum = checksum_byte(entropy)?;

    // Build the bit vector: entropy bits (MSB-first) then checksum bits.
    let mut bits = Vec::with_capacity(ent + checksum_length);
    for &byte in entropy {
        for j in 0..8 {
            bits.push((byte >> (7 - j)) & 1 == 1);
        }
    }
    for i in 0..checksum_length {
        bits.push((checksum >> (7 - i)) & 1 == 1);
    }

    let iterations = (ent + checksum_length) / 11;
    let mut out = String::new();
    for i in 0..iterations {
        let mut index = 0usize;
        for k in 0..11 {
            index = (index << 1) | bits[i * 11 + k] as usize;
        }
        if i > 0 {
            out.push(' ');
        }
        out.push_str(words[index]);
    }
    Ok(out)
}

/// Decode a mnemonic phrase back to its entropy bytes (the private key),
/// verifying the checksum. Equivalent to `MnemonicUtils.generateKey`.
pub fn mnemonic_to_entropy(mnemonic: &str) -> Result<Vec<u8>> {
    let vocab = words();

    // Each word contributes 11 bits, MSB-first.
    let mut bits: Vec<bool> = Vec::new();
    for word in mnemonic.split_whitespace() {
        let index = vocab
            .iter()
            .position(|w| *w == word)
            .ok_or_else(|| CryptoError::InvalidMnemonic(format!("unknown word '{word}'")))?;
        for k in 0..11 {
            bits.push((index >> (10 - k)) & 1 == 1);
        }
    }

    let size = bits.len();
    if size == 0 {
        return Err(CryptoError::InvalidMnemonic("empty mnemonic".into()));
    }
    let ent = 32 * size / 33;
    if ent % 8 != 0 {
        return Err(CryptoError::InvalidMnemonic("wrong mnemonic size".into()));
    }

    let entropy_len = ent / 8;
    let mut entropy = vec![0u8; entropy_len];
    for (i, byte) in entropy.iter_mut().enumerate() {
        *byte = read_byte(&bits, i);
    }
    validate_entropy(&entropy)?;

    let expected = checksum_byte(&entropy)?;
    let actual = read_byte(&bits, entropy_len);
    if expected != actual {
        return Err(CryptoError::InvalidMnemonic("wrong checksum".into()));
    }

    Ok(entropy)
}

/// Whether a mnemonic is structurally valid (round-trips with a correct checksum).
pub fn is_valid(mnemonic: &str) -> bool {
    mnemonic_to_entropy(mnemonic).is_ok()
}

/// Read 8 bits MSB-first starting at byte `start_byte`; bit positions beyond the
/// vector read as 0 (matching `BitSet` semantics in the Android client).
fn read_byte(bits: &[bool], start_byte: usize) -> u8 {
    let mut res = 0u8;
    for k in 0..8 {
        let idx = start_byte * 8 + k;
        if idx < bits.len() && bits[idx] {
            res |= 1 << (7 - k);
        }
    }
    res
}
