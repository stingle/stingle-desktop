//! Thumbnail generation and webview-friendly transcoding.
//!
//! SECURITY: this module must never write DECRYPTED bytes to disk. ffmpeg is
//! driven purely through stdin/stdout pipes (no temp files). The only file
//! ffmpeg ever reads from a path is the user's own local file during import —
//! with one narrow exception: the legacy [`ffmpeg_container_to_jpeg`] fallback
//! (see its SECURITY NOTE), which is only reached for exotic files the primary
//! pipe-based HEIF path can't parse.

use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader};

use crate::error::{CoreError, Result};
use crate::heif;

/// Max thumbnail edge length (px).
pub const THUMB_MAX_DIM: u32 = 512;
pub const THUMB_JPEG_QUALITY: u8 = 80;
/// Full-screen preview transcodes (HEIC/TIFF → JPEG) keep more detail.
pub const PREVIEW_JPEG_QUALITY: u8 = 90;

/// Decode an image and produce a downscaled JPEG thumbnail.
pub fn image_thumbnail(image_bytes: &[u8]) -> Result<Vec<u8>> {
    let img = if heif::is_heif(image_bytes) {
        // The image crate can't decode HEVC; go through the HEIF pipeline
        // (so imported iPhone photos get real thumbnails, not placeholders).
        decode_heif(image_bytes, "heic")?
    } else {
        decode_with_orientation(image_bytes)?
    };
    let thumb = img.thumbnail(THUMB_MAX_DIM, THUMB_MAX_DIM);
    encode_jpeg(&thumb)
}

/// Decode via the image crate, honoring EXIF orientation so the result matches
/// how the webview renders the full image. Cameras (Nikon, iPhone, …) store
/// portrait shots in landscape pixels plus an orientation tag; without this the
/// output comes out sideways. encode_jpeg re-encodes raw RGB and writes no
/// EXIF, so baking the rotation in here can't cause a double-rotation later.
pub(crate) fn decode_with_orientation(bytes: &[u8]) -> Result<DynamicImage> {
    let mut decoder = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()?
        .into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut img = DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    Ok(img)
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

/// Transcode an in-memory image the webview can't render (HEIC/HEIF/TIFF) into
/// a JPEG for full-screen preview / clipboard copy.
///
/// HEIC/HEIF goes through [`decode_heif`]: we extract the container's PRIMARY
/// image ourselves and use ffmpeg only as a raw HEVC decoder over pipes. This
/// is deliberate — handing the whole container to ffmpeg leaves the choice of
/// image to its automatic stream selection, which varies across ffmpeg
/// builds/versions and on some platforms picked the depth map or HDR gain map
/// instead of the photo. TIFF is decoded by the image crate directly. Only if
/// those fail do we fall back to the legacy whole-container ffmpeg call.
pub fn transcode_to_jpeg(bytes: &[u8], ext: &str) -> Result<Vec<u8>> {
    if heif::is_heif(bytes) {
        return encode_jpeg_quality(&decode_heif(bytes, ext)?, PREVIEW_JPEG_QUALITY);
    }
    if let Ok(img) = decode_with_orientation(bytes) {
        return encode_jpeg_quality(&img, PREVIEW_JPEG_QUALITY);
    }
    ffmpeg_container_to_jpeg(bytes, ext)
}

/// Decode a HEIF/HEIC still to its primary image, falling back to the legacy
/// whole-container ffmpeg decode for containers our parser doesn't cover.
fn decode_heif(bytes: &[u8], ext: &str) -> Result<DynamicImage> {
    match decode_heif_primary(bytes) {
        Ok(img) => Ok(img),
        Err(e) => {
            tracing::warn!(
                "HEIF primary-item decode failed ({e}); falling back to whole-container ffmpeg"
            );
            let jpg = ffmpeg_container_to_jpeg(bytes, ext)?;
            Ok(image::load_from_memory(&jpg)?)
        }
    }
}

/// Decode the PRIMARY image of a HEIF/HEIC container, deterministically.
///
/// The container is parsed in Rust ([`heif::parse_primary`]) to extract the
/// primary item's HEVC tiles as one Annex-B stream; ffmpeg decodes that stream
/// to raw RGB frames over stdin/stdout (a raw bitstream needs no seeking, so —
/// unlike the container path — no decrypted temp file is ever written). Tiles
/// are stitched, cropped and rotated/mirrored here, per the container's
/// `grid`/`irot`/`imir` metadata. Same result on every platform and every
/// ffmpeg version, because nothing is left to ffmpeg's stream selection.
fn decode_heif_primary(bytes: &[u8]) -> Result<DynamicImage> {
    let parsed = heif::parse_primary(bytes)?;
    let (tw, th) = (parsed.tile_width as usize, parsed.tile_height as usize);
    let (rows, cols) = (parsed.rows as usize, parsed.cols as usize);
    let n = parsed.tile_count as usize;
    let frame_len = tw * th * 3;

    let raw = run_ffmpeg(
        &[
            "-f", "hevc", "-i", "pipe:0", "-fps_mode", "passthrough", "-pix_fmt", "rgb24", "-f",
            "rawvideo", "pipe:1",
        ],
        Some(parsed.annexb),
    )?;
    // Every tile must decode to exactly its declared size — anything else
    // means the decoder disagreed with the container metadata; bail out (the
    // caller falls back) rather than render garbage.
    if frame_len == 0 || raw.len() != frame_len * n {
        return Err(CoreError::Other(format!(
            "heif: decoded {} bytes, expected {} ({n} tiles of {tw}x{th})",
            raw.len(),
            frame_len * n
        )));
    }

    let (cw, ch) = (cols * tw, rows * th);
    let mut canvas = image::RgbImage::new(cw as u32, ch as u32);
    let stride = cw * 3;
    {
        let buf: &mut [u8] = &mut canvas;
        for i in 0..n {
            let tile = &raw[i * frame_len..(i + 1) * frame_len];
            let (row, col) = (i / cols, i % cols);
            for y in 0..th {
                let dst = (row * th + y) * stride + col * tw * 3;
                buf[dst..dst + tw * 3].copy_from_slice(&tile[y * tw * 3..(y + 1) * tw * 3]);
            }
        }
    }

    let mut img = DynamicImage::ImageRgb8(canvas);
    // The grid canvas is tile-aligned; crop (top-left anchored) to the real size.
    let (ow, oh) = (parsed.output_width, parsed.output_height);
    if (ow as usize) <= cw && (oh as usize) <= ch && ((ow as usize) < cw || (oh as usize) < ch) {
        img = img.crop_imm(0, 0, ow, oh);
    }
    for t in &parsed.transforms {
        img = match t {
            // irot angles are anti-clockwise; the image crate rotates clockwise.
            heif::Transform::Rotate(1) => img.rotate270(),
            heif::Transform::Rotate(2) => img.rotate180(),
            heif::Transform::Rotate(3) => img.rotate90(),
            heif::Transform::Rotate(_) => img,
            heif::Transform::MirrorVertical => img.fliph(),
            heif::Transform::MirrorHorizontal => img.flipv(),
        };
    }
    // irot/imir are authoritative when declared, but some encoders (Samsung,
    // the iPhone 17 front camera) record orientation only in the EXIF block.
    // Apply EXIF exactly when the container declares NO transform — never
    // both, which would double-rotate files carrying consistent copies.
    if !parsed.has_transform_props {
        if let Some(o) = parsed.exif_orientation.and_then(Orientation::from_exif) {
            img.apply_orientation(o);
        }
    }
    Ok(img)
}

/// Legacy fallback: hand the whole container to ffmpeg and let it pick a
/// stream. Only used when the targeted decoders above can't handle the file.
///
/// SECURITY NOTE: unlike video frame-grabs, this cannot use a stdin pipe.
/// iPhone HEICs are "grid" (tiled) images and ffmpeg can only assemble the grid
/// from a SEEKABLE input — a pipe fails with "grid box with non seekable input".
/// There is no in-memory seekable input for the ffmpeg CLI, so we write the
/// (already-decrypted) bytes to a temp file for the single transcode call and
/// delete it immediately afterwards (on success or error). Output still streams
/// over stdout — only the input briefly touches disk.
fn ffmpeg_container_to_jpeg(bytes: &[u8], ext: &str) -> Result<Vec<u8>> {
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
    encode_jpeg_quality(img, THUMB_JPEG_QUALITY)
}

fn encode_jpeg_quality(img: &image::DynamicImage, quality: u8) -> Result<Vec<u8>> {
    let mut out = Cursor::new(Vec::new());
    let rgb = img.to_rgb8();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
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
    let img = decode_with_orientation(image_bytes)?;
    let resized = img.resize(max_dim, max_dim, FilterType::Triangle);
    encode_jpeg(&resized)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 16×8 JPEG: left half red, right half blue (no EXIF).
    fn test_jpeg() -> Vec<u8> {
        let mut img = image::RgbImage::new(16, 8);
        for (x, _, p) in img.enumerate_pixels_mut() {
            *p = if x < 8 { image::Rgb([255, 0, 0]) } else { image::Rgb([0, 0, 255]) };
        }
        encode_jpeg_quality(&DynamicImage::ImageRgb8(img), 100).expect("encode")
    }

    /// Splice an EXIF APP1 segment carrying only `Orientation = n` after SOI.
    fn with_exif_orientation(jpeg: &[u8], orientation: u8) -> Vec<u8> {
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend_from_slice(b"II*\0");
        app1.extend_from_slice(&8u32.to_le_bytes()); // IFD0 offset
        app1.extend_from_slice(&1u16.to_le_bytes()); // entry count
        app1.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation
        app1.extend_from_slice(&3u16.to_le_bytes()); // type SHORT
        app1.extend_from_slice(&1u32.to_le_bytes()); // count
        app1.extend_from_slice(&(orientation as u16).to_le_bytes());
        app1.extend_from_slice(&0u16.to_le_bytes()); // value padding
        app1.extend_from_slice(&0u32.to_le_bytes()); // next-IFD offset
        let mut out = jpeg[..2].to_vec(); // SOI
        out.extend_from_slice(&[0xFF, 0xE1]);
        out.extend_from_slice(&((app1.len() as u16 + 2).to_be_bytes()));
        out.extend_from_slice(&app1);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    #[test]
    fn applies_all_eight_exif_orientations() {
        let base = test_jpeg();
        let upright = image::load_from_memory(&base).expect("decode base").to_rgb8();
        for n in 1u8..=8 {
            let tagged = with_exif_orientation(&base, n);
            let got = decode_with_orientation(&tagged).expect("decode").to_rgb8();
            let mut want = DynamicImage::ImageRgb8(upright.clone());
            want.apply_orientation(Orientation::from_exif(n).expect("valid exif value"));
            let want = want.to_rgb8();
            assert_eq!(got.dimensions(), want.dimensions(), "orientation {n}");
            assert_eq!(got.as_raw(), want.as_raw(), "orientation {n}");
            // 5..=8 swap width and height.
            let expect_dims = if n >= 5 { (8, 16) } else { (16, 8) };
            assert_eq!(got.dimensions(), expect_dims, "orientation {n}");
        }

        // Semantic spot checks (independent of the image crate's mapping):
        let red = |p: &image::Rgb<u8>| p.0[0] > 150 && p.0[2] < 100;
        let blue = |p: &image::Rgb<u8>| p.0[2] > 150 && p.0[0] < 100;
        // 2 = mirror horizontal → left edge turns blue.
        let o2 = decode_with_orientation(&with_exif_orientation(&base, 2)).unwrap().to_rgb8();
        assert!(blue(o2.get_pixel(0, 0)) && red(o2.get_pixel(15, 0)));
        // 3 = rotate 180 → left edge turns blue.
        let o3 = decode_with_orientation(&with_exif_orientation(&base, 3)).unwrap().to_rgb8();
        assert!(blue(o3.get_pixel(0, 0)) && red(o3.get_pixel(15, 7)));
        // 6 = rotate 90 CW → the red left half becomes the TOP half.
        let o6 = decode_with_orientation(&with_exif_orientation(&base, 6)).unwrap().to_rgb8();
        assert!(red(o6.get_pixel(0, 0)) && blue(o6.get_pixel(7, 15)));
    }
}
