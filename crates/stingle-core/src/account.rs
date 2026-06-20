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
#[derive(Serialize, Deserialize, Clone)]
pub struct AccountInfo {
    pub email: String,
    pub user_id: String,
    pub home_folder: String,
    pub server_url: String,
    pub server_pk_b64: String,
    pub key_bundle_b64: String,
    pub token: String,
    pub is_key_backed_up: bool,
}

impl AccountInfo {
    fn save(&self, paths: &AccountPaths) -> Result<()> {
        std::fs::write(paths.account_file(), serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
    fn load(path: &Path) -> Result<Self> {
        Ok(serde_json::from_slice(&std::fs::read(path)?)?)
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
    /// Throttle for cache-limit enforcement (ms epoch of last check).
    pub(crate) last_cache_check_ms: std::sync::atomic::AtomicI64,
}

/// Max concurrent downloads across the whole account.
pub(crate) const MAX_CONCURRENT_DOWNLOADS: usize = 24;

impl Account {
    /// Register a new account on the server and return the logged-in session.
    /// `is_backup` controls whether the (password-encrypted) private key is
    /// stored server-side, enabling mnemonic recovery.
    pub async fn register(
        server_url: &str,
        email: &str,
        password: &str,
        base_dir: &Path,
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
        Self::login(server_url, email, password, base_dir).await
    }

    /// Log in (online) and build the session, persisting account info locally.
    pub async fn login(
        server_url: &str,
        email: &str,
        password: &str,
        base_dir: &Path,
    ) -> Result<Account> {
        let client = Client::new(Some(server_url))?;
        let salt_hex = client.pre_login(email).await?;
        let salt = hex::decode(salt_hex.trim())?;
        let login_hash = pwhash::password_hash_for_storage(password, &salt)?;
        let login = client.login(email, &login_hash).await?;

        let keypair = KeyBundle::parse_base64(&login.key_bundle)?.unlock(password)?;
        let server_pk = B64.decode(login.server_public_key.trim())?;

        let key = account_key(server_url, email);
        let paths = AccountPaths::new(base_dir, &key);
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
            last_cache_check_ms: std::sync::atomic::AtomicI64::new(0),
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
        base_dir: &Path,
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

        Self::login(server_url, email, new_password, base_dir).await
    }

    /// Resume a previously logged-in account from disk, unlocking the stored key
    /// bundle with the password (offline — no server round-trip). Use this on
    /// app start when a valid session token already exists.
    pub fn resume(base_dir: &Path, account_key_hex: &str, password: &str) -> Result<Account> {
        let paths = AccountPaths::new(base_dir, account_key_hex);
        let info = AccountInfo::load(&paths.account_file())?;
        let keypair = KeyBundle::parse_base64(&info.key_bundle_b64)?.unlock(password)?;
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
            last_cache_check_ms: std::sync::atomic::AtomicI64::new(0),
        })
    }

    /// List locally-known accounts (their account-key directories).
    pub fn list_local(base_dir: &Path) -> Vec<(String, AccountInfo)> {
        let mut out = Vec::new();
        let accounts_dir = base_dir.join("accounts");
        if let Ok(entries) = std::fs::read_dir(&accounts_dir) {
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
