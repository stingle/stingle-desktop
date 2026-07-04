//! In-app auto-update via the Tauri updater plugin.
//!
//! Signed bundles + the `latest.json` manifest live on GitHub Releases; the
//! manifest endpoint and minisign public key are configured under
//! `plugins.updater` in `tauri.conf.json`. Only signature-verified bundles are
//! ever installed, so hosting the manifest on GitHub is safe even over an
//! untrusted network.
//!
//! We check once shortly after launch and then every [`CHECK_INTERVAL`] for as
//! long as the app keeps running (see [`run_update_loop`]).
//!
//! Whenever a newer version is found we emit `update-available` so the sidebar
//! shows the "restart to apply" card; clicking it is user-driven via
//! [`install_update_now`](crate::install_update_now), which installs + restarts
//! immediately. In addition, when the `auto_update` setting is on (default) we
//! download the update in the background and *stage* it, so it is applied
//! automatically when the app next quits (see [`install_staged_on_exit`]) even
//! if the user never clicks the card.

use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::AppState;

/// How often to re-check for updates while the app is running.
const CHECK_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// A downloaded-but-not-yet-installed update, applied at quit time.
pub struct StagedUpdate {
    update: Update,
    bytes: Vec<u8>,
}

/// Background update loop: check shortly after startup, then every
/// [`CHECK_INTERVAL`] for as long as the app runs. All failures are logged and
/// swallowed — a failed check must never disrupt the running session.
pub async fn run_update_loop(app: AppHandle) {
    loop {
        check_once(&app).await;
        tokio::time::sleep(CHECK_INTERVAL).await;
    }
}

/// A single check. Emits `update-available` when a newer version exists so the
/// sidebar card appears, and additionally stages the download when `auto_update`
/// is on so it can be applied on quit without user action.
async fn check_once(app: &AppHandle) {
    let state = app.state::<AppState>();
    let enabled = {
        let cfg = state.config.lock().await;
        cfg.auto_update.unwrap_or(true)
    };

    let update = match app.updater() {
        Ok(updater) => match updater.check().await {
            Ok(Some(u)) => u,
            Ok(None) => return, // already up to date
            Err(err) => {
                tracing::warn!("update check failed: {err}");
                return;
            }
        },
        Err(err) => {
            tracing::warn!("updater unavailable: {err}");
            return;
        }
    };

    // Surface the sidebar card regardless of the auto-update setting so the user
    // can restart-and-apply on demand.
    let _ = app.emit("update-available", update.version.clone());

    // With auto-update on, also download once and stage it so the quit handler
    // applies it even if the user never clicks the card. Skip if already staged
    // to avoid re-downloading on every interval.
    let already_staged = state.staged_update.lock().unwrap().is_some();
    if enabled && !already_staged {
        match update.download(|_, _| {}, || {}).await {
            Ok(bytes) => {
                *state.staged_update.lock().unwrap() = Some(StagedUpdate { update, bytes });
                tracing::info!("update staged; will install on quit");
            }
            Err(err) => tracing::warn!("update download failed: {err}"),
        }
    }
}

/// Install a staged update synchronously from the quit path. No-op if nothing is
/// staged. Errors are logged, not propagated — the app is already exiting.
///
/// This path is crash-safe on macOS *because it only installs and exits* — it
/// never re-execs the replaced binary. The caller quits right after (the user
/// asked to quit), and the next launch is a fresh process. Contrast
/// [`relaunch`], which must avoid the in-process re-exec.
pub fn install_staged_on_exit(app: &AppHandle) {
    let staged = app
        .state::<AppState>()
        .staged_update
        .lock()
        .unwrap()
        .take();
    if let Some(StagedUpdate { update, bytes }) = staged {
        if let Err(err) = update.install(bytes) {
            tracing::warn!("staged update install failed: {err}");
        }
    }
}

/// Relaunch the app after applying an in-place update (the user-driven
/// "restart to apply now" path).
///
/// On macOS, `AppHandle::restart()` `execv`s the executable that the updater
/// just overwrote, straight from the still-running process. macOS still holds a
/// cached code-signing verdict for that path — computed from the *pre-update*
/// bytes — so the re-exec is killed with `SIGKILL (Code Signature Invalid)`.
/// That is the "crash on update" users report (the freshly-swapped bundle is
/// perfectly valid; only the in-process re-exec of it is rejected). Instead we
/// launch a detached helper that waits for THIS process to fully exit — which
/// releases the stale vnode — then opens a brand-new instance, and we exit
/// cleanly here rather than exec-ing.
#[cfg(target_os = "macos")]
pub fn relaunch(app: &AppHandle) {
    if let Some(bundle) = current_app_bundle() {
        // Detached `sh`: spin (bounded to ~10s) until our pid is gone, add a
        // small buffer, then re-open the bundle. Orphaned to launchd when we
        // exit, so it outlives us.
        let script = format!(
            "i=0; while /bin/kill -0 {pid} 2>/dev/null && [ $i -lt 50 ]; \
             do sleep 0.2; i=$((i+1)); done; sleep 0.4; /usr/bin/open \"{bundle}\"",
            pid = std::process::id(),
            bundle = bundle.to_string_lossy(),
        );
        if let Err(err) = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .spawn()
        {
            tracing::warn!("failed to spawn relaunch helper: {err}");
        }
    }
    app.exit(0);
}

/// Path to the enclosing `.app` bundle (`.../Foo.app`), if we're running inside
/// one. `current_exe()` is `.../Foo.app/Contents/MacOS/<bin>`, so we walk up to
/// the first ancestor whose name ends in `.app`.
#[cfg(target_os = "macos")]
fn current_app_bundle() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors()
        .find(|p| p.extension().is_some_and(|ext| ext == "app"))
        .map(|p| p.to_path_buf())
}

/// Non-macOS: the in-process re-exec is correct. Windows relaunches via the
/// NSIS/MSI installer and Linux via the AppImage swap — neither has the macOS
/// code-signing-vnode problem.
#[cfg(not(target_os = "macos"))]
pub fn relaunch(app: &AppHandle) {
    app.restart();
}
