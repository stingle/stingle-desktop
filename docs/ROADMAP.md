# Stingle Photos Desktop — Implementation Roadmap

Goal: a cross-platform (Windows/macOS/Linux) desktop client that replicates the
**full** functionality of the Stingle Photos Android app, is byte-compatible
with the `.sp` format, and talks to the same `api.stingle.org` v2 server.

This roadmap is derived directly from the Android source
(`C:\Users\Alex\work\android\stingle-photos-android`). Each phase has an
explicit checklist; nothing from the Android app is dropped except where it is
inherently mobile-only (noted under **Desktop adaptations**).

Stack: **Tauri 2** (Rust core owns crypto/keys/net/db/sync) + **React 19 + TS**.
Crypto via real libsodium. See [CRYPTO_SPEC.md](CRYPTO_SPEC.md).

Legend: ☐ todo · ◐ in progress · ☑ done

---

## Desktop adaptations (scope decisions)

- **Camera capture (CameraX) → OUT of scope.** Replaced by "Import from PC".
  All camera settings screens are dropped.
- **Auto-import from MediaStore → folder watching.** Watch user-chosen folders
  (e.g. a phone-sync dir, DCIM on a mounted device) instead of Android MediaStore.
- **Biometric unlock → OS authenticator** (Windows Hello / Touch ID) optional;
  password app-lock is the baseline.
- **WorkManager / JobScheduler / BootReceiver → Tauri autostart + background
  tokio tasks + tray.** Periodic sync via an in-process scheduler.
- **Push (FCM) → not required** (desktop polls via periodic sync).
- **Play Billing → not available;** Storage/plan view links to web (Stripe)
  billing in the browser. `billing/info` is still shown read-only.
- **Notifications → OS/tray notifications.**
- **New desktop-only feature: Takeout** — decrypt the entire library to a
  chosen plaintext folder (mirrors "Save to device" but for everything).

---

## Architecture (target)

```
crates/
  stingle-crypto/   ☑ libsodium core: pwhash, key bundle, .sp, albums, mnemonic
  stingle-api/      ☐ api.stingle.org v2 client (typed endpoints, param encryption)
  stingle-db/       ☐ SQLite mirror of the Android schema + queries
  stingle-core/     ☐ session, sync engine, import, download/cache, takeout, sharing
src-tauri/          ☐ Tauri shell: commands, tray, autostart, single-instance,
                    #   secure in-memory key handling, background scheduler
ui/                 ☐ React + Vite: auth, gallery, viewer, albums, sharing,
                    #   trash, settings, import, storage, sync status
docs/               ☑ CRYPTO_SPEC.md, ROADMAP.md
```

Protocol constants (frozen): set ids `GALLERY=0 TRASH=1 ALBUM=2`; delete-event
types `MAIN=1 TRASH=2 DELETE=3 ALBUM=4 ALBUM_FILE=5 CONTACT=6`; `API_VERSION=2`;
`.sp` ext, 32-char random filenames.

---

## Phase 0 — Crypto core ☑ (DONE)

- ☑ `stingle-crypto`: Argon2id (2/3/4), keypair, secretbox private-key storage,
  `SPK` key bundle, `.sp` sealed header + chunked XChaCha20-Poly1305, albums,
  server `crypto_box_easy` params, BIP39 mnemonic.
- ☑ Tests: BIP39 KATs + round-trips (12 passing).
- ☐ **Android-sourced fixtures** for Argon2 / `.sp` / key-bundle KATs (the last
  gate). Documented in CRYPTO_SPEC.md.

---

## Phase 1 — `stingle-api`: server client

Reference: `Net/HttpsClient.java`, `Net/StingleResponse.java`, `AsyncTasks/*`,
`Sync/SyncManager.java`, `res/values/config.xml`, `Auth/KeyManagement.java`.

- ☐ HTTP client (`reqwest`): form-urlencoded POST, multipart upload, raw download.
  - User-Agent `Stingle Photos HTTP Client <version>`; read 60s / connect 15s.
  - Base URL `https://api.stingle.org/`, paths joined as `v2/...` per config.xml.
- ☐ `StingleResponse` envelope: `status` ("ok"/"nok"), `parts`, `infos`,
  `errors`, `logout`. Surface `logout=1` → session-expired signal.
- ☐ Param encryption helper: JSON → `crypto_box_easy` → base64 in `params` field
  (uses `stingle-crypto::keys::encrypt_params_for_server`).
- ☐ Typed endpoints (all 35), grouped:
  - **Auth:** `login/preLogin`, `login/login`, `register/createAccount`,
    `login/logout`, `login/changePass`, `login/changeEmail`, `login/deleteUser`,
    `login/recoverAccount`, `login/checkKey`.
  - **Keys:** `keys/uploadKeyBundle`, `keys/getServerPK`, `keys/reuploadKeys`.
  - **Sync:** `sync/getUpdates`, `sync/upload` (multipart), `sync/download`,
    `sync/getDownloadUrls`, `sync/moveFile`, `sync/trash`, `sync/restore`,
    `sync/delete`, `sync/emptyTrash`.
  - **Albums:** `sync/addAlbum`, `sync/deleteAlbum`, `sync/renameAlbum`,
    `sync/changeAlbumCover`, `sync/share`, `sync/unshareAlbum`, `sync/editPerms`,
    `sync/removeAlbumMember`, `sync/leaveAlbum`.
  - **Contacts:** `sync/getContact`.
  - **Billing (read/redirect):** `billing/info`, `billing/downgrade`,
    `billing/stripe` (web), `billing/purchase` (n/a on desktop).
- ☐ Typed request/response structs for `getUpdates` (files, trash, albums,
  albumFiles, contacts, deletes, spaceUsed, spaceQuota) and login.
- ☐ Integration tests against a disposable real account (gated behind an env var
  so CI without creds skips them).

---

## Phase 2 — `stingle-db`: local store

Reference: `Db/StingleDbContract.java`, `Db/Objects/*`, `Db/Query/*`.

- ☐ SQLite schema (rusqlite, bundled), tables + indexes matching Android:
  - `files`, `trash` (filename UNIQUE, is_local, is_remote, version, reupload,
    date_created, date_modified, headers; idx on filename + (is_local,is_remote)).
  - `albums` (album_id UNIQUE, album_sk, album_pk, metadata, is_shared,
    is_hidden, is_owner, members, permissions, sync_local, is_locked, cover,
    date_created, date_modified).
  - `album_files` (UNIQUE (album_id, filename), …, headers, dates).
  - `contacts` (user_id UNIQUE, email, pk, date_used, date_modified).
  - `imported_ids` (media_id/source-path UNIQUE) — desktop: hash of source path.
- ☐ Migrations + schema version.
- ☐ Query layer mirroring `FilesDb`/`AlbumsDb`/`AlbumFilesDb`/`ContactsDb`:
  list by set with sort/limit/paging, by-date grouping, counts, get-by-filename,
  insert/update, `mark_remote`, `mark_reuploaded`, reupload list, position lookup.
- ☐ A "settings/kv" table for the per-set sync timestamps + cached space.

---

## Phase 3 — `stingle-core`: session, keys, sync engine

Reference: `Auth/LoginManager.java`, `Auth/KeyManagement.java`,
`Sync/SyncManager.java`, `Sync/SyncAsyncTask.java`, `Sync/SyncSteps/*`,
`Files/FileManager.java`.

### 3a. Session & key lifecycle
- ☐ Login flow: preLogin→hash→login→store token/userId/serverPK/homeFolder/
  isKeyBackedUp/addons; parse `keyBundle`; unlock private key with password.
- ☐ In-memory key holding (zeroized), app-lock with timeout, lock/unlock,
  "stay unlocked" option (encrypted-at-rest key via OS keychain), logout
  (local + `login/logout`, wipe local state).
- ☐ Registration (createAccount) incl. `isBackup`, upload key bundle.
- ☐ Account recovery (mnemonic → private key → `recoverAccount` with encrypted
  params), `checkKey` challenge, change password (reencrypt + `reuploadKeys`/
  `changePass`), change email, delete account.
- ☐ Backup-phrase reveal/verify; `is_key_backed_up` tracking.
- ☐ On-disk layout (`FileManager` equivalent): per-server/home-folder dirs for
  encrypted originals, encrypted thumbs, decrypted caches, temp/share, key files.

### 3b. Sync engine (mirror `SyncAsyncTask` modes & steps)
- ☐ Modes: FULL, IMPORT_AND_UPLOAD, CLOUD_TO_LOCAL, CLOUD_TO_LOCAL_AND_UPLOAD.
- ☐ Step **FSSync** (first run): reconcile disk `.sp` files ↔ DB (is_local).
- ☐ Step **ImportMedia** (desktop: watched folders) — see Phase 5.
- ☐ Step **SyncCloudToLocalDb**: `getUpdates` with the six per-set timestamps;
  process files/trash/albums/albumFiles/contacts/deletes in order; advance each
  `lastSeen`; cache space; handle `logout`. Delete-event type logic (1–6) with
  server-date-vs-local precedence. Thumbnail fetch queue.
- ☐ Step **UploadToCloud**: gate on backup-enabled, quota, (optional) network,
  reupload list; multipart upload (token,set,albumId,version,dateCreated,
  dateModified,headers + file + thumb @ `application/stinglephoto`); mark remote;
  suspend on out-of-space.
- ☐ Step **DownloadThumbs**: bulk thumbnail backfill (once); parallel downloader.
- ☐ Sync status states (idle/refreshing/uploading/no-space/disabled/importing),
  surfaced to UI; periodic scheduler; restart-after-finish coalescing.
- ☐ Conflict/version handling (remote version > local → re-download), reupload.

### 3c. File operations
- ☐ Move/copy between sets & albums (re-seal headers as needed), trash, restore,
  delete, empty trash — all calling the matching API + updating the DB.
- ☐ Download + decrypt on demand (full file & thumb); decrypt cache mgmt.
- ☐ **Save to device** (single/multi decrypt+export) and **Takeout** (whole
  library → plaintext folder, preserving album structure + dates).

---

## Phase 4 — `src-tauri`: shell

- ☐ Tauri 2 app, window, single-instance, **tray icon** (open/sync now/lock/
  quit), **autostart** (load with system), updater, dialog/fs for pickers.
- ☐ Secure command bridge: keys & plaintext never cross into the WebView; only
  decrypted thumbnails/images stream to the UI via protocol handler.
- ☐ Background scheduler driving the sync engine; OS notifications for progress.
- ☐ `#[tauri::command]`s for every UI action (auth, gallery paging, file ops,
  albums, sharing, trash, import, settings, storage, takeout, sync control).
- ☐ App-lock overlay + screenshot-block equivalent where feasible.

---

## Phase 5 — UI (React) — full feature parity

Reference: the Android screen inventory. Each maps to a desktop view/route.

### 5a. Auth & onboarding
- ☐ Welcome/onboarding, custom-server option.
- ☐ Login (email/password), Sign Up (with advanced: server URL, backup-key info).
- ☐ Forgot password / enter recovery phrase → set new password.
- ☐ Backup-phrase reveal (gated by password, screenshot-blocked), copy.
- ☐ App-lock / unlock screen (password; optional OS biometric).

### 5b. Gallery
- ☐ Virtualized grid, **date-grouped** headers, adjustable column/zoom.
- ☐ Selection mode (click + shift/ctrl + drag-select), selection action bar.
- ☐ Actions: share, add-to-album, save-to-device, move-to-trash, download.
- ☐ Fast scrollbar with date tooltip; empty state; pull/refresh; sync status bar.

### 5c. Viewer
- ☐ Full-screen photo viewer: zoom/pan, double-click zoom, prev/next, info panel.
- ☐ **Video playback with streaming chunked decrypt** (seek by chunk).
- ☐ Toolbar: share, add-to-album, save, trash, info, set-as-album-cover,
  download; permission-aware disabling (no-copy albums).

### 5d. Albums & sharing
- ☐ Albums view (grid + shared list), create/rename/delete, set cover, leave.
- ☐ Add-to-album selector; hidden/shared distinction; album info & settings.
- ☐ Sharing flow (step 1 recipients via `getContact`/contacts; step 2 name +
  permissions allowAdd/allowShare/allowCopy); members management (add/remove);
  edit permissions; unshare. Re-seal album SK to each recipient PK.

### 5e. Trash
- ☐ Trash view, restore, permanently delete, empty trash, confirmations.

### 5f. Import
- ☐ Import from PC: file/folder picker, **bulk** import with progress.
- ☐ Watched-folder auto-import setup (source folders, delete-after-import
  ask/yes/no, preserve original dates). Dedup via `imported_ids`.

### 5g. Settings (parity with Android preference screens)
- ☐ **Account:** change email, change password, backup key toggle, delete account.
- ☐ **Security:** lock timeout, OS-biometric toggle, "skip auth/stay unlocked",
  block-screenshots.
- ☐ **Sync:** enable backup, (network: wifi/metered) policy, battery/AC policy
  (desktop: "only on AC"), resync now.
- ☐ **Import:** enable auto-import, watched folders, delete-after, preserve dates.
- ☐ **Appearance:** theme (light/dark/auto), language/locale, grid density.
- ☐ **Advanced:** preserve import dates, resync DB, cache size, download missing
  thumbs, clear cache.

### 5h. Storage / billing
- ☐ Storage dashboard (used/quota from `billing/info`/getUpdates), plan list,
  open web (Stripe) checkout in browser; downgrade.

---

## Phase 6 — Hardening, packaging, release

- ☐ Security review vs whitepaper: key zeroization, at-rest key protection,
  screenshot/lock behavior, no secrets in WebView/logs, TLS pinning option.
- ☐ Cross-platform QA (Win/mac/Linux): import large libraries, full sync, video
  streaming, takeout byte-equality, account recovery.
- ☐ Signed installers per OS + auto-update channel.
- ☐ Localization scaffolding; accessibility pass.

---

## End-to-end verification per phase

- Crypto: Android-produced `.sp`/bundle/mnemonic decode in Rust (and vice-versa).
- API: register→login→upload→getUpdates→download→decrypt→trash on a real test
  account.
- Sync: import a folder, full sync, verify server & local converge; kill mid-sync
  and resume.
- App: `cargo tauri dev`, log into a real account, browse, import, sync, share,
  takeout; confirm exported bytes equal originals.
