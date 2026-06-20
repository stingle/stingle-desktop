//! On-disk storage layout (the `FileManager` equivalent).
//!
//! ```text
//! <base>/accounts/<account_key>/
//!   stingle.db          SQLite store
//!   account.json        non-secret account info + password-encrypted key bundle
//!   originals/<name>    encrypted .sp originals
//!   thumbs/<name>       encrypted .sp thumbnails
//!   cache/              decrypted media cache (transient)
//!   tmp/                in-progress downloads/imports
//! ```

use std::path::{Path, PathBuf};

use stingle_crypto::sodium;

use crate::error::Result;

/// Stable per-account directory key = hex(sha256("server_url|email")).
pub fn account_key(server_url: &str, email: &str) -> String {
    let h = sodium::sha256(format!("{server_url}|{email}").as_bytes()).expect("sha256");
    hex::encode(h)
}

/// Default base directory for app data (e.g. `%APPDATA%/StinglePhotos`).
pub fn default_base_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("StinglePhotos")
}

#[derive(Clone)]
pub struct AccountPaths {
    pub root: PathBuf,
}

impl AccountPaths {
    pub fn new(base_dir: &Path, account_key: &str) -> Self {
        Self {
            root: base_dir.join("accounts").join(account_key),
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
