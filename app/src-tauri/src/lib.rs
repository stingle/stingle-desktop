//! Stingle Desktop — Tauri backend.
//!
//! Holds the logged-in [`Account`] (from `stingle-core`) and exposes it to the
//! React UI via commands. Decrypted thumbnails/originals are streamed to the
//! webview through the `stingle://` URI scheme so plaintext never round-trips as
//! base64 through JS.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::Serialize;
use stingle_core::{Account, DbFile, FileSet, Sort};
use tauri::{Emitter, Manager, State};
use tokio::sync::Mutex;

mod clipboard_files;
mod config;
mod secure_store;
mod tray;
mod updater;
mod watch;

use config::{AppConfig, WatchFolder};

/// Interval between background "sync everything" cycles.
const SYNC_EVERYTHING_INTERVAL_SECS: u64 = 300;

/// Interval between watch-folder scans.
const WATCH_INTERVAL_SECS: u64 = 15;

pub struct AppState {
    /// Directory holding the per-account folders; mutable because the storage
    /// location is a setting. Account dirs live directly under it.
    accounts_dir: Mutex<PathBuf>,
    /// Global, pre-login app config (persisted to a fixed config location).
    config: Mutex<AppConfig>,
    /// Lock-free read of `minimize_to_tray` for the window close handler.
    minimize_to_tray: AtomicBool,
    /// The logged-in account, shared via `Arc` so command handlers and the
    /// `stingle://` media protocol can clone a handle and release the lock
    /// immediately — allowing fully concurrent thumbnail serving.
    account: Mutex<Option<Arc<Account>>>,
    /// Handle to the background continuous-sync loop, if running.
    sync_task: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    /// Handle to the background watch-folder import loop, if running.
    watch_task: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    /// An update downloaded at startup (auto-update on) but not yet installed;
    /// applied when the app quits. A plain `std::sync::Mutex` so the quit handler
    /// (sync context) can lock it without a runtime.
    staged_update: std::sync::Mutex<Option<updater::StagedUpdate>>,
}

impl AppState {
    fn new() -> Self {
        // Migrate the old `StinglePhotos` app-data/config dirs to the new
        // product-neutral `Stingle` name before anything reads them. Both are
        // no-ops once migrated. (On Windows/macOS these resolve to the same
        // folder, so either call performs the single rename; the other then
        // sees the new dir already present and does nothing.)
        config::migrate_legacy_config_dir();
        stingle_core::paths::migrate_legacy_base_dir();
        let config = AppConfig::load();
        let accounts_dir = config.effective_accounts_dir();
        let minimize = config.minimize_to_tray;
        Self {
            accounts_dir: Mutex::new(accounts_dir),
            config: Mutex::new(config),
            minimize_to_tray: AtomicBool::new(minimize),
            account: Mutex::new(None),
            sync_task: Mutex::new(None),
            watch_task: Mutex::new(None),
            staged_update: std::sync::Mutex::new(None),
        }
    }

    /// Clone the current account handle (brief lock), or `None` if logged out.
    async fn current(&self) -> Option<Arc<Account>> {
        self.account.lock().await.clone()
    }

    /// The directory holding per-account folders (brief lock).
    async fn accounts_dir(&self) -> PathBuf {
        self.accounts_dir.lock().await.clone()
    }
}

/// Record `account_key` as the last unlocked account and persist.
async fn remember_last_account(state: &AppState, account_key: &str) {
    let mut cfg = state.config.lock().await;
    cfg.last_account = Some(account_key.to_string());
    let _ = cfg.save();
}

fn set_from_i32(s: i32) -> FileSet {
    match s {
        1 => FileSet::Trash,
        2 => FileSet::Album,
        _ => FileSet::Gallery,
    }
}

// ----------------------------- DTOs -----------------------------

#[derive(Serialize)]
struct SessionDto {
    logged_in: bool,
    email: Option<String>,
    user_id: Option<String>,
    server_url: Option<String>,
    space_used: i64,
    space_quota: i64,
    is_key_backed_up: bool,
}

#[derive(Serialize)]
struct FileDto {
    filename: String,
    album_id: Option<String>,
    date_created: i64,
    date_modified: i64,
    is_local: bool,
    is_remote: bool,
    is_video: bool,
}

/// Build a `FileDto`, computing `is_video` from the row's stored header.
fn file_dto(acc: &Account, set: FileSet, album_id: Option<&str>, f: DbFile) -> FileDto {
    let is_video = acc.row_is_video(set, album_id, &f.headers);
    FileDto {
        filename: f.filename,
        album_id: f.album_id,
        date_created: f.date_created,
        date_modified: f.date_modified,
        is_local: f.is_local,
        is_remote: f.is_remote,
        is_video,
    }
}

#[derive(Serialize)]
struct AlbumDto {
    album_id: String,
    name: String,
    is_owner: bool,
    is_shared: bool,
    cover: String,
    count: i64,
}

#[derive(Serialize)]
struct LocalAccountDto {
    account_key: String,
    email: String,
    server_url: String,
}

#[derive(Serialize)]
struct SyncResultDto {
    gallery: i64,
    trash: i64,
    albums: usize,
}

#[derive(Serialize)]
struct TakeoutDto {
    written: usize,
    errors: usize,
}

type CmdResult<T> = Result<T, String>;

fn e<E: std::fmt::Display>(err: E) -> String {
    err.to_string()
}

fn session_dto(acc: &Account) -> SessionDto {
    let sp = acc.space();
    SessionDto {
        logged_in: true,
        email: Some(acc.info.email.clone()),
        user_id: Some(acc.info.user_id.clone()),
        server_url: Some(acc.info.server_url.clone()),
        space_used: sp.used,
        space_quota: sp.quota,
        is_key_backed_up: acc.info.is_key_backed_up,
    }
}

// ----------------------------- Commands -----------------------------

#[tauri::command]
async fn list_local_accounts(state: State<'_, AppState>) -> CmdResult<Vec<LocalAccountDto>> {
    let base = state.accounts_dir().await;
    Ok(Account::list_local(&base)
        .into_iter()
        .map(|(account_key, info)| LocalAccountDto {
            account_key,
            email: info.email,
            server_url: info.server_url,
        })
        .collect())
}

/// The last unlocked account (for the returning-user login screen), if it still
/// exists locally.
#[tauri::command]
async fn last_account(state: State<'_, AppState>) -> CmdResult<Option<LocalAccountDto>> {
    let key = match state.config.lock().await.last_account.clone() {
        Some(k) => k,
        None => return Ok(None),
    };
    let base = state.accounts_dir().await;
    Ok(Account::list_local(&base)
        .into_iter()
        .find(|(k, _)| *k == key)
        .map(|(account_key, info)| LocalAccountDto {
            account_key,
            email: info.email,
            server_url: info.server_url,
        }))
}

/// Forget an account from the login screen: clear it as the last account and
/// disarm auto-unlock for it. Local data is kept (use `logout(wipe)` to delete).
#[tauri::command]
async fn forget_account(state: State<'_, AppState>, account_key: String) -> CmdResult<()> {
    secure_store::delete(&account_key);
    let mut cfg = state.config.lock().await;
    if cfg.last_account.as_deref() == Some(account_key.as_str()) {
        cfg.last_account = None;
    }
    if cfg
        .auto_unlock_blob
        .as_ref()
        .map(|b| b.account_key.as_str())
        == Some(account_key.as_str())
    {
        cfg.auto_unlock = false;
        cfg.auto_unlock_blob = None;
    }
    cfg.save()
}

#[tauri::command]
async fn register(
    state: State<'_, AppState>,
    server_url: String,
    email: String,
    password: String,
    is_backup: bool,
) -> CmdResult<SessionDto> {
    let base = state.accounts_dir().await;
    let acc = Account::register(&server_url, &email, &password, &base, is_backup)
        .await
        .map_err(e)?;
    let key = stingle_core::paths::account_key(&server_url, &email);
    let dto = session_dto(&acc);
    *state.account.lock().await = Some(Arc::new(acc));
    remember_last_account(&state, &key).await;
    Ok(dto)
}

#[tauri::command]
async fn login(
    state: State<'_, AppState>,
    server_url: String,
    email: String,
    password: String,
) -> CmdResult<SessionDto> {
    let base = state.accounts_dir().await;
    let acc = Account::login(&server_url, &email, &password, &base)
        .await
        .map_err(e)?;
    let key = stingle_core::paths::account_key(&server_url, &email);
    let dto = session_dto(&acc);
    *state.account.lock().await = Some(Arc::new(acc));
    remember_last_account(&state, &key).await;
    Ok(dto)
}

#[tauri::command]
async fn resume(
    state: State<'_, AppState>,
    account_key: String,
    password: String,
) -> CmdResult<SessionDto> {
    let base = state.accounts_dir().await;
    let acc = Account::resume(&base, &account_key, &password).map_err(e)?;
    let dto = session_dto(&acc);
    *state.account.lock().await = Some(Arc::new(acc));
    remember_last_account(&state, &account_key).await;
    Ok(dto)
}

#[tauri::command]
async fn session(state: State<'_, AppState>) -> CmdResult<SessionDto> {
    let guard = state.account.lock().await;
    Ok(match guard.as_ref() {
        Some(acc) => session_dto(acc),
        None => SessionDto {
            logged_in: false,
            email: None,
            user_id: None,
            server_url: None,
            space_used: 0,
            space_quota: 0,
            is_key_backed_up: false,
        },
    })
}

#[tauri::command]
async fn lock(state: State<'_, AppState>) -> CmdResult<()> {
    *state.account.lock().await = None;
    Ok(())
}

#[tauri::command]
async fn logout(state: State<'_, AppState>, wipe: bool) -> CmdResult<()> {
    if let Some(acc) = state.account.lock().await.take() {
        let account_key = stingle_core::paths::account_key(&acc.info.server_url, &acc.info.email);
        acc.logout(wipe).await.map_err(e)?;
        // A full wipe also forgets the account: clear its secure-store key and any
        // pre-login config (last account / auto-unlock) so it doesn't reappear on
        // the unlock screen. Same logic as `forget_account`.
        if wipe {
            secure_store::delete(&account_key);
            let mut cfg = state.config.lock().await;
            if cfg.last_account.as_deref() == Some(account_key.as_str()) {
                cfg.last_account = None;
            }
            if cfg
                .auto_unlock_blob
                .as_ref()
                .map(|b| b.account_key.as_str())
                == Some(account_key.as_str())
            {
                cfg.auto_unlock = false;
                cfg.auto_unlock_blob = None;
            }
            cfg.save()?;
        }
    }
    Ok(())
}

/// The server rejected our token (session expired). Tear down the live session
/// so the UI returns to the sign-in screen instead of looping on a dead token,
/// and tell the frontend to force a full re-login (a plain offline resume would
/// just reuse the dead token).
async fn handle_session_expired(app: &tauri::AppHandle, state: &AppState) {
    stop_sync_loop(state).await;
    if let Some(acc) = state.account.lock().await.take() {
        acc.request_stop_originals(); // halt any in-flight bulk download
    }
    let _ = app.emit("session-expired", ());
}

#[tauri::command]
async fn sync(app: tauri::AppHandle, state: State<'_, AppState>) -> CmdResult<SyncResultDto> {
    let acc = state.current().await.ok_or("Not logged in")?;
    if let Err(err) = acc.full_sync().await {
        if err.is_logged_out() {
            handle_session_expired(&app, &state).await;
        }
        return Err(e(err));
    }
    let result = SyncResultDto {
        gallery: acc.db.count_files(FileSet::Gallery).map_err(e)?,
        trash: acc.db.count_files(FileSet::Trash).map_err(e)?,
        albums: acc.db.list_albums(true).map_err(e)?.len(),
    };

    // Bulk-download every missing thumbnail in the background, highly
    // concurrent, emitting progress to the UI. When "sync everything" is on,
    // also pull all originals afterwards.
    let sync_all = state.config.lock().await.sync_everything;
    let acc2 = acc.clone();
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        let app_cb = app2.clone();
        let cb = move |done: usize, total: usize| {
            let _ = app_cb.emit("thumbs-progress", (done, total));
        };
        let n = acc2.download_all_thumbs(64, Some(&cb)).await.unwrap_or(0);
        let _ = app2.emit("thumbs-done", n);

        if sync_all {
            let app_cb2 = app2.clone();
            let cb2 = move |done: usize, total: usize| {
                let _ = app_cb2.emit("originals-progress", (done, total));
            };
            let m = acc2.download_all_originals(6, Some(&cb2)).await.unwrap_or(0);
            let _ = app2.emit("originals-done", m);
        }
    });

    Ok(result)
}

/// Manually trigger the bulk thumbnail prefetch (e.g. from a menu).
#[tauri::command]
async fn download_thumbs(app: tauri::AppHandle, state: State<'_, AppState>) -> CmdResult<usize> {
    let acc = state.current().await.ok_or("Not logged in")?;
    let app_cb = app.clone();
    let cb = move |done: usize, total: usize| {
        let _ = app_cb.emit("thumbs-progress", (done, total));
    };
    let n = acc.download_all_thumbs(64, Some(&cb)).await.map_err(e)?;
    let _ = app.emit("thumbs-done", n);
    Ok(n)
}

#[tauri::command]
async fn list_gallery(
    state: State<'_, AppState>,
    offset: i64,
    limit: i64,
) -> CmdResult<Vec<FileDto>> {
    let acc = state.current().await.ok_or("Not logged in")?;
    Ok(acc
        .db
        .list_files(FileSet::Gallery, Sort::Desc, Some(limit), offset)
        .map_err(e)?
        .into_iter()
        .map(|f| file_dto(&acc, FileSet::Gallery, None, f))
        .collect())
}

#[tauri::command]
async fn list_trash(state: State<'_, AppState>) -> CmdResult<Vec<FileDto>> {
    let acc = state.current().await.ok_or("Not logged in")?;
    Ok(acc
        .db
        .list_files(FileSet::Trash, Sort::Desc, None, 0)
        .map_err(e)?
        .into_iter()
        .map(|f| file_dto(&acc, FileSet::Trash, None, f))
        .collect())
}

#[tauri::command]
async fn list_albums(state: State<'_, AppState>) -> CmdResult<Vec<AlbumDto>> {
    let acc = state.current().await.ok_or("Not logged in")?;
    let mut out = Vec::new();
    for (a, name) in acc.list_albums_with_names(false).map_err(e)? {
        let count = acc.db.count_album_files(&a.album_id).map_err(e)?;
        // Use the album's chosen cover, else fall back to its newest photo.
        let cover = if a.cover.is_empty() {
            acc.db
                .list_album_files(&a.album_id, Sort::Desc, Some(1), 0)
                .map_err(e)?
                .first()
                .map(|f| f.filename.clone())
                .unwrap_or_default()
        } else {
            a.cover.clone()
        };
        out.push(AlbumDto {
            album_id: a.album_id,
            name,
            is_owner: a.is_owner,
            is_shared: a.is_shared,
            cover,
            count,
        });
    }
    Ok(out)
}

#[tauri::command]
async fn list_album_files(
    state: State<'_, AppState>,
    album_id: String,
) -> CmdResult<Vec<FileDto>> {
    let acc = state.current().await.ok_or("Not logged in")?;
    Ok(acc
        .db
        .list_album_files(&album_id, Sort::Desc, None, 0)
        .map_err(e)?
        .into_iter()
        .map(|f| file_dto(&acc, FileSet::Album, Some(&album_id), f))
        .collect())
}

#[tauri::command]
async fn import_paths(
    state: State<'_, AppState>,
    paths: Vec<String>,
    album_id: Option<String>,
) -> CmdResult<usize> {
    let acc = state.current().await.ok_or("Not logged in")?;
    let set = if album_id.is_some() {
        FileSet::Album
    } else {
        FileSet::Gallery
    };
    let mut imported = 0;
    for p in paths {
        let pb = PathBuf::from(&p);
        if pb.is_dir() {
            imported += acc
                .import_folder(&pb, set, album_id.as_deref())
                .await
                .map_err(e)?
                .len();
        } else if let Some(name) = acc
            .import_file(&pb, set, album_id.as_deref())
            .await
            .map_err(e)?
        {
            let _ = name;
            imported += 1;
        }
    }
    Ok(imported)
}

#[tauri::command]
async fn trash(state: State<'_, AppState>, filenames: Vec<String>) -> CmdResult<()> {
    let guard = state.account.lock().await;
    guard.as_ref().ok_or("Not logged in")?.trash(&filenames).await.map_err(e)
}

#[tauri::command]
async fn trash_ctx(
    state: State<'_, AppState>,
    set: i32,
    album_id: Option<String>,
    filenames: Vec<String>,
) -> CmdResult<()> {
    let acc = state.current().await.ok_or("Not logged in")?;
    acc.trash_from(set_from_i32(set), album_id.as_deref(), &filenames)
        .await
        .map_err(e)
}

#[tauri::command]
async fn restore(state: State<'_, AppState>, filenames: Vec<String>) -> CmdResult<()> {
    let guard = state.account.lock().await;
    guard.as_ref().ok_or("Not logged in")?.restore(&filenames).await.map_err(e)
}

#[tauri::command]
async fn delete_permanently(state: State<'_, AppState>, filenames: Vec<String>) -> CmdResult<()> {
    let guard = state.account.lock().await;
    guard
        .as_ref()
        .ok_or("Not logged in")?
        .delete_permanently(&filenames)
        .await
        .map_err(e)
}

#[tauri::command]
async fn empty_trash(state: State<'_, AppState>) -> CmdResult<()> {
    let guard = state.account.lock().await;
    guard.as_ref().ok_or("Not logged in")?.empty_trash().await.map_err(e)
}

#[tauri::command]
async fn create_album(state: State<'_, AppState>, name: String) -> CmdResult<String> {
    let guard = state.account.lock().await;
    guard.as_ref().ok_or("Not logged in")?.create_album(&name).await.map_err(e)
}

#[tauri::command]
async fn rename_album(state: State<'_, AppState>, album_id: String, name: String) -> CmdResult<()> {
    let guard = state.account.lock().await;
    guard
        .as_ref()
        .ok_or("Not logged in")?
        .rename_album(&album_id, &name)
        .await
        .map_err(e)
}

#[tauri::command]
async fn delete_album(state: State<'_, AppState>, album_id: String) -> CmdResult<()> {
    let guard = state.account.lock().await;
    guard.as_ref().ok_or("Not logged in")?.delete_album(&album_id).await.map_err(e)
}

#[tauri::command]
async fn set_album_cover(
    state: State<'_, AppState>,
    album_id: String,
    filename: String,
) -> CmdResult<()> {
    let guard = state.account.lock().await;
    guard
        .as_ref()
        .ok_or("Not logged in")?
        .set_album_cover(&album_id, &filename)
        .await
        .map_err(e)
}

#[tauri::command]
async fn set_album_blank_cover(state: State<'_, AppState>, album_id: String) -> CmdResult<()> {
    let guard = state.account.lock().await;
    guard
        .as_ref()
        .ok_or("Not logged in")?
        .set_album_blank_cover(&album_id)
        .await
        .map_err(e)
}

#[tauri::command]
async fn takeout(
    state: State<'_, AppState>,
    out_dir: String,
    include_trash: bool,
) -> CmdResult<TakeoutDto> {
    let acc = state.current().await.ok_or("Not logged in")?;
    let stats = acc
        .takeout(&PathBuf::from(out_dir), include_trash)
        .await
        .map_err(e)?;
    Ok(TakeoutDto {
        written: stats.written,
        errors: stats.errors,
    })
}

#[tauri::command]
async fn is_video(
    state: State<'_, AppState>,
    set: i32,
    album_id: Option<String>,
    filename: String,
) -> CmdResult<bool> {
    let guard = state.account.lock().await;
    guard
        .as_ref()
        .ok_or("Not logged in")?
        .is_video(set_from_i32(set), album_id.as_deref(), &filename)
        .map_err(e)
}

#[tauri::command]
async fn recovery_phrase(state: State<'_, AppState>) -> CmdResult<String> {
    let guard = state.account.lock().await;
    guard.as_ref().ok_or("Not logged in")?.recovery_phrase().map_err(e)
}

#[tauri::command]
async fn recover(
    state: State<'_, AppState>,
    server_url: String,
    email: String,
    mnemonic: String,
    new_password: String,
) -> CmdResult<SessionDto> {
    let base = state.accounts_dir().await;
    let acc = Account::recover(&server_url, &email, &mnemonic, &new_password, &base)
        .await
        .map_err(e)?;
    let key = stingle_core::paths::account_key(&server_url, &email);
    let dto = session_dto(&acc);
    *state.account.lock().await = Some(Arc::new(acc));
    remember_last_account(&state, &key).await;
    Ok(dto)
}

#[tauri::command]
async fn share_album(
    state: State<'_, AppState>,
    album_id: String,
    emails: Vec<String>,
    allow_add: bool,
    allow_share: bool,
    allow_copy: bool,
) -> CmdResult<()> {
    let guard = state.account.lock().await;
    guard
        .as_ref()
        .ok_or("Not logged in")?
        .share_album(&album_id, &emails, allow_add, allow_share, allow_copy)
        .await
        .map_err(e)
}

#[tauri::command]
async fn unshare_album(state: State<'_, AppState>, album_id: String) -> CmdResult<()> {
    let guard = state.account.lock().await;
    guard.as_ref().ok_or("Not logged in")?.unshare_album(&album_id).await.map_err(e)
}

#[tauri::command]
async fn leave_album(state: State<'_, AppState>, album_id: String) -> CmdResult<()> {
    let guard = state.account.lock().await;
    guard.as_ref().ok_or("Not logged in")?.leave_album(&album_id).await.map_err(e)
}

#[tauri::command]
async fn get_cache_limit(state: State<'_, AppState>) -> CmdResult<i64> {
    Ok(state.current().await.ok_or("Not logged in")?.cache_limit_bytes())
}

#[tauri::command]
async fn set_cache_limit(state: State<'_, AppState>, bytes: i64) -> CmdResult<()> {
    state.current().await.ok_or("Not logged in")?.set_cache_limit_bytes(bytes).map_err(e)
}

#[tauri::command]
async fn cache_size(state: State<'_, AppState>) -> CmdResult<i64> {
    Ok(state.current().await.ok_or("Not logged in")?.cache_size_bytes() as i64)
}

#[tauri::command]
async fn clear_cache(state: State<'_, AppState>) -> CmdResult<()> {
    state.current().await.ok_or("Not logged in")?.clear_cache().map_err(e)
}

#[tauri::command]
async fn save_files(
    state: State<'_, AppState>,
    set: i32,
    album_id: Option<String>,
    filenames: Vec<String>,
    dest_dir: String,
) -> CmdResult<usize> {
    let acc = state.current().await.ok_or("Not logged in")?;
    acc.save_files(set_from_i32(set), album_id.as_deref(), &filenames, &PathBuf::from(dest_dir))
        .await
        .map_err(e)
}

#[tauri::command]
async fn move_to_album(
    state: State<'_, AppState>,
    set: i32,
    album_id: Option<String>,
    filenames: Vec<String>,
    to_album: String,
    is_moving: bool,
) -> CmdResult<()> {
    let acc = state.current().await.ok_or("Not logged in")?;
    acc.move_to_album(set_from_i32(set), album_id.as_deref(), &filenames, &to_album, is_moving)
        .await
        .map_err(e)
}

#[tauri::command]
async fn move_to_gallery(
    state: State<'_, AppState>,
    album_id: String,
    filenames: Vec<String>,
    is_moving: bool,
) -> CmdResult<()> {
    let acc = state.current().await.ok_or("Not logged in")?;
    acc.move_to_gallery(&album_id, &filenames, is_moving).await.map_err(e)
}

// ----------------------------- clipboard -----------------------------

/// Copy a photo to the OS clipboard as an image (decrypted in memory only —
/// never written to disk). Videos can't be put on the clipboard as an image;
/// the caller should drag them out instead.
#[tauri::command]
async fn copy_to_clipboard(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    set: i32,
    album_id: Option<String>,
    filename: String,
) -> CmdResult<()> {
    use tauri_plugin_clipboard_manager::ClipboardExt;
    let acc = state.current().await.ok_or("Not logged in")?;
    let (w, h, rgba) = acc
        .decrypt_to_rgba(set_from_i32(set), album_id.as_deref(), &filename)
        .await
        .map_err(e)?;
    let img = tauri::image::Image::new_owned(rgba, w, h);
    app.clipboard().write_image(&img).map_err(e)
}

/// Copy library items to the clipboard as real files (`CF_HDROP`), the way
/// Explorer does — so a multi-select copy pastes all of them into Telegram/etc.
/// The files are decrypted to a temp folder that persists until the next copy
/// (the paste consumer reads them later); it's cleared on the next copy and at
/// startup. Returns the number of files placed on the clipboard.
#[tauri::command]
async fn copy_files_to_clipboard(
    state: State<'_, AppState>,
    set: i32,
    album_id: Option<String>,
    filenames: Vec<String>,
) -> CmdResult<usize> {
    let acc = state.current().await.ok_or("Not logged in")?;
    let s = set_from_i32(set);
    let dir = clipboard_temp_dir();
    // Drop the previous copy's decrypted files before writing new ones.
    let _ = std::fs::remove_dir_all(&dir);
    create_private_dir(&dir).map_err(e)?;

    let mut paths = Vec::new();
    for name in &filenames {
        let plain = acc.get_decrypted(s, album_id.as_deref(), name, false).await.map_err(e)?;
        let orig = acc
            .original_name(s, album_id.as_deref(), name)
            .unwrap_or_else(|_| name.clone());
        // `orig` is header-derived (attacker-controllable) — reduce it to a safe
        // bare filename so it can't escape the temp dir.
        let out = unique_temp(&dir, &stingle_core::safe_filename(&orig));
        std::fs::write(&out, &plain).map_err(e)?;
        paths.push(out);
    }
    clipboard_files::set_files(&paths)?;
    Ok(paths.len())
}

/// File paths currently on the clipboard (`CF_HDROP`), for paste-into-app.
#[tauri::command]
fn clipboard_files() -> Vec<String> {
    clipboard_files::get_files()
}

/// Paste an image from the OS clipboard into the library (encrypted on import).
/// Returns the number imported (0 if the clipboard had no image / a duplicate).
#[tauri::command]
async fn paste_from_clipboard(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    album_id: Option<String>,
) -> CmdResult<usize> {
    use tauri_plugin_clipboard_manager::ClipboardExt;
    let acc = state.current().await.ok_or("Not logged in")?;
    let img = app.clipboard().read_image().map_err(e)?;
    let rgba = img.rgba().to_vec();
    let (w, h) = (img.width(), img.height());
    let set = if album_id.is_some() {
        FileSet::Album
    } else {
        FileSet::Gallery
    };
    let n = acc
        .import_rgba(&rgba, w, h, set, album_id.as_deref())
        .await
        .map_err(e)?;
    Ok(if n.is_some() { 1 } else { 0 })
}

// ----------------------------- drag-out export -----------------------------
//
// Dragging items OUT to other apps needs real file paths, so we decrypt to a
// temp folder for the duration of the drag (the documented plaintext exception,
// like Takeout). `cleanup_drag_export` deletes them once the drop completes, and
// the temp folder is also wiped on startup in case a drag was interrupted.

#[derive(Serialize)]
struct DragExportDto {
    files: Vec<String>,
    icon: String,
}

fn drag_temp_dir() -> PathBuf {
    std::env::temp_dir().join("stingle-drag")
}

/// Temp dir holding decrypted files placed on the clipboard (see
/// `copy_files_to_clipboard`). Persists until the next copy / startup.
fn clipboard_temp_dir() -> PathBuf {
    std::env::temp_dir().join("stingle-clip")
}

/// Create `dir` (and parents) restricted to the current user (0700 on Unix).
/// Used for the short-lived folders that hold DECRYPTED drag/clipboard files so
/// other users on the machine can't read them while they exist.
fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// A non-colliding path in `dir` for `name`.
fn unique_temp(dir: &std::path::Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    if !p.exists() {
        return p;
    }
    let stem = std::path::Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let ext = std::path::Path::new(name).extension().and_then(|s| s.to_str());
    for i in 1.. {
        let cand = match ext {
            Some(x) => format!("{stem} ({i}).{x}"),
            None => format!("{stem} ({i})"),
        };
        let p = dir.join(cand);
        if !p.exists() {
            return p;
        }
    }
    unreachable!()
}

#[tauri::command]
async fn export_for_drag(
    state: State<'_, AppState>,
    set: i32,
    album_id: Option<String>,
    filenames: Vec<String>,
) -> CmdResult<DragExportDto> {
    let acc = state.current().await.ok_or("Not logged in")?;
    let s = set_from_i32(set);
    let dir = drag_temp_dir();
    create_private_dir(&dir).map_err(e)?;

    let mut files = Vec::new();
    for name in &filenames {
        let plain = acc.get_decrypted(s, album_id.as_deref(), name, false).await.map_err(e)?;
        let orig = acc
            .original_name(s, album_id.as_deref(), name)
            .unwrap_or_else(|_| name.clone());
        let out = unique_temp(&dir, &stingle_core::safe_filename(&orig));
        std::fs::write(&out, &plain).map_err(e)?;
        files.push(out.to_string_lossy().to_string());
    }

    // Drag preview icon: the first item's (jpeg) thumbnail.
    let icon = match acc.get_decrypted(s, album_id.as_deref(), &filenames[0], true).await {
        Ok(thumb) => {
            let p = unique_temp(&dir, "drag-icon.jpg");
            let _ = std::fs::write(&p, &thumb);
            p.to_string_lossy().to_string()
        }
        Err(_) => String::new(),
    };

    Ok(DragExportDto { files, icon })
}

#[tauri::command]
fn cleanup_drag_export(paths: Vec<String>) -> CmdResult<()> {
    for p in paths {
        let _ = std::fs::remove_file(p);
    }
    Ok(())
}

// ----------------------------- app settings -----------------------------

#[tauri::command]
async fn get_minimize_to_tray(state: State<'_, AppState>) -> CmdResult<bool> {
    Ok(state.config.lock().await.minimize_to_tray)
}

#[tauri::command]
async fn set_minimize_to_tray(state: State<'_, AppState>, enabled: bool) -> CmdResult<()> {
    state.minimize_to_tray.store(enabled, Ordering::Relaxed);
    let mut cfg = state.config.lock().await;
    cfg.minimize_to_tray = enabled;
    cfg.save()
}

#[tauri::command]
async fn get_auto_update(state: State<'_, AppState>) -> CmdResult<bool> {
    Ok(state.config.lock().await.auto_update.unwrap_or(true))
}

#[tauri::command]
async fn set_auto_update(state: State<'_, AppState>, enabled: bool) -> CmdResult<()> {
    let mut cfg = state.config.lock().await;
    cfg.auto_update = Some(enabled);
    cfg.save()
}

#[tauri::command]
fn get_app_version(app: tauri::AppHandle) -> String {
    app.package_info().version.to_string()
}

/// Manual "check for updates now": returns the new version string if one is
/// available, else `None`.
#[tauri::command]
async fn check_for_update(app: tauri::AppHandle) -> CmdResult<Option<String>> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = app.updater().map_err(e)?;
    Ok(updater.check().await.map_err(e)?.map(|u| u.version))
}

/// User-driven install (sidebar banner / manual "update now"): download, install,
/// and restart immediately.
#[tauri::command]
async fn install_update_now(app: tauri::AppHandle) -> CmdResult<()> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = app.updater().map_err(e)?;
    let update = updater
        .check()
        .await
        .map_err(e)?
        .ok_or_else(|| "No update available".to_string())?;
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(e)?;
    app.restart();
    #[allow(unreachable_code)]
    Ok(())
}

#[tauri::command]
fn get_autostart(app: tauri::AppHandle) -> CmdResult<bool> {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().map_err(e)
}

#[tauri::command]
fn set_autostart(app: tauri::AppHandle, enabled: bool) -> CmdResult<()> {
    use tauri_plugin_autostart::ManagerExt;
    let m = app.autolaunch();
    if enabled {
        m.enable().map_err(e)
    } else {
        m.disable().map_err(e)
    }
}

#[tauri::command]
async fn get_storage_path(state: State<'_, AppState>) -> CmdResult<String> {
    Ok(state.accounts_dir().await.to_string_lossy().to_string())
}

// ----------------------------- auto-unlock -----------------------------

#[derive(Serialize)]
struct SecureStoreStatusDto {
    biometric: bool,
}

#[tauri::command]
fn secure_store_status() -> SecureStoreStatusDto {
    SecureStoreStatusDto {
        biometric: secure_store::biometric_available(),
    }
}

#[tauri::command]
async fn is_auto_unlock_enabled(state: State<'_, AppState>) -> CmdResult<bool> {
    Ok(state.config.lock().await.auto_unlock)
}

#[derive(Serialize)]
struct EnableAutoUnlockDto {
    used_plaintext: bool,
}

#[tauri::command]
async fn enable_auto_unlock(
    state: State<'_, AppState>,
    password: String,
    allow_plaintext: bool,
) -> CmdResult<EnableAutoUnlockDto> {
    let acc = state.current().await.ok_or("Not logged in")?;
    let account_key = stingle_core::paths::account_key(&acc.info.server_url, &acc.info.email);

    // Verify the password actually unlocks this account (offline, via resume).
    let base = state.accounts_dir().await;
    Account::resume(&base, &account_key, &password)
        .map_err(|_| "Incorrect password".to_string())?;

    // Random 32-byte key; secretbox-encrypt the password with it.
    let key_vec = stingle_crypto::sodium::random_bytes(32).map_err(e)?;
    let mut key = [0u8; 32];
    key.copy_from_slice(&key_vec);
    let nonce = stingle_crypto::sodium::random_bytes(24).map_err(e)?;
    let cipher =
        stingle_crypto::sodium::secretbox_easy(&key, &nonce, password.as_bytes()).map_err(e)?;

    // Store the key: biometric store if available, else plaintext on opt-in only.
    let used_plaintext = if secure_store::biometric_available() {
        secure_store::store_biometric(&account_key, &key)?;
        false
    } else if allow_plaintext {
        secure_store::store_plaintext(&account_key, &key)?;
        true
    } else {
        return Err("No secure store available; plaintext fallback was not permitted".into());
    };

    let mut cfg = state.config.lock().await;
    cfg.auto_unlock = true;
    cfg.auto_unlock_blob = Some(config::AutoUnlockBlob {
        account_key: account_key.clone(),
        nonce_b64: B64.encode(&nonce),
        cipher_b64: B64.encode(&cipher),
    });
    cfg.last_account = Some(account_key);
    cfg.save()?;
    Ok(EnableAutoUnlockDto { used_plaintext })
}

#[tauri::command]
async fn disable_auto_unlock(state: State<'_, AppState>) -> CmdResult<()> {
    let mut cfg = state.config.lock().await;
    if let Some(blob) = cfg.auto_unlock_blob.take() {
        secure_store::delete(&blob.account_key);
    }
    cfg.auto_unlock = false;
    cfg.save()
}

/// Attempt to unlock the saved account using the stored key (prompts biometric).
#[tauri::command]
async fn try_auto_unlock(state: State<'_, AppState>) -> CmdResult<SessionDto> {
    // Already unlocked (e.g. a duplicate call raced in) — don't prompt again.
    if let Some(acc) = state.current().await {
        return Ok(session_dto(&acc));
    }
    let blob = {
        let cfg = state.config.lock().await;
        if !cfg.auto_unlock {
            return Err("Auto-unlock is not enabled".into());
        }
        cfg.auto_unlock_blob.clone().ok_or("No saved credentials")?
    };

    let key = secure_store::retrieve(&blob.account_key)?; // may prompt biometric
    let nonce = B64.decode(&blob.nonce_b64).map_err(e)?;
    let cipher = B64.decode(&blob.cipher_b64).map_err(e)?;
    let pw = stingle_crypto::sodium::secretbox_open_easy(&key, &nonce, &cipher).map_err(e)?;
    let password = String::from_utf8(pw.to_vec()).map_err(e)?;

    let base = state.accounts_dir().await;
    let acc = Account::resume(&base, &blob.account_key, &password).map_err(e)?;
    let dto = session_dto(&acc);
    *state.account.lock().await = Some(Arc::new(acc));
    remember_last_account(&state, &blob.account_key).await;
    Ok(dto)
}

// ----------------------------- continuous sync -----------------------------

/// Start the background "sync everything" loop (no-op if already running).
async fn start_sync_loop(app: tauri::AppHandle, state: &AppState) {
    let mut guard = state.sync_task.lock().await;
    if guard.is_some() {
        return;
    }
    let app2 = app.clone();
    let handle = tauri::async_runtime::spawn(async move {
        loop {
            if let Some(acc) = app2.state::<AppState>().current().await {
                if let Err(err) = acc.full_sync().await {
                    if err.is_logged_out() {
                        // Token died: tear down the session and stop the loop so we
                        // don't keep hammering the server with a dead token. Done
                        // inline (not via handle_session_expired) so we never abort
                        // our own task before emitting the event — clearing the
                        // handle just detaches it; `return` ends the loop cleanly.
                        let state = app2.state::<AppState>();
                        if let Some(a) = state.account.lock().await.take() {
                            a.request_stop_originals();
                        }
                        *state.sync_task.lock().await = None;
                        let _ = app2.emit("session-expired", ());
                        return;
                    }
                }
                let app_cb = app2.clone();
                let cb = move |done: usize, total: usize| {
                    let _ = app_cb.emit("originals-progress", (done, total));
                };
                let n = acc.download_all_originals(6, Some(&cb)).await.unwrap_or(0);
                let _ = app2.emit("originals-done", n);
            }
            tokio::time::sleep(std::time::Duration::from_secs(SYNC_EVERYTHING_INTERVAL_SECS)).await;
        }
    });
    *guard = Some(handle);
}

/// Stop the background sync loop if running.
async fn stop_sync_loop(state: &AppState) {
    if let Some(h) = state.sync_task.lock().await.take() {
        h.abort();
    }
}

#[tauri::command]
async fn get_sync_everything(state: State<'_, AppState>) -> CmdResult<bool> {
    Ok(state.config.lock().await.sync_everything)
}

#[tauri::command]
async fn set_sync_everything(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    enabled: bool,
) -> CmdResult<()> {
    {
        let mut cfg = state.config.lock().await;
        cfg.sync_everything = enabled;
        cfg.save()?;
    }
    if enabled {
        start_sync_loop(app, &state).await;
    } else {
        stop_sync_loop(&state).await;
        // Aborting the loop task doesn't touch an originals download that was
        // spawned detached (e.g. from the initial sync), so signal it to stop too.
        if let Some(acc) = state.current().await {
            acc.request_stop_originals();
        }
    }
    Ok(())
}

// ----------------------------- watch folders -----------------------------

/// Start the background watch-folder import loop (no-op if already running).
/// The loop idles while logged out or while no folders are configured.
async fn start_watch_loop(app: tauri::AppHandle, state: &AppState) {
    let mut guard = state.watch_task.lock().await;
    if guard.is_some() {
        return;
    }
    let app2 = app.clone();
    let handle = tauri::async_runtime::spawn(async move {
        // Per-file `(size, mtime)` fingerprints, carried across passes so a file
        // must be stable for two consecutive scans before it is imported.
        let mut stable = std::collections::HashMap::new();
        loop {
            let state = app2.state::<AppState>();
            let folders = state.config.lock().await.watch_folders.clone();
            if !folders.is_empty() {
                if let Some(acc) = state.current().await {
                    watch::scan_folders(&app2, &acc, &folders, &mut stable).await;
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(WATCH_INTERVAL_SECS)).await;
        }
    });
    *guard = Some(handle);
}

/// Stop the background watch-folder loop if running.
async fn stop_watch_loop(state: &AppState) {
    if let Some(h) = state.watch_task.lock().await.take() {
        h.abort();
    }
}

#[tauri::command]
async fn get_watch_folders(state: State<'_, AppState>) -> CmdResult<Vec<WatchFolder>> {
    Ok(state.config.lock().await.watch_folders.clone())
}

#[tauri::command]
async fn set_watch_folders(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    folders: Vec<WatchFolder>,
) -> CmdResult<()> {
    {
        let mut cfg = state.config.lock().await;
        cfg.watch_folders = folders;
        cfg.save()?;
    }
    // Restart so a freshly-stable map re-scans (and picks up newly-added folders
    // immediately).
    stop_watch_loop(&state).await;
    if !state.config.lock().await.watch_folders.is_empty() {
        start_watch_loop(app, &state).await;
    }
    Ok(())
}

// ----------------------------- storage path move -----------------------------

/// Max directory depth for the storage-move walkers — a stack-overflow guard.
/// Symlinks are skipped (not followed), so a loop can't recurse regardless.
const MAX_MOVE_DEPTH: u32 = 64;

/// Total size in bytes of all files under `dir` (0 if it doesn't exist).
fn dir_size(dir: &std::path::Path, depth: u32) -> u64 {
    if depth >= MAX_MOVE_DEPTH {
        return 0;
    }
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                // Skip symlinks so a loop can't spin and an external target isn't
                // counted; only descend into real directories.
                Ok(ft) if ft.is_symlink() => {}
                Ok(ft) if ft.is_dir() => total += dir_size(&path, depth + 1),
                Ok(_) => total += std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
                Err(_) => {}
            }
        }
    }
    total
}

/// Recursively copy `src` into `dst`, emitting `storage-move-progress` as bytes
/// are copied. Symlinks are skipped (never followed).
fn copy_recursive(
    app: &tauri::AppHandle,
    src: &std::path::Path,
    dst: &std::path::Path,
    done: &mut u64,
    total: u64,
    depth: u32,
) -> Result<(), String> {
    if depth >= MAX_MOVE_DEPTH {
        return Ok(());
    }
    std::fs::create_dir_all(dst).map_err(|x| x.to_string())?;
    for entry in std::fs::read_dir(src).map_err(|x| x.to_string())? {
        let entry = entry.map_err(|x| x.to_string())?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ft = entry.file_type().map_err(|x| x.to_string())?;
        if ft.is_symlink() {
            continue;
        } else if ft.is_dir() {
            copy_recursive(app, &from, &to, done, total, depth + 1)?;
        } else {
            let n = std::fs::copy(&from, &to).map_err(|x| x.to_string())?;
            *done += n;
            let _ = app.emit("storage-move-progress", (*done, total));
        }
    }
    Ok(())
}

/// Move all data from `old` to `new` (copy-then-delete, cross-volume safe).
fn move_dir_with_progress(
    app: &tauri::AppHandle,
    old: &std::path::Path,
    new: &std::path::Path,
) -> Result<(), String> {
    std::fs::create_dir_all(new).map_err(|x| x.to_string())?;
    if !old.exists() {
        return Ok(());
    }
    let total = dir_size(old, 0);
    let _ = app.emit("storage-move-progress", (0u64, total));
    let mut done = 0u64;
    copy_recursive(app, old, new, &mut done, total, 0)?;
    // Only remove the source after a successful full copy.
    let _ = std::fs::remove_dir_all(old);
    let _ = app.emit("storage-move-progress", (total, total));
    Ok(())
}

#[tauri::command]
async fn change_storage_path(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    new_path: String,
) -> CmdResult<()> {
    // The selected folder becomes the accounts dir itself: account folders are
    // placed directly inside it (no extra `accounts/` segment).
    let new_accounts_dir = PathBuf::from(new_path.trim());
    if new_accounts_dir.as_os_str().is_empty() {
        return Err("Empty path".into());
    }
    let old_accounts_dir = state.accounts_dir().await;
    if new_accounts_dir == old_accounts_dir {
        return Ok(());
    }

    // Snapshot the live session (keypair + info) so we can re-open it at the new
    // location afterwards WITHOUT a re-login. Then release the handle so the DB
    // is closed before the files move.
    let session_parts = state.current().await.map(|a| {
        (
            a.info.clone(),
            stingle_crypto::keys::KeyPair {
                public_key: a.keypair.public_key.clone(),
                secret_key: a.keypair.secret_key.clone(),
            },
            a.server_pk.clone(),
        )
    });
    *state.account.lock().await = None;
    stop_sync_loop(&state).await;
    stop_watch_loop(&state).await;

    // Move only the photo library (the account folders) — never the app-data
    // config/keys, which live in `config_dir()` and stay put. Copying is
    // blocking, so run it off the async runtime.
    let app2 = app.clone();
    let from = old_accounts_dir.clone();
    let to = new_accounts_dir.clone();
    tauri::async_runtime::spawn_blocking(move || move_dir_with_progress(&app2, &from, &to))
        .await
        .map_err(e)??;

    // Persist the new location and switch the live accounts dir.
    {
        let mut cfg = state.config.lock().await;
        cfg.storage_path = Some(new_accounts_dir.to_string_lossy().to_string());
        cfg.save()?;
    }
    *state.accounts_dir.lock().await = new_accounts_dir.clone();

    // Re-open the session at the new location so the user stays logged in.
    if let Some((info, keypair, server_pk)) = session_parts {
        match Account::reopen_at(&new_accounts_dir, info, keypair, server_pk) {
            Ok(acc) => *state.account.lock().await = Some(Arc::new(acc)),
            Err(err) => tracing::warn!("reopen after storage move failed: {err}"),
        }
    }

    // Resume the background loops at the new location.
    let (sync_on, has_watch) = {
        let cfg = state.config.lock().await;
        (cfg.sync_everything, !cfg.watch_folders.is_empty())
    };
    if sync_on {
        start_sync_loop(app.clone(), &state).await;
    }
    if has_watch {
        start_watch_loop(app, &state).await;
    }
    Ok(())
}

// ----------------------------- stingle:// protocol -----------------------------
//
// URL path = "/<set>/<isThumb>/<album-or-->/<filename>". `convertFileSrc`
// percent-encodes the path (so the slashes arrive as %2F), so we decode first.

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse an HTTP `Range: bytes=START-[END]` header.
fn parse_range(h: &str) -> Option<(u64, Option<u64>)> {
    let rest = h.trim().strip_prefix("bytes=")?;
    let mut it = rest.splitn(2, '-');
    let start = it.next()?.trim().parse::<u64>().ok()?;
    let end = it.next().and_then(|e| {
        let e = e.trim();
        if e.is_empty() {
            None
        } else {
            e.parse::<u64>().ok()
        }
    });
    Some((start, end))
}

fn not_found() -> tauri::http::Response<Vec<u8>> {
    tauri::http::Response::builder()
        .status(404)
        .body(Vec::new())
        .unwrap()
}

async fn build_media_response(
    app: tauri::AppHandle,
    raw_path: String,
    range_header: Option<String>,
) -> tauri::http::Response<Vec<u8>> {
    // Delimiter is `!` (never appears in base64, never percent-encoded), so a
    // `/`, `+` or `=` inside a base64 filename/album-id can't break parsing.
    // Each component is then percent-decoded back to its raw value.
    let parts: Vec<&str> = raw_path.trim_start_matches('/').splitn(4, '!').collect();
    if parts.len() < 4 {
        return not_found();
    }
    let set = set_from_i32(parts[0].parse().unwrap_or(0));
    let is_thumb = parts[1] == "1";
    let album_dec = percent_decode(parts[2]);
    let album = if album_dec == "-" {
        None
    } else {
        Some(album_dec.as_str())
    };
    let filename = percent_decode(parts[3]);
    let range = range_header.as_deref().and_then(parse_range);

    // Clone the account handle and release the state lock immediately so many
    // thumbnail requests decrypt/serve concurrently.
    let acc = match app.state::<AppState>().current().await {
        Some(a) => a,
        None => return not_found(),
    };

    match acc.media_response(set, album, &filename, is_thumb, range).await {
        Ok(m) => {
            let builder = tauri::http::Response::builder()
                .header("Accept-Ranges", "bytes")
                .header("Content-Type", m.content_type)
                .header("Access-Control-Allow-Origin", "*")
                // SECURITY: never let the webview persist DECRYPTED bytes to its
                // on-disk HTTP cache. Re-display re-decrypts from the encrypted
                // on-disk cache (fast, in-process). Only encrypted data on disk.
                .header("Cache-Control", "no-store");
            match m.range {
                Some((s, e)) => builder
                    .status(206)
                    .header("Content-Range", format!("bytes {}-{}/{}", s, e, m.total_size))
                    .header("Content-Length", (e - s + 1).to_string())
                    .body(m.body)
                    .unwrap(),
                None => builder
                    .status(200)
                    .header("Content-Length", m.body.len().to_string())
                    .body(m.body)
                    .unwrap(),
            }
        }
        Err(err) => {
            tracing::warn!("media error for {filename}: {err}");
            not_found()
        }
    }
}

// ----------------------------- run -----------------------------

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt().init();

    tauri::Builder::default()
        // Single-instance must be the FIRST plugin registered. When a second
        // launch is attempted, this fires in the already-running instance instead
        // of starting a new process — we just surface the existing window.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            tray::show_main(app);
        }))
        // Persist & restore the main window's size and position across restarts.
        .plugin(tauri_plugin_window_state::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_drag::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(AppState::new())
        .register_asynchronous_uri_scheme_protocol("stingle", |ctx, request, responder| {
            let app = ctx.app_handle().clone();
            let raw_path = request.uri().path().to_string();
            let range_header = request
                .headers()
                .get("range")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            tauri::async_runtime::spawn(async move {
                responder.respond(build_media_response(app, raw_path, range_header).await);
            });
        })
        .on_window_event(|window, event| {
            // Minimize-to-tray: intercept the window close and hide instead.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let state = window.app_handle().state::<AppState>();
                if state.minimize_to_tray.load(Ordering::Relaxed) {
                    api.prevent_close();
                    let _ = window.hide();
                } else {
                    // Real quit: apply any staged update before the app exits.
                    updater::install_staged_on_exit(window.app_handle());
                }
            }
        })
        .setup(|app| {
            tray::setup_tray(app.handle())?;
            // Clear any decrypted files left behind by an interrupted drag-out
            // or HEIC transcode.
            let _ = std::fs::remove_dir_all(drag_temp_dir());
            let _ = std::fs::remove_dir_all(clipboard_temp_dir());
            let _ = std::fs::remove_dir_all(stingle_core::thumbnail::transcode_temp_dir());
            // Pre-fetch ffmpeg (full build, with HEIC support) in the background
            // so the first HEIC preview/copy doesn't stall on the download.
            std::thread::spawn(|| stingle_core::thumbnail::prepare_media_tools());
            // Check for app updates in the background (honors the auto_update
            // setting): silently stage when on, or emit `update-available` when
            // off so the UI can show the install banner.
            tauri::async_runtime::spawn(updater::check_on_startup(app.handle().clone()));
            // Start the continuous-sync loop if the setting is on; it idles until
            // an account is unlocked.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = handle.state::<AppState>();
                let (sync_on, has_watch) = {
                    let cfg = state.config.lock().await;
                    (cfg.sync_everything, !cfg.watch_folders.is_empty())
                };
                if sync_on {
                    start_sync_loop(handle.clone(), &state).await;
                }
                if has_watch {
                    start_watch_loop(handle.clone(), &state).await;
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_local_accounts,
            register,
            login,
            resume,
            session,
            lock,
            logout,
            sync,
            list_gallery,
            list_trash,
            list_albums,
            list_album_files,
            import_paths,
            trash,
            restore,
            delete_permanently,
            empty_trash,
            create_album,
            rename_album,
            delete_album,
            set_album_cover,
            set_album_blank_cover,
            takeout,
            download_thumbs,
            recovery_phrase,
            is_video,
            recover,
            share_album,
            unshare_album,
            leave_album,
            get_cache_limit,
            set_cache_limit,
            cache_size,
            clear_cache,
            save_files,
            move_to_album,
            move_to_gallery,
            trash_ctx,
            last_account,
            forget_account,
            get_minimize_to_tray,
            set_minimize_to_tray,
            get_auto_update,
            set_auto_update,
            get_app_version,
            check_for_update,
            install_update_now,
            get_autostart,
            set_autostart,
            get_storage_path,
            change_storage_path,
            secure_store_status,
            is_auto_unlock_enabled,
            enable_auto_unlock,
            disable_auto_unlock,
            try_auto_unlock,
            get_sync_everything,
            set_sync_everything,
            get_watch_folders,
            set_watch_folders,
            copy_to_clipboard,
            copy_files_to_clipboard,
            clipboard_files,
            paste_from_clipboard,
            export_for_drag,
            cleanup_drag_export,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
