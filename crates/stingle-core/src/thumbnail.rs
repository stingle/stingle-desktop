//! Thumbnail generation and webview-friendly transcoding.
//!
//! SECURITY: this module must never write DECRYPTED bytes to disk. ffmpeg is
//! driven purely through stdin/stdout pipes (no temp files). The only file
//! ffmpeg ever reads from a path is the user's own local file during import.

use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use image::imageops::FilterType;
use image::{DynamicImage, ImageDecoder, ImageReader};

use crate::error::{CoreError, Result};

/// Max thumbnail edge length (px).
pub const THUMB_MAX_DIM: u32 = 512;
pub const THUMB_JPEG_QUALITY: u8 = 80;

/// Decode an image and produce a downscaled JPEG thumbnail.
pub fn image_thumbnail(image_bytes: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = ImageReader::new(Cursor::new(image_bytes))
        .with_guessed_format()?
        .into_decoder()?;
    // Honor EXIF orientation so the thumbnail matches how the webview renders
    // the full image. Cameras (Nikon, iPhone, …) store portrait shots in
    // landshape pixels plus an orientation tag; without this the thumbnail
    // comes out sideways. encode_jpeg re-encodes raw RGB and writes no EXIF, so
    // baking the rotation in here can't cause a double-rotation in the webview.
    let orientation = decoder.orientation()?;
    let mut img = DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    let thumb = img.thumbnail(THUMB_MAX_DIM, THUMB_MAX_DIM);
    encode_jpeg(&thumb)
}

static FFMPEG: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Install ffmpeg if needed (ffmpeg-sidecar's managed "essentials" build, which
/// does decode HEIC/HEIF — see [`transcode_to_jpeg`] for the seekable-input
/// caveat).
fn install_ffmpeg() -> Result<()> {
    ffmpeg_sidecar::download::auto_download().map_err(|e| CoreError::Other(e.to_string()))
}

/// Resolve a usable ffmpeg binary exactly once.
///
/// SECURITY (supply chain): ffmpeg is fed DECRYPTED image bytes on stdin, so a
/// tampered binary is a plaintext-disclosure / code-execution risk. By default
/// we fall back to ffmpeg-sidecar's *managed* build, which is downloaded over
/// HTTPS on first use but is NOT signature-pinned. To close that hole:
///   - `STINGLE_FFMPEG` — absolute path to a trusted ffmpeg (e.g. one bundled
///     into the signed installer). When set, no download happens.
///   - `STINGLE_FFMPEG_SHA256` — hex SHA-256 the resolved binary MUST match,
///     else ffmpeg is refused. This is the only setting that fully verifies
///     integrity; packaged builds should set both.
fn resolve_ffmpeg() -> Option<PathBuf> {
    let path = match std::env::var_os("STINGLE_FFMPEG") {
        Some(p) => PathBuf::from(p),
        None => {
            if install_ffmpeg().is_err() {
                return None;
            }
            ffmpeg_sidecar::paths::ffmpeg_path()
        }
    };
    if !path.exists() {
        return None;
    }
    if let Ok(expected) = std::env::var("STINGLE_FFMPEG_SHA256") {
        if !sha256_matches(&path, expected.trim()) {
            tracing::error!("ffmpeg SHA-256 verification failed for {path:?}; refusing to use it");
            return None;
        }
    }
    Some(path)
}

/// Whether `path`'s SHA-256 equals `expected_hex` (case-insensitive).
fn sha256_matches(path: &Path, expected_hex: &str) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    match stingle_crypto::sodium::sha256(&bytes) {
        Ok(d) => hex::encode(d).eq_ignore_ascii_case(expected_hex),
        Err(_) => false,
    }
}

/// Pre-install the media toolchain (ffmpeg) on a background thread at startup so
/// the first HEIC preview/copy or video frame-grab doesn't stall on the download.
pub fn prepare_media_tools() {
    let _ = ffmpeg_path();
}

fn ffmpeg_path() -> Result<PathBuf> {
    FFMPEG
        .get_or_init(resolve_ffmpeg)
        .clone()
        .ok_or_else(|| CoreError::Other("ffmpeg unavailable".into()))
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
    // On Windows, spawning a console subprocess from the GUI app pops a console
    // window for the child's lifetime. CREATE_NO_WINDOW (0x08000000) suppresses it.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
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

static XCODE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Temp dir for the short-lived seekable input used by [`transcode_to_jpeg`].
pub fn transcode_temp_dir() -> std::path::PathBuf {
    std::env::temp_dir().join("stingle-xcode")
}

/// Transcode an in-memory image the webview can't render (HEIC/HEIF/TIFF) into a
/// JPEG for full-screen preview / clipboard copy.
///
/// SECURITY NOTE: unlike video frame-grabs, this cannot use a stdin pipe.
/// iPhone HEICs are "grid" (tiled) images and ffmpeg can only assemble the grid
/// from a SEEKABLE input — a pipe fails with "grid box with non seekable input".
/// There is no in-memory seekable input for the ffmpeg CLI, so we write the
/// (already-decrypted) bytes to a temp file for the single transcode call and
/// delete it immediately afterwards (on success or error). Output still streams
/// over stdout — only the input briefly touches disk.
pub fn transcode_to_jpeg(bytes: &[u8], ext: &str) -> Result<Vec<u8>> {
    let dir = transcode_temp_dir();
    std::fs::create_dir_all(&dir).map_err(|e| CoreError::Other(format!("temp dir: {e}")))?;
    // The input briefly holds DECRYPTED bytes — restrict the dir to this user.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let safe_ext = if ext.is_empty() { "bin" } else { ext };
    let seq = XCODE_SEQ.fetch_add(1, Ordering::Relaxed);
    let in_path = dir.join(format!("x_{}_{}.{}", std::process::id(), seq, safe_ext));
    std::fs::write(&in_path, bytes).map_err(|e| CoreError::Other(format!("temp write: {e}")))?;

    let in_str = in_path.to_string_lossy().to_string();
    let result = run_ffmpeg(
        &["-y", "-i", &in_str, "-frames:v", "1", "-q:v", "3", "-f", "mjpeg", "pipe:1"],
        None,
    );
    // Always remove the decrypted input, success or failure.
    let _ = std::fs::remove_file(&in_path);
    result
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
