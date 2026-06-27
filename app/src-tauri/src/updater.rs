//! In-app auto-update via the Tauri updater plugin.
//!
//! Signed bundles + the `latest.json` manifest live on GitHub Releases; the
//! manifest endpoint and minisign public key are configured under
//! `plugins.updater` in `tauri.conf.json`. Only signature-verified bundles are
//! ever installed, so hosting the manifest on GitHub is safe even over an
//! untrusted network.
//!
//! Behavior, driven by the `auto_update` setting (default on):
//! - **ON:** at launch we download the update in the background and *stage* it,
//!   installing only when the app actually quits (see [`install_staged_on_exit`])
//!   so the running session is never disrupted — the new version is live on the
//!   next launch.
//! - **OFF:** we only check; if an update exists we emit `update-available` so
//!   the UI shows a sidebar banner. Installing is then user-driven via
//!   [`install_update_now`](crate::install_update_now), which installs + restarts
//!   immediately.

use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::AppState;

/// A downloaded-but-not-yet-installed update, applied at quit time.
pub struct StagedUpdate {
    update: Update,
    bytes: Vec<u8>,
}

/// Background update check, run once at startup. Honors the `auto_update`
/// setting. All failures are logged and swallowed — a failed check must never
/// disrupt normal startup.
pub async fn check_on_startup(app: AppHandle) {
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

    if enabled {
        // Download now, install at quit so the running session is untouched.
        match update.download(|_, _| {}, || {}).await {
            Ok(bytes) => {
                *state.staged_update.lock().unwrap() = Some(StagedUpdate { update, bytes });
                tracing::info!("update staged; will install on quit");
            }
            Err(err) => tracing::warn!("update download failed: {err}"),
        }
    } else {
        // Disabled: just notify the UI to show the install banner.
        let _ = app.emit("update-available", update.version.clone());
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
