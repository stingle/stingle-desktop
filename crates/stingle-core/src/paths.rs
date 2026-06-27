//! On-disk storage layout (the `FileManager` equivalent).
//!
//! ```text
//! <accounts_dir>/<account_key>/
//!   stingle.db          SQLite store
//!   account.json        non-secret account info + password-encrypted key bundle
//!   originals/<name>    encrypted .sp originals
//!   thumbs/<name>       encrypted .sp thumbnails
//!   cache/              decrypted media cache (transient)
//!   tmp/                in-progress downloads/imports
//! ```
//!
//! `<accounts_dir>` is `<app-data>/Stingle/accounts` by default, or — when
//! the user moves their library — the folder they selected (account folders are
//! placed directly inside it, with no extra `accounts/` segment).

use std::path::{Component, Path, PathBuf};

use stingle_crypto::sodium;

use crate::error::Result;

/// True iff `name` is a safe single-path-component storage name: non-empty, no
/// path separators, no `.`/`..`, not absolute, no NUL. Server-assigned `.sp`
/// filenames are attacker-controlled (the server is untrusted in the E2E model),
/// so they MUST pass this before being used to build any cache path — otherwise
/// a crafted name like `..\..\evil` or `C:\…` would escape the account dir and
/// let the server write or delete arbitrary files.
pub fn is_safe_component(name: &str) -> bool {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains('\0') {
        return false;
    }
    let p = Path::new(name);
    if p.is_absolute() {
        return false;
    }
    // Exactly one component, and it must be a plain (Normal) name — this rejects
    // `.`, `..`, and Windows prefixes like `C:`.
    let mut it = p.components();
    matches!(
        (it.next(), it.next()),
        (Some(Component::Normal(_)), None)
    )
}

/// Stable per-account directory key = hex(sha256("server_url|email")).
pub fn account_key(server_url: &str, email: &str) -> String {
    let h = sodium::sha256(format!("{server_url}|{email}").as_bytes()).expect("sha256");
    hex::encode(h)
}

/// Default base directory for app data (e.g. `%APPDATA%/Stingle`). The brand
/// name is product-neutral ("Stingle", not "StinglePhotos") so the same desktop
/// app can host Stingle Photos and the future Stingle Drive.
pub fn default_base_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Stingle")
}

/// Pre-rename app-data directory (`…/StinglePhotos`), kept only so existing
/// installs can be migrated to [`default_base_dir`].
fn legacy_base_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("StinglePhotos")
}

/// One-time migration of the app-data directory from the old `StinglePhotos`
/// name to the new `Stingle` one. No-op unless the legacy dir exists and the new
/// one does not, so it is safe to call on every startup. (On Windows/macOS the
/// app-data and config dirs are the same folder, so a single rename here also
/// moves `config.json`; see [`crate::paths`] callers.)
pub fn migrate_legacy_base_dir() {
    let new = default_base_dir();
    let old = legacy_base_dir();
    if old.exists() && !new.exists() {
        let _ = std::fs::rename(&old, &new);
    }
}

#[derive(Clone)]
pub struct AccountPaths {
    pub root: PathBuf,
}

impl AccountPaths {
    pub fn new(accounts_dir: &Path, account_key: &str) -> Self {
        Self {
            root: accounts_dir.join(account_key),
        }
    }

    pub fn ensure(&self) -> Result<()> {
        for d in [&self.originals_dir(), &self.thumbs_dir(), &self.cache_dir(), &self.tmp_dir()] {
            std::fs::create_dir_all(d)?;
        }
        Ok(())
    }

    pub fn db_file(&self) -> PathBuf {
        self.root.join("stingle.db")
    }
    pub fn account_file(&self) -> PathBuf {
        self.root.join("account.json")
    }
    pub fn originals_dir(&self) -> PathBuf {
        self.root.join("originals")
    }
    pub fn thumbs_dir(&self) -> PathBuf {
        self.root.join("thumbs")
    }
    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }
    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// Path to an encrypted original (`filename` already includes `.sp`).
    pub fn original(&self, filename: &str) -> PathBuf {
        self.originals_dir().join(filename)
    }
    /// Path to an encrypted thumbnail.
    pub fn thumb(&self, filename: &str) -> PathBuf {
        self.thumbs_dir().join(filename)
    }
}

#[cfg(test)]
mod tests {
    use super::is_safe_component;

    #[test]
    fn accepts_plain_names_rejects_traversal() {
        assert!(is_safe_component("a1B2c3.sp"));
        assert!(is_safe_component("file with spaces.jpg"));

        assert!(!is_safe_component(""));
        assert!(!is_safe_component("."));
        assert!(!is_safe_component(".."));
        assert!(!is_safe_component("a/b"));
        assert!(!is_safe_component("a\\b"));
        assert!(!is_safe_component("../x"));
        assert!(!is_safe_component("..\\x"));
        assert!(!is_safe_component("/etc/passwd"));
        assert!(!is_safe_component("C:\\Windows\\System32"));
        assert!(!is_safe_component("a\0b"));
    }
}
