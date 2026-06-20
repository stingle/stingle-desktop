//! # stingle-api
//!
//! Typed async client for the `api.stingle.org` v2 server, matching the Android
//! client's request/response formats exactly. Pure networking + (de)serialization;
//! it does not own session state or the local DB (that lives in `stingle-core`).
//!
//! - [`Client`] — the HTTP entry point and all typed endpoint methods.
//! - [`models`] — server object types (files, albums, contacts, delete events).
//! - [`response::StingleResponse`] — the `{status, parts, infos, errors}` envelope.

mod client;
mod de;
pub mod endpoints;
pub mod error;
pub mod models;
pub mod response;

pub use client::{Client, ServerCrypto, UploadBlob, API_VERSION, DEFAULT_SERVER_URL, STINGLE_MIME};
pub use endpoints::paths;
pub use error::{ApiError, Result};
pub use models::*;
pub use response::StingleResponse;
