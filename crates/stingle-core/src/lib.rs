//! # stingle-core
//!
//! The orchestration layer: ties `stingle-crypto`, `stingle-api`, and
//! `stingle-db` into a working client — sessions, the sync engine, import,
//! takeout, file operations, and album sharing.
//!
//! Everything hangs off an [`Account`] (a logged-in, unlocked session):
//! `account.full_sync()`, `account.import_folder(..)`, `account.get_decrypted(..)`,
//! `account.takeout(..)`, `account.trash(..)`, `account.create_album(..)`, etc.

pub mod account;
pub mod albums;
pub mod cache;
pub mod error;
pub mod fileops;
pub mod heif;
pub mod import;
pub mod media;
pub mod paths;
pub mod prefetch;
pub mod media_cache;
pub mod sharing;
pub mod sync;
pub mod takeout;
pub mod thumbnail;
mod util;

pub use account::{Account, AccountInfo};
pub use error::{CoreError, Result};
pub use media::{HeaderMeta, MediaResponse, MediaStream};
pub use sync::Space;
pub use takeout::{safe_filename, TakeoutStats};

// Re-export the set/sort types so callers don't need a direct `stingle-db` dep.
pub use stingle_db::{DbAlbum, DbContact, DbFile, FileSet, Sort};
