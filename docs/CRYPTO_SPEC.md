# Stingle Crypto Compatibility Spec (FROZEN)

This documents the exact cryptography the desktop client must reproduce to stay
compatible with the Stingle Photos Android/web clients and `api.stingle.org`.
Source of truth: the Android app's `Crypto.java`, `CryptoHelpers.java`,
`MnemonicUtils.java`, plus the whitepaper at https://stingle.org/security/.

**Rule #1:** use the real libsodium (here via `libsodium-sys-stable`). Never a
reimplementation — a subtle Argon2/KDF difference permanently locks users out.

All integers in the `.sp`/header/metadata formats are **big-endian**.

## Argon2id (crypto_pwhash, ALG_ARGON2ID13), 16-byte salt

| Use | Difficulty | opslimit | memlimit | Output |
|---|---|---|---|---|
| Local private-key encryption key | INTERACTIVE | 2 | 64 MiB | 32 bytes |
| Login auth hash (`getPasswordHashForStorage`) | MODERATE | 3 | 256 MiB | **64 bytes → UPPER-case hex** |
| Key-bundle export key (`HARD`) | MODERATE | 3 | 256 MiB | 32 bytes |
| Recovery import key (`ULTRA`) | SENSITIVE | 4 | 1 GiB | 32 bytes |

> The opslimits are libsodium's standard INTERACTIVE/MODERATE/SENSITIVE values
> (2/3/4), **not** 4/6/8. The login hash is **upper-case** hex — the server
> hashes the received string verbatim, so case must match.

## Identity keys

- Keypair: Curve25519 `crypto_box_keypair` (32-byte sk/pk).
- Private key stored locally: `crypto_secretbox_easy` (XSalsa20-Poly1305),
  24-byte nonce, key = INTERACTIVE-derived password key. Ciphertext = sk + 16.
- Public key from secret: `crypto_scalarmult_base`.

## Key bundle (`SPK`) — 125 bytes

```
"SPK"(3) | version(1)=1 | type(1)=0 | publicKey(32)
        | encPrivateKey(48 = 32+secretbox MAC, encrypted with HARD/MODERATE key + skNonce)
        | pwdSalt(16) | skNonce(24)
```
Uploaded base64 (standard alphabet) to `register/createAccount` and
`keys/uploadKeyBundle`. To unlock: derive MODERATE key from password+pwdSalt,
`secretbox_open` encPrivateKey with skNonce.

## `.sp` file format

```
"SP"(2) | fileVersion(1)=1 | fileId(32) | encHeaderLen(4) | sealedHeader
then repeated:  nonce(24) | aeadCiphertext(chunkPlaintext + 16)
```

Sealed header = `crypto_box_seal` (anonymous, +48) to the recipient public key
(user keypair for gallery/trash; album keypair for album files). Opened header:

```
headerVersion(1)=1 | chunkSize(4)=1048576 | dataSize(8) | symmetricKey(32)
| fileType(1) [1=general,2=photo,3=video] | filenameLen(4) | filename(n) | videoDuration(4)
```

Data chunks: per chunk, derive a key with
`crypto_kdf_derive_from_key(subkey_len=32, subkey_id=chunkNumber starting at 1,
context="__data__", master=symmetricKey)` (BLAKE2b), then
`crypto_aead_xchacha20poly1305_ietf_encrypt` with a fresh random 24-byte nonce.
Each record on disk is `nonce(24) || ciphertext(plaintext+16)`. The final chunk
is the only short one; an exact-multiple file writes no trailing empty chunk; a
0-byte file writes no chunks.

The DB `headers` field is `base64url_nopad(fileOuterHeader) + "*" +
base64url_nopad(thumbOuterHeader)` where each outer header is the bytes from
`"SP"` through the end of the sealed header (see `file::extract_header_bytes`).

**An item's file and its thumbnail MUST share the same 32-byte fileId.** The
server reads the fileId from the plaintext outer header of both the uploaded
`file` and `thumb` parts and rejects the upload ("This thumb is not for this
file") if they differ. (Verified live.) Each is otherwise encrypted
independently, with its own symmetric key.

## Albums

- Album keypair: Curve25519. Album sk sealed (`crypto_box_seal`) to the user PK.
- Metadata sealed to the album PK: `version(1)=1 | nameLen(4) | name`.
- `addAlbum` sends `publicKey`, `encPrivateKey`, `metadata` as **standard**
  base64.
- Sharing re-seals the album sk to each recipient's PK (files are never
  re-encrypted).

## Server parameter encryption (`CryptoHelpers.encryptParamsForServer`)

JSON-encode the params object, then `crypto_box_easy` from the user's secret key
to the server's public key with a random 24-byte nonce; transmit
`base64_standard(nonce(24) || cipher(json+16))` in the `params` field.

## Mnemonic (recovery phrase)

BIP39 **entropy encoding** (not seed derivation): the 2048-word English
wordlist (`crates/stingle-crypto/src/wordlist.txt`, identical bytes to Android's
`res/raw/dictionary.txt`). The words encode the raw private key as entropy +
SHA-256 checksum (`checksum_byte = sha256(entropy)[0] & (0xff << (8 - bits/32))`,
top `bits/32` bits appended). A 32-byte private key → **24 words**; decoding
returns the private key verbatim. Anchored by the canonical BIP39 KATs in
`tests/vectors.rs`.

## Cross-client fixture tests (TODO)

`tests/vectors.rs` anchors the mnemonic to external BIP39 truth. To likewise
anchor Argon2id / key bundle / `.sp` decode against the **Android** output,
drop fixtures generated from the Android app into `tests/fixtures/` (a `.sp`
file + its keypair, an exported key bundle + password, a known
password/salt→hash triple) and assert this crate reproduces/decrypts them.
This is the final gate before building higher layers on top.
