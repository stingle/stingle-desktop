//! Stingle Photos Desktop — Tauri backend.
//!
//! Holds the logged-in [`Account`] (from `stingle-core`) and exposes it to the
//! React UI via commands. Decrypted thumbnails/originals are streamed to the
//! webview through the `stingle://` URI scheme so plaintext never round-trips as
//! base64 through JS.

use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use stingle_core::{Account, DbFile, FileSet, Sort};
use tauri::{Emitter, Manager, State};
use tokio::sync::Mutex;

mod tray;

pub struct AppState {
    base_dir: PathBuf,
    /// The logged-in account, shared via `Arc` so command handlers and the
    /// `stingle://` media protocol can clone a handle and release the lock
    /// immediately — allowing fully concurrent thumbnail serving.
    account: Mutex<Option<Arc<Account>>>,
}

impl AppState {
    fn new() -> Self {
        Self {
            base_dir: stingle_core::paths::default_base_dir(),
            account: Mutex::new(None),
        }
    }

    /// Clone the current account handle (brief lock), or `None` if logged out.
    async fn current(&self) -> Option<Arc<Account>> {
        self.account.lock().await.clone()
    }
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
}

impl From<DbFile> for FileDto {
    fn from(f: DbFile) -> Self {
        FileDto {
            filename: f.filename,
            album_id: f.album_id,
            date_created: f.date_created,
            date_modified: f.date_modified,
            is_local: f.is_local,
            is_remote: f.is_remote,
        }
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
fn list_local_accounts(state: State<'_, AppState>) -> Vec<LocalAccountDto> {
    Account::list_local(&state.base_dir)
        .into_iter()
        .map(|(account_key, info)| LocalAccountDto {
            account_key,
            email: info.email,
            server_url: info.server_url,
        })
        .collect()
}

#[tauri::command]
async fn register(
    state: State<'_, AppState>,
    server_url: String,
    email: String,
    password: String,
    is_backup: bool,
) -> CmdResult<SessionDto> {
    let acc = Account::register(&server_url, &email, &password, &state.base_dir, is_backup)
        .await
        .map_err(e)?;
    let dto = session_dto(&acc);
    *state.account.lock().await = Some(Arc::new(acc));
    Ok(dto)
}

#[tauri::command]
async fn login(
    state: State<'_, AppState>,
    server_url: String,
    email: String,
    password: String,
) -> CmdResult<SessionDto> {
    let acc = Account::login(&server_url, &email, &password, &state.base_dir)
        .await
        .map_err(e)?;
    let dto = session_dto(&acc);
    *state.account.lock().await = Some(Arc::new(acc));
    Ok(dto)
}

#[tauri::command]
async fn resume(
    state: State<'_, AppState>,
    account_key: String,
    password: String,
) -> CmdResult<SessionDto> {
    let acc = Account::resume(&state.base_dir, &account_key, &password).map_err(e)?;
    let dto = session_dto(&acc);
    *state.account.lock().await = Some(Arc::new(acc));
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
        acc.logout(wipe).await.map_err(e)?;
    }
    Ok(())
}

#[tauri::command]
async fn sync(app: tauri::AppHandle, state: State<'_, AppState>) -> CmdResult<SyncResultDto> {
    let acc = state.current().await.ok_or("Not logged in")?;
    acc.full_sync().await.map_err(e)?;
    let result = SyncResultDto {
        gallery: acc.db.count_files(FileSet::Gallery).map_err(e)?,
        trash: acc.db.count_files(FileSet::Trash).map_err(e)?,
        albums: acc.db.list_albums(true).map_err(e)?.len(),
    };

    // Bulk-download every missing thumbnail in the background, highly
    // concurrent, emitting progress to the UI.
    let acc2 = acc.clone();
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        let app_cb = app2.clone();
        let cb = move |done: usize, total: usize| {
            let _ = app_cb.emit("thumbs-progress", (done, total));
        };
        let n = acc2.download_all_thumbs(64, Some(&cb)).await.unwrap_or(0);
        let _ = app2.emit("thumbs-done", n);
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
        .map(Into::into)
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
        .map(Into::into)
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
        .map(Into::into)
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
    let acc = Account::recover(&server_url, &email, &mnemonic, &new_password, &state.base_dir)
        .await
        .map_err(e)?;
    let dto = session_dto(&acc);
    *state.account.lock().await = Some(Arc::new(acc));
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
) -> CmdResult<()> {
    let acc = state.current().await.ok_or("Not logged in")?;
    acc.move_to_album(set_from_i32(set), album_id.as_deref(), &filenames, &to_album)
        .await
        .map_err(e)
}

#[tauri::command]
async fn move_to_gallery(
    state: State<'_, AppState>,
    album_id: String,
    filenames: Vec<String>,
) -> CmdResult<()> {
    let acc = state.current().await.ok_or("Not logged in")?;
    acc.move_to_gallery(&album_id, &filenames).await.map_err(e)
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
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
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
        .setup(|app| {
            tray::setup_tray(app.handle())?;
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
