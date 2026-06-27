# Stingle Desktop

A cross-platform (Windows/macOS/Linux) desktop client for
[Stingle Photos](https://stingle.org), the end-to-end encrypted photo
gallery/backup app. Fully compatible with the existing `api.stingle.org` server
and the `.sp` file format, replicating the Android client's cryptography exactly.

## Stack

- **Tauri 2** shell — a Rust backend owns all crypto, keys, networking, sync and
  the local DB; the system WebView renders the UI. Keys never enter the WebView.
- **React 19 + TypeScript + Vite** frontend.
- **libsodium** (via `libsodium-sys-stable`) for all cryptography — the real C
  library, never a reimplementation. See [docs/CRYPTO_SPEC.md](docs/CRYPTO_SPEC.md).

## Workspace

```
crates/
  stingle-crypto/   # libsodium-based crypto core: pwhash, key bundle, .sp format,
                    #   albums, mnemonic. Pure & deterministic. (implemented)
  stingle-api/      # api.stingle.org v2 client                 (planned)
  stingle-db/       # local SQLite mirror of the Android schema (planned)
  stingle-core/     # sync / import / takeout orchestration     (planned)
src-tauri/          # Tauri shell                               (planned)
ui/                 # React + Vite frontend                     (planned)
docs/CRYPTO_SPEC.md # frozen crypto compatibility spec
```

## Develop

Prereqs: Rust (stable, MSVC on Windows) and a C toolchain (for building
libsodium). Node 20+ for the UI (added later).

```sh
cargo build                      # build all crates
cargo test -p stingle-crypto     # run the crypto compatibility/round-trip tests
```

## Releases & auto-update

The app self-updates via the Tauri updater plugin. Signed bundles and the
`latest.json` manifest are published to **GitHub Releases** by
[`.github/workflows/release.yml`](.github/workflows/release.yml), which builds
all three platforms with `tauri-action`. The client checks
`releases/latest/download/latest.json` on launch; only bundles whose minisign
signature matches the public key in `app/src-tauri/tauri.conf.json` are installed.

**Behavior:** with auto-update on (default) a new version downloads silently in
the background and is applied on the next launch. With it off, the app shows a
one-click "Update" banner in the sidebar instead. Both are surfaced under
Settings → General / About.

### Cutting a release

1. Bump `version` in **both** `app/src-tauri/tauri.conf.json` and
   `app/src-tauri/Cargo.toml` (the updater compares semver).
2. Commit, then `git tag vX.Y.Z && git push --tags`.
3. CI builds + signs all platforms and creates a **draft** release with the
   bundles + `latest.json`. Publish the draft to make it live.

### Signing key (one-time setup)

The updater keypair lives in `app/src-tauri/stingle-updater.key{,.pub}`. The
`.pub` is committed (it's in `tauri.conf.json`); the **private** key is
git-ignored and must be added to the repo's GitHub Actions secrets:

- `TAURI_SIGNING_PRIVATE_KEY` — contents of `stingle-updater.key`.
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` — empty (the key has no passphrase).

Treat the private key like the crypto material in `docs/CRYPTO_SPEC.md`: if it
leaks, rotate `pubkey` in a new release. Regenerate with
`npm --prefix app run tauri signer generate`.

### Free-route signing caveats

No paid OS code-signing certs are used, so on first install:

- **Windows:** SmartScreen shows an "unknown publisher" warning (uses the NSIS
  installer). The in-app updater still works — it's minisign-verified.
- **macOS:** Gatekeeper blocks double-click; right-click → **Open** once.
- **Linux:** the updater applies to the **AppImage** target only; `.deb`/`.rpm`
  are managed by the system package manager.

## License

AGPL-3.0-or-later (matching the upstream Stingle Photos apps).
