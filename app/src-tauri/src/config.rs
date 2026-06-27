//! Global, pre-login app configuration.
//!
//! All *account* settings live in the per-account SQLite `kv` table, but that is
//! unreachable until the account is unlocked. These settings (last account,
//! auto-unlock, storage path, minimize-to-tray, continuous sync) must be read
//! *before* login, so they live in a single JSON file at a **fixed** location:
//! `dirs::config_dir()/Stingle/config.json`.
//!
//! It cannot live inside the storage folder, because the storage path is itself
//! one of these settings.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The `secretbox`-encrypted account password, persisted so the app can unlock
/// itself after retrieving the symmetric key from the OS secure store. Only the
/// ciphertext lives here — the key never touches this file.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct AutoUnlockBlob {
    /// Which account this blob unlocks (account-key hex).
    pub account_key: String,
    /// secretbox nonce (base64).
    pub nonce_b64: String,
    /// secretbox ciphertext of the UTF-8 password (base64).
    pub cipher_b64: String,
}

/// A folder the app watches for new media to auto-import. Lives in the global
/// config so the watcher can be configured before (and idle until) login.
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct WatchFolder {
    /// Absolute path of the folder to watch.
    pub path: String,
    /// Permanently delete each original after — and only after — its encrypted
    /// import is verified successful.
    pub delete_originals: bool,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct AppConfig {
    /// Overrides the default base dir when set.
    pub storage_path: Option<String>,
    /// account-key hex of the last account that was unlocked.
    pub last_account: Option<String>,
    /// Whether auto-unlock is armed.
    pub auto_unlock: bool,
    /// The encrypted password, present iff auto-unlock is armed.
    pub auto_unlock_blob: Option<AutoUnlockBlob>,
    /// Hide to tray instead of quitting when the window is closed.
    pub minimize_to_tray: bool,
    /// Auto-download updates and apply them on the next relaunch. `None` is the
    /// default (enabled); `Some(false)` disables auto-update, in which case the
    /// UI shows a sidebar banner to install manually. A missing/old config file
    /// therefore defaults to enabled.
    pub auto_update: Option<bool>,
    /// Continuously sync & download all originals in the background.
    pub sync_everything: bool,
    /// Folders watched for new media to auto-import (each with its own
    /// delete-after-import setting).
    pub watch_folders: Vec<WatchFolder>,
    // NOTE: start-with-PC is read from the autostart plugin (its own source of
    // truth), so it is intentionally not duplicated here.
}

/// Directory holding the global config + plaintext-fallback key. Fixed location,
/// independent of the (configurable) storage path.
pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Stingle")
}

/// Pre-rename config directory (`…/StinglePhotos`), kept only for migration.
fn legacy_config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("StinglePhotos")
}

/// One-time migration of the config directory from the old `StinglePhotos` name
/// to the new `Stingle` one. No-op unless the legacy dir exists and the new one
/// does not, so it is safe to call on every startup.
pub fn migrate_legacy_config_dir() {
    let new = config_dir();
    let old = legacy_config_dir();
    if old.exists() && !new.exists() {
        let _ = std::fs::rename(&old, &new);
    }
}

fn config_file() -> PathBuf {
    config_dir().join("config.json")
}

impl AppConfig {
    /// Load the config, returning defaults if the file is missing or unreadable.
    pub fn load() -> Self {
        match std::fs::read(config_file()) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Persist the config (best-effort dir creation).
    pub fn save(&self) -> Result<(), String> {
        let dir = config_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(config_file(), bytes).map_err(|e| e.to_string())
    }

    /// The directory that holds the per-account folders. When the user has chosen
    /// a custom location (`storage_path`), the account folders live *directly*
    /// inside it. Otherwise they sit in an `accounts/` subfolder of the app-data
    /// dir, alongside `config.json`.
    pub fn effective_accounts_dir(&self) -> PathBuf {
        match &self.storage_path {
            Some(p) if !p.trim().is_empty() => PathBuf::from(p),
            _ => stingle_core::paths::default_base_dir().join("accounts"),
        }
    }
}
