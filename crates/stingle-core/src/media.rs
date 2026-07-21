//! Media serving for the UI: decrypt thumbnails/originals, with HTTP Range
//! support for streaming video (decrypts only the requested chunks).

use std::io::Cursor;
use std::path::{Path, PathBuf};

use stingle_crypto::file;
use stingle_db::FileSet;
use zeroize::Zeroizing;

use crate::account::Account;
use crate::error::{CoreError, Result};
use crate::sync::headers_part;

/// Max bytes returned for a single open-ended/large range request, so video
/// playback streams in pieces instead of decrypting the whole file up front.
const MAX_RANGE_BYTES: u64 = 4 * 1024 * 1024;

pub struct MediaResponse {
    pub content_type: String,
    /// Full plaintext size of the media.
    pub total_size: u64,
    pub body: Vec<u8>,
    /// `Some((start, end_inclusive))` for a partial (206) response.
    pub range: Option<(u64, u64)>,
}

/// Metadata decoded from a file's (already-local) `.sp` header — no blob read,
/// no network. Returned by [`Account::row_header_meta`] to populate the virtual
/// filesystem's directory listings and `stat` responses.
#[derive(Debug, Clone)]
pub struct HeaderMeta {
    /// Original filename stored in the sealed header (may be empty).
    pub original_filename: String,
    /// Plaintext size of the media in bytes (what a decrypt yields).
    pub data_size: u64,
    pub is_video: bool,
}

/// A prepared handle for streaming decrypted byte ranges of one media file
/// without repeating the per-read setup (DB lookup, header seal-open, blob
/// download, outer-header scan). Built once by [`Account::open_media_stream`];
/// the virtual filesystem caches it per file so windowed reads stay cheap.
pub struct MediaStream {
    /// Local encrypted blob (already ensured present).
    path: PathBuf,
    symmetric_key: Zeroizing<Vec<u8>>,
    chunk_size: u32,
    outer_header_len: u64,
    /// Full plaintext size in bytes.
    pub data_size: u64,
}

impl MediaStream {
    /// Decrypt the inclusive byte range `[start, end]` from the local encrypted
    /// blob, in memory (nothing persisted). Clamps to the file size and returns
    /// empty at/after EOF. Cheap and repeatable — just a file open plus
    /// `decrypt_range` over the already-cached blob.
    pub fn read(&self, start: u64, end: u64) -> Result<Vec<u8>> {
        if self.data_size == 0 || start >= self.data_size {
            return Ok(Vec::new());
        }
        let end = end.min(self.data_size - 1);
        if end < start {
            return Ok(Vec::new());
        }
        let mut f = std::fs::File::open(&self.path)?;
        Ok(file::decrypt_range(
            &mut f,
            self.outer_header_len,
            &self.symmetric_key,
            self.chunk_size,
            start,
            end,
        )?)
    }
}

/// Image types the webview (Chromium/WebView2) can't natively render, so we
/// transcode them to JPEG for full-screen preview.
fn needs_transcode(content_type: &str) -> bool {
    matches!(content_type, "image/heic" | "image/heif" | "image/tiff")
}

/// Detect a decrypted image's real format from its magic bytes. Returns the
/// content type to serve and, for formats the webview can't render, the ffmpeg
/// input extension to transcode from.
///
/// Thumbnails are *usually* JPEG, but some clients store PNG/HEIC/TIFF/… ones.
/// The webview sniffs the web formats (JPEG/PNG/GIF/WebP/BMP/AVIF) from the bytes
/// regardless of the declared content type, so those only need a correct label;
/// HEIC/HEIF/TIFF it cannot decode, so they must be transcoded to JPEG — the same
/// treatment full-resolution images already get.
fn sniff_image(body: &[u8]) -> (&'static str, Option<&'static str>) {
    let starts = |sig: &[u8]| body.len() >= sig.len() && &body[..sig.len()] == sig;
    if starts(&[0xff, 0xd8, 0xff]) {
        ("image/jpeg", None)
    } else if starts(b"\x89PNG\r\n\x1a\n") {
        ("image/png", None)
    } else if starts(b"GIF8") {
        ("image/gif", None)
    } else if starts(b"RIFF") && body.len() >= 12 && &body[8..12] == b"WEBP" {
        ("image/webp", None)
    } else if starts(b"BM") {
        ("image/bmp", None)
    } else if body.len() >= 12 && &body[4..8] == b"ftyp" {
        // ISO-BMFF container: AVIF renders in the webview; HEIC/HEIF do not.
        match &body[8..12] {
            b"avif" | b"avis" => ("image/avif", None),
            _ => ("image/heic", Some("heic")),
        }
    } else if starts(&[0x49, 0x49, 0x2a, 0x00]) || starts(&[0x4d, 0x4d, 0x00, 0x2a]) {
        ("image/tiff", Some("tiff"))
    } else {
        // Unknown: label it JPEG and let the webview sniff the bytes.
        ("image/jpeg", None)
    }
}

fn mime_for(filename: &str, file_type: u8) -> String {
    let ext = Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let m = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "heic" | "heif" => "image/heic",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "3gp" => "video/3gpp",
        "wmv" => "video/x-ms-wmv",
        "" => {
            if file_type == stingle_crypto::constants::FILE_TYPE_VIDEO {
                "video/mp4"
            } else {
                "application/octet-stream"
            }
        }
        _ => "application/octet-stream",
    };
    m.to_string()
}

impl Account {
    /// Whether a file is a video, from its (already-local) header — no download.
    pub fn is_video(
        &self,
        set: FileSet,
        album_id: Option<&str>,
        filename: &str,
    ) -> Result<bool> {
        let headers = self.headers_for(set, album_id, filename)?;
        let part = headers_part(&headers, false)?;
        let kp = self.keypair_for(set, album_id)?;
        let header = file::read_header(&mut Cursor::new(&part), &kp.public_key, &kp.secret_key)?;
        Ok(header.file_type == stingle_crypto::constants::FILE_TYPE_VIDEO)
    }

    /// Decrypt a photo to raw RGBA pixels (for clipboard copy). Goes through
    /// [`media_response`] so formats the `image` crate can't read natively
    /// (HEIC/HEIF/TIFF) are first transcoded to JPEG via ffmpeg, exactly like
    /// the full-screen preview. EXIF orientation is baked into the pixels —
    /// clipboard consumers only ever see the raw raster, never the metadata.
    /// Errors for videos / undecodable media.
    pub async fn decrypt_to_rgba(
        &self,
        set: FileSet,
        album_id: Option<&str>,
        filename: &str,
    ) -> Result<(u32, u32, Vec<u8>)> {
        let resp = self.media_response(set, album_id, filename, false, None).await?;
        if resp.content_type.starts_with("video/") {
            return Err(CoreError::Other("cannot copy a video as an image".into()));
        }
        let img = crate::thumbnail::decode_with_orientation(&resp.body)
            .map_err(|err| CoreError::Other(format!("decode image: {err}")))?
            .to_rgba8();
        let (w, h) = img.dimensions();
        Ok((w, h, img.into_raw()))
    }

    /// Cheap is-video check straight from an already-loaded row's `headers`
    /// string (no extra DB query). For listing many files at once. Returns
    /// false on any decode error rather than failing the whole listing.
    pub fn row_is_video(&self, set: FileSet, album_id: Option<&str>, headers: &str) -> bool {
        self.try_row_is_video(set, album_id, headers).unwrap_or(false)
    }

    /// Like [`Self::row_is_video`] but distinguishes "couldn't decode" (`None`)
    /// from a real answer — so callers persisting the derived flag never store
    /// a guessed `false` for an undecodable header.
    pub fn try_row_is_video(
        &self,
        set: FileSet,
        album_id: Option<&str>,
        headers: &str,
    ) -> Option<bool> {
        (|| -> Result<bool> {
            let part = headers_part(headers, false)?;
            let kp = self.keypair_for(set, album_id)?;
            let header =
                file::read_header(&mut Cursor::new(&part), &kp.public_key, &kp.secret_key)?;
            Ok(header.file_type == stingle_crypto::constants::FILE_TYPE_VIDEO)
        })()
        .ok()
    }

    /// Header metadata (original filename, plaintext size, video flag) decoded
    /// from a file's stored `headers` string in a single in-memory seal-open —
    /// no blob read, no network, the same path [`Self::try_row_is_video`] uses.
    /// Powers the virtual filesystem's directory listing / `stat` without ever
    /// touching the encrypted blob. `headers` is the DB `headers` column.
    pub fn row_header_meta(
        &self,
        set: FileSet,
        album_id: Option<&str>,
        headers: &str,
    ) -> Result<HeaderMeta> {
        let part = headers_part(headers, false)?;
        let kp = self.keypair_for(set, album_id)?;
        let header = file::read_header(&mut Cursor::new(&part), &kp.public_key, &kp.secret_key)?;
        Ok(HeaderMeta {
            original_filename: header.filename,
            data_size: header.data_size,
            is_video: header.file_type == stingle_crypto::constants::FILE_TYPE_VIDEO,
        })
    }

    /// Prepare a [`MediaStream`] for a file: ensure the encrypted blob is local
    /// (downloading once if needed), decode its header, and scan the outer
    /// header length. This is the expensive per-file setup, done ONCE so
    /// subsequent range reads are just a file open + `decrypt_range`. Reuses the
    /// same on-disk encrypted cache (`originals/`) and key path as
    /// [`Self::media_response`], so already-downloaded files never re-fetch.
    pub async fn open_media_stream(
        &self,
        set: FileSet,
        album_id: Option<&str>,
        filename: &str,
    ) -> Result<MediaStream> {
        let headers = self.headers_for(set, album_id, filename)?;
        let part = headers_part(&headers, false)?;
        let kp = self.keypair_for(set, album_id)?;
        let header = file::read_header(&mut Cursor::new(&part), &kp.public_key, &kp.secret_key)?;
        let path = self.ensure_encrypted(set, filename, false).await?;
        let p = path.clone();
        let outer_header_len = tokio::task::spawn_blocking(move || -> Result<u64> {
            let mut f = std::fs::File::open(&p)?;
            Ok(file::outer_header_len(&mut f)?)
        })
        .await
        .map_err(|err| CoreError::Other(format!("outer-header task failed: {err}")))??;
        Ok(MediaStream {
            path,
            symmetric_key: header.symmetric_key,
            chunk_size: header.chunk_size,
            outer_header_len,
            data_size: header.data_size,
        })
    }

    /// Produce a decrypted media response for the UI/protocol handler.
    /// `range` is `Some((start, end_opt))` from an HTTP `Range` header.
    pub async fn media_response(
        &self,
        set: FileSet,
        album_id: Option<&str>,
        filename: &str,
        is_thumb: bool,
        range: Option<(u64, Option<u64>)>,
    ) -> Result<MediaResponse> {
        // Fast path: a decrypted thumbnail we've already served this session.
        // Skips the DB lookup, header decrypt, disk read, and blob decrypt.
        // (Thumbnails are whole-file requests; ranges only apply to video.)
        if is_thumb && range.is_none() {
            if let Some((content_type, body)) = self.thumb_cache.get(filename) {
                return Ok(MediaResponse {
                    content_type,
                    total_size: body.len() as u64,
                    body,
                    range: None,
                });
            }
        }
        // Same fast path for full-resolution images (viewer back/forward and
        // re-opens): skips the disk read, blob decrypt, and any transcode.
        if !is_thumb && range.is_none() {
            if let Some((content_type, body)) = self.media_cache.get(filename) {
                return Ok(MediaResponse {
                    content_type,
                    total_size: body.len() as u64,
                    body,
                    range: None,
                });
            }
        }

        let headers = self.headers_for(set, album_id, filename)?;
        let part = headers_part(&headers, is_thumb)?;
        let kp = self.keypair_for(set, album_id)?;
        let header = file::read_header(&mut Cursor::new(&part), &kp.public_key, &kp.secret_key)?;

        let content_type = if is_thumb {
            "image/jpeg".to_string()
        } else {
            mime_for(&header.filename, header.file_type)
        };

        let path = self.ensure_encrypted(set, filename, is_thumb).await?;

        match range {
            None => {
                // Cache miss: read the encrypted blob and decrypt it. The disk read
                // is blocking and the libsodium decrypt is CPU-bound, so run both on
                // the blocking pool (never the async workers — a slow disk or a burst
                // of misses would otherwise stall every concurrent request).
                //
                // Only *full-resolution* decrypts take a permit: they're large and may
                // transcode, so we bound how many run at once. Thumbnails are tiny AND
                // are the instant-preview layer the viewer paints under the full image,
                // so they must never queue behind a heavy full decrypt — gating them
                // would make the preview wait for the full download. The frontend
                // observer already caps how many thumbnails are in flight, so they need
                // no backend bound of their own.
                let _permit = if is_thumb {
                    None
                } else {
                    Some(
                        self.decrypt_sem
                            .acquire()
                            .await
                            .map_err(|_| CoreError::Other("decrypt semaphore closed".into()))?,
                    )
                };
                let path = path.clone();
                let part = part.clone();
                let pk = kp.public_key.clone();
                let sk = kp.secret_key.clone();
                let mut body = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
                    let blob = std::fs::read(&path)?;
                    Ok(file::decrypt_with_external_header(&part, &blob, &pk, &sk)?)
                })
                .await
                .map_err(|e| CoreError::Other(format!("decrypt task failed: {e}")))??;
                let mut ct = content_type;
                if is_thumb {
                    // A thumbnail's declared type was assumed JPEG, but the actual
                    // bytes may be PNG/HEIC/TIFF/… Serve the real type; transcode
                    // the ones the webview can't render (HEIC/HEIF/TIFF) to JPEG.
                    let (sniffed_ct, transcode_ext) = sniff_image(&body);
                    match transcode_ext {
                        Some(ext) => match crate::thumbnail::transcode_to_jpeg(&body, ext) {
                            Ok(jpg) => {
                                body = jpg;
                                ct = "image/jpeg".to_string();
                            }
                            // Transcode failed: send the real type so the webview
                            // can at least try, rather than mislabelling it JPEG.
                            Err(_) => ct = sniffed_ct.to_string(),
                        },
                        None => ct = sniffed_ct.to_string(),
                    }
                } else if needs_transcode(&ct) {
                    // Transcode formats the webview can't display for full preview.
                    let ext = Path::new(&header.filename)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_lowercase();
                    if let Ok(jpg) = crate::thumbnail::transcode_to_jpeg(&body, &ext) {
                        body = jpg;
                        ct = "image/jpeg".to_string();
                    }
                }
                // Keep decrypted thumbnails / full images in memory for instant
                // re-display. Videos are excluded: they stream via ranges and a
                // single body could blow the whole budget.
                if is_thumb {
                    self.thumb_cache.put(filename.to_string(), ct.clone(), body.clone());
                } else if ct.starts_with("image/") {
                    self.media_cache.put(filename.to_string(), ct.clone(), body.clone());
                }
                Ok(MediaResponse {
                    content_type: ct,
                    total_size: body.len() as u64,
                    body,
                    range: None,
                })
            }
            Some((start, end_opt)) => {
                let total = header.data_size;
                let last = total.saturating_sub(1);
                // Reject an unsatisfiable range up front. `total`/`last` derive
                // from the (attacker-controllable) header's data_size, so without
                // this guard a start past EOF — or an inverted `end < start` —
                // would later underflow `end - start + 1` (panic in debug, a
                // bogus huge Content-Length in release). Use saturating_add so a
                // near-`u64::MAX` start can't overflow either.
                if total == 0 || start > last {
                    return Err(CoreError::Other("range not satisfiable".into()));
                }
                let req_end = end_opt.unwrap_or(last).min(last);
                let end = req_end.min(start.saturating_add(MAX_RANGE_BYTES - 1));
                if end < start {
                    return Err(CoreError::Other("range not satisfiable".into()));
                }
                // Open + decrypt the requested slice on the blocking pool so a slow
                // disk (the cache may live on an HDD) can't stall the async runtime.
                // Not gated by `decrypt_sem`: video streams are self-paced by
                // playback, and we don't want a thumbnail burst to starve them.
                let path = path.clone();
                let sym = header.symmetric_key.clone();
                let chunk_size = header.chunk_size;
                let body = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
                    let mut f = std::fs::File::open(&path)?;
                    let outer_len = file::outer_header_len(&mut f)?;
                    let mut f2 = std::fs::File::open(&path)?;
                    Ok(file::decrypt_range(&mut f2, outer_len, &sym, chunk_size, start, end)?)
                })
                .await
                .map_err(|e| CoreError::Other(format!("range decrypt task failed: {e}")))??;
                Ok(MediaResponse {
                    content_type,
                    total_size: total,
                    body,
                    range: Some((start, end)),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::sniff_image;

    fn ftyp(brand: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 0x18];
        v.extend_from_slice(b"ftyp");
        v.extend_from_slice(brand);
        v.extend_from_slice(&[0u8; 8]);
        v
    }

    #[test]
    fn web_formats_are_served_verbatim() {
        assert_eq!(sniff_image(&[0xff, 0xd8, 0xff, 0xe0]), ("image/jpeg", None));
        assert_eq!(sniff_image(b"\x89PNG\r\n\x1a\n....."), ("image/png", None));
        assert_eq!(sniff_image(b"GIF89a"), ("image/gif", None));
        let mut webp = b"RIFF\0\0\0\0WEBPVP8 ".to_vec();
        webp.extend_from_slice(&[0u8; 8]);
        assert_eq!(sniff_image(&webp), ("image/webp", None));
        assert_eq!(sniff_image(b"BM\0\0\0\0"), ("image/bmp", None));
        assert_eq!(sniff_image(&ftyp(b"avif")), ("image/avif", None));
    }

    #[test]
    fn non_web_formats_request_transcode() {
        assert_eq!(sniff_image(&ftyp(b"heic")), ("image/heic", Some("heic")));
        assert_eq!(sniff_image(&ftyp(b"mif1")), ("image/heic", Some("heic")));
        assert_eq!(sniff_image(&[0x49, 0x49, 0x2a, 0x00, 0x08]), ("image/tiff", Some("tiff")));
    }

    #[test]
    fn unknown_falls_back_to_jpeg_label() {
        assert_eq!(sniff_image(b"\x00\x01\x02\x03garbage"), ("image/jpeg", None));
        assert_eq!(sniff_image(b""), ("image/jpeg", None));
    }
}
