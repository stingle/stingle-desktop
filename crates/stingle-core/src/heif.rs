//! Minimal HEIF/HEIC (ISO-BMFF, ISO 23008-12) still-image parser.
//!
//! Why this exists: an iPhone HEIC holds SEVERAL coded images — the primary
//! photo (usually a grid of HEVC tiles) plus auxiliary items such as a depth
//! map and an HDR gain map. Handing the whole container to `ffmpeg -i` leaves
//! the choice of image to ffmpeg's automatic stream selection, which differs
//! across ffmpeg builds/versions — on some platforms it picks the depth/gain
//! map (the infamous "negative" preview). This parser reads the container's
//! `pitm` (primary item) box and extracts exactly the primary image's HEVC
//! payload as an Annex-B stream, so ffmpeg is only ever used as a dumb HEVC
//! decoder over pipes and the result is identical on every platform.
//!
//! Scope: `hvc1`-coded primaries, standalone or `grid`-derived (covers Apple
//! and Samsung HEICs). Anything else errors and the caller falls back to the
//! whole-container ffmpeg path.

use std::collections::HashMap;

use crate::error::{CoreError, Result};

/// The container's primary image, ready for decoding: an Annex-B HEVC stream
/// of `tile_count` independent frames plus the layout/transforms needed to
/// reassemble the final picture.
pub struct PrimaryImage {
    /// VPS/SPS/PPS + tile slices with start codes, in row-major tile order.
    pub annexb: Vec<u8>,
    pub tile_count: u32,
    /// Coded size of each tile (from the tile items' `ispe`).
    pub tile_width: u32,
    pub tile_height: u32,
    /// Grid layout (1×1 for a non-grid primary).
    pub rows: u32,
    pub cols: u32,
    /// Final image size — the grid canvas is cropped (top-left anchored) to
    /// this, per the `grid` payload / `ispe`.
    pub output_width: u32,
    pub output_height: u32,
    /// `irot`/`imir` of the primary item, in association (application) order.
    pub transforms: Vec<Transform>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transform {
    /// `irot`: rotate `angle` × 90° anti-clockwise (angle in 1..=3).
    Rotate(u8),
    /// `imir` axis 0: mirror about the vertical axis (left↔right flip).
    MirrorVertical,
    /// `imir` axis 1: mirror about the horizontal axis (top↔bottom flip).
    MirrorHorizontal,
}

/// Cheap content sniff: an ISO-BMFF `ftyp` with a HEIF still-image brand.
pub fn is_heif(bytes: &[u8]) -> bool {
    bytes.len() >= 12
        && &bytes[4..8] == b"ftyp"
        && matches!(
            &bytes[8..12],
            b"heic" | b"heix" | b"heim" | b"heis" | b"hevc" | b"hevx" | b"hevm" | b"hevs"
                | b"mif1" | b"msf1"
        )
}

fn err(msg: impl std::fmt::Display) -> CoreError {
    CoreError::Other(format!("heif: {msg}"))
}

// ----------------------------- box scanning -----------------------------

/// A box's payload as an absolute range into the file buffer (absolute so
/// `iloc` construction-method-0 file offsets can be resolved directly).
#[derive(Clone, Copy)]
struct BoxRef {
    typ: [u8; 4],
    start: usize,
    end: usize,
}

/// Parse the box header at `pos`; return the box and the offset of the next
/// sibling. `end` bounds the enclosing container.
fn read_box(buf: &[u8], pos: usize, end: usize) -> Result<(BoxRef, usize)> {
    if pos + 8 > end {
        return Err(err("truncated box header"));
    }
    let size32 = u32::from_be_bytes(buf[pos..pos + 4].try_into().unwrap()) as u64;
    let typ: [u8; 4] = buf[pos + 4..pos + 8].try_into().unwrap();
    let mut payload = pos + 8;
    let box_end = match size32 {
        0 => end as u64, // box extends to the end of the enclosing container
        1 => {
            if payload + 8 > end {
                return Err(err("truncated largesize"));
            }
            let large = u64::from_be_bytes(buf[payload..payload + 8].try_into().unwrap());
            payload += 8;
            pos as u64 + large
        }
        s => pos as u64 + s,
    };
    if &typ == b"uuid" {
        payload += 16; // skip usertype
    }
    let box_end = usize::try_from(box_end).map_err(|_| err("box size overflow"))?;
    if box_end < payload || box_end > end {
        return Err(err("box overruns container"));
    }
    Ok((BoxRef { typ, start: payload, end: box_end }, box_end))
}

/// All direct children of `[start, end)`.
fn child_boxes(buf: &[u8], start: usize, end: usize) -> Result<Vec<BoxRef>> {
    let mut out = Vec::new();
    let mut pos = start;
    while pos + 8 <= end {
        let (b, next) = read_box(buf, pos, end)?;
        out.push(b);
        if next <= pos {
            return Err(err("non-advancing box"));
        }
        pos = next;
    }
    Ok(out)
}

// ----------------------------- byte reader -----------------------------

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    end: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8], b: BoxRef) -> Self {
        Reader { buf, pos: b.start, end: b.end }
    }
    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos + n > self.end {
            return Err(err("truncated field"));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.bytes(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.bytes(4)?.try_into().unwrap()))
    }
    /// Big-endian unsigned integer of `n` bytes (0 ⇒ 0), for `iloc`'s
    /// variable-width offset/length fields.
    fn uint(&mut self, n: usize) -> Result<u64> {
        if n > 8 {
            return Err(err("integer field too wide"));
        }
        let mut v = 0u64;
        for &b in self.bytes(n)? {
            v = (v << 8) | b as u64;
        }
        Ok(v)
    }
    /// FullBox header; returns `version` (flags are returned too when needed).
    fn fullbox(&mut self) -> Result<(u8, u32)> {
        let v = self.u8()?;
        let f = self.uint(3)? as u32;
        Ok((v, f))
    }
}

// ----------------------------- meta parsing -----------------------------

struct ItemLocation {
    construction_method: u8,
    base_offset: u64,
    /// (offset, length) pairs; length 0 on a sole extent means "to the end".
    extents: Vec<(u64, u64)>,
}

#[derive(Default)]
struct Meta {
    primary_id: Option<u32>,
    item_types: HashMap<u32, [u8; 4]>,
    /// `dimg` references: derived item → source items, in order.
    dimg: HashMap<u32, Vec<u32>>,
    iloc: HashMap<u32, ItemLocation>,
    /// Property boxes of `ipco`, in declaration order (indices are 1-based).
    ipco: Vec<BoxRef>,
    /// Item → 1-based `ipco` indices, in association order.
    ipma: HashMap<u32, Vec<u16>>,
    /// Absolute range of the `idat` payload, for construction method 1.
    idat: Option<(usize, usize)>,
}

fn parse_meta(buf: &[u8]) -> Result<Meta> {
    let meta_box = child_boxes(buf, 0, buf.len())?
        .into_iter()
        .find(|b| &b.typ == b"meta")
        .ok_or_else(|| err("no meta box"))?;
    // meta is a FullBox: skip version/flags.
    let mut meta = Meta::default();
    for b in child_boxes(buf, meta_box.start + 4, meta_box.end)? {
        match &b.typ {
            b"pitm" => {
                let mut r = Reader::new(buf, b);
                let (v, _) = r.fullbox()?;
                meta.primary_id = Some(if v == 0 { r.u16()? as u32 } else { r.u32()? });
            }
            b"iinf" => parse_iinf(buf, b, &mut meta)?,
            b"iref" => parse_iref(buf, b, &mut meta)?,
            b"iloc" => parse_iloc(buf, b, &mut meta)?,
            b"iprp" => {
                for p in child_boxes(buf, b.start, b.end)? {
                    match &p.typ {
                        b"ipco" => meta.ipco = child_boxes(buf, p.start, p.end)?,
                        b"ipma" => parse_ipma(buf, p, &mut meta)?,
                        _ => {}
                    }
                }
            }
            b"idat" => meta.idat = Some((b.start, b.end)),
            _ => {}
        }
    }
    Ok(meta)
}

fn parse_iinf(buf: &[u8], b: BoxRef, meta: &mut Meta) -> Result<()> {
    let mut r = Reader::new(buf, b);
    let (v, _) = r.fullbox()?;
    let count = if v == 0 { r.u16()? as u32 } else { r.u32()? };
    let mut pos = r.pos;
    for _ in 0..count {
        let (infe, next) = read_box(buf, pos, b.end)?;
        if &infe.typ == b"infe" {
            let mut ir = Reader::new(buf, infe);
            let (iv, _) = ir.fullbox()?;
            // v0/v1 infe carries no item_type; HEIF files use v2/v3.
            if iv >= 2 {
                let item_id = if iv == 2 { ir.u16()? as u32 } else { ir.u32()? };
                let _protection = ir.u16()?;
                let item_type: [u8; 4] = ir.bytes(4)?.try_into().unwrap();
                meta.item_types.insert(item_id, item_type);
            }
        }
        pos = next;
    }
    Ok(())
}

fn parse_iref(buf: &[u8], b: BoxRef, meta: &mut Meta) -> Result<()> {
    let mut r = Reader::new(buf, b);
    let (v, _) = r.fullbox()?;
    let mut pos = r.pos;
    while pos + 8 <= b.end {
        let (refbox, next) = read_box(buf, pos, b.end)?;
        if &refbox.typ == b"dimg" {
            let mut rr = Reader::new(buf, refbox);
            let from = if v == 0 { rr.u16()? as u32 } else { rr.u32()? };
            let n = rr.u16()?;
            let mut to = Vec::with_capacity(n as usize);
            for _ in 0..n {
                to.push(if v == 0 { rr.u16()? as u32 } else { rr.u32()? });
            }
            meta.dimg.insert(from, to);
        }
        pos = next;
    }
    Ok(())
}

fn parse_iloc(buf: &[u8], b: BoxRef, meta: &mut Meta) -> Result<()> {
    let mut r = Reader::new(buf, b);
    let (v, _) = r.fullbox()?;
    let sizes = r.u8()?;
    let (offset_size, length_size) = ((sizes >> 4) as usize, (sizes & 0xf) as usize);
    let sizes2 = r.u8()?;
    let base_offset_size = (sizes2 >> 4) as usize;
    let index_size = if v >= 1 { (sizes2 & 0xf) as usize } else { 0 };
    let count = if v < 2 { r.u16()? as u32 } else { r.u32()? };
    for _ in 0..count {
        let item_id = if v < 2 { r.u16()? as u32 } else { r.u32()? };
        let construction_method = if v >= 1 { (r.u16()? & 0xf) as u8 } else { 0 };
        let data_reference_index = r.u16()?;
        let base_offset = r.uint(base_offset_size)?;
        let extent_count = r.u16()?;
        let mut extents = Vec::with_capacity(extent_count as usize);
        for _ in 0..extent_count {
            if index_size > 0 {
                let _extent_index = r.uint(index_size)?;
            }
            let off = r.uint(offset_size)?;
            let len = r.uint(length_size)?;
            extents.push((off, len));
        }
        // data_reference_index != 0 points at an external file — unsupported;
        // record nothing so lookups fail cleanly.
        if data_reference_index == 0 {
            meta.iloc
                .insert(item_id, ItemLocation { construction_method, base_offset, extents });
        }
    }
    Ok(())
}

fn parse_ipma(buf: &[u8], b: BoxRef, meta: &mut Meta) -> Result<()> {
    let mut r = Reader::new(buf, b);
    let (v, flags) = r.fullbox()?;
    let count = r.u32()?;
    for _ in 0..count {
        let item_id = if v < 1 { r.u16()? as u32 } else { r.u32()? };
        let n = r.u8()?;
        let mut props = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let idx = if flags & 1 != 0 { r.u16()? & 0x7fff } else { (r.u8()? & 0x7f) as u16 };
            if idx != 0 {
                props.push(idx);
            }
        }
        meta.ipma.insert(item_id, props);
    }
    Ok(())
}

// ----------------------------- item helpers -----------------------------

fn item_data(buf: &[u8], meta: &Meta, item_id: u32) -> Result<Vec<u8>> {
    let loc = meta
        .iloc
        .get(&item_id)
        .ok_or_else(|| err(format!("item {item_id} has no location")))?;
    let (src_base, src_end) = match loc.construction_method {
        0 => (0usize, buf.len()),
        1 => meta.idat.ok_or_else(|| err("iloc references missing idat"))?,
        m => return Err(err(format!("unsupported iloc construction method {m}"))),
    };
    let mut out = Vec::new();
    for (i, &(off, len)) in loc.extents.iter().enumerate() {
        let start = (src_base as u64)
            .checked_add(loc.base_offset)
            .and_then(|v| v.checked_add(off))
            .and_then(|v| usize::try_from(v).ok())
            .ok_or_else(|| err("extent offset overflow"))?;
        // A sole zero-length extent means "to the end of the data source".
        let end = if len == 0 && loc.extents.len() == 1 && i == 0 {
            src_end
        } else {
            start
                .checked_add(usize::try_from(len).map_err(|_| err("extent length overflow"))?)
                .ok_or_else(|| err("extent end overflow"))?
        };
        if end > src_end || start > end {
            return Err(err("extent out of bounds"));
        }
        out.extend_from_slice(&buf[start..end]);
    }
    Ok(out)
}

/// Properties of `item_id`, in association order.
fn item_props<'a>(buf: &'a [u8], meta: &Meta, item_id: u32) -> Vec<(BoxRef, &'a [u8])> {
    let Some(indices) = meta.ipma.get(&item_id) else {
        return Vec::new();
    };
    indices
        .iter()
        .filter_map(|&idx| meta.ipco.get(idx as usize - 1))
        .map(|&b| (b, &buf[b.start..b.end]))
        .collect()
}

fn find_ispe(buf: &[u8], meta: &Meta, item_id: u32) -> Option<(u32, u32)> {
    for (b, payload) in item_props(buf, meta, item_id) {
        // ispe is a FullBox: 4 bytes version/flags, then width and height.
        if &b.typ == b"ispe" && payload.len() >= 12 {
            let w = u32::from_be_bytes(payload[4..8].try_into().unwrap());
            let h = u32::from_be_bytes(payload[8..12].try_into().unwrap());
            return Some((w, h));
        }
    }
    None
}

struct HvcConfig {
    nal_length_size: usize,
    /// VPS/SPS/PPS NAL units, in stored order.
    param_sets: Vec<Vec<u8>>,
}

/// Parse an `HEVCDecoderConfigurationRecord` (the `hvcC` property payload).
fn parse_hvcc(data: &[u8]) -> Result<HvcConfig> {
    if data.len() < 23 {
        return Err(err("hvcC too short"));
    }
    let nal_length_size = (data[21] & 0x3) as usize + 1;
    let num_arrays = data[22] as usize;
    let mut param_sets = Vec::new();
    let mut pos = 23;
    for _ in 0..num_arrays {
        if pos + 3 > data.len() {
            return Err(err("hvcC truncated array header"));
        }
        let num_nalus = u16::from_be_bytes(data[pos + 1..pos + 3].try_into().unwrap()) as usize;
        pos += 3;
        for _ in 0..num_nalus {
            if pos + 2 > data.len() {
                return Err(err("hvcC truncated nalu length"));
            }
            let len = u16::from_be_bytes(data[pos..pos + 2].try_into().unwrap()) as usize;
            pos += 2;
            if pos + len > data.len() {
                return Err(err("hvcC truncated nalu"));
            }
            param_sets.push(data[pos..pos + len].to_vec());
            pos += len;
        }
    }
    if param_sets.is_empty() {
        return Err(err("hvcC has no parameter sets"));
    }
    Ok(HvcConfig { nal_length_size, param_sets })
}

fn find_hvcc(buf: &[u8], meta: &Meta, item_id: u32) -> Result<HvcConfig> {
    for (b, payload) in item_props(buf, meta, item_id) {
        if &b.typ == b"hvcC" {
            return parse_hvcc(payload);
        }
    }
    Err(err(format!("item {item_id} has no hvcC")))
}

const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Append one tile as a complete access unit: parameter sets + slice NALUs,
/// all with Annex-B start codes. Parameter sets are repeated per tile — the
/// decoder ignores duplicates, and this stays correct even if tiles were to
/// carry differing configurations.
fn append_tile_annexb(out: &mut Vec<u8>, cfg: &HvcConfig, item: &[u8]) -> Result<()> {
    for ps in &cfg.param_sets {
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(ps);
    }
    let mut pos = 0;
    while pos < item.len() {
        if pos + cfg.nal_length_size > item.len() {
            return Err(err("truncated NAL length"));
        }
        let mut len = 0usize;
        for &b in &item[pos..pos + cfg.nal_length_size] {
            len = (len << 8) | b as usize;
        }
        pos += cfg.nal_length_size;
        if len == 0 || pos + len > item.len() {
            return Err(err("truncated NAL unit"));
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&item[pos..pos + len]);
        pos += len;
    }
    Ok(())
}

/// `grid` item payload: tile layout and the final (cropped) output size.
fn parse_grid(data: &[u8]) -> Result<(u32, u32, u32, u32)> {
    if data.len() < 4 {
        return Err(err("grid payload too short"));
    }
    let flags = data[1];
    let rows = data[2] as u32 + 1;
    let cols = data[3] as u32 + 1;
    let (w, h) = if flags & 1 != 0 {
        if data.len() < 12 {
            return Err(err("grid payload too short for 32-bit fields"));
        }
        (
            u32::from_be_bytes(data[4..8].try_into().unwrap()),
            u32::from_be_bytes(data[8..12].try_into().unwrap()),
        )
    } else {
        if data.len() < 8 {
            return Err(err("grid payload too short for 16-bit fields"));
        }
        (
            u16::from_be_bytes(data[4..6].try_into().unwrap()) as u32,
            u16::from_be_bytes(data[6..8].try_into().unwrap()) as u32,
        )
    };
    Ok((rows, cols, w, h))
}

// ----------------------------- entry point -----------------------------

/// Locate the container's PRIMARY image and return its HEVC payload as an
/// Annex-B stream plus reassembly metadata. Depth maps, gain maps, thumbnails
/// and other auxiliary items are ignored by construction.
pub fn parse_primary(buf: &[u8]) -> Result<PrimaryImage> {
    let meta = parse_meta(buf)?;
    let primary = meta.primary_id.ok_or_else(|| err("no primary item (pitm)"))?;
    let ptype = *meta
        .item_types
        .get(&primary)
        .ok_or_else(|| err("primary item has no infe entry"))?;

    let (tile_ids, rows, cols, out_w, out_h) = match &ptype {
        b"grid" => {
            let (rows, cols, w, h) = parse_grid(&item_data(buf, &meta, primary)?)?;
            let tiles = meta
                .dimg
                .get(&primary)
                .cloned()
                .ok_or_else(|| err("grid primary has no dimg references"))?;
            if tiles.len() != (rows as usize) * (cols as usize) {
                return Err(err(format!(
                    "grid tile count mismatch: {} refs for {rows}x{cols}",
                    tiles.len()
                )));
            }
            (tiles, rows, cols, w, h)
        }
        b"hvc1" => {
            let (w, h) =
                find_ispe(buf, &meta, primary).ok_or_else(|| err("primary item has no ispe"))?;
            (vec![primary], 1, 1, w, h)
        }
        other => {
            return Err(err(format!(
                "unsupported primary item type {:?}",
                String::from_utf8_lossy(other)
            )))
        }
    };

    let (tile_width, tile_height) = find_ispe(buf, &meta, tile_ids[0])
        .ok_or_else(|| err("tile item has no ispe"))?;
    if tile_width == 0 || tile_height == 0 || out_w == 0 || out_h == 0 {
        return Err(err("zero image dimensions"));
    }

    let mut annexb = Vec::new();
    for &tile in &tile_ids {
        if meta.item_types.get(&tile).map(|t| t != b"hvc1").unwrap_or(true) {
            return Err(err("non-hvc1 tile"));
        }
        let cfg = find_hvcc(buf, &meta, tile)?;
        let data = item_data(buf, &meta, tile)?;
        append_tile_annexb(&mut annexb, &cfg, &data)?;
    }

    // Transformative properties of the PRIMARY item, in association order.
    let transforms = item_props(buf, &meta, primary)
        .into_iter()
        .filter_map(|(b, payload)| match (&b.typ, payload.first()) {
            (b"irot", Some(&v)) if v & 3 != 0 => Some(Transform::Rotate(v & 3)),
            (b"imir", Some(&v)) => Some(if v & 1 == 0 {
                Transform::MirrorVertical
            } else {
                Transform::MirrorHorizontal
            }),
            _ => None,
        })
        .collect();

    Ok(PrimaryImage {
        annexb,
        tile_count: tile_ids.len() as u32,
        tile_width,
        tile_height,
        rows,
        cols,
        output_width: out_w,
        output_height: out_h,
        transforms,
    })
}

// ----------------------------- tests -----------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize a box with the given type and payload.
    fn mkbox(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&(8 + payload.len() as u32).to_be_bytes());
        b.extend_from_slice(typ);
        b.extend_from_slice(payload);
        b
    }

    fn fullbox(typ: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
        let mut p = vec![version];
        p.extend_from_slice(&flags.to_be_bytes()[1..]);
        p.extend_from_slice(body);
        mkbox(typ, &p)
    }

    /// A synthetic two-tile (1 row × 2 cols) grid HEIC. The "HEVC" payloads are
    /// dummies — the parser never decodes video, it only slices the container.
    /// Layout mimics an iPhone file: primary=grid(id 1) → tiles 2,3; an extra
    /// "auxiliary" hvc1 item (id 4) that must NOT be picked; iloc v1 with
    /// construction methods 0 (mdat) and 1 (idat, for the grid payload).
    fn synthetic_heic() -> Vec<u8> {
        let ftyp = mkbox(b"ftyp", b"heic\0\0\0\0mif1heic");

        // grid payload (in idat): 1x2, output 6x4 (16-bit fields)
        let grid_payload: &[u8] = &[0, 0, 0, 1, 0, 6, 0, 4];

        // infe entries (v2): ids 1..4
        let infe = |id: u16, typ: &[u8; 4]| {
            let mut body = Vec::new();
            body.extend_from_slice(&id.to_be_bytes());
            body.extend_from_slice(&0u16.to_be_bytes()); // protection
            body.extend_from_slice(typ);
            body.push(0); // empty item_name
            fullbox(b"infe", 2, 0, &body)
        };
        let mut iinf_body = 4u16.to_be_bytes().to_vec();
        for b in [infe(1, b"grid"), infe(2, b"hvc1"), infe(3, b"hvc1"), infe(4, b"hvc1")] {
            iinf_body.extend_from_slice(&b);
        }
        let iinf = fullbox(b"iinf", 0, 0, &iinf_body);

        let pitm = fullbox(b"pitm", 0, 0, &1u16.to_be_bytes());

        // iref v0: dimg from 1 -> [2, 3]
        let mut dimg_body = Vec::new();
        dimg_body.extend_from_slice(&1u16.to_be_bytes());
        dimg_body.extend_from_slice(&2u16.to_be_bytes());
        dimg_body.extend_from_slice(&2u16.to_be_bytes());
        dimg_body.extend_from_slice(&3u16.to_be_bytes());
        let iref = fullbox(b"iref", 0, 0, &mkbox(b"dimg", &dimg_body));

        // ipco: [1]=hvcC, [2]=ispe(tile 3x4), [3]=ispe(grid 6x4), [4]=irot(1)
        // hvcC: 22 bytes header with lengthSizeMinusOne=3, 1 array, 1 "SPS" [0xAA,0xBB]
        let mut hvcc = vec![0u8; 22];
        hvcc[21] = 0x03; // 4-byte NAL lengths
        hvcc.push(1); // numArrays
        hvcc.extend_from_slice(&[0x21, 0x00, 0x01, 0x00, 0x02, 0xAA, 0xBB]);
        let ispe = |w: u32, h: u32| {
            let mut body = Vec::new();
            body.extend_from_slice(&w.to_be_bytes());
            body.extend_from_slice(&h.to_be_bytes());
            fullbox(b"ispe", 0, 0, &body)
        };
        let mut ipco_body = Vec::new();
        ipco_body.extend_from_slice(&mkbox(b"hvcC", &hvcc));
        ipco_body.extend_from_slice(&ispe(3, 4));
        ipco_body.extend_from_slice(&ispe(6, 4));
        ipco_body.extend_from_slice(&mkbox(b"irot", &[1]));
        let ipco = mkbox(b"ipco", &ipco_body);

        // ipma v0 flags0: item1 -> [3(ispe grid), 4(irot)]; items 2,3,4 -> [1(hvcC), 2(ispe)]
        let mut ipma_body = 4u32.to_be_bytes().to_vec();
        for (id, props) in [(1u16, vec![3u8, 4]), (2, vec![1, 2]), (3, vec![1, 2]), (4, vec![1, 2])]
        {
            ipma_body.extend_from_slice(&id.to_be_bytes());
            ipma_body.push(props.len() as u8);
            ipma_body.extend_from_slice(&props);
        }
        let ipma = fullbox(b"ipma", 0, 0, &ipma_body);
        let mut iprp_body = ipco;
        iprp_body.extend_from_slice(&ipma);
        let iprp = mkbox(b"iprp", &iprp_body);

        let idat = mkbox(b"idat", grid_payload);

        // Tile payloads in mdat: 4-byte NAL length + body.
        let tile2: &[u8] = &[0, 0, 0, 3, 0x11, 0x22, 0x33];
        let tile3: &[u8] = &[0, 0, 0, 2, 0x44, 0x55];
        let aux4: &[u8] = &[0, 0, 0, 1, 0x99];
        let mut mdat_payload = Vec::new();
        mdat_payload.extend_from_slice(tile2);
        mdat_payload.extend_from_slice(tile3);
        mdat_payload.extend_from_slice(aux4);
        let mdat = mkbox(b"mdat", &mdat_payload);

        // iloc v1: item1 via idat (method 1), items 2-4 via absolute offsets
        // (method 0). Offsets into mdat are computed after layout below.
        let build = |mdat_start: usize| {
            let mut body = Vec::new();
            body.push(0x44); // offset_size=4, length_size=4
            body.push(0x40); // base_offset_size=4, index_size=0
            body.extend_from_slice(&4u16.to_be_bytes()); // item_count
            let mut item = |id: u16, method: u16, base: u32, off: u32, len: u32| {
                body.extend_from_slice(&id.to_be_bytes());
                body.extend_from_slice(&method.to_be_bytes());
                body.extend_from_slice(&0u16.to_be_bytes()); // data_reference_index
                body.extend_from_slice(&base.to_be_bytes());
                body.extend_from_slice(&1u16.to_be_bytes()); // extent_count
                body.extend_from_slice(&off.to_be_bytes());
                body.extend_from_slice(&len.to_be_bytes());
            };
            item(1, 1, 0, 0, 8); // grid payload in idat
            let m = mdat_start as u32;
            item(2, 0, m, 0, 7);
            item(3, 0, m, 7, 6);
            item(4, 0, m, 13, 5);
            fullbox(b"iloc", 1, 0, &body)
        };

        // Two-pass layout: sizes are fixed, only mdat's start must be known.
        let assemble = |iloc: Vec<u8>| {
            let mut meta_body = Vec::new();
            meta_body.extend_from_slice(&[0, 0, 0, 0]); // meta FullBox header
            for b in [&pitm, &iinf, &iref, &iprp, &idat, &iloc] {
                meta_body.extend_from_slice(b);
            }
            let meta = mkbox(b"meta", &meta_body);
            let mut file = ftyp.clone();
            file.extend_from_slice(&meta);
            let mdat_payload_start = file.len() + 8;
            file.extend_from_slice(&mdat);
            (file, mdat_payload_start)
        };
        let (_, guess) = assemble(build(0));
        let (file, actual) = assemble(build(guess));
        assert_eq!(guess, actual, "layout must be stable across passes");
        file
    }

    #[test]
    fn sniffs_heif_brands() {
        assert!(is_heif(&synthetic_heic()));
        assert!(!is_heif(b"\xff\xd8\xff\xe0 not a heic ......"));
        assert!(!is_heif(&mkbox(b"ftyp", b"avif\0\0\0\0")));
    }

    #[test]
    fn parses_grid_primary_ignoring_auxiliary_items() {
        let file = synthetic_heic();
        let img = parse_primary(&file).expect("parse");
        assert_eq!((img.rows, img.cols), (1, 2));
        assert_eq!((img.tile_width, img.tile_height), (3, 4));
        assert_eq!((img.output_width, img.output_height), (6, 4));
        assert_eq!(img.tile_count, 2);
        assert_eq!(img.transforms, vec![Transform::Rotate(1)]);
        // Annex-B stream: per tile, the SPS then the slice — and never the
        // auxiliary item 4's payload (0x99).
        let sc = &[0u8, 0, 0, 1][..];
        let mut expected = Vec::new();
        expected.extend_from_slice(sc);
        expected.extend_from_slice(&[0xAA, 0xBB]);
        expected.extend_from_slice(sc);
        expected.extend_from_slice(&[0x11, 0x22, 0x33]);
        expected.extend_from_slice(sc);
        expected.extend_from_slice(&[0xAA, 0xBB]);
        expected.extend_from_slice(sc);
        expected.extend_from_slice(&[0x44, 0x55]);
        assert_eq!(img.annexb, expected);
        assert!(!img.annexb.contains(&0x99));
    }

    #[test]
    fn rejects_missing_primary() {
        // Strip pitm by building a file whose meta lacks it: simplest check —
        // corrupt the pitm box type so it's skipped.
        let mut file = synthetic_heic();
        let pos = file
            .windows(4)
            .position(|w| w == b"pitm")
            .expect("pitm present");
        file[pos..pos + 4].copy_from_slice(b"xxxx");
        assert!(parse_primary(&file).is_err());
    }
}
