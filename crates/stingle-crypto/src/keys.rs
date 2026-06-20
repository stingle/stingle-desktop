//! Identity keypair, encrypted private-key storage, the `SPK` key bundle, and
//! server parameter encryption.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use zeroize::Zeroizing;

use crate::constants::*;
use crate::error::{CryptoError, Result};
use crate::pwhash::{derive_key, KdfDifficulty};
use crate::sodium;

/// A Curve25519 identity keypair.
pub struct KeyPair {
    pub public_key: Vec<u8>,
    pub secret_key: Zeroizing<Vec<u8>>,
}

impl KeyPair {
    /// Generate a new random keypair.
    pub fn generate() -> Result<Self> {
        let (public_key, secret_key) = sodium::box_keypair()?;
        Ok(Self { public_key, secret_key })
    }

    /// Reconstruct a keypair from a known secret key (e.g. recovered from a
    /// mnemonic), deriving the public key via `crypto_scalarmult_base`.
    pub fn from_secret_key(secret_key: &[u8]) -> Result<Self> {
        let public_key = sodium::scalarmult_base(secret_key)?;
        Ok(Self {
            public_key,
            secret_key: Zeroizing::new(secret_key.to_vec()),
        })
    }
}

/// The decoded contents of an `SPK` encrypted key bundle.
pub struct KeyBundle {
    pub public_key: Vec<u8>,            // 32
    pub encrypted_private_key: Vec<u8>, // 48 = 32 + secretbox MAC, HARD-encrypted
    pub pwd_salt: Vec<u8>,              // 16
    pub sk_nonce: Vec<u8>,              // 24
}

impl KeyBundle {
    /// Build a key bundle for the given keypair and password, generating a
    /// fresh password salt and private-key nonce. The private key is encrypted
    /// at HARD difficulty, matching `Crypto.exportKeyBundle` /
    /// `getPrivateKeyForExport`.
    pub fn create(password: &str, keypair: &KeyPair) -> Result<Self> {
        let pwd_salt = sodium::random_bytes(PWHASH_SALTBYTES)?;
        let sk_nonce = sodium::random_bytes(SECRETBOX_NONCEBYTES)?;
        Self::create_with(password, keypair, pwd_salt, sk_nonce)
    }

    /// Same as [`KeyBundle::create`] but with caller-provided salt and nonce
    /// (used to preserve an account's existing salt/nonce on password change).
    pub fn create_with(
        password: &str,
        keypair: &KeyPair,
        pwd_salt: Vec<u8>,
        sk_nonce: Vec<u8>,
    ) -> Result<Self> {
        let hard_key = derive_key(password, &pwd_salt, KdfDifficulty::Hard)?;
        let encrypted_private_key =
            sodium::secretbox_easy(&hard_key, &sk_nonce, &keypair.secret_key)?;
        Ok(Self {
            public_key: keypair.public_key.clone(),
            encrypted_private_key,
            pwd_salt,
            sk_nonce,
        })
    }

    /// Serialize to the `SPK` wire/file format.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            KEY_FILE_BEGINNING.len()
                + 2
                + self.public_key.len()
                + self.encrypted_private_key.len()
                + self.pwd_salt.len()
                + self.sk_nonce.len(),
        );
        out.extend_from_slice(KEY_FILE_BEGINNING);
        out.push(CURRENT_KEY_FILE_VERSION);
        out.push(KEY_FILE_TYPE_BUNDLE_ENCRYPTED);
        out.extend_from_slice(&self.public_key);
        out.extend_from_slice(&self.encrypted_private_key);
        out.extend_from_slice(&self.pwd_salt);
        out.extend_from_slice(&self.sk_nonce);
        out
    }

    /// Serialize and base64-encode (standard alphabet), as uploaded to the
    /// `keys/uploadKeyBundle` and `register/createAccount` endpoints.
    pub fn to_base64(&self) -> String {
        B64.encode(self.serialize())
    }

    /// Parse an `SPK` encrypted key bundle.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let enc_sk_len = SECRETKEYBYTES + SECRETBOX_MACBYTES; // 48
        let min = KEY_FILE_BEGINNING.len()
            + 2
            + PUBLICKEYBYTES
            + enc_sk_len
            + PWHASH_SALTBYTES
            + SECRETBOX_NONCEBYTES;
        if bytes.len() < min {
            return Err(CryptoError::MalformedFile("key bundle too short"));
        }
        if &bytes[0..3] != KEY_FILE_BEGINNING {
            return Err(CryptoError::MalformedFile("not an SPK key file"));
        }
        let version = bytes[3];
        if version > CURRENT_KEY_FILE_VERSION {
            return Err(CryptoError::UnsupportedVersion(format!(
                "key file version {version}"
            )));
        }
        let key_type = bytes[4];
        if key_type != KEY_FILE_TYPE_BUNDLE_ENCRYPTED {
            return Err(CryptoError::MalformedFile(
                "key file is not an encrypted bundle",
            ));
        }
        let mut p = 5;
        let public_key = bytes[p..p + PUBLICKEYBYTES].to_vec();
        p += PUBLICKEYBYTES;
        let encrypted_private_key = bytes[p..p + enc_sk_len].to_vec();
        p += enc_sk_len;
        let pwd_salt = bytes[p..p + PWHASH_SALTBYTES].to_vec();
        p += PWHASH_SALTBYTES;
        let sk_nonce = bytes[p..p + SECRETBOX_NONCEBYTES].to_vec();
        Ok(Self {
            public_key,
            encrypted_private_key,
            pwd_salt,
            sk_nonce,
        })
    }

    /// Parse a base64-encoded (standard alphabet) `SPK` bundle.
    pub fn parse_base64(s: &str) -> Result<Self> {
        let bytes = B64
            .decode(s.trim())
            .map_err(|_| CryptoError::MalformedFile("invalid base64 key bundle"))?;
        Self::parse(&bytes)
    }

    /// Decrypt the private key from this bundle using `password` (HARD
    /// difficulty + the bundle's own salt and nonce), returning the full
    /// reconstructed keypair.
    pub fn unlock(&self, password: &str) -> Result<KeyPair> {
        let hard_key = derive_key(password, &self.pwd_salt, KdfDifficulty::Hard)?;
        let secret_key = sodium::secretbox_open_easy(
            &hard_key,
            &self.sk_nonce,
            &self.encrypted_private_key,
        )?;
        // Cross-check the embedded public key against the secret key.
        let derived_pk = sodium::scalarmult_base(&secret_key)?;
        if derived_pk != self.public_key {
            return Err(CryptoError::Decryption(
                "key bundle public key does not match decrypted private key",
            ));
        }
        Ok(KeyPair {
            public_key: self.public_key.clone(),
            secret_key,
        })
    }
}

/// Encrypt a set of request parameters for the server using `crypto_box_easy`
/// (authenticated, from the user's secret key to the server's public key), then
/// standard-base64 encode. Equivalent to `CryptoHelpers.encryptParamsForServer`.
///
/// `params_json` must already be the JSON object string bytes.
pub fn encrypt_params_for_server(
    params_json: &[u8],
    server_pk: &[u8],
    user_sk: &[u8],
) -> Result<String> {
    let boxed = sodium::box_easy(params_json, server_pk, user_sk)?;
    Ok(B64.encode(boxed))
}
