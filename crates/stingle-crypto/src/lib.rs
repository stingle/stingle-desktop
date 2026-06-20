//! # stingle-crypto
//!
//! A byte-compatible re-implementation of the Stingle Photos cryptography,
//! built directly on libsodium (`libsodium-sys-stable`). It must reproduce the
//! Android/web clients exactly so existing accounts, key bundles, and uploaded
//! `.sp` files remain readable.
//!
//! Pure and deterministic given its random inputs: no file or network I/O.
//!
//! ## Layout
//! - [`pwhash`] — Argon2id password derivation and the login auth hash
//! - [`keys`] — identity keypair, the `SPK` key bundle, server param encryption
//! - [`file`] — the `.sp` file format and chunked XChaCha20-Poly1305 data
//! - [`album`] — album keypair, sealed album keys and metadata
//! - [`mnemonic`] — recovery-phrase encoding of the private key
//! - [`sodium`] — thin safe wrappers over the libsodium primitives

pub mod album;
pub mod constants;
pub mod error;
pub mod file;
pub mod keys;
pub mod mnemonic;
pub mod pwhash;
pub mod sodium;

pub use error::{CryptoError, Result};

/// Initialize libsodium. Optional — every entry point initializes lazily — but
/// useful to call once at startup to surface init failures early.
pub fn init() -> Result<()> {
    sodium::init()
}
