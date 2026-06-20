# Stingle Photos Desktop

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

## License

AGPL-3.0-or-later (matching the upstream Stingle Photos apps).
