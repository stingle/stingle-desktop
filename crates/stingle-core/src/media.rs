//! Media serving for the UI: decrypt thumbnails/originals, with HTTP Range
//! support for streaming video (decrypts only the requested chunks).

use std::io::Cursor;
use std::path::Path;

use stingle_crypto::file;
use stingle_db::FileSet;

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

/// Image types the webview (Chromium/WebView2) can't natively render, so we
/// transcode them to JPEG for full-screen preview.
fn needs_transcode(content_type: &str) -> bool {
    matches!(content_type, "image/heic" | "image/heif" | "image/tiff")
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
        (|| -> Result<bool> {
            let part = headers_part(headers, false)?;
            let kp = self.keypair_for(set, album_id)?;
            let header =
                file::read_header(&mut Cursor::new(&part), &kp.public_key, &kp.secret_key)?;
            Ok(header.file_type == stingle_crypto::constants::FILE_TYPE_VIDEO)
        })()
        .unwrap_or(false)
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
            if let Some(body) = self.thumb_cache.get(filename) {
                return Ok(MediaResponse {
                    content_type: "image/jpeg".to_string(),
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
                // Transcode formats the webview can't display for full preview.
                if !is_thumb && needs_transcode(&ct) {
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
                // Keep decrypted thumbnails in memory for instant re-display.
                if is_thumb {
                    self.thumb_cache.put(filename.to_string(), body.clone());
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
