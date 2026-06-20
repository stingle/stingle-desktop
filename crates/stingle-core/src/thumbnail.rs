//! Thumbnail generation and webview-friendly transcoding.
//!
//! SECURITY: this module must never write DECRYPTED bytes to disk. ffmpeg is
//! driven purely through stdin/stdout pipes (no temp files). The only file
//! ffmpeg ever reads from a path is the user's own local file during import.

use std::io::{Cursor, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Once;

use image::imageops::FilterType;
use image::ImageReader;

use crate::error::{CoreError, Result};

/// Max thumbnail edge length (px).
pub const THUMB_MAX_DIM: u32 = 512;
pub const THUMB_JPEG_QUALITY: u8 = 80;

/// Decode an image and produce a downscaled JPEG thumbnail.
pub fn image_thumbnail(image_bytes: &[u8]) -> Result<Vec<u8>> {
    let img = ImageReader::new(Cursor::new(image_bytes))
        .with_guessed_format()?
        .decode()?;
    let thumb = img.thumbnail(THUMB_MAX_DIM, THUMB_MAX_DIM);
    encode_jpeg(&thumb)
}

static FFMPEG_INIT: Once = Once::new();
static mut FFMPEG_OK: bool = false;

/// Ensure an ffmpeg binary is available (downloads a managed copy on first use).
fn ensure_ffmpeg() -> bool {
    FFMPEG_INIT.call_once(|| {
        let ok = ffmpeg_sidecar::download::auto_download().is_ok();
        // SAFETY: written exactly once inside call_once.
        unsafe { FFMPEG_OK = ok };
    });
    unsafe { FFMPEG_OK }
}

fn ffmpeg_path() -> Result<std::path::PathBuf> {
    if !ensure_ffmpeg() {
        return Err(CoreError::Other("ffmpeg unavailable".into()));
    }
    Ok(ffmpeg_sidecar::paths::ffmpeg_path())
}

/// Run ffmpeg with the given args, optionally feeding `input` via stdin, and
/// return its stdout bytes. No data ever touches the disk.
fn run_ffmpeg(args: &[&str], input: Option<Vec<u8>>) -> Result<Vec<u8>> {
    let bin = ffmpeg_path()?;
    let mut cmd = Command::new(bin);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::null());
    if input.is_some() {
        cmd.stdin(Stdio::piped());
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| CoreError::Other(format!("ffmpeg spawn: {e}")))?;

    // Feed stdin from a thread to avoid pipe-buffer deadlock with stdout.
    let writer = input.map(|bytes| {
        let mut stdin = child.stdin.take().expect("stdin piped");
        std::thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
            // dropping stdin closes the pipe
        })
    });

    let output = child
        .wait_with_output()
        .map_err(|e| CoreError::Other(format!("ffmpeg wait: {e}")))?;
    if let Some(w) = writer {
        let _ = w.join();
    }
    if !output.status.success() || output.stdout.is_empty() {
        return Err(CoreError::Other("ffmpeg produced no output".into()));
    }
    Ok(output.stdout)
}

/// Extract the first frame of a (user's local) video file and produce a
/// downscaled JPEG thumbnail. Reads the path directly; writes nothing to disk.
pub fn video_thumbnail(path: &Path) -> Result<Vec<u8>> {
    let path_str = path.to_string_lossy().to_string();
    let frame = run_ffmpeg(
        &["-i", &path_str, "-frames:v", "1", "-q:v", "3", "-f", "mjpeg", "pipe:1"],
        None,
    )?;
    image_thumbnail(&frame)
}

/// Transcode an in-memory image the webview can't render (HEIC/HEIF/TIFF) into a
/// JPEG for full-screen preview. Pure stdin→stdout; nothing hits the disk.
pub fn transcode_to_jpeg(bytes: &[u8], _ext: &str) -> Result<Vec<u8>> {
    run_ffmpeg(
        &["-i", "pipe:0", "-frames:v", "1", "-q:v", "3", "-f", "mjpeg", "pipe:1"],
        Some(bytes.to_vec()),
    )
}

/// A simple solid placeholder thumbnail (fallback when no frame can be grabbed).
pub fn placeholder_thumbnail() -> Result<Vec<u8>> {
    let mut img = image::RgbImage::new(THUMB_MAX_DIM, THUMB_MAX_DIM / 2);
    for p in img.pixels_mut() {
        *p = image::Rgb([32, 32, 38]);
    }
    encode_jpeg(&image::DynamicImage::ImageRgb8(img))
}

fn encode_jpeg(img: &image::DynamicImage) -> Result<Vec<u8>> {
    let mut out = Cursor::new(Vec::new());
    let rgb = img.to_rgb8();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, THUMB_JPEG_QUALITY);
    enc.encode(
        rgb.as_raw(),
        rgb.width(),
        rgb.height(),
        image::ExtendedColorType::Rgb8,
    )?;
    Ok(out.into_inner())
}

/// Resize an already-decoded image (utility).
#[allow(dead_code)]
pub fn resize_jpeg(image_bytes: &[u8], max_dim: u32) -> Result<Vec<u8>> {
    let img = ImageReader::new(Cursor::new(image_bytes))
        .with_guessed_format()?
        .decode()?;
    let resized = img.resize(max_dim, max_dim, FilterType::Triangle);
    encode_jpeg(&resized)
}
