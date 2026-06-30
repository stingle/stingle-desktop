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
