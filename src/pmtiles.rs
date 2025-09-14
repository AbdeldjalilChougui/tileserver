//! PMTiles v3 reader (and a small writer for fixtures), written from the
//! spec: <https://github.com/protomaps/PMTiles/blob/main/spec/v3/spec.md>.
//!
//! A PMTiles archive is a 127-byte header, a root directory, JSON metadata,
//! optional leaf directories and the tile data. Tiles are addressed by a
//! single integer "tile id": the number of tiles in all lower zoom levels
//! plus the tile's position along a Hilbert curve at its own zoom.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Mutex;

use flate2::Compression as GzLevel;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

use crate::source::{Error, Result, TileFormat};

pub const HEADER_LEN: usize = 127;
const MAGIC: &[u8; 7] = b"PMTiles";

/// Compression codes from the spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    Unknown,
    None,
    Gzip,
    Brotli,
    Zstd,
}

impl Compression {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::None,
            2 => Self::Gzip,
            3 => Self::Brotli,
            4 => Self::Zstd,
            _ => Self::Unknown,
        }
    }
    fn to_u8(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::None => 1,
            Self::Gzip => 2,
            Self::Brotli => 3,
            Self::Zstd => 4,
        }
    }
    /// Value for the HTTP `Content-Encoding` header, if any.
    pub fn content_encoding(self) -> Option<&'static str> {
        match self {
            Self::Gzip => Some("gzip"),
            Self::Brotli => Some("br"),
            Self::Zstd => Some("zstd"),
            _ => None,
        }
    }
}

fn tile_type(v: u8) -> TileFormat {
    match v {
        1 => TileFormat::Mvt,
        2 => TileFormat::Png,
        3 => TileFormat::Jpeg,
        4 => TileFormat::Webp,
        5 => TileFormat::Avif,
        _ => TileFormat::Unknown,
    }
}

fn tile_type_code(f: TileFormat) -> u8 {
    match f {
        TileFormat::Mvt => 1,
        TileFormat::Png => 2,
        TileFormat::Jpeg => 3,
        TileFormat::Webp => 4,
        TileFormat::Avif => 5,
        TileFormat::Unknown => 0,
    }
}

/// The fixed-size header.
#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub root_offset: u64,
    pub root_length: u64,
    pub metadata_offset: u64,
    pub metadata_length: u64,
    pub leaf_offset: u64,
    pub leaf_length: u64,
    pub data_offset: u64,
    pub data_length: u64,
    pub addressed_tiles: u64,
    pub tile_entries: u64,
    pub tile_contents: u64,
    pub clustered: bool,
    pub internal_compression: Compression,
    pub tile_compression: Compression,
    pub tile_type: TileFormat,
    pub min_zoom: u8,
    pub max_zoom: u8,
    /// [west, south, east, north] in degrees.
    pub bounds: [f64; 4],
    pub center_zoom: u8,
    pub center: [f64; 2],
}

impl Header {
    pub fn parse(b: &[u8]) -> Result<Self> {
        if b.len() < HEADER_LEN || &b[0..7] != MAGIC {
            return Err(Error::Format("not a PMTiles file".into()));
        }
        if b[7] != 3 {
            return Err(Error::Format(format!("unsupported PMTiles version {}", b[7])));
        }
        let u64_at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        let e7 = |i: usize| i32::from_le_bytes(b[i..i + 4].try_into().unwrap()) as f64 / 1e7;
        Ok(Header {
            root_offset: u64_at(8),
            root_length: u64_at(16),
            metadata_offset: u64_at(24),
            metadata_length: u64_at(32),
            leaf_offset: u64_at(40),
            leaf_length: u64_at(48),
            data_offset: u64_at(56),
            data_length: u64_at(64),
            addressed_tiles: u64_at(72),
            tile_entries: u64_at(80),
            tile_contents: u64_at(88),
            clustered: b[96] == 1,
            internal_compression: Compression::from_u8(b[97]),
            tile_compression: Compression::from_u8(b[98]),
            tile_type: tile_type(b[99]),
            min_zoom: b[100],
            max_zoom: b[101],
            bounds: [e7(102), e7(106), e7(110), e7(114)],
            center_zoom: b[118],
            center: [e7(119), e7(123)],
        })
    }

    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0..7].copy_from_slice(MAGIC);
        b[7] = 3;
        let fields = [
            self.root_offset,
            self.root_length,
            self.metadata_offset,
            self.metadata_length,
            self.leaf_offset,
            self.leaf_length,
            self.data_offset,
            self.data_length,
            self.addressed_tiles,
            self.tile_entries,
            self.tile_contents,
        ];
        for (i, v) in fields.iter().enumerate() {
            b[8 + i * 8..16 + i * 8].copy_from_slice(&v.to_le_bytes());
        }
        b[96] = self.clustered as u8;
        b[97] = self.internal_compression.to_u8();
        b[98] = self.tile_compression.to_u8();
        b[99] = tile_type_code(self.tile_type);
        b[100] = self.min_zoom;
        b[101] = self.max_zoom;
        let e7 = |v: f64| ((v * 1e7).round() as i32).to_le_bytes();
        for (i, v) in self.bounds.iter().enumerate() {
            b[102 + i * 4..106 + i * 4].copy_from_slice(&e7(*v));
        }
        b[118] = self.center_zoom;
        b[119..123].copy_from_slice(&e7(self.center[0]));
        b[123..127].copy_from_slice(&e7(self.center[1]));
        b
    }
}

// ------------------------------------------------------------------ tile ids

/// Hilbert-curve tile id for z/x/y (spec section "Tile IDs").
pub fn zxy_to_id(z: u8, x: u32, y: u32) -> u64 {
    let mut base: u64 = 0;
    for i in 0..z {
        base += 1u64 << (2 * i as u64); // 4^i tiles at zoom i
    }
    let n: u64 = 1 << z;
    let (mut x, mut y) = (x as u64, y as u64);
    let mut d: u64 = 0;
    let mut s = n / 2;
    while s > 0 {
        let rx = u64::from(x & s > 0);
        let ry = u64::from(y & s > 0);
        d += s * s * ((3 * rx) ^ ry);
        // rotate the quadrant
        if ry == 0 {
            if rx == 1 {
                x = n - 1 - x;
                y = n - 1 - y;
            }
            std::mem::swap(&mut x, &mut y);
        }
        s /= 2;
    }
    base + d
}

/// Inverse of [`zxy_to_id`].
pub fn id_to_zxy(id: u64) -> (u8, u32, u32) {
    let mut z = 0u8;
    let mut acc = 0u64;
    loop {
        let count = 1u64 << (2 * z as u64);
        if id < acc + count {
            break;
        }
        acc += count;
        z += 1;
    }
    let n: u64 = 1 << z;
    let mut t = id - acc;
    let (mut x, mut y) = (0u64, 0u64);
    let mut s = 1u64;
    while s < n {
        let rx = 1 & (t / 2);
        let ry = 1 & (t ^ rx);
        if ry == 0 {
            if rx == 1 {
                x = s - 1 - x;
                y = s - 1 - y;
            }
            std::mem::swap(&mut x, &mut y);
        }
        x += s * rx;
        y += s * ry;
        t /= 4;
        s *= 2;
    }
    (z, x as u32, y as u32)
}

// --------------------------------------------------------------- directories

/// One directory entry. `run_length == 0` marks a pointer to a leaf directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub tile_id: u64,
    pub offset: u64,
    pub length: u32,
    pub run_length: u32,
}

fn read_varint(b: &[u8], pos: &mut usize) -> Result<u64> {
    let mut v: u64 = 0;
    let mut shift = 0;
    loop {
        let byte = *b
            .get(*pos)
            .ok_or_else(|| Error::Format("truncated varint".into()))?;
        *pos += 1;
        v |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
        if shift > 63 {
            return Err(Error::Format("varint too long".into()));
        }
    }
}

fn write_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Decodes an uncompressed directory.
pub fn decode_directory(b: &[u8]) -> Result<Vec<Entry>> {
    let mut pos = 0;
    let n = read_varint(b, &mut pos)? as usize;
    if n > b.len() {
        return Err(Error::Format("directory entry count too large".into()));
    }
    let mut entries = vec![
        Entry {
            tile_id: 0,
            offset: 0,
            length: 0,
            run_length: 0
        };
        n
    ];
    let mut last = 0u64;
    for e in entries.iter_mut() {
        last += read_varint(b, &mut pos)?;
        e.tile_id = last;
    }
    for e in entries.iter_mut() {
        e.run_length = read_varint(b, &mut pos)? as u32;
    }
    for e in entries.iter_mut() {
        e.length = read_varint(b, &mut pos)? as u32;
    }
    for i in 0..n {
        let v = read_varint(b, &mut pos)?;
        entries[i].offset = if v == 0 && i > 0 {
            entries[i - 1].offset + entries[i - 1].length as u64
        } else {
            v.checked_sub(1)
                .ok_or_else(|| Error::Format("bad offset".into()))?
        };
    }
    Ok(entries)
}

/// Encodes a directory (entries sorted by tile id).
pub fn encode_directory(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    write_varint(&mut out, entries.len() as u64);
    let mut last = 0;
    for e in entries {
        write_varint(&mut out, e.tile_id - last);
        last = e.tile_id;
    }
    for e in entries {
        write_varint(&mut out, e.run_length as u64);
    }
    for e in entries {
        write_varint(&mut out, e.length as u64);
    }
    for (i, e) in entries.iter().enumerate() {
        if i > 0 && e.offset == entries[i - 1].offset + entries[i - 1].length as u64 {
            write_varint(&mut out, 0);
        } else {
            write_varint(&mut out, e.offset + 1);
        }
    }
    out
}

/// Finds the entry covering `tile_id` (binary search for the last entry whose
/// id is <= tile_id).
pub fn find(entries: &[Entry], tile_id: u64) -> Option<Entry> {
    let idx = entries.partition_point(|e| e.tile_id <= tile_id);
    if idx == 0 {
        return None;
    }
    let e = entries[idx - 1];
    if e.run_length == 0 || tile_id < e.tile_id + e.run_length as u64 {
        Some(e)
    } else {
        None
    }
}

fn decompress(data: Vec<u8>, c: Compression) -> Result<Vec<u8>> {
    match c {
        Compression::None | Compression::Unknown => Ok(data),
        Compression::Gzip => {
            let mut out = Vec::new();
            GzDecoder::new(&data[..])
                .read_to_end(&mut out)
                .map_err(|e| Error::Format(format!("gzip: {e}")))?;
            Ok(out)
        }
        other => Err(Error::Format(format!(
            "internal compression {other:?} not supported"
        ))),
    }
}

// -------------------------------------------------------------------- reader

/// An open archive. Reads use positional I/O (`pread`), so it is `Sync`;
/// decoded leaf directories are cached.
pub struct PmTiles {
    file: File,
    pub header: Header,
    root: Vec<Entry>,
    pub metadata: serde_json::Value,
    leaves: Mutex<HashMap<(u64, u64), std::sync::Arc<Vec<Entry>>>>,
}

impl PmTiles {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let mut hb = [0u8; HEADER_LEN];
        file.read_exact_at(&mut hb, 0)?;
        let header = Header::parse(&hb)?;
        let read = |off: u64, len: u64| -> Result<Vec<u8>> {
            if len > 64 << 20 {
                return Err(Error::Format("section larger than 64 MiB".into()));
            }
            let mut buf = vec![0u8; len as usize];
            file.read_exact_at(&mut buf, off)?;
            Ok(buf)
        };
        let root = decode_directory(&decompress(
            read(header.root_offset, header.root_length)?,
            header.internal_compression,
        )?)?;
        let meta_bytes = decompress(
            read(header.metadata_offset, header.metadata_length)?,
            header.internal_compression,
        )?;
        let metadata = if meta_bytes.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_slice(&meta_bytes)
                .map_err(|e| Error::Format(format!("metadata JSON: {e}")))?
        };
        Ok(PmTiles {
            file,
            header,
            root,
            metadata,
            leaves: Mutex::new(HashMap::new()),
        })
    }

    fn read(&self, off: u64, len: u64) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len as usize];
        self.file.read_exact_at(&mut buf, off)?;
        Ok(buf)
    }

    fn leaf(&self, offset: u64, length: u64) -> Result<std::sync::Arc<Vec<Entry>>> {
        if let Some(d) = self.leaves.lock().unwrap().get(&(offset, length)) {
            return Ok(d.clone());
        }
        let raw = self.read(self.header.leaf_offset + offset, length)?;
        let dir = std::sync::Arc::new(decode_directory(&decompress(
            raw,
            self.header.internal_compression,
        )?)?);
        let mut cache = self.leaves.lock().unwrap();
        if cache.len() > 4096 {
            cache.clear(); // crude bound; leaves are cheap to re-read
        }
        cache.insert((offset, length), dir.clone());
        Ok(dir)
    }

    /// Raw (possibly compressed) tile bytes, or `None` if absent.
    pub fn tile(&self, z: u8, x: u32, y: u32) -> Result<Option<Vec<u8>>> {
        let id = zxy_to_id(z, x, y);
        let mut dir_entries: std::sync::Arc<Vec<Entry>> = std::sync::Arc::new(self.root.clone());
        // Spec: at most 3 levels of leaves below the root; allow a little slack.
        for _ in 0..4 {
            match find(&dir_entries, id) {
                None => return Ok(None),
                Some(e) if e.run_length > 0 => {
                    return self
                        .read(self.header.data_offset + e.offset, e.length as u64)
                        .map(Some);
                }
                Some(e) => dir_entries = self.leaf(e.offset, e.length as u64)?,
            }
        }
        Err(Error::Format("directory nesting too deep".into()))
    }
}

// -------------------------------------------------------------------- writer

/// Builds a PMTiles archive in memory. Identical tile contents are stored
/// once, and consecutive tile ids with the same contents become one run, as
/// the spec recommends. When the root directory would hold more than
/// `max_root_entries`, entries are split into leaf directories. Intended for
/// fixtures and small datasets.
pub struct Writer {
    pub tile_type: TileFormat,
    pub tile_compression: Compression,
    pub metadata: serde_json::Value,
    pub max_root_entries: usize,
    tiles: Vec<(u64, Vec<u8>)>,
}

impl Writer {
    pub fn new(tile_type: TileFormat, tile_compression: Compression) -> Self {
        Writer {
            tile_type,
            tile_compression,
            metadata: serde_json::json!({}),
            max_root_entries: 1000,
            tiles: Vec::new(),
        }
    }

    pub fn add(&mut self, z: u8, x: u32, y: u32, data: Vec<u8>) {
        self.tiles.push((zxy_to_id(z, x, y), data));
    }

    pub fn write_to(mut self, path: &Path) -> Result<()> {
        self.tiles.sort_by_key(|t| t.0);
        let mut data = Vec::new();
        let mut entries: Vec<Entry> = Vec::new();
        let mut seen: HashMap<Vec<u8>, (u64, u32)> = HashMap::new();
        for (id, bytes) in &self.tiles {
            if let Some(last) = entries.last_mut() {
                let same = seen.get(bytes).map(|&(o, _)| o) == Some(last.offset);
                if same && last.tile_id + last.run_length as u64 == *id {
                    last.run_length += 1;
                    continue;
                }
            }
            let (offset, length) = *seen.entry(bytes.clone()).or_insert_with(|| {
                let o = data.len() as u64;
                data.extend_from_slice(bytes);
                (o, bytes.len() as u32)
            });
            entries.push(Entry {
                tile_id: *id,
                offset,
                length,
                run_length: 1,
            });
        }

        let gz = |b: &[u8]| -> Vec<u8> {
            let mut e = GzEncoder::new(Vec::new(), GzLevel::default());
            e.write_all(b).unwrap();
            e.finish().unwrap()
        };
        let (root, leaves) = if entries.len() <= self.max_root_entries {
            (gz(&encode_directory(&entries)), Vec::new())
        } else {
            let chunk = entries.len().div_ceil(self.max_root_entries).max(self.max_root_entries);
            let mut leaves = Vec::new();
            let mut root_entries = Vec::new();
            for part in entries.chunks(chunk) {
                let enc = gz(&encode_directory(part));
                root_entries.push(Entry {
                    tile_id: part[0].tile_id,
                    offset: leaves.len() as u64,
                    length: enc.len() as u32,
                    run_length: 0,
                });
                leaves.extend_from_slice(&enc);
            }
            (gz(&encode_directory(&root_entries)), leaves)
        };
        let meta = gz(&serde_json::to_vec(&self.metadata).unwrap());

        let (mut minz, mut maxz) = (u8::MAX, 0u8);
        for (id, _) in &self.tiles {
            let (z, _, _) = id_to_zxy(*id);
            minz = minz.min(z);
            maxz = maxz.max(z);
        }
        let root_offset = HEADER_LEN as u64;
        let metadata_offset = root_offset + root.len() as u64;
        let leaf_offset = metadata_offset + meta.len() as u64;
        let data_offset = leaf_offset + leaves.len() as u64;
        let header = Header {
            root_offset,
            root_length: root.len() as u64,
            metadata_offset,
            metadata_length: meta.len() as u64,
            leaf_offset,
            leaf_length: leaves.len() as u64,
            data_offset,
            data_length: data.len() as u64,
            addressed_tiles: self.tiles.len() as u64,
            tile_entries: entries.len() as u64,
            tile_contents: seen.len() as u64,
            clustered: true,
            internal_compression: Compression::Gzip,
            tile_compression: self.tile_compression,
            tile_type: self.tile_type,
            min_zoom: if self.tiles.is_empty() { 0 } else { minz },
            max_zoom: maxz,
            bounds: [-180.0, -85.051129, 180.0, 85.051129],
            center_zoom: minz.min(maxz),
            center: [0.0, 0.0],
        };
        let mut f = File::create(path)?;
        f.write_all(&header.to_bytes())?;
        f.write_all(&root)?;
        f.write_all(&meta)?;
        f.write_all(&leaves)?;
        f.write_all(&data)?;
        Ok(())
    }
}
