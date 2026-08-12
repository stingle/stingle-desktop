//! Structured "what is this file?" metadata for the viewer's info panel.
//!
//! Gathers what we know about one item from three places — the DB row (sync
//! state, dates), the decrypted `.sp` header (original name, size, kind), and
//! the media bytes themselves (dimensions + EXIF) — and returns it as ordered
//! sections of label/value pairs the UI can render without knowing any of the
//! details.
//!
//! Nothing here writes to disk: the file is decrypted in memory only, exactly
//! like the viewer already does to display it.

use std::io::Cursor;

use serde::Serialize;
use stingle_db::FileSet;

use crate::account::Account;
use crate::error::Result;

/// One `label: value` row in the info panel.
#[derive(Serialize, Clone, Debug)]
pub struct InfoField {
    pub label: String,
    pub value: String,
    /// `Some("epoch_ms")` when `value` is a millisecond timestamp the UI should
    /// render in the user's own locale/timezone (we deliberately don't format
    /// dates here — the frontend already does it everywhere else). `None` means
    /// the value is display-ready text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// A titled group of fields (e.g. "Camera", "Location").
#[derive(Serialize, Clone, Debug)]
pub struct InfoSection {
    pub title: String,
    pub fields: Vec<InfoField>,
}

/// Everything we can say about one library item.
#[derive(Serialize, Clone, Debug, Default)]
pub struct MediaInfo {
    pub sections: Vec<InfoSection>,
}

fn field(label: &str, value: impl Into<String>) -> InfoField {
    InfoField {
        label: label.to_string(),
        value: value.into(),
        kind: None,
    }
}

/// A timestamp row: the value is epoch milliseconds for the UI to localize.
fn date_field(label: &str, ms: i64) -> Option<InfoField> {
    (ms > 0).then(|| InfoField {
        label: label.to_string(),
        value: ms.to_string(),
        kind: Some("epoch_ms".to_string()),
    })
}

/// Human-readable byte size (binary units, matching what file managers show).
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

/// `h:mm:ss` / `m:ss` from whole seconds.
fn human_duration(secs: u32) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

impl Account {
    /// Collect display metadata for one item.
    ///
    /// Photos are decrypted in memory so their dimensions and EXIF can be read.
    /// **Videos deliberately are not** — a video can be hundreds of megabytes and
    /// decrypting one just to open an info panel would stall the UI, so they
    /// report header/DB facts only.
    pub async fn media_info(
        &self,
        set: FileSet,
        album_id: Option<&str>,
        filename: &str,
    ) -> Result<MediaInfo> {
        let headers = self.headers_for(set, album_id, filename)?;
        let meta = self.row_header_meta(set, album_id, &headers)?;
        let row = match set {
            FileSet::Album => {
                let aid = album_id.unwrap_or_default();
                self.db.get_album_file(aid, filename)?
            }
            _ => self.db.get_file(set, filename)?,
        };

        let mut sections = Vec::new();

        // ---- File -------------------------------------------------------
        let mut file_fields = vec![
            field(
                "Name",
                if meta.original_filename.is_empty() {
                    filename.to_string()
                } else {
                    meta.original_filename.clone()
                },
            ),
            field("Kind", if meta.is_video { "Video" } else { "Photo" }),
            field("Size", human_size(meta.data_size)),
        ];
        if let Some(r) = &row {
            file_fields.extend(date_field("Date taken", r.date_created));
            file_fields.extend(date_field("Date modified", r.date_modified));
        }
        if let Some(aid) = album_id {
            if let Ok(Some(album)) = self.db.get_album(aid) {
                if let Ok(name) = self.album_name(&album) {
                    file_fields.push(field("Album", name));
                }
            }
        }
        sections.push(InfoSection {
            title: "File".to_string(),
            fields: file_fields,
        });

        // ---- Storage ----------------------------------------------------
        if let Some(r) = &row {
            sections.push(InfoSection {
                title: "Storage".to_string(),
                fields: vec![
                    field(
                        "Downloaded to this device",
                        if self.paths.original(filename).exists() { "Yes" } else { "No" },
                    ),
                    field("Backed up to cloud", if r.is_remote { "Yes" } else { "No" }),
                    field("Encrypted filename", filename),
                ],
            });
        }

        if meta.is_video {
            if meta.data_size > 0 {
                // Duration lives in the header, so it costs nothing to report.
                if let Ok(dur) = self.video_duration_secs(set, album_id, filename) {
                    if dur > 0 {
                        sections.push(InfoSection {
                            title: "Video".to_string(),
                            fields: vec![field("Duration", human_duration(dur))],
                        });
                    }
                }
            }
            return Ok(MediaInfo { sections });
        }

        // ---- Photo: dimensions + EXIF (needs the decrypted bytes) -------
        let bytes = match self.get_decrypted(set, album_id, filename, false).await {
            Ok(b) => b,
            // No bytes (not downloaded / offline) — still return what we have.
            Err(_) => return Ok(MediaInfo { sections }),
        };

        if let Some((w, h)) = image_dimensions(&bytes) {
            sections.push(InfoSection {
                title: "Image".to_string(),
                fields: vec![
                    field("Dimensions", format!("{w} × {h}")),
                    field("Megapixels", format!("{:.1} MP", (w as f64 * h as f64) / 1e6)),
                ],
            });
        }
        sections.extend(exif_sections(&bytes));
        Ok(MediaInfo { sections })
    }

    /// Video duration (seconds) straight from the decrypted header.
    fn video_duration_secs(
        &self,
        set: FileSet,
        album_id: Option<&str>,
        filename: &str,
    ) -> Result<u32> {
        use stingle_crypto::file;
        let headers = self.headers_for(set, album_id, filename)?;
        let part = crate::sync::headers_part(&headers, false)?;
        let kp = self.keypair_for(set, album_id)?;
        let header = file::read_header(&mut Cursor::new(&part), &kp.public_key, &kp.secret_key)?;
        Ok(header.video_duration)
    }
}

/// Pixel dimensions without decoding the whole image where possible.
fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if crate::heif::is_heif(bytes) {
        use crate::heif::Transform;
        let p = crate::heif::parse_primary(bytes).ok()?;
        // Report the size as DISPLAYED: a quarter-turn swaps the axes. Container
        // transforms win when present, else the EXIF orientation tag (5..=8 are
        // the transposed ones) — the same precedence the decoder applies.
        let quarter_turn = if p.has_transform_props {
            p.transforms
                .iter()
                .filter(|t| matches!(t, Transform::Rotate(1) | Transform::Rotate(3)))
                .count()
                % 2
                == 1
        } else {
            matches!(p.exif_orientation, Some(5..=8))
        };
        return Some(if quarter_turn {
            (p.output_height, p.output_width)
        } else {
            (p.output_width, p.output_height)
        });
    }
    // Header-only read for the formats the `image` crate knows.
    let reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.into_dimensions().ok()
}

/// Curated EXIF groups plus a complete dump of everything else.
///
/// The curated sections put the handful of tags people actually look for up
/// front; "All metadata" then lists every remaining tag so nothing is hidden.
fn exif_sections(bytes: &[u8]) -> Vec<InfoSection> {
    use exif::{In, Tag};

    // HEIC stores EXIF in a container item a generic reader can't reach, so pull
    // the raw TIFF out first; everything else can be read in place.
    let parsed = if crate::heif::is_heif(bytes) {
        crate::heif::extract_exif_tiff(bytes)
            .and_then(|tiff| exif::Reader::new().read_raw(tiff).ok())
    } else {
        exif::Reader::new()
            .read_from_container(&mut Cursor::new(bytes))
            .ok()
    };
    let Some(ex) = parsed else { return Vec::new() };

    // `display_value().with_unit()` renders "1/125 s", "f/1.8", "27 mm" etc.
    let show = |tag: Tag| -> Option<String> {
        ex.get_field(tag, In::PRIMARY)
            .map(|f| f.display_value().with_unit(&ex).to_string())
    };
    let push = |out: &mut Vec<InfoSection>, title: &str, wanted: &[(&str, Tag)]| {
        let fields: Vec<InfoField> = wanted
            .iter()
            .filter_map(|(label, tag)| show(*tag).map(|v| field(label, v)))
            .collect();
        if !fields.is_empty() {
            out.push(InfoSection {
                title: title.to_string(),
                fields,
            });
        }
    };

    let mut out = Vec::new();
    push(
        &mut out,
        "Camera",
        &[
            ("Make", Tag::Make),
            ("Model", Tag::Model),
            ("Lens", Tag::LensModel),
            ("Software", Tag::Software),
        ],
    );
    push(
        &mut out,
        "Capture",
        &[
            ("Taken", Tag::DateTimeOriginal),
            ("Exposure", Tag::ExposureTime),
            ("Aperture", Tag::FNumber),
            ("ISO", Tag::PhotographicSensitivity),
            ("Focal length", Tag::FocalLength),
            ("35mm equivalent", Tag::FocalLengthIn35mmFilm),
            ("Exposure bias", Tag::ExposureBiasValue),
            ("Metering", Tag::MeteringMode),
            ("Flash", Tag::Flash),
            ("White balance", Tag::WhiteBalance),
        ],
    );
    push(
        &mut out,
        "Location",
        &[
            ("Latitude", Tag::GPSLatitude),
            ("Longitude", Tag::GPSLongitude),
            ("Altitude", Tag::GPSAltitude),
        ],
    );

    // Everything not already shown, so "all available info" really is all of it.
    let shown: std::collections::HashSet<String> = out
        .iter()
        .flat_map(|s| s.fields.iter().map(|f| f.value.clone()))
        .collect();
    let mut rest: Vec<InfoField> = ex
        .fields()
        .filter(|f| {
            let v = f.display_value().with_unit(&ex).to_string();
            !shown.contains(&v)
        })
        .map(|f| {
            let label = match f.tag.description() {
                Some(d) if !d.is_empty() => d.to_string(),
                _ => f.tag.to_string(),
            };
            let label = if f.ifd_num == In::PRIMARY {
                label
            } else {
                format!("{label} ({})", f.ifd_num)
            };
            field(&label, f.display_value().with_unit(&ex).to_string())
        })
        .collect();
    rest.sort_by(|a, b| a.label.cmp(&b.label));
    if !rest.is_empty() {
        out.push(InfoSection {
            title: "All metadata".to_string(),
            fields: rest,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sizes() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.00 KB");
        assert_eq!(human_size(1_572_864), "1.50 MB");
    }

    #[test]
    fn formats_durations() {
        assert_eq!(human_duration(0), "0:00");
        assert_eq!(human_duration(65), "1:05");
        assert_eq!(human_duration(3661), "1:01:01");
    }

    #[test]
    fn no_exif_yields_no_sections() {
        // Not an image at all — must not panic, just produce nothing.
        assert!(exif_sections(b"definitely not an image").is_empty());
    }
}
