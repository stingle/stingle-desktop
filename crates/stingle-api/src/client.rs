//! Core HTTP client: base URL handling and the three request shapes the server
//! uses (form POST, multipart upload, raw download), plus encrypted-params POST.

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::multipart::{Form, Part};
use serde_json::Value;
use stingle_crypto::keys::encrypt_params_for_server;

use crate::error::{ApiError, Result};
use crate::response::StingleResponse;

/// Default production API host.
pub const DEFAULT_SERVER_URL: &str = "https://api.stingle.org/";
/// API version segment (`StinglePhotosApplication.API_VERSION`).
pub const API_VERSION: u32 = 2;
/// MIME type used for encrypted file/thumbnail upload parts.
pub const STINGLE_MIME: &str = "application/stinglephoto";

/// Server public key + the user's secret key, used to encrypt request params
/// (`CryptoHelpers.encryptParamsForServer`).
#[derive(Clone, Copy)]
pub struct ServerCrypto<'a> {
    pub server_pk: &'a [u8],
    pub user_sk: &'a [u8],
}

/// A file/thumbnail blob to upload.
pub struct UploadBlob {
    /// Part name: `"file"` or `"thumb"`.
    pub name: &'static str,
    /// The encrypted filename used as the multipart `filename`.
    pub filename: String,
    pub bytes: Vec<u8>,
}

pub struct Client {
    http: reqwest::Client,
    base_url: String,
}

impl Client {
    /// Create a client. `server_url` defaults to [`DEFAULT_SERVER_URL`]; the
    /// `vN/` segment is appended automatically.
    pub fn new(server_url: Option<&str>) -> Result<Self> {
        let mut base = server_url.unwrap_or(DEFAULT_SERVER_URL).to_string();
        if !base.ends_with('/') {
            base.push('/');
        }
        base.push_str(&format!("v{API_VERSION}/"));

        let http = reqwest::Client::builder()
            .user_agent("Stingle Photos HTTP Client desktop")
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            // Allow many concurrent thumbnail downloads to reuse connections.
            .pool_max_idle_per_host(64)
            .build()?;
        Ok(Self {
            http,
            base_url: base,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Form-encoded POST returning the parsed (and status-validated) envelope.
    pub async fn post_form(&self, path: &str, params: &[(&str, String)]) -> Result<StingleResponse> {
        let text = self
            .http
            .post(self.url(path))
            .form(params)
            .send()
            .await?
            .text()
            .await?;
        let value: Value =
            serde_json::from_str(&text).map_err(|e| ApiError::BadResponse(e.to_string()))?;
        StingleResponse::from_value(value).into_result()
    }

    /// POST with token plus a `params` field carrying the encrypted parameter
    /// map, matching the Android encrypted-endpoint convention.
    pub async fn post_encrypted(
        &self,
        path: &str,
        token: &str,
        params: BTreeMap<String, String>,
        sc: ServerCrypto<'_>,
    ) -> Result<StingleResponse> {
        let json = serde_json::to_vec(&params)?;
        let enc = encrypt_params_for_server(&json, sc.server_pk, sc.user_sk)?;
        self.post_form(path, &[("token", token.to_string()), ("params", enc)])
            .await
    }

    /// Multipart upload (`sync/upload`).
    pub async fn post_multipart(
        &self,
        path: &str,
        fields: &[(&str, String)],
        blobs: Vec<UploadBlob>,
    ) -> Result<StingleResponse> {
        let mut form = Form::new();
        for (k, v) in fields {
            // Android sends text parts as `text/plain`.
            let part = Part::text(v.clone()).mime_str("text/plain")?;
            form = form.part((*k).to_string(), part);
        }
        for blob in blobs {
            let part = Part::bytes(blob.bytes)
                .file_name(blob.filename)
                .mime_str(STINGLE_MIME)?;
            form = form.part(blob.name.to_string(), part);
        }
        let text = self
            .http
            .post(self.url(path))
            .multipart(form)
            .send()
            .await?
            .text()
            .await?;
        let value: Value =
            serde_json::from_str(&text).map_err(|e| ApiError::BadResponse(e.to_string()))?;
        StingleResponse::from_value(value).into_result()
    }

    /// Raw binary POST download (`sync/download`). Returns the response bytes
    /// (an encrypted `.sp` blob on success).
    pub async fn post_download(&self, path: &str, params: &[(&str, String)]) -> Result<Vec<u8>> {
        let bytes = self
            .http
            .post(self.url(path))
            .form(params)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        Ok(bytes.to_vec())
    }
}
