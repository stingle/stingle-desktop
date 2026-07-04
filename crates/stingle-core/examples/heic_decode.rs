//! Dev check for the HEIF primary-image pipeline: parse + decode a local
//! HEIC/HEIF/TIFF and write the transcoded JPEG next to it (or to the given
//! output path). Usage:
//!
//! ```text
//! STINGLE_FFMPEG=... cargo run -p stingle-core --example heic_decode -- IMG.HEIC [out.jpg]
//! ```

fn main() {
    let mut args = std::env::args().skip(1);
    let input = args.next().expect("usage: heic_decode <image> [out.jpg]");
    let out = args.next().unwrap_or_else(|| format!("{input}.jpg"));

    let bytes = std::fs::read(&input).expect("read input");
    match stingle_core::heif::parse_primary(&bytes) {
        Ok(p) => println!(
            "primary item: {} tile(s) of {}x{} in a {}x{} grid -> {}x{}, transforms {:?} \
             (declared: {}), exif orientation {:?}, {} bytes HEVC",
            p.tile_count, p.tile_width, p.tile_height, p.rows, p.cols,
            p.output_width, p.output_height, p.transforms, p.has_transform_props,
            p.exif_orientation, p.annexb.len(),
        ),
        Err(e) => println!("container parse failed ({e}) — transcode will use the ffmpeg fallback"),
    }

    let ext = std::path::Path::new(&input)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let jpg = stingle_core::thumbnail::transcode_to_jpeg(&bytes, &ext).expect("transcode");
    std::fs::write(&out, &jpg).expect("write output");
    println!("wrote {out} ({} bytes)", jpg.len());
}
