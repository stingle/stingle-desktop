//! Storage for the auto-unlock symmetric key.
//!
//! The 32-byte key encrypts the account password (kept as ciphertext in
//! `config.json`). The key itself is kept in the OS secure store:
//!
//! - **Windows:** the key bytes live in the per-user `PasswordVault`
//!   (DPAPI-protected at rest). Before retrieving, we additionally show a
//!   Windows Hello (`UserConsentVerifier`) prompt.
//! - **macOS:** the key lives in the login Keychain (encrypted at rest).
//! - **Other:** no biometric store; only the plaintext fallback applies.
//!
//! SECURITY (threat model): the Hello prompt is a UX speed-bump, NOT a
//! cryptographic gate. The `PasswordVault` entry is bound to the OS *user
//! account*, so any code already running as the same user can read it directly
//! (and thus recover the stored password) WITHOUT passing the prompt. Likewise
//! the Keychain item is readable by the unlocked login session. Enabling
//! auto-unlock therefore lowers account security to "anything running as this
//! OS user can obtain the account password." This is an explicit, user-opt-in
//! trade-off; it is not protection against a malicious local process.
//!
//! **Plaintext fallback:** when no biometric store is available and the user
//! explicitly opts in (after a danger warning), the key is written to disk in
//! the clear (no extra protection beyond the per-user profile's filesystem
//! permissions). This is the one CLAUDE.md exception — a user-initiated
//! weakening.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

use crate::config::config_dir;

fn key_to_b64(key: &[u8; 32]) -> String {
    B64.encode(key)
}

fn b64_to_key(s: &str) -> Result<[u8; 32], String> {
    let v = B64.decode(s.trim()).map_err(|e| e.to_string())?;
    if v.len() != 32 {
        return Err("stored key has wrong length".into());
    }
    let mut k = [0u8; 32];
    k.copy_from_slice(&v);
    Ok(k)
}

// ----------------------------- plaintext fallback -----------------------------

fn plaintext_path(account_key: &str) -> std::path::PathBuf {
    config_dir().join(format!("auto_unlock_{account_key}.key"))
}

pub fn has_plaintext(account_key: &str) -> bool {
    plaintext_path(account_key).exists()
}

pub fn store_plaintext(account_key: &str, key: &[u8; 32]) -> Result<(), String> {
    std::fs::create_dir_all(config_dir()).map_err(|e| e.to_string())?;
    let path = plaintext_path(account_key);
    std::fs::write(&path, key_to_b64(key)).map_err(|e| e.to_string())?;
    restrict_permissions(&path);
    Ok(())
}

fn retrieve_plaintext(account_key: &str) -> Result<[u8; 32], String> {
    let s = std::fs::read_to_string(plaintext_path(account_key)).map_err(|e| e.to_string())?;
    b64_to_key(&s)
}

fn delete_plaintext(account_key: &str) {
    let _ = std::fs::remove_file(plaintext_path(account_key));
}

#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &std::path::Path) {
    // Windows: the file sits under the per-user profile dir, which the default
    // ACL restricts to this user + Administrators/SYSTEM. We do NOT tighten the
    // ACL further here, so treat this as "readable by any process running as
    // this user" — the same trust level as the plaintext fallback overall.
}

// ----------------------------- unified API -----------------------------

/// Is a biometric / OS secure store available on this machine?
pub fn biometric_available() -> bool {
    imp::biometric_available()
}

/// Store the key in the biometric-gated secure store.
pub fn store_biometric(account_key: &str, key: &[u8; 32]) -> Result<(), String> {
    imp::store(account_key, key)
}

/// Retrieve the key — prompts for biometric/user consent if biometric-backed,
/// otherwise reads the plaintext fallback. Errors if nothing is stored.
pub fn retrieve(account_key: &str) -> Result<[u8; 32], String> {
    if has_plaintext(account_key) {
        return retrieve_plaintext(account_key);
    }
    imp::retrieve(account_key)
}

/// Remove every trace of the key (biometric store + plaintext fallback).
pub fn delete(account_key: &str) {
    delete_plaintext(account_key);
    imp::delete(account_key);
}

// ----------------------------- Windows -----------------------------

#[cfg(windows)]
mod imp {
    use super::{b64_to_key, key_to_b64};
    use windows::core::HSTRING;
    use windows::Security::Credentials::UI::{
        UserConsentVerificationResult, UserConsentVerifier, UserConsentVerifierAvailability,
    };
    use windows::Security::Credentials::{PasswordCredential, PasswordVault};

    const RESOURCE: &str = "StingleAutoUnlock";

    pub fn biometric_available() -> bool {
        match UserConsentVerifier::CheckAvailabilityAsync() {
            Ok(op) => matches!(op.get(), Ok(UserConsentVerifierAvailability::Available)),
            Err(_) => false,
        }
    }

    fn prompt(message: &str) -> Result<(), String> {
        let op = UserConsentVerifier::RequestVerificationAsync(&HSTRING::from(message))
            .map_err(|e| e.to_string())?;
        match op.get() {
            Ok(UserConsentVerificationResult::Verified) => Ok(()),
            Ok(_) => Err("Windows Hello verification was not completed".into()),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn store(account_key: &str, key: &[u8; 32]) -> Result<(), String> {
        delete(account_key); // replace any existing entry
        let vault = PasswordVault::new().map_err(|e| e.to_string())?;
        let cred = PasswordCredential::CreatePasswordCredential(
            &HSTRING::from(RESOURCE),
            &HSTRING::from(account_key),
            &HSTRING::from(key_to_b64(key)),
        )
        .map_err(|e| e.to_string())?;
        vault.Add(&cred).map_err(|e| e.to_string())
    }

    pub fn retrieve(account_key: &str) -> Result<[u8; 32], String> {
        prompt("Unlock Stingle Desktop")?;
        let vault = PasswordVault::new().map_err(|e| e.to_string())?;
        let cred = vault
            .Retrieve(&HSTRING::from(RESOURCE), &HSTRING::from(account_key))
            .map_err(|e| e.to_string())?;
        cred.RetrievePassword().map_err(|e| e.to_string())?;
        let pw = cred.Password().map_err(|e| e.to_string())?;
        b64_to_key(&pw.to_string())
    }

    pub fn delete(account_key: &str) {
        if let Ok(vault) = PasswordVault::new() {
            if let Ok(cred) =
                vault.Retrieve(&HSTRING::from(RESOURCE), &HSTRING::from(account_key))
            {
                let _ = vault.Remove(&cred);
            }
        }
    }
}

// ----------------------------- macOS -----------------------------

#[cfg(target_os = "macos")]
mod imp {
    use super::{b64_to_key, key_to_b64};
    use security_framework::passwords::{
        delete_generic_password, get_generic_password, set_generic_password,
    };

    const SERVICE: &str = "StingleAutoUnlock";

    // The login Keychain is always present; treat it as the secure store. (The
    // key is encrypted at rest and unlocked with the macOS login session.)
    pub fn biometric_available() -> bool {
        true
    }

    pub fn store(account_key: &str, key: &[u8; 32]) -> Result<(), String> {
        set_generic_password(SERVICE, account_key, key_to_b64(key).as_bytes())
            .map_err(|e| e.to_string())
    }

    pub fn retrieve(account_key: &str) -> Result<[u8; 32], String> {
        let bytes = get_generic_password(SERVICE, account_key).map_err(|e| e.to_string())?;
        let s = String::from_utf8(bytes).map_err(|e| e.to_string())?;
        b64_to_key(&s)
    }

    pub fn delete(account_key: &str) {
        let _ = delete_generic_password(SERVICE, account_key);
    }
}

// ----------------------------- other platforms -----------------------------

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    pub fn biometric_available() -> bool {
        false
    }
    pub fn store(_account_key: &str, _key: &[u8; 32]) -> Result<(), String> {
        Err("no secure store available on this platform".into())
    }
    pub fn retrieve(_account_key: &str) -> Result<[u8; 32], String> {
        Err("no secure store available on this platform".into())
    }
    pub fn delete(_account_key: &str) {}
}
