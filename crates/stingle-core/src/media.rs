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
    /// the full-screen preview. Errors for videos / undecodable media.
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
        let img = image::load_from_memory(&resp.body)
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
                let blob = std::fs::read(&path)?;
                let mut body = file::decrypt_with_external_header(
                    &part,
                    &blob,
                    &kp.public_key,
                    &kp.secret_key,
                )?;
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
                Ok(MediaResponse {
                    content_type: ct,
                    total_size: body.len() as u64,
                    body,
                    range: None,
                })
            }
            Some((start, end_opt)) => {
                let total = header.data_size;
                let mut f = std::fs::File::open(&path)?;
                let outer_len = file::outer_header_len(&mut f)?;
                let last = total.saturating_sub(1);
                let req_end = end_opt.unwrap_or(last).min(last);
                let end = req_end.min(start + MAX_RANGE_BYTES - 1);
                let mut f2 = std::fs::File::open(&path)?;
                let body = file::decrypt_range(
                    &mut f2,
                    outer_len,
                    &header.symmetric_key,
                    header.chunk_size,
                    start,
                    end,
                )?;
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
