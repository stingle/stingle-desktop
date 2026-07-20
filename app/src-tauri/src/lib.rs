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
use stingle_core::{Account, DbAlbum, DbFile, FileSet, Sort};
use tauri::{Emitter, Manager, State};
use tokio::sync::Mutex;

mod clipboard_files;
mod config;
mod media_server;
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
    /// Handle to the always-on periodic idle-sync loop, if started.
    idle_sync_task: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    /// Serializes sync passes (`sync_cloud_to_local` + `upload_to_cloud`) so a
    /// manual sync and the periodic idle-sync never run concurrently. The idle
    /// loop `try_lock`s it and skips its tick when the guard is held — that's
    /// how "sync only while idle" is enforced.
    sync_guard: Arc<Mutex<()>>,
    /// Handle to the background watch-folder import loop, if running.
    watch_task: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    /// An update downloaded at startup (auto-update on) but not yet installed;
    /// applied when the app quits. A plain `std::sync::Mutex` so the quit handler
    /// (sync context) can lock it without a runtime.
    staged_update: std::sync::Mutex<Option<updater::StagedUpdate>>,
    /// Version string of an available update, set by the background check loop
    /// as soon as one is found. The frontend polls this on mount (via the
    /// `pending_update` command) so a newer version discovered *before* the UI's
    /// `update-available` listener was ready — the loop's very first check races
    /// the login/mount — still surfaces the restart-to-apply card.
    pending_update: std::sync::Mutex<Option<String>>,
    /// Port + auth token of the loopback video server, set once at startup on
    /// Linux (WebKitGTK can't play custom-scheme media — see `media_server`).
    /// Stays unset on Windows/macOS where stingle:// videos work natively.
    video_server: std::sync::OnceLock<(u16, String)>,
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
            idle_sync_task: Mutex::new(None),
            sync_guard: Arc::new(Mutex::new(())),
            watch_task: Mutex::new(None),
            staged_update: std::sync::Mutex::new(None),
            pending_update: std::sync::Mutex::new(None),
            video_server: std::sync::OnceLock::new(),
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

/// Build `FileDto`s from listed rows. `is_video` comes from the DB column
/// (derived once at ingest); rows that predate the column are decoded here and
/// the result is written back in one batch, so each legacy row pays the header
/// seal-open exactly once ever — never per listing.
fn file_dtos(acc: &Account, set: FileSet, album_id: Option<&str>, rows: Vec<DbFile>) -> Vec<FileDto> {
    let mut backfill: Vec<(String, bool)> = Vec::new();
    let out = rows
        .into_iter()
        .map(|f| {
            let is_video = match f.is_video {
                Some(v) => v,
                None => {
                    let derived = acc.try_row_is_video(set, album_id, &f.headers);
                    if let Some(v) = derived {
                        backfill.push((f.filename.clone(), v));
                    }
                    // An undecodable header renders as a photo (same fallback
                    // as before) but is NOT persisted, so it can heal later.
                    derived.unwrap_or(false)
                }
            };
            FileDto {
                filename: f.filename,
                album_id: f.album_id,
                date_created: f.date_created,
                date_modified: f.date_modified,
                is_local: f.is_local,
                is_remote: f.is_remote,
                is_video,
            }
        })
        .collect();
    if !backfill.is_empty() {
        let res = match (set, album_id) {
            (FileSet::Album, Some(aid)) => {
                let items: Vec<(String, String, bool)> = backfill
                    .into_iter()
                    .map(|(f, v)| (aid.to_string(), f, v))
                    .collect();
                acc.db.set_album_is_video_batch(&items)
            }
            _ => acc.db.set_is_video_batch(set, &backfill),
        };
        if let Err(err) = res {
            tracing::warn!("is_video backfill failed: {err}");
        }
    }
    out
}

#[derive(Serialize)]
struct AlbumDto {
    album_id: String,
    name: String,
    is_owner: bool,
    is_shared: bool,
    cover: String,
    count: i64,
    /// 4-char permission string `"1"+add+share+copy` (empty for un-shared albums).
    permissions: String,
}

#[derive(Serialize)]
struct ContactDto {
    /// i64 stringified so JS never rounds a large user-id.
    user_id: String,
    email: String,
    date_used: i64,
}

/// An album in the Sharing view: the album fields plus its resolved members.
#[derive(Serialize)]
struct SharedAlbumDto {
    #[serde(flatten)]
    album: AlbumDto,
    members: Vec<MemberDto>,
}

#[derive(Serialize)]
struct MemberDto {
    user_id: String,
    /// `None` when the member isn't in the local contacts table.
    email: Option<String>,
    /// True for the account viewing this list (the album owner, in practice).
    is_owner: bool,
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
    /// Remote updates applied by this pass; 0 = nothing changed locally, so
    /// the frontend can skip reloading its lists.
    changes: usize,
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
    // Hold the sync guard for the metadata+upload phases so the periodic
    // idle-sync loop backs off while a manual sync is running.
    let _sync_guard = state.sync_guard.lock().await;
    // Run the two sync phases directly (instead of full_sync) so we can surface
    // per-file upload progress to the UI between the metadata pull and prefetch.
    let changes = match acc.sync_cloud_to_local().await {
        Ok(n) => n,
        Err(err) => {
            if err.is_logged_out() {
                handle_session_expired(&app, &state).await;
            }
            return Err(e(err));
        }
    };
    {
        let app_cb = app.clone();
        let upcb = move |done: usize, total: usize| {
            let _ = app_cb.emit("upload-progress", (done, total));
        };
        let r = acc.upload_to_cloud(Some(&upcb)).await;
        // Terminal event only when an upload row could be showing (something was
        // attempted) — a no-op pass must not trigger a full frontend reload.
        if !matches!(r, Ok(0)) {
            let _ = app.emit("upload-done", ());
        }
        if let Err(err) = r {
            if err.is_logged_out() {
                handle_session_expired(&app, &state).await;
            }
            return Err(e(err));
        }
    }
    let result = SyncResultDto {
        gallery: acc.db.count_files(FileSet::Gallery).map_err(e)?,
        trash: acc.db.count_files(FileSet::Trash).map_err(e)?,
        albums: acc.db.list_albums(true).map_err(e)?.len(),
        changes,
    };
    if changes > 0 {
        let _ = app.emit("library-changed", ());
    }

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
        if n > 0 {
            let _ = app2.emit("thumbs-done", n);
        }

        if sync_all {
            let app_cb2 = app2.clone();
            let cb2 = move |done: usize, total: usize| {
                let _ = app_cb2.emit("originals-progress", (done, total));
            };
            let m = acc2.download_all_originals(6, Some(&cb2)).await.unwrap_or(0);
            if m > 0 {
                let _ = app2.emit("originals-done", m);
            }
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
    if n > 0 {
        let _ = app.emit("thumbs-done", n);
    }
    Ok(n)
}

#[tauri::command]
async fn list_gallery(
    state: State<'_, AppState>,
    offset: i64,
    limit: i64,
) -> CmdResult<Vec<FileDto>> {
    let acc = state.current().await.ok_or("Not logged in")?;
    let rows = acc
        .db
        .list_files(FileSet::Gallery, Sort::Desc, Some(limit), offset)
        .map_err(e)?;
    Ok(file_dtos(&acc, FileSet::Gallery, None, rows))
}

#[tauri::command]
async fn list_trash(state: State<'_, AppState>) -> CmdResult<Vec<FileDto>> {
    let acc = state.current().await.ok_or("Not logged in")?;
    let rows = acc
        .db
        .list_files(FileSet::Trash, Sort::Desc, None, 0)
        .map_err(e)?;
    Ok(file_dtos(&acc, FileSet::Trash, None, rows))
}

/// Build an `AlbumDto` from a row + decrypted name, filling the cover fallback.
fn album_dto(acc: &Account, a: DbAlbum, name: String) -> CmdResult<AlbumDto> {
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
    Ok(AlbumDto {
        album_id: a.album_id,
        name,
        is_owner: a.is_owner,
        is_shared: a.is_shared,
        cover,
        count,
        permissions: a.permissions,
    })
}

#[tauri::command]
async fn list_albums(state: State<'_, AppState>) -> CmdResult<Vec<AlbumDto>> {
    let acc = state.current().await.ok_or("Not logged in")?;
    let mut out = Vec::new();
    for (a, name) in acc.list_albums_with_names(false).map_err(e)? {
        out.push(album_dto(&acc, a, name)?);
    }
    Ok(out)
}

/// Shared albums only (owned-and-shared + received), each with resolved members,
/// most-recently-modified first. Hidden albums are included — a shared album
/// auto-created hidden on mobile still belongs in the Sharing list.
#[tauri::command]
async fn list_shared_albums(state: State<'_, AppState>) -> CmdResult<Vec<SharedAlbumDto>> {
    let acc = state.current().await.ok_or("Not logged in")?;
    let me = acc.info.user_id.clone();
    let mut shared: Vec<(DbAlbum, String)> = acc
        .list_albums_with_names(true)
        .map_err(e)?
        .into_iter()
        .filter(|(a, _)| a.is_shared)
        .collect();
    // Most recently updated first.
    shared.sort_by_key(|(a, _)| std::cmp::Reverse(a.date_modified));
    let mut out = Vec::new();
    for (a, name) in shared {
        let members = acc
            .album_members(&a.album_id)
            .map_err(e)?
            .into_iter()
            .map(|(uid, email)| MemberDto {
                is_owner: uid.to_string() == me,
                user_id: uid.to_string(),
                email,
            })
            .collect();
        out.push(SharedAlbumDto {
            album: album_dto(&acc, a, name)?,
            members,
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
    let rows = acc
        .db
        .list_album_files(&album_id, Sort::Desc, None, 0)
        .map_err(e)?;
    Ok(file_dtos(&acc, FileSet::Album, Some(&album_id), rows))
}

#[tauri::command]
async fn import_paths(
    app: tauri::AppHandle,
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
    // Drop paths that are our OWN decrypted exports (a drag-OUT or clipboard copy
    // released/pasted back onto the window): they're already in the library and
    // cleanup is about to delete them, so importing would race that read and hang.
    // Then expand folders up front so the progress bar has a real total, and
    // import the flat list with live progress (and cooperative cancellation).
    let inputs: Vec<PathBuf> = paths
        .iter()
        .map(PathBuf::from)
        .filter(|p| !is_own_temp_export(p))
        .collect();
    let files = acc.collect_import_paths(&inputs);
    let app_cb = app.clone();
    let cb = move |done: usize, total: usize| {
        let _ = app_cb.emit("import-progress", (done, total));
    };
    let result = acc
        .import_files_progress(&files, set, album_id.as_deref(), Some(&cb))
        .await;
    // Emit `import-done` in EVERY case (success or error). The UI's "Importing"
    // progress row is only cleared by this event (or a zero-total tick), so an
    // import error would otherwise leave it stuck on screen — the "stalls
    // forever" symptom when a drag-out is dropped back onto the window.
    let _ = app.emit("import-done", ());
    let imported = result.map_err(e)?;
    Ok(imported)
}

#[tauri::command]
async fn cancel_import(state: State<'_, AppState>) -> CmdResult<()> {
    if let Some(acc) = state.current().await {
        acc.request_stop_import();
    }
    Ok(())
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
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    out_dir: String,
    include_trash: bool,
) -> CmdResult<TakeoutDto> {
    let acc = state.current().await.ok_or("Not logged in")?;
    let app_cb = app.clone();
    let cb = move |done: usize, total: usize| {
        let _ = app_cb.emit("takeout-progress", (done, total));
    };
    let stats = acc
        .takeout(&PathBuf::from(out_dir), include_trash, Some(&cb))
        .await
        .map_err(e)?;
    let _ = app.emit("takeout-done", ());
    Ok(TakeoutDto {
        written: stats.written,
        errors: stats.errors,
    })
}

#[tauri::command]
async fn cancel_takeout(state: State<'_, AppState>) -> CmdResult<()> {
    if let Some(acc) = state.current().await {
        acc.request_stop_takeout();
    }
    Ok(())
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

/// Warm the on-disk encrypted cache for files the viewer is likely to show
/// next (its neighbors). Download-only — no decrypt, no transcode — so it
/// costs nothing but idle network lanes; already-local files return instantly.
/// Fire-and-forget: spawns and returns immediately.
#[tauri::command]
async fn prefetch_media(
    state: State<'_, AppState>,
    set: i32,
    filenames: Vec<String>,
) -> CmdResult<()> {
    let acc = state.current().await.ok_or("Not logged in")?;
    let set = set_from_i32(set);
    // Defensive cap: the viewer only ever asks for a couple of neighbors.
    for filename in filenames.into_iter().take(4) {
        let acc = acc.clone();
        tauri::async_runtime::spawn(async move {
            let _ = acc.ensure_encrypted(set, &filename, false).await;
        });
    }
    Ok(())
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

/// Share loose files: auto-create a hidden album from `filenames` (in `set` /
/// optional source `album_id`), then share it with `emails`. Returns the new
/// album id. Source files stay put.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn share_new_album(
    state: State<'_, AppState>,
    set: i32,
    album_id: Option<String>,
    filenames: Vec<String>,
    name: String,
    emails: Vec<String>,
    allow_add: bool,
    allow_share: bool,
    allow_copy: bool,
) -> CmdResult<String> {
    let acc = state.current().await.ok_or("Not logged in")?;
    acc.share_new_album(
        set_from_i32(set),
        album_id.as_deref(),
        &filenames,
        &name,
        &emails,
        allow_add,
        allow_share,
        allow_copy,
    )
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
async fn list_contacts(state: State<'_, AppState>) -> CmdResult<Vec<ContactDto>> {
    let acc = state.current().await.ok_or("Not logged in")?;
    Ok(acc
        .contacts()
        .map_err(e)?
        .into_iter()
        .map(|c| ContactDto {
            user_id: c.user_id.to_string(),
            email: c.email,
            date_used: c.date_used,
        })
        .collect())
}

#[tauri::command]
async fn list_album_members(
    state: State<'_, AppState>,
    album_id: String,
) -> CmdResult<Vec<MemberDto>> {
    let acc = state.current().await.ok_or("Not logged in")?;
    let me = acc.info.user_id.clone();
    Ok(acc
        .album_members(&album_id)
        .map_err(e)?
        .into_iter()
        .map(|(uid, email)| MemberDto {
            is_owner: uid.to_string() == me,
            user_id: uid.to_string(),
            email,
        })
        .collect())
}

#[tauri::command]
async fn edit_album_perms(
    state: State<'_, AppState>,
    album_id: String,
    allow_add: bool,
    allow_share: bool,
    allow_copy: bool,
) -> CmdResult<()> {
    let guard = state.account.lock().await;
    guard
        .as_ref()
        .ok_or("Not logged in")?
        .edit_album_perms(&album_id, allow_add, allow_share, allow_copy)
        .await
        .map_err(e)
}

#[tauri::command]
async fn remove_album_member(
    state: State<'_, AppState>,
    album_id: String,
    member_user_id: String,
) -> CmdResult<()> {
    let uid: i64 = member_user_id.parse().map_err(e)?;
    let guard = state.account.lock().await;
    guard
        .as_ref()
        .ok_or("Not logged in")?
        .remove_album_member(&album_id, uid)
        .await
        .map_err(e)
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
    let convert_heic = state.config.lock().await.convert_heic_on_export_enabled();
    let s = set_from_i32(set);
    let dir = clipboard_temp_dir();
    // Drop the previous copy's decrypted files before writing new ones.
    let _ = std::fs::remove_dir_all(&dir);
    create_private_dir(&dir).map_err(e)?;

    let mut paths = Vec::new();
    for name in &filenames {
        let (bytes, out_name) =
            export_decrypted(&acc, s, album_id.as_deref(), name, convert_heic).await.map_err(e)?;
        // `out_name` is header-derived (attacker-controllable) — reduce it to a
        // safe bare filename so it can't escape the temp dir.
        let out = unique_temp(&dir, &stingle_core::safe_filename(&out_name));
        std::fs::write(&out, &bytes).map_err(e)?;
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

/// Decrypt one library item for hand-off to another app (drag-out / clipboard).
/// When `convert_heic` is on and the item is a HEIC/HEIF still, it is transcoded
/// to JPEG — many apps can't read HEIC — and the output name's extension is
/// switched to `.jpg`. Otherwise the original bytes and name are returned
/// unchanged. A failed transcode falls back to the untouched original rather
/// than failing the whole export. The returned name is header-derived and MUST
/// still be passed through `safe_filename` by the caller before it becomes a path.
async fn export_decrypted(
    acc: &Account,
    s: FileSet,
    album_id: Option<&str>,
    name: &str,
    convert_heic: bool,
) -> stingle_core::Result<(Vec<u8>, String)> {
    let plain = acc.get_decrypted(s, album_id, name, false).await?;
    let orig = acc
        .original_name(s, album_id, name)
        .unwrap_or_else(|_| name.to_string());
    if convert_heic && stingle_core::heif::is_heif(&plain) {
        let ext = std::path::Path::new(&orig)
            .extension()
            .and_then(|x| x.to_str())
            .unwrap_or("heic")
            .to_lowercase();
        // The ffmpeg-backed transcode is blocking and can take a beat on a
        // full-size photo — run it off the async runtime. On failure the
        // original HEIC bytes come back out of the closure untouched.
        let (bytes, converted) = tokio::task::spawn_blocking(move || {
            match stingle_core::thumbnail::transcode_to_jpeg(&plain, &ext) {
                Ok(jpg) => (jpg, true),
                Err(_) => (plain, false),
            }
        })
        .await
        .map_err(|err| stingle_core::CoreError::Other(format!("transcode task failed: {err}")))?;
        if converted {
            let stem = std::path::Path::new(&orig)
                .file_stem()
                .and_then(|x| x.to_str())
                .unwrap_or("image");
            return Ok((bytes, format!("{stem}.jpg")));
        }
        return Ok((bytes, orig));
    }
    Ok((plain, orig))
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

/// True if `path` lives inside one of our private temp dirs holding DECRYPTED
/// exports (`stingle-drag` / `stingle-clip`). A drop or paste of these back onto
/// our own window — e.g. releasing a drag-OUT too early — must never be
/// re-imported: the files are already in the library and are about to be deleted
/// by cleanup, which would otherwise race the import read and leave it hung.
/// Slash/case-normalized so an OS-reported drop path still matches on Windows.
fn is_own_temp_export(path: &std::path::Path) -> bool {
    fn norm(p: &std::path::Path) -> String {
        let s = p.to_string_lossy().replace('\\', "/");
        if cfg!(windows) { s.to_lowercase() } else { s }
    }
    let target = norm(path);
    [drag_temp_dir(), clipboard_temp_dir()].iter().any(|base| {
        let mut prefix = norm(base);
        prefix.push('/');
        target.starts_with(&prefix)
    })
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
    let convert_heic = state.config.lock().await.convert_heic_on_export_enabled();
    let s = set_from_i32(set);
    let dir = drag_temp_dir();
    create_private_dir(&dir).map_err(e)?;

    let mut files = Vec::new();
    for name in &filenames {
        let (bytes, out_name) =
            export_decrypted(&acc, s, album_id.as_deref(), name, convert_heic).await.map_err(e)?;
        let out = unique_temp(&dir, &stingle_core::safe_filename(&out_name));
        std::fs::write(&out, &bytes).map_err(e)?;
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
async fn get_start_minimized(state: State<'_, AppState>) -> CmdResult<bool> {
    Ok(state.config.lock().await.start_minimized)
}

#[tauri::command]
async fn set_start_minimized(state: State<'_, AppState>, enabled: bool) -> CmdResult<()> {
    let mut cfg = state.config.lock().await;
    cfg.start_minimized = enabled;
    cfg.save()
}

#[tauri::command]
async fn get_convert_heic_on_export(state: State<'_, AppState>) -> CmdResult<bool> {
    Ok(state.config.lock().await.convert_heic_on_export_enabled())
}

#[tauri::command]
async fn set_convert_heic_on_export(state: State<'_, AppState>, enabled: bool) -> CmdResult<()> {
    let mut cfg = state.config.lock().await;
    cfg.convert_heic_on_export = Some(enabled);
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

/// Base URL (`http://127.0.0.1:<port>/<token>`) of the loopback video server,
/// or `None` on platforms where videos stream via stingle:// directly.
#[tauri::command]
fn video_server_base(state: State<'_, AppState>) -> Option<String> {
    state
        .video_server
        .get()
        .map(|(port, token)| format!("http://127.0.0.1:{port}/{token}"))
}

/// Version of an update already discovered by the background check loop, or
/// `None` if none is pending. Read-only (no network) — the frontend calls this
/// on mount so an update found before its `update-available` listener existed
/// still shows the restart-to-apply card. See `AppState::pending_update`.
#[tauri::command]
fn pending_update(state: State<'_, AppState>) -> Option<String> {
    state.pending_update.lock().unwrap().clone()
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
    // NOT `app.restart()`: on macOS that re-execs the just-overwritten binary
    // in-process and macOS SIGKILLs it as "Code Signature Invalid". `relaunch`
    // exits cleanly and re-opens a fresh instance instead. See updater.rs.
    updater::relaunch(&app);
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
    keyring: bool,
}

// async: the availability probes touch the OS keychain / D-Bus and must not
// run on the main thread (sync Tauri commands do).
#[tauri::command]
async fn secure_store_status() -> SecureStoreStatusDto {
    let avail = secure_store::availability();
    SecureStoreStatusDto {
        biometric: avail.biometric,
        keyring: avail.keyring,
    }
}

#[tauri::command]
async fn is_auto_unlock_enabled(state: State<'_, AppState>) -> CmdResult<bool> {
    Ok(state.config.lock().await.auto_unlock)
}

#[derive(Serialize)]
struct EnableAutoUnlockDto {
    /// Which tier holds the key: "biometric" | "keyring" | "plaintext".
    store: &'static str,
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

    // Store the key: best secure store if any, else plaintext on opt-in only.
    let avail = secure_store::availability();
    let kind = if avail.biometric || avail.keyring {
        secure_store::store_secure(&account_key, &key)?
    } else if allow_plaintext {
        secure_store::store_plaintext(&account_key, &key)?;
        secure_store::StoreKind::Plaintext
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
    Ok(EnableAutoUnlockDto {
        store: kind.as_str(),
    })
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
        // The first pass always scans for missing originals (it may be resuming
        // a backlog after the toggle flipped on); later passes only when the
        // metadata sync actually changed something.
        let mut first_pass = true;
        loop {
            if let Some(acc) = app2.state::<AppState>().current().await {
                let app_up = app2.clone();
                let upcb = move |done: usize, total: usize| {
                    let _ = app_up.emit("upload-progress", (done, total));
                };
                let synced = acc.sync_cloud_to_local().await;
                let changes = *synced.as_ref().ok().unwrap_or(&0);
                let uploaded = if synced.is_ok() {
                    acc.upload_to_cloud(Some(&upcb)).await
                } else {
                    Ok(0)
                };
                // Terminal/refresh events only when something actually happened —
                // an idle no-op pass must not jolt the frontend into a full reload.
                if !matches!(uploaded, Ok(0)) {
                    let _ = app2.emit("upload-done", ());
                }
                if changes > 0 {
                    let _ = app2.emit("library-changed", ());
                }
                if let Err(err) = synced.and(uploaded) {
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
                if first_pass || changes > 0 {
                    first_pass = false;
                    let app_cb = app2.clone();
                    let cb = move |done: usize, total: usize| {
                        let _ = app_cb.emit("originals-progress", (done, total));
                    };
                    let n = acc.download_all_originals(6, Some(&cb)).await.unwrap_or(0);
                    if n > 0 {
                        let _ = app2.emit("originals-done", n);
                    }
                }
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

/// Start the always-on periodic idle-sync loop (no-op if already running).
///
/// Every `auto_sync_interval` (from config) it pulls new cloud metadata, pushes
/// local changes, and prefetches thumbnails — but only *while idle*: it skips the
/// tick when disabled in settings, logged out, when the "sync everything" loop
/// already handles this, or when another sync holds `sync_guard`. Started once at
/// launch; it self-idles until an account is unlocked, so it never needs stopping.
async fn start_idle_sync_loop(app: tauri::AppHandle, state: &AppState) {
    let mut guard = state.idle_sync_task.lock().await;
    if guard.is_some() {
        return;
    }
    let app2 = app.clone();
    let handle = tauri::async_runtime::spawn(async move {
        loop {
            // Re-read the interval each cycle so a tuned value takes effect on the
            // next tick without restarting the loop.
            let interval = app2.state::<AppState>().config.lock().await.auto_sync_interval();
            tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
            let state = app2.state::<AppState>();
            // Skip while disabled (keep looping so re-enabling takes effect) or
            // while the continuous "sync everything" loop already covers this.
            {
                let cfg = state.config.lock().await;
                if !cfg.auto_sync_enabled() || cfg.sync_everything {
                    continue;
                }
            }
            let Some(acc) = state.current().await else { continue };
            // Only sync while idle: if a manual sync is running, skip this tick.
            let synced;
            let uploaded;
            {
                let Ok(_busy) = state.sync_guard.try_lock() else { continue };
                let app_up = app2.clone();
                let upcb = move |done: usize, total: usize| {
                    let _ = app_up.emit("upload-progress", (done, total));
                };
                synced = acc.sync_cloud_to_local().await;
                uploaded = if synced.is_ok() {
                    acc.upload_to_cloud(Some(&upcb)).await
                } else {
                    Ok(0)
                };
                // No events for a no-op tick — the common every-N-minutes case
                // must not make the frontend re-fetch and re-render its lists.
                if !matches!(uploaded, Ok(0)) {
                    let _ = app2.emit("upload-done", ());
                }
            }
            let changes = *synced.as_ref().ok().unwrap_or(&0);
            if changes > 0 {
                let _ = app2.emit("library-changed", ());
            }
            if let Err(err) = synced.and(uploaded) {
                if err.is_logged_out() {
                    // Token died: tear down the session so we stop hammering the
                    // server with a dead token (mirrors the "sync everything"
                    // loop). The loop keeps running, idling until the next login.
                    if let Some(a) = state.account.lock().await.take() {
                        a.request_stop_originals();
                    }
                    stop_sync_loop(&state).await;
                    let _ = app2.emit("session-expired", ());
                }
                continue;
            }
            // Prefetch any newly-arrived thumbnails — only when the metadata
            // sync changed something. A no-change tick has no new thumbnails,
            // and the scan itself stats every cached thumb file (tens of
            // thousands of syscalls on a big library), so don't pay it idly.
            if changes > 0 {
                let app_cb = app2.clone();
                let cb = move |done: usize, total: usize| {
                    let _ = app_cb.emit("thumbs-progress", (done, total));
                };
                let n = acc.download_all_thumbs(64, Some(&cb)).await.unwrap_or(0);
                if n > 0 {
                    let _ = app2.emit("thumbs-done", n);
                }
            }
        }
    });
    *guard = Some(handle);
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
        // The aborted loop never reached its own terminal emits, so the sidebar
        // would keep showing a frozen progress row — clear those rows here.
        let _ = app.emit("originals-done", 0usize);
        let _ = app.emit("upload-done", ());
    }
    Ok(())
}

#[tauri::command]
async fn get_auto_sync(state: State<'_, AppState>) -> CmdResult<bool> {
    Ok(state.config.lock().await.auto_sync_enabled())
}

#[tauri::command]
async fn set_auto_sync(state: State<'_, AppState>, enabled: bool) -> CmdResult<()> {
    let mut cfg = state.config.lock().await;
    cfg.auto_sync = Some(enabled);
    cfg.save()?;
    Ok(())
}

/// The idle-sync interval in **minutes** (what the UI shows), clamped to the
/// configured floor.
#[tauri::command]
async fn get_auto_sync_interval(state: State<'_, AppState>) -> CmdResult<u64> {
    Ok(state.config.lock().await.auto_sync_interval() / 60)
}

/// Set the idle-sync interval, given in **minutes**; stored in seconds and
/// clamped to at least 1 minute. Takes effect on the loop's next tick.
#[tauri::command]
async fn set_auto_sync_interval(state: State<'_, AppState>, minutes: u64) -> CmdResult<()> {
    let mut cfg = state.config.lock().await;
    cfg.auto_sync_interval_secs = Some(minutes.saturating_mul(60).max(config::MIN_AUTO_SYNC_INTERVAL_SECS));
    cfg.save()?;
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
                    let imported = watch::scan_folders(&app2, &acc, &folders, &mut stable).await;
                    // Newly imported files are local-only; push them to the cloud
                    // right away (with progress) so watch-folder import "just syncs"
                    // without waiting for a manual sync.
                    if imported > 0 {
                        let app_up = app2.clone();
                        let upcb = move |done: usize, total: usize| {
                            let _ = app_up.emit("upload-progress", (done, total));
                        };
                        let r = acc.upload_to_cloud(Some(&upcb)).await;
                        if !matches!(r, Ok(0)) {
                            let _ = app2.emit("upload-done", ());
                        }
                    }
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

/// Split a `<set>!<isThumb>!<album-or-->!<filename>` media payload into its
/// decoded components. Delimiter is `!` (never appears in base64, never
/// percent-encoded), so a `/`, `+` or `=` inside a base64 filename/album-id
/// can't break parsing. Each component is percent-decoded back to its raw
/// value. Shared by the stingle:// protocol and the Linux loopback video
/// server, whose URLs carry the identical payload.
fn parse_media_path(raw_path: &str) -> Option<(FileSet, bool, Option<String>, String)> {
    let parts: Vec<&str> = raw_path.trim_start_matches('/').splitn(4, '!').collect();
    if parts.len() < 4 {
        return None;
    }
    let set = set_from_i32(parts[0].parse().unwrap_or(0));
    let is_thumb = parts[1] == "1";
    let album_dec = percent_decode(parts[2]);
    let album = if album_dec == "-" { None } else { Some(album_dec) };
    Some((set, is_thumb, album, percent_decode(parts[3])))
}

async fn build_media_response(
    app: tauri::AppHandle,
    raw_path: String,
    range_header: Option<String>,
) -> tauri::http::Response<Vec<u8>> {
    let Some((set, is_thumb, album, filename)) = parse_media_path(&raw_path) else {
        return not_found();
    };
    let range = range_header.as_deref().and_then(parse_range);

    // Clone the account handle and release the state lock immediately so many
    // thumbnail requests decrypt/serve concurrently.
    let acc = match app.state::<AppState>().current().await {
        Some(a) => a,
        None => return not_found(),
    };

    match acc
        .media_response(set, album.as_deref(), &filename, is_thumb, range)
        .await
    {
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

/// Point `stingle-core` at the ffmpeg bundled next to our executable, if one is
/// present. Release builds ship a current ffmpeg as a Tauri `externalBin`, which
/// lands beside the main binary (e.g. `Contents/MacOS/ffmpeg` on macOS). Using
/// it avoids the unpinned ffmpeg-sidecar download whose cached version could be
/// too old to decode iOS-18 HEICs. Setting `STINGLE_FFMPEG` makes
/// `stingle_core::thumbnail::resolve_ffmpeg` use it verbatim (no network). A
/// no-op when the env var is already set or no bundled binary exists (dev).
fn use_bundled_ffmpeg() {
    if std::env::var_os("STINGLE_FFMPEG").is_some() {
        return;
    }
    let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    if let Some(path) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(name)))
        .filter(|p| p.exists())
    {
        std::env::set_var("STINGLE_FFMPEG", path);
    }
}

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
            // Login launches carry this flag so we can tell them apart from a
            // manual launch and honor the "start in tray" setting.
            Some(vec!["--minimized"]),
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
            // Linux: WebKitGTK's GStreamer media player can't load
            // custom-scheme URLs (WebKit bug 146351), so videos stream from a
            // token-guarded loopback HTTP server instead of stingle://.
            // Bound synchronously so `video_server_base` is ready before the
            // webview's first request.
            if cfg!(target_os = "linux") {
                match tauri::async_runtime::block_on(media_server::start(app.handle().clone())) {
                    Ok(info) => {
                        let _ = app.state::<AppState>().video_server.set(info);
                    }
                    Err(err) => tracing::warn!("video server failed to start: {err}"),
                }
            }
            // The window starts hidden (`"visible": false`). Show it now unless
            // this is a login launch (carries `--minimized`) and the user asked
            // to start in the tray — in which case it stays hidden until they
            // open it from the tray icon.
            {
                let state = app.state::<AppState>();
                let start_minimized =
                    tauri::async_runtime::block_on(async { state.config.lock().await.start_minimized });
                let launched_at_login = std::env::args().any(|a| a == "--minimized");
                if !(launched_at_login && start_minimized) {
                    if let Some(w) = app.get_webview_window("main") {
                        let _ = w.show();
                        let _ = w.set_focus();
                    }
                }
            }
            // Clear any decrypted files left behind by an interrupted drag-out
            // or HEIC transcode.
            let _ = std::fs::remove_dir_all(drag_temp_dir());
            let _ = std::fs::remove_dir_all(clipboard_temp_dir());
            let _ = std::fs::remove_dir_all(stingle_core::thumbnail::transcode_temp_dir());
            // Prefer the ffmpeg shipped next to our binary (bundled by the
            // release CI as a current build with iOS-18 adaptive-HDR HEIC
            // support). If it's absent — e.g. `tauri dev` — fall through to
            // ffmpeg-sidecar's runtime download below.
            use_bundled_ffmpeg();
            // Pre-fetch ffmpeg (full build, with HEIC support) in the background
            // so the first HEIC preview/copy doesn't stall on the download.
            std::thread::spawn(|| stingle_core::thumbnail::prepare_media_tools());
            // Check for app updates in the background now and every 30 minutes:
            // emit `update-available` so the UI can show the restart-to-apply
            // card, and (when auto_update is on) stage the download for install
            // on quit.
            tauri::async_runtime::spawn(updater::run_update_loop(app.handle().clone()));
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
                // Always-on: syncs every 10 min while idle (self-idles until an
                // account is unlocked, and backs off when the loops above run).
                start_idle_sync_loop(handle.clone(), &state).await;
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
            list_shared_albums,
            list_album_files,
            prefetch_media,
            import_paths,
            cancel_import,
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
            cancel_takeout,
            download_thumbs,
            recovery_phrase,
            is_video,
            recover,
            share_album,
            share_new_album,
            unshare_album,
            leave_album,
            list_contacts,
            list_album_members,
            edit_album_perms,
            remove_album_member,
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
            get_start_minimized,
            set_start_minimized,
            get_auto_update,
            set_auto_update,
            get_convert_heic_on_export,
            set_convert_heic_on_export,
            get_app_version,
            video_server_base,
            pending_update,
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
            get_auto_sync,
            set_auto_sync,
            get_auto_sync_interval,
            set_auto_sync_interval,
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
