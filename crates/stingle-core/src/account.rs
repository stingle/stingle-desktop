//! Session & key lifecycle: register, login, persistence, offline unlock, logout.

use std::path::Path;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use stingle_api::{Client, ServerCrypto};
use stingle_crypto::keys::{KeyBundle, KeyPair};
use stingle_crypto::pwhash;
use stingle_db::Db;

use crate::error::{CoreError, Result};
use crate::paths::{account_key, AccountPaths};

/// Non-secret account info persisted to `account.json`. The key bundle stored
/// here is password-encrypted (Argon2id MODERATE), so it is safe at rest.
///
/// The session `token` is a bearer credential: anyone holding it can act as the
/// user against the API (list/move/delete/share — they still can't decrypt
/// content without the password, but they can destroy or exfiltrate ciphertext).
/// So at rest it is NOT written in the clear: [`AccountInfo::save`] seals it to
/// the account's own public key (`token_enc`) and blanks the plaintext field,
/// and [`AccountInfo::unseal_token`] recovers it after the keypair is unlocked.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct AccountInfo {
    pub email: String,
    pub user_id: String,
    pub home_folder: String,
    pub server_url: String,
    pub server_pk_b64: String,
    pub key_bundle_b64: String,
    /// In-memory plaintext token. On disk this is empty for accounts saved by a
    /// current build (see `token_enc`); a legacy file may still carry it here.
    #[serde(default)]
    pub token: String,
    /// `crypto_box_seal`(token) to the account public key, base64 — the at-rest
    /// form of the token. Absent in legacy files. Set by [`AccountInfo::save`];
    /// not a secret on its own (opening it needs the password-unlocked key).
    #[serde(default)]
    pub token_enc: Option<String>,
    pub is_key_backed_up: bool,
}

impl AccountInfo {
    fn save(&self, paths: &AccountPaths) -> Result<()> {
        let mut on_disk = self.clone();
        // Seal the token to our own public key (derivable from the bundle, no
        // password needed) so the stored file never contains a usable token.
        if !self.token.is_empty() {
            if let Ok(bundle) = KeyBundle::parse_base64(&self.key_bundle_b64) {
                if let Ok(sealed) =
                    stingle_crypto::sodium::box_seal(self.token.as_bytes(), &bundle.public_key)
                {
                    on_disk.token_enc = Some(B64.encode(sealed));
                    on_disk.token = String::new();
                }
            }
        }
        std::fs::write(paths.account_file(), serde_json::to_vec_pretty(&on_disk)?)?;
        Ok(())
    }
    fn load(path: &Path) -> Result<Self> {
        Ok(serde_json::from_slice(&std::fs::read(path)?)?)
    }

    /// Recover the plaintext token from `token_enc` using the unlocked keypair.
    /// No-op if the token is already present (legacy plaintext file or already
    /// unsealed). Must be called after the keypair is unlocked.
    fn unseal_token(&mut self, keypair: &KeyPair) -> Result<()> {
        if self.token.is_empty() {
            if let Some(enc) = &self.token_enc {
                let raw = B64.decode(enc.trim())?;
                let opened = stingle_crypto::sodium::box_seal_open(
                    &raw,
                    &keypair.public_key,
                    &keypair.secret_key,
                )?;
                self.token = String::from_utf8(opened.to_vec())
                    .map_err(|_| CoreError::Other("token is not valid UTF-8".into()))?;
            }
        }
        Ok(())
    }
}

/// A fully unlocked, logged-in account: clients, DB, storage, and the in-memory
/// keypair. Everything the engine needs hangs off this.
pub struct Account {
    pub client: Client,
    pub db: Db,
    pub paths: AccountPaths,
    pub info: AccountInfo,
    pub server_pk: Vec<u8>,
    pub keypair: KeyPair,
    /// Bounds total concurrent downloads (prefetch + on-demand) so we never
    /// exhaust the connection pool / trip server connection limits.
    pub(crate) download_sem: tokio::sync::Semaphore,
    /// Sub-limit held *only* by the bulk prefetch passes, so they can never
    /// occupy every `download_sem` permit. This reserves lanes for on-demand
    /// requests (thumbnails/originals the user is actually looking at), which go
    /// straight through `download_sem` and so jump ahead of the bulk backlog.
    pub(crate) bulk_sem: tokio::sync::Semaphore,
    /// Bounds concurrent *full-resolution* media decrypts behind the `stingle://`
    /// protocol — the large, sometimes-transcoding originals/previews. Sized to the
    /// machine's parallelism so several can't thrash every core at once. Thumbnails
    /// deliberately bypass this (they're the instant-preview layer and must never
    /// queue behind a heavy decrypt); the frontend observer caps their fan-out.
    pub(crate) decrypt_sem: tokio::sync::Semaphore,
    /// In-memory LRU of decrypted thumbnails so scrolling back is instant.
    pub(crate) thumb_cache: crate::thumb_cache::ThumbCache,
    /// Throttle for cache-limit enforcement (ms epoch of last check).
    pub(crate) last_cache_check_ms: std::sync::atomic::AtomicI64,
    /// Cooperative cancellation flag for the bulk "download all originals" pass.
    /// Set when the user turns "keep originals locally" off so an in-flight
    /// download stops promptly instead of running to completion.
    pub(crate) stop_originals: std::sync::atomic::AtomicBool,
    /// Cooperative cancellation flag for an in-flight Takeout (decrypt & export).
    pub(crate) stop_takeout: std::sync::atomic::AtomicBool,
    /// Cooperative cancellation flag for an in-flight manual import pass.
    pub(crate) stop_import: std::sync::atomic::AtomicBool,
}

/// Max concurrent downloads across the whole account. Thumbnails are small, so
/// a high fan-out finishes the prefetch backlog quickly; the reqwest pool is
/// sized to match (`pool_max_idle_per_host`).
pub(crate) const MAX_CONCURRENT_DOWNLOADS: usize = 56;
/// Max of those a bulk prefetch may hold at once; the rest (8 lanes) stay free
/// for the on-demand requests the user is actively waiting on. Kept high enough
/// that bulk prefetch stays fast when nothing is competing.
pub(crate) const MAX_BULK_DOWNLOADS: usize = 48;
/// Byte budget for the in-memory decrypted-thumbnail LRU (~128 MB).
const THUMB_CACHE_BYTES: usize = 128 * 1024 * 1024;

/// Permits for concurrent on-demand decrypts — the machine's parallelism, so a
/// burst of cache-misses uses every core without oversubscribing them.
fn decrypt_permits() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

impl Account {
    /// Register a new account on the server and return the logged-in session.
    /// `is_backup` controls whether the (password-encrypted) private key is
    /// stored server-side, enabling mnemonic recovery.
    pub async fn register(
        server_url: &str,
        email: &str,
        password: &str,
        accounts_dir: &Path,
        is_backup: bool,
    ) -> Result<Account> {
        let client = Client::new(Some(server_url))?;
        let keypair = KeyPair::generate()?;
        let bundle = KeyBundle::create(password, &keypair)?;
        let salt = pwhash::generate_salt()?;
        let salt_hex = hex::encode_upper(&salt);
        let login_hash = pwhash::password_hash_for_storage(password, &salt)?;
        client
            .register(email, &login_hash, &salt_hex, is_backup, &bundle.to_base64())
            .await?;
        Self::login(server_url, email, password, accounts_dir).await
    }

    /// Log in (online) and build the session, persisting account info locally.
    pub async fn login(
        server_url: &str,
        email: &str,
        password: &str,
        accounts_dir: &Path,
    ) -> Result<Account> {
        let client = Client::new(Some(server_url))?;
        let salt_hex = client.pre_login(email).await?;
        let salt = hex::decode(salt_hex.trim())?;
        let login_hash = pwhash::password_hash_for_storage(password, &salt)?;
        let login = client.login(email, &login_hash).await?;

        let keypair = KeyBundle::parse_base64(&login.key_bundle)?.unlock(password)?;
        let server_pk = B64.decode(login.server_public_key.trim())?;

        let key = account_key(server_url, email);
        let paths = AccountPaths::new(accounts_dir, &key);
        paths.ensure()?;
        let db = Db::open(paths.db_file())?;

        let info = AccountInfo {
            email: email.to_string(),
            user_id: login.user_id,
            home_folder: login.home_folder,
            server_url: server_url.to_string(),
            server_pk_b64: login.server_public_key,
            key_bundle_b64: login.key_bundle,
            token: login.token,
            token_enc: None,
            is_key_backed_up: login.is_key_backed_up,
        };
        info.save(&paths)?;

        Ok(Account {
            client,
            db,
            paths,
            info,
            server_pk,
            keypair,
            download_sem: tokio::sync::Semaphore::new(MAX_CONCURRENT_DOWNLOADS),
            bulk_sem: tokio::sync::Semaphore::new(MAX_BULK_DOWNLOADS),
            decrypt_sem: tokio::sync::Semaphore::new(decrypt_permits()),
            thumb_cache: crate::thumb_cache::ThumbCache::new(THUMB_CACHE_BYTES),
            last_cache_check_ms: std::sync::atomic::AtomicI64::new(0),
            stop_originals: std::sync::atomic::AtomicBool::new(false),
            stop_takeout: std::sync::atomic::AtomicBool::new(false),
            stop_import: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Recover an account with its mnemonic recovery phrase, setting a new
    /// password, then log in. Mirrors `CheckRecoveryPhraseAsyncTask` +
    /// `SetNewPasswordAsyncTask`.
    pub async fn recover(
        server_url: &str,
        email: &str,
        mnemonic: &str,
        new_password: &str,
        accounts_dir: &Path,
    ) -> Result<Account> {
        let client = Client::new(Some(server_url))?;
        let sk = stingle_crypto::mnemonic::mnemonic_to_entropy(mnemonic)?;
        let keypair = KeyPair::from_secret_key(&sk)?;

        // Verify the key against the server's sealed challenge.
        let ck = client.check_key(email).await?;
        let challenge = B64.decode(ck.challenge.trim())?;
        let msg = stingle_crypto::sodium::box_seal_open(
            &challenge,
            &keypair.public_key,
            &keypair.secret_key,
        )?;
        if !msg.starts_with(b"validkey_") {
            return Err(CoreError::Other("recovery phrase did not validate".into()));
        }
        let server_pk = B64.decode(ck.server_pk.trim())?;

        // Set a new password: build a fresh key bundle and a login hash.
        let bundle = KeyBundle::create(new_password, &keypair)?;
        let salt = pwhash::generate_salt()?;
        let salt_hex = hex::encode_upper(&salt);
        let login_hash = pwhash::password_hash_for_storage(new_password, &salt)?;

        let mut params = std::collections::BTreeMap::new();
        params.insert("newPassword".to_string(), login_hash);
        params.insert("newSalt".to_string(), salt_hex);
        params.insert("keyBundle".to_string(), bundle.to_base64());
        let params_json = serde_json::to_vec(&params)?;
        let params_b64 = stingle_crypto::keys::encrypt_params_for_server(
            &params_json,
            &server_pk,
            &keypair.secret_key,
        )?;
        client.recover_account(email, &params_b64).await?;

        Self::login(server_url, email, new_password, accounts_dir).await
    }

    /// Resume a previously logged-in account from disk, unlocking the stored key
    /// bundle with the password (offline — no server round-trip). Use this on
    /// app start when a valid session token already exists.
    pub fn resume(accounts_dir: &Path, account_key_hex: &str, password: &str) -> Result<Account> {
        let paths = AccountPaths::new(accounts_dir, account_key_hex);
        let mut info = AccountInfo::load(&paths.account_file())?;
        let keypair = KeyBundle::parse_base64(&info.key_bundle_b64)?.unlock(password)?;
        // Recover the at-rest-sealed session token now that the keypair is open.
        info.unseal_token(&keypair)?;
        let server_pk = B64.decode(info.server_pk_b64.trim())?;
        let client = Client::new(Some(&info.server_url))?;
        let db = Db::open(paths.db_file())?;
        Ok(Account {
            client,
            db,
            paths,
            info,
            server_pk,
            keypair,
            download_sem: tokio::sync::Semaphore::new(MAX_CONCURRENT_DOWNLOADS),
            bulk_sem: tokio::sync::Semaphore::new(MAX_BULK_DOWNLOADS),
            decrypt_sem: tokio::sync::Semaphore::new(decrypt_permits()),
            thumb_cache: crate::thumb_cache::ThumbCache::new(THUMB_CACHE_BYTES),
            last_cache_check_ms: std::sync::atomic::AtomicI64::new(0),
            stop_originals: std::sync::atomic::AtomicBool::new(false),
            stop_takeout: std::sync::atomic::AtomicBool::new(false),
            stop_import: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Rebuild a live session at a new storage base dir, reusing the already
    /// in-memory `info`/`keypair` (no password, no server round-trip). The
    /// caller must have moved the on-disk files to `new_base` first. Used by
    /// "change storage location" so the user is not logged out by the move.
    pub fn reopen_at(
        new_base: &Path,
        info: AccountInfo,
        keypair: KeyPair,
        server_pk: Vec<u8>,
    ) -> Result<Account> {
        let key = account_key(&info.server_url, &info.email);
        let paths = AccountPaths::new(new_base, &key);
        paths.ensure()?;
        let client = Client::new(Some(&info.server_url))?;
        let db = Db::open(paths.db_file())?;
        Ok(Account {
            client,
            db,
            paths,
            info,
            server_pk,
            keypair,
            download_sem: tokio::sync::Semaphore::new(MAX_CONCURRENT_DOWNLOADS),
            bulk_sem: tokio::sync::Semaphore::new(MAX_BULK_DOWNLOADS),
            decrypt_sem: tokio::sync::Semaphore::new(decrypt_permits()),
            thumb_cache: crate::thumb_cache::ThumbCache::new(THUMB_CACHE_BYTES),
            last_cache_check_ms: std::sync::atomic::AtomicI64::new(0),
            stop_originals: std::sync::atomic::AtomicBool::new(false),
            stop_takeout: std::sync::atomic::AtomicBool::new(false),
            stop_import: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// List locally-known accounts (their account-key directories) found directly
    /// under `accounts_dir`.
    pub fn list_local(accounts_dir: &Path) -> Vec<(String, AccountInfo)> {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(accounts_dir) {
            for e in entries.flatten() {
                let info_path = e.path().join("account.json");
                if let Ok(info) = AccountInfo::load(&info_path) {
                    if let Some(key) = e.file_name().to_str() {
                        out.push((key.to_string(), info));
                    }
                }
            }
        }
        out
    }

    /// The token for authenticated requests.
    pub fn token(&self) -> &str {
        &self.info.token
    }

    /// Server-param encryption context (server PK + this user's secret key).
    pub fn server_crypto(&self) -> ServerCrypto<'_> {
        ServerCrypto {
            server_pk: &self.server_pk,
            user_sk: &self.keypair.secret_key,
        }
    }

    /// The account's BIP39 recovery phrase (encodes the private key).
    pub fn recovery_phrase(&self) -> Result<String> {
        Ok(stingle_crypto::mnemonic::entropy_to_mnemonic(
            &self.keypair.secret_key,
        )?)
    }

    /// Ask any in-flight "download all originals" pass to stop as soon as it can.
    /// Called when the user turns "keep originals locally" off.
    pub fn request_stop_originals(&self) {
        self.stop_originals
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Ask an in-flight Takeout (decrypt & export) to stop as soon as it can.
    pub fn request_stop_takeout(&self) {
        self.stop_takeout
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Ask an in-flight manual import to stop as soon as it can.
    pub fn request_stop_import(&self) {
        self.stop_import
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Log out: best-effort server logout, then wipe local account data.
    pub async fn logout(&self, wipe_local: bool) -> Result<()> {
        let _ = self.client.logout(&self.info.token).await;
        if wipe_local {
            let _ = std::fs::remove_dir_all(&self.paths.root);
        }
        Ok(())
    }
}

impl From<&str> for CoreError {
    fn from(s: &str) -> Self {
        CoreError::Other(s.to_string())
    }
}
