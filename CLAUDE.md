# Stingle Photos Desktop — Project Guide

Cross-platform desktop client for Stingle Photos (E2E-encrypted photo backup),
fully compatible with `api.stingle.org` and the Android `.sp` format.
Stack: Tauri 2 (Rust core) + React/TS. See `docs/ROADMAP.md` and `docs/CRYPTO_SPEC.md`.

## 🔒 SECURITY RULES (non-negotiable)

1. **Never write unencrypted/decrypted data to disk.** All on-disk caches store
   ONLY encrypted data:
   - Encrypted originals → `originals/`, encrypted thumbnails → `thumbs/` (the
     downloaded/imported `.sp` blobs). These ARE the local cache; full images and
     videos are cached here (encrypted) after first download.
   - Decryption happens in memory only; decrypted bytes are streamed to the
     webview via the `stingle://` protocol with `Cache-Control: no-store` so the
     webview never persists them to its on-disk HTTP cache.
   - ffmpeg (HEIC transcode, video frame-grab) is driven via stdin/stdout pipes —
     never temp files. The only path ffmpeg reads is the user's own local file
     during import.
   - The **only** exception is **Takeout** / an explicit "decrypt & save" action
     the user initiates, which writes plaintext to a user-chosen folder.

2. **Crypto must stay byte-compatible** with the Android client / server. Use the
   real libsodium (`libsodium-sys-stable`), never a reimplementation. Frozen
   parameters are in `docs/CRYPTO_SPEC.md`.

## Build / run

```
cargo test                         # all crates
cargo run -p stingle-core --example verify_real   # live media check (test acct)
cd app && npm run tauri dev        # run the desktop app
```
`cargo build -p app` needs the frontend built first (`npm --prefix app run build`).

## Layout
- `crates/stingle-crypto` — libsodium core (pwhash, key bundle, `.sp`, albums, mnemonic)
- `crates/stingle-api` — api.stingle.org v2 client
- `crates/stingle-db` — SQLite mirror of the Android schema
- `crates/stingle-core` — session, sync, import, takeout, media serving, sharing
- `app/src-tauri` — Tauri shell (commands, `stingle://` protocol, tray)
- `app/src` — React UI
