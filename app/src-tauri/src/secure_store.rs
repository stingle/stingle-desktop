//! Storage for the auto-unlock symmetric key.
//!
//! The 32-byte key encrypts the account password (kept as ciphertext in
//! `config.json`). The key itself is kept in the strongest store the platform
//! offers, in three tiers:
//!
//! - **Biometric** — reading the key requires an OS authenticator prompt:
//!   - *Windows:* key bytes in the per-user `PasswordVault` (DPAPI-protected
//!     at rest); retrieval first shows a Windows Hello (`UserConsentVerifier`)
//!     prompt.
//!   - *macOS:* key in the **data-protection keychain** with a
//!     `SecAccessControl` requiring user presence, so the OS itself demands
//!     Touch ID (or the login password) on every read. This needs the app to
//!     be code-signed with an application-identifier / team-identifier /
//!     keychain-access-groups entitlement; unsigned dev builds fall back to
//!     the Keyring tier below (the availability probe reports which one is
//!     active).
//! - **Keyring** — encrypted at rest, but readable without a prompt while the
//!   login session is unlocked:
//!   - *macOS fallback:* a plain login-Keychain item.
//!   - *Linux:* the freedesktop Secret Service (GNOME Keyring / KWallet) over
//!     D-Bus. Reading may pop the desktop's "unlock keyring" dialog if the
//!     keyring is locked.
//! - **Plaintext fallback** — when neither tier exists and the user explicitly
//!   opts in (after a danger warning), the key is written to disk in the clear
//!   (mode 0600 on Unix). This is the one CLAUDE.md exception — a
//!   user-initiated weakening.
//!
//! SECURITY (threat model): the Windows Hello prompt is a UX speed-bump, NOT a
//! cryptographic gate — the `PasswordVault` entry is bound to the OS *user
//! account*, so any code running as the same user can read it directly. The
//! Keyring tier is likewise readable by the unlocked login session. The macOS
//! biometric tier is the strongest: the OS enforces the user-presence check at
//! read time. Enabling auto-unlock still lowers account security to roughly
//! "anything running as this OS user (plus, on macOS, anyone who can pass the
//! Touch ID / login-password prompt) can obtain the account password." This is
//! an explicit, user-opt-in trade-off.

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

/// Which store actually holds (or would hold) the auto-unlock key.
// Each platform's `imp` constructs a different subset of variants, so on any
// single target one of them looks dead to the compiler.
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StoreKind {
    /// OS-authenticator-gated: Windows Hello prompt / Touch ID-enforced
    /// keychain item.
    Biometric,
    /// OS keyring: encrypted at rest, no prompt while the session is unlocked.
    Keyring,
    /// Key file on disk; explicit opt-in only.
    Plaintext,
}

impl StoreKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StoreKind::Biometric => "biometric",
            StoreKind::Keyring => "keyring",
            StoreKind::Plaintext => "plaintext",
        }
    }
}

/// What this machine offers for the auto-unlock key.
pub struct Availability {
    /// An OS authenticator gates reads (Windows Hello / Touch ID).
    pub biometric: bool,
    /// An OS keyring is available (no read prompt, but encrypted at rest).
    pub keyring: bool,
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

/// What secure stores exist on this machine (probed, not assumed).
pub fn availability() -> Availability {
    imp::availability()
}

/// Store the key in the best available secure store (biometric if possible,
/// else keyring) and report which tier was used. Errors if neither exists —
/// the caller decides whether the plaintext fallback is permitted.
pub fn store_secure(account_key: &str, key: &[u8; 32]) -> Result<StoreKind, String> {
    imp::store(account_key, key)
}

/// Retrieve the key — prompts for biometric/user consent if biometric-backed,
/// otherwise reads the keyring or plaintext fallback. Errors if nothing is
/// stored.
pub fn retrieve(account_key: &str) -> Result<[u8; 32], String> {
    if has_plaintext(account_key) {
        return retrieve_plaintext(account_key);
    }
    imp::retrieve(account_key)
}

/// Remove every trace of the key (secure stores + plaintext fallback).
pub fn delete(account_key: &str) {
    delete_plaintext(account_key);
    imp::delete(account_key);
}

// ----------------------------- Windows -----------------------------

#[cfg(windows)]
mod imp {
    use super::{b64_to_key, key_to_b64, Availability, StoreKind};
    use windows::core::HSTRING;
    use windows::Security::Credentials::UI::{
        UserConsentVerificationResult, UserConsentVerifier, UserConsentVerifierAvailability,
    };
    use windows::Security::Credentials::{PasswordCredential, PasswordVault};

    const RESOURCE: &str = "StingleAutoUnlock";

    pub fn availability() -> Availability {
        let biometric = match UserConsentVerifier::CheckAvailabilityAsync() {
            Ok(op) => matches!(op.join(), Ok(UserConsentVerifierAvailability::Available)),
            Err(_) => false,
        };
        Availability {
            biometric,
            keyring: false,
        }
    }

    fn prompt(message: &str) -> Result<(), String> {
        let op = UserConsentVerifier::RequestVerificationAsync(&HSTRING::from(message))
            .map_err(|e| e.to_string())?;
        match op.join() {
            Ok(UserConsentVerificationResult::Verified) => Ok(()),
            Ok(_) => Err("Windows Hello verification was not completed".into()),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn store(account_key: &str, key: &[u8; 32]) -> Result<StoreKind, String> {
        delete(account_key); // replace any existing entry
        let vault = PasswordVault::new().map_err(|e| e.to_string())?;
        let cred = PasswordCredential::CreatePasswordCredential(
            &HSTRING::from(RESOURCE),
            &HSTRING::from(account_key),
            &HSTRING::from(key_to_b64(key)),
        )
        .map_err(|e| e.to_string())?;
        vault.Add(&cred).map_err(|e| e.to_string())?;
        Ok(StoreKind::Biometric)
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
    use super::{b64_to_key, key_to_b64, Availability, StoreKind};
    use security_framework::passwords::{
        delete_generic_password, delete_generic_password_options, generic_password,
        get_generic_password, set_generic_password, set_generic_password_options,
    };
    use security_framework::passwords_options::{AccessControlOptions, PasswordOptions};
    use std::sync::OnceLock;

    const SERVICE: &str = "StingleAutoUnlock";
    const PROBE_ACCOUNT: &str = "__stingle_probe__";

    // OSStatus codes we branch on.
    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;
    const ERR_SEC_MISSING_ENTITLEMENT: i32 = -34018;
    const ERR_SEC_PARAM: i32 = -50;
    const ERR_SEC_USER_CANCELED: i32 = -128;

    /// Query targeting the data-protection keychain (the only keychain that
    /// honors `SecAccessControl`, i.e. can force a Touch ID / password check
    /// on every read).
    fn dp_options(account_key: &str) -> PasswordOptions {
        let mut o = PasswordOptions::new_generic_password(SERVICE, account_key);
        o.use_protected_keychain(); // kSecUseDataProtectionKeychain
        o
    }

    fn dp_options_gated(account_key: &str) -> PasswordOptions {
        let mut o = dp_options(account_key);
        // USER_PRESENCE = Touch ID with the login password as fallback, so a
        // broken/re-enrolled sensor can't lock the user out (unlike
        // BIOMETRY_CURRENT_SET).
        o.set_access_control_options(AccessControlOptions::USER_PRESENCE);
        o
    }

    /// Can this build use the data-protection keychain? Requires the app to be
    /// signed with an application-identifier / team-identifier /
    /// keychain-access-groups entitlement; plain `cargo run` / unsigned dev
    /// builds get errSecMissingEntitlement. Adding an item never prompts (only
    /// reads do), so the probe is silent. The answer is fixed by the code
    /// signature, so cache it for the process lifetime.
    fn dp_available() -> bool {
        static AVAILABLE: OnceLock<bool> = OnceLock::new();
        *AVAILABLE.get_or_init(|| {
            match set_generic_password_options(b"probe", dp_options_gated(PROBE_ACCOUNT)) {
                Ok(()) => {
                    let _ = delete_generic_password_options(dp_options(PROBE_ACCOUNT));
                    true
                }
                Err(_) => false,
            }
        })
    }

    pub fn availability() -> Availability {
        Availability {
            biometric: dp_available(),
            // The login Keychain is always present as the promptless fallback.
            keyring: true,
        }
    }

    pub fn store(account_key: &str, key: &[u8; 32]) -> Result<StoreKind, String> {
        delete(account_key); // replace any existing entry, in either keychain
        if dp_available()
            && set_generic_password_options(
                key_to_b64(key).as_bytes(),
                dp_options_gated(account_key),
            )
            .is_ok()
        {
            return Ok(StoreKind::Biometric);
        }
        set_generic_password(SERVICE, account_key, key_to_b64(key).as_bytes())
            .map_err(|e| e.to_string())?;
        Ok(StoreKind::Keyring)
    }

    pub fn retrieve(account_key: &str) -> Result<[u8; 32], String> {
        // Reading the gated item makes the OS demand Touch ID / the login
        // password before returning the data.
        match generic_password(dp_options(account_key)) {
            Ok(bytes) => key_from_bytes(bytes),
            // No gated item (or this build can't see that keychain): fall back
            // to the legacy login-Keychain item.
            Err(e)
                if matches!(
                    e.code(),
                    ERR_SEC_ITEM_NOT_FOUND | ERR_SEC_MISSING_ENTITLEMENT | ERR_SEC_PARAM
                ) =>
            {
                let bytes =
                    get_generic_password(SERVICE, account_key).map_err(|e| e.to_string())?;
                let key = key_from_bytes(bytes)?;
                migrate_legacy(account_key, &key);
                Ok(key)
            }
            Err(e) if e.code() == ERR_SEC_USER_CANCELED => Err("Unlock was canceled".into()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Upgrade a pre-gate login-Keychain item to the Touch ID-gated store.
    /// Add-then-delete so a failed add can't lose the only copy of the key.
    /// From the next launch on, auto-unlock will prompt.
    fn migrate_legacy(account_key: &str, key: &[u8; 32]) {
        if !dp_available() {
            return;
        }
        if set_generic_password_options(key_to_b64(key).as_bytes(), dp_options_gated(account_key))
            .is_ok()
        {
            let _ = delete_generic_password(SERVICE, account_key);
        }
    }

    fn key_from_bytes(bytes: Vec<u8>) -> Result<[u8; 32], String> {
        let s = String::from_utf8(bytes).map_err(|e| e.to_string())?;
        b64_to_key(&s)
    }

    pub fn delete(account_key: &str) {
        let _ = delete_generic_password_options(dp_options(account_key));
        let _ = delete_generic_password(SERVICE, account_key);
    }
}

// ----------------------------- Linux -----------------------------

#[cfg(target_os = "linux")]
mod imp {
    use super::{b64_to_key, key_to_b64, Availability, StoreKind};
    use secret_service::{EncryptionType, SecretService};
    use std::collections::HashMap;

    const APP_ATTR: (&str, &str) = ("application", "stingle-desktop");
    const ACCOUNT_ATTR: &str = "stingle-account";

    fn attrs(account: &str) -> HashMap<&str, &str> {
        HashMap::from([APP_ATTR, (ACCOUNT_ATTR, account)])
    }

    // The secret-service crate is async (zbus). Drive each operation on a
    // dedicated thread with its own single-threaded runtime so this module
    // keeps a sync API and never blocks from inside the app's tokio runtime.
    // Ops are rare (startup, settings toggles), so a thread per op is fine.
    fn block_on<T, F>(fut: F) -> Result<T, String>
    where
        T: Send + 'static,
        F: std::future::Future<Output = Result<T, String>> + Send + 'static,
    {
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?
                .block_on(fut)
        })
        .join()
        .map_err(|_| "secret-service worker thread panicked".to_string())?
    }

    pub fn availability() -> Availability {
        // Probe for a live Secret Service with a default collection; absent on
        // headless setups or DEs without GNOME Keyring / KWallet.
        let keyring = block_on(async {
            let ss = SecretService::connect(EncryptionType::Dh)
                .await
                .map_err(|e| e.to_string())?;
            ss.get_default_collection()
                .await
                .map_err(|e| e.to_string())?;
            Ok(())
        })
        .is_ok();
        Availability {
            biometric: false,
            keyring,
        }
    }

    pub fn store(account_key: &str, key: &[u8; 32]) -> Result<StoreKind, String> {
        let account = account_key.to_string();
        let secret = key_to_b64(key);
        block_on(async move {
            let ss = SecretService::connect(EncryptionType::Dh)
                .await
                .map_err(|e| e.to_string())?;
            let coll = ss
                .get_default_collection()
                .await
                .map_err(|e| e.to_string())?;
            // No-op if already unlocked; otherwise the DE shows its own
            // "unlock keyring" dialog.
            coll.unlock().await.map_err(|e| e.to_string())?;
            coll.create_item(
                &format!("Stingle Desktop auto-unlock ({account})"),
                attrs(&account),
                secret.as_bytes(),
                true, // replace an existing item with the same attributes
                "text/plain",
            )
            .await
            .map_err(|e| e.to_string())?;
            Ok(StoreKind::Keyring)
        })
    }

    pub fn retrieve(account_key: &str) -> Result<[u8; 32], String> {
        let account = account_key.to_string();
        let bytes = block_on(async move {
            let ss = SecretService::connect(EncryptionType::Dh)
                .await
                .map_err(|e| e.to_string())?;
            let found = ss
                .search_items(attrs(&account))
                .await
                .map_err(|e| e.to_string())?;
            let item = if let Some(item) = found.unlocked.into_iter().next() {
                item
            } else {
                let item = found
                    .locked
                    .into_iter()
                    .next()
                    .ok_or_else(|| "no auto-unlock key in the system keyring".to_string())?;
                // May pop the DE's unlock dialog.
                item.unlock().await.map_err(|e| e.to_string())?;
                item
            };
            item.get_secret().await.map_err(|e| e.to_string())
        })?;
        let s = String::from_utf8(bytes).map_err(|e| e.to_string())?;
        b64_to_key(&s)
    }

    pub fn delete(account_key: &str) {
        let account = account_key.to_string();
        let _ = block_on(async move {
            let ss = SecretService::connect(EncryptionType::Dh)
                .await
                .map_err(|e| e.to_string())?;
            let found = ss
                .search_items(attrs(&account))
                .await
                .map_err(|e| e.to_string())?;
            for item in found.unlocked.into_iter().chain(found.locked) {
                let _ = item.unlock().await;
                let _ = item.delete().await;
            }
            Ok(())
        });
    }
}

// ----------------------------- other platforms -----------------------------

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod imp {
    use super::{Availability, StoreKind};

    pub fn availability() -> Availability {
        Availability {
            biometric: false,
            keyring: false,
        }
    }
    pub fn store(_account_key: &str, _key: &[u8; 32]) -> Result<StoreKind, String> {
        Err("no secure store available on this platform".into())
    }
    pub fn retrieve(_account_key: &str) -> Result<[u8; 32], String> {
        Err("no secure store available on this platform".into())
    }
    pub fn delete(_account_key: &str) {}
}
