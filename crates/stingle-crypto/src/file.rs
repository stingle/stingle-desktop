//! The Stingle `.sp` file format: sealed header + chunked XChaCha20-Poly1305 data.
//!
//! Layout (all integers big-endian):
//! ```text
//!   "SP" (2) | file_version (1) | file_id (32) | enc_header_len (4) | sealed_header
//!   then repeated: nonce (24) | aead_ciphertext (chunk_plaintext + 16)
//! ```
//! The sealed header is `crypto_box_seal`ed to the recipient's public key and,
//! once opened, contains:
//! ```text
//!   header_version (1) | chunk_size (4) | data_size (8) | symmetric_key (32)
//!   | file_type (1) | filename_len (4) | filename (n) | video_duration (4)
//! ```

use std::io::{Read, Seek, SeekFrom, Write};

use zeroize::Zeroizing;

use crate::constants::*;
use crate::error::{CryptoError, Result};
use crate::sodium;

/// Decrypted file header.
#[derive(Clone)]
pub struct FileHeader {
    pub file_version: u8,
    pub file_id: Vec<u8>, // 32
    pub header_version: u8,
    pub chunk_size: u32,
    pub data_size: u64,
    pub symmetric_key: Zeroizing<Vec<u8>>, // 32 (KDF master key)
    pub file_type: u8,
    pub filename: String,
    pub video_duration: u32,
}

impl FileHeader {
    /// Build a new header for an about-to-be-encrypted file with the default
    /// chunk size and a freshly generated symmetric (KDF master) key.
    pub fn new(
        symmetric_key: Zeroizing<Vec<u8>>,
        data_size: u64,
        filename: &str,
        file_type: u8,
        file_id: Vec<u8>,
        video_duration: u32,
    ) -> Result<Self> {
        if symmetric_key.len() != KDF_KEYBYTES {
            return Err(CryptoError::InvalidInput("symmetric key must be 32 bytes".into()));
        }
        if file_id.len() != FILE_FILE_ID_LEN {
            return Err(CryptoError::InvalidInput("file id must be 32 bytes".into()));
        }
        Ok(Self {
            file_version: CURRENT_FILE_VERSION,
            file_id,
            header_version: CURRENT_HEADER_VERSION,
            chunk_size: DEFAULT_CHUNK_SIZE,
            data_size,
            symmetric_key,
            file_type,
            filename: filename.to_string(),
            video_duration,
        })
    }

    /// Serialize the inner (pre-seal) header bytes.
    fn inner_bytes(&self) -> Vec<u8> {
        let mut h = Vec::new();
        h.push(self.header_version);
        h.extend_from_slice(&self.chunk_size.to_be_bytes());
        h.extend_from_slice(&self.data_size.to_be_bytes());
        h.extend_from_slice(&self.symmetric_key);
        h.push(self.file_type);
        if self.filename.is_empty() {
            h.extend_from_slice(&0u32.to_be_bytes());
        } else {
            let fname = self.filename.as_bytes();
            h.extend_from_slice(&(fname.len() as u32).to_be_bytes());
            h.extend_from_slice(fname);
        }
        h.extend_from_slice(&self.video_duration.to_be_bytes());
        h
    }

    /// Serialize the full outer header bytes (`SP` … sealed header), sealed to
    /// `recipient_pk`. This is exactly the byte string stored in the DB
    /// `headers` field (before base64url encoding).
    pub fn serialize(&self, recipient_pk: &[u8]) -> Result<Vec<u8>> {
        let sealed = sodium::box_seal(&self.inner_bytes(), recipient_pk)?;
        let mut out = Vec::with_capacity(FILE_HEADER_BEGINNING_LEN + sealed.len());
        out.extend_from_slice(FILE_BEGINNING);
        out.push(CURRENT_FILE_VERSION);
        out.extend_from_slice(&self.file_id);
        out.extend_from_slice(&(sealed.len() as u32).to_be_bytes());
        out.extend_from_slice(&sealed);
        Ok(out)
    }
}

/// Outer header byte length before the sealed blob: 2 + 1 + 32 + 4 = 39.
pub const FILE_HEADER_BEGINNING_LEN: usize =
    FILE_BEGINNING.len() + 1 + FILE_FILE_ID_LEN + FILE_HEADER_SIZE_LEN;

/// Parse and decrypt a file header from a reader, leaving the reader positioned
/// at the first data chunk. `pk`/`sk` are the keypair the header was sealed to
/// (user keypair for gallery/trash, album keypair for album files).
pub fn read_header<R: Read>(reader: &mut R, pk: &[u8], sk: &[u8]) -> Result<FileHeader> {
    let mut beginning = [0u8; 2];
    reader.read_exact(&mut beginning)?;
    if &beginning != FILE_BEGINNING {
        return Err(CryptoError::MalformedFile("invalid file header, not an SP file"));
    }

    let mut ver = [0u8; 1];
    reader.read_exact(&mut ver)?;
    if ver[0] != CURRENT_FILE_VERSION {
        return Err(CryptoError::UnsupportedVersion(format!("file version {}", ver[0])));
    }

    let mut file_id = vec![0u8; FILE_FILE_ID_LEN];
    reader.read_exact(&mut file_id)?;

    let mut header_size_bytes = [0u8; FILE_HEADER_SIZE_LEN];
    reader.read_exact(&mut header_size_bytes)?;
    let header_size = u32::from_be_bytes(header_size_bytes) as usize;
    if header_size < 1 || header_size > MAX_BUFFER_LENGTH {
        return Err(CryptoError::MalformedFile("invalid header size"));
    }

    let mut enc_header = vec![0u8; header_size];
    reader.read_exact(&mut enc_header)?;

    let inner = sodium::box_seal_open(&enc_header, pk, sk)?;
    parse_inner_header(file_id, ver[0], &inner)
}

fn parse_inner_header(file_id: Vec<u8>, file_version: u8, inner: &[u8]) -> Result<FileHeader> {
    let mut c = std::io::Cursor::new(inner);

    let header_version = read_u8(&mut c)?;
    if header_version != CURRENT_HEADER_VERSION {
        return Err(CryptoError::UnsupportedVersion(format!(
            "header version {header_version}"
        )));
    }
    let chunk_size = read_u32_be(&mut c)?;
    if chunk_size < 1 || chunk_size as usize > MAX_BUFFER_LENGTH {
        return Err(CryptoError::MalformedFile("invalid chunk size"));
    }
    let data_size = read_u64_be(&mut c)?;

    let mut symmetric_key = Zeroizing::new(vec![0u8; KDF_KEYBYTES]);
    c.read_exact(&mut symmetric_key)?;

    let file_type = read_u8(&mut c)?;

    let filename_len = read_u32_be(&mut c)? as usize;
    let filename = if filename_len > 0 {
        if filename_len > MAX_BUFFER_LENGTH {
            return Err(CryptoError::MalformedFile("invalid filename length"));
        }
        let mut fbytes = vec![0u8; filename_len];
        c.read_exact(&mut fbytes)?;
        String::from_utf8_lossy(&fbytes).into_owned()
    } else {
        String::new()
    };

    let video_duration = read_u32_be(&mut c)?;

    Ok(FileHeader {
        file_version,
        file_id,
        header_version,
        chunk_size,
        data_size,
        symmetric_key,
        file_type,
        filename,
        video_duration,
    })
}

/// Read the full outer header bytes (without decrypting) from a reader. This is
/// what gets stored in the DB `headers` field and re-sealed for sharing.
pub fn extract_header_bytes<R: Read>(reader: &mut R) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut beginning = [0u8; 2];
    reader.read_exact(&mut beginning)?;
    if &beginning != FILE_BEGINNING {
        return Err(CryptoError::MalformedFile("invalid file header, not an SP file"));
    }
    out.extend_from_slice(&beginning);
    let mut ver = [0u8; 1];
    reader.read_exact(&mut ver)?;
    out.extend_from_slice(&ver);
    let mut file_id = vec![0u8; FILE_FILE_ID_LEN];
    reader.read_exact(&mut file_id)?;
    out.extend_from_slice(&file_id);
    let mut hs = [0u8; FILE_HEADER_SIZE_LEN];
    reader.read_exact(&mut hs)?;
    out.extend_from_slice(&hs);
    let header_size = u32::from_be_bytes(hs) as usize;
    if header_size < 1 || header_size > MAX_BUFFER_LENGTH {
        return Err(CryptoError::MalformedFile("invalid header size"));
    }
    let mut enc_header = vec![0u8; header_size];
    reader.read_exact(&mut enc_header)?;
    out.extend_from_slice(&enc_header);
    Ok(out)
}

/// Encrypt a stream into the `.sp` format, sealing the header to `recipient_pk`.
/// Returns the generated header (so the caller can store the `headers` string).
pub fn encrypt_stream<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    filename: &str,
    file_type: u8,
    data_size: u64,
    file_id: Vec<u8>,
    video_duration: u32,
    recipient_pk: &[u8],
) -> Result<FileHeader> {
    let symmetric_key = sodium::kdf_keygen()?;
    let header = FileHeader::new(
        symmetric_key,
        data_size,
        filename,
        file_type,
        file_id,
        video_duration,
    )?;
    writer.write_all(&header.serialize(recipient_pk)?)?;
    encrypt_data(reader, writer, &header)?;
    Ok(header)
}

fn encrypt_data<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    header: &FileHeader,
) -> Result<()> {
    let chunk_size = header.chunk_size as usize;
    let mut buf = vec![0u8; chunk_size];
    let mut chunk_number: u64 = 1;
    loop {
        let n = read_fill(reader, &mut buf)?;
        if n == 0 {
            break;
        }
        let mut nonce = vec![0u8; AEAD_NPUBBYTES];
        sodium::random_into(&mut nonce)?;
        let chunk_key = sodium::kdf_derive_from_key(
            AEAD_KEYBYTES,
            chunk_number,
            XCHACHA20POLY1305_IETF_CONTEXT,
            &header.symmetric_key,
        )?;
        let cipher = sodium::aead_encrypt(&buf[..n], &nonce, &chunk_key)?;
        writer.write_all(&nonce)?;
        writer.write_all(&cipher)?;
        chunk_number += 1;
        if n < chunk_size {
            break; // final, partial chunk
        }
    }
    writer.flush()?;
    Ok(())
}

/// Decrypt a `.sp` stream, writing plaintext to `writer`. The header is read
/// from the stream using the given keypair.
pub fn decrypt_stream<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    pk: &[u8],
    sk: &[u8],
) -> Result<FileHeader> {
    let header = read_header(reader, pk, sk)?;
    decrypt_data(reader, writer, &header)?;
    Ok(header)
}

/// Decrypt the data portion using an already-parsed header (the reader must be
/// positioned at the first data chunk).
pub fn decrypt_data<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    header: &FileHeader,
) -> Result<()> {
    let chunk_size = header.chunk_size as usize;
    if chunk_size < 1 || chunk_size > MAX_BUFFER_LENGTH {
        return Err(CryptoError::MalformedFile("invalid chunk size"));
    }
    let record_max = AEAD_NPUBBYTES + chunk_size + AEAD_ABYTES;
    let mut buf = vec![0u8; record_max];
    let mut chunk_number: u64 = 1;
    loop {
        let n = read_fill(reader, &mut buf)?;
        if n == 0 {
            break;
        }
        if n < AEAD_NPUBBYTES + AEAD_ABYTES + 1 {
            return Err(CryptoError::MalformedFile("invalid chunk length"));
        }
        let nonce = &buf[..AEAD_NPUBBYTES];
        let cipher = &buf[AEAD_NPUBBYTES..n];
        let chunk_key = sodium::kdf_derive_from_key(
            AEAD_KEYBYTES,
            chunk_number,
            XCHACHA20POLY1305_IETF_CONTEXT,
            &header.symmetric_key,
        )?;
        let plain = sodium::aead_decrypt(cipher, nonce, &chunk_key)?;
        writer.write_all(&plain)?;
        chunk_number += 1;
        if n < record_max {
            break; // final, partial chunk
        }
    }
    writer.flush()?;
    Ok(())
}

// ---- in-memory convenience wrappers (thumbnails, tests) ----

/// Encrypt a byte slice fully in memory. Returns `(sp_bytes, header)`.
pub fn encrypt_bytes(
    data: &[u8],
    filename: &str,
    file_type: u8,
    file_id: Vec<u8>,
    video_duration: u32,
    recipient_pk: &[u8],
) -> Result<(Vec<u8>, FileHeader)> {
    let mut reader = std::io::Cursor::new(data);
    let mut out = Vec::new();
    let header = encrypt_stream(
        &mut reader,
        &mut out,
        filename,
        file_type,
        data.len() as u64,
        file_id,
        video_duration,
        recipient_pk,
    )?;
    Ok((out, header))
}

/// Decrypt a full `.sp` byte slice in memory using the blob's own embedded
/// header. Correct for a file sealed directly to `pk`/`sk` (e.g. gallery files
/// you uploaded yourself).
pub fn decrypt_bytes(sp_bytes: &[u8], pk: &[u8], sk: &[u8]) -> Result<Vec<u8>> {
    let mut reader = std::io::Cursor::new(sp_bytes);
    let mut out = Vec::new();
    decrypt_stream(&mut reader, &mut out, pk, sk)?;
    Ok(out)
}

/// Decrypt a `.sp` blob using a **separately stored** outer header (the DB
/// `headers` field), as the Android client does for album/shared files: the
/// header is sealed to the album/recipient key while the blob's own embedded
/// header may be sealed to a different (owner's) key. The blob's embedded header
/// is skipped; data chunks are decrypted with the symmetric key from
/// `header_bytes`.
pub fn decrypt_with_external_header(
    header_bytes: &[u8],
    blob: &[u8],
    pk: &[u8],
    sk: &[u8],
) -> Result<Vec<u8>> {
    let header = read_header(&mut std::io::Cursor::new(header_bytes), pk, sk)?;
    let mut br = std::io::Cursor::new(blob);
    let _ = extract_header_bytes(&mut br)?; // skip the blob's embedded header
    let mut out = Vec::new();
    decrypt_data(&mut br, &mut out, &header)?;
    Ok(out)
}

/// Generate a fresh random 32-byte file id.
pub fn new_file_id() -> Result<Vec<u8>> {
    sodium::random_bytes(FILE_FILE_ID_LEN)
}

/// The total byte length of a `.sp` file's outer header (`"SP"` … sealed header),
/// read from `reader` (which is left positioned at the first data chunk).
pub fn outer_header_len<R: Read>(reader: &mut R) -> Result<u64> {
    Ok(extract_header_bytes(reader)?.len() as u64)
}

/// Decrypt a plaintext byte range `[start, end_inclusive]` from a seekable `.sp`
/// blob, decrypting only the chunks the range covers. This is what powers
/// streaming video playback (mirrors the Android `StingleDataSource` math).
///
/// `outer_header_len` is the blob's own outer header length (see
/// [`outer_header_len`]); `symmetric_key`/`chunk_size` come from the file's
/// (DB) header so it also works for album/shared files.
pub fn decrypt_range<R: Read + Seek>(
    reader: &mut R,
    outer_header_len: u64,
    symmetric_key: &[u8],
    chunk_size: u32,
    start: u64,
    end_inclusive: u64,
) -> Result<Vec<u8>> {
    if chunk_size < 1 || chunk_size as usize > MAX_BUFFER_LENGTH {
        return Err(CryptoError::MalformedFile("invalid chunk size"));
    }
    let chunk = chunk_size as u64;
    let first = start / chunk;
    let last = end_inclusive / chunk;
    let stride = AEAD_NPUBBYTES as u64 + chunk + AEAD_ABYTES as u64;

    let mut out = Vec::new();
    let mut record = vec![0u8; chunk_size as usize + AEAD_ABYTES];
    let mut nonce = vec![0u8; AEAD_NPUBBYTES];
    for ci in first..=last {
        reader.seek(SeekFrom::Start(outer_header_len + ci * stride))?;
        reader.read_exact(&mut nonce)?;
        let n = read_fill(reader, &mut record)?;
        if n < AEAD_ABYTES + 1 {
            return Err(CryptoError::MalformedFile("invalid chunk length"));
        }
        // chunk_number is 1-based (matches the encryptor / StingleDataSource).
        let chunk_key = sodium::kdf_derive_from_key(
            AEAD_KEYBYTES,
            ci + 1,
            XCHACHA20POLY1305_IETF_CONTEXT,
            symmetric_key,
        )?;
        let plain = sodium::aead_decrypt(&record[..n], &nonce, &chunk_key)?;
        out.extend_from_slice(&plain);
    }

    let base = first * chunk;
    let s = (start - base) as usize;
    let e = ((end_inclusive - base) as usize).min(out.len().saturating_sub(1));
    if out.is_empty() || s > e {
        return Ok(Vec::new());
    }
    Ok(out[s..=e].to_vec())
}

// ---- small read helpers ----

fn read_fill<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        let n = reader.read(&mut buf[total..])?;
        if n == 0 {
            break;
        }
        total += n;
    }
    Ok(total)
}

fn read_u8<R: Read>(r: &mut R) -> Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}

fn read_u32_be<R: Read>(r: &mut R) -> Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

fn read_u64_be<R: Read>(r: &mut R) -> Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_be_bytes(b))
}
