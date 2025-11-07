//! Generates small demo archives: raster PNG tiles (a coloured grid with the
//! tile's own z/x/y drawn as bars) and a few hand-encoded vector tiles, as
//! both PMTiles and MBTiles. No external data.

use std::io::Write;
use std::path::Path;

use flate2::Compression as GzLevel;
use flate2::write::{GzEncoder, ZlibEncoder};
use serde_json::json;

use crate::mbtiles;
use crate::pmtiles::{Compression, Writer};
use crate::source::{Result, TileFormat};

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for b in data {
        crc ^= *b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// Minimal PNG encoder: 8-bit RGB, no filtering.
pub fn png(width: u32, height: u32, rgb: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((width * 3 + 1) as usize * height as usize);
    for row in rgb.chunks(width as usize * 3) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut z = ZlibEncoder::new(Vec::new(), GzLevel::default());
    z.write_all(&raw).unwrap();
    let idat = z.finish().unwrap();
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut chunk = |kind: &[u8], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut c = kind.to_vec();
        c.extend_from_slice(data);
        out.extend_from_slice(&c);
        out.extend_from_slice(&crc32(&c).to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &idat);
    chunk(b"IEND", &[]);
    out
}

/// A 256x256 raster tile: hue by zoom, a border, and z/x/y as bar counts.
pub fn raster_tile(z: u8, x: u32, y: u32) -> Vec<u8> {
    let palette = [[66, 133, 244], [52, 168, 83], [251, 188, 5], [234, 67, 53], [155, 81, 224], [0, 172, 193]];
    let base = palette[z as usize % palette.len()];
    let mut px = vec![0u8; 256 * 256 * 3];
    for j in 0..256u32 {
        for i in 0..256u32 {
            let border = i < 2 || j < 2;
            let checker = ((i / 32) + (j / 32)) % 2 == 0;
            let bar = |row: u32, count: u32| j / 16 == row && i >= 16 && i < 16 + (count.min(14)) * 16 && (i - 16) % 16 < 12;
            let ink = bar(4, z as u32 + 1) || bar(6, x % 15 + 1) || bar(8, y % 15 + 1);
            let c = if border || ink {
                [20, 20, 20]
            } else if checker {
                base
            } else {
                [base[0] / 2 + 100, base[1] / 2 + 100, base[2] / 2 + 100]
            };
            let k = ((j * 256 + i) * 3) as usize;
            px[k..k + 3].copy_from_slice(&c);
        }
    }
    png(256, 256, &px)
}

// --- a hand-written MVT encoder, just enough for points --------------------

fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}
fn field_bytes(out: &mut Vec<u8>, field: u32, b: &[u8]) {
    varint(out, ((field << 3) | 2) as u64);
    varint(out, b.len() as u64);
    out.extend_from_slice(b);
}
fn field_varint(out: &mut Vec<u8>, field: u32, v: u64) {
    varint(out, (field << 3) as u64); // wire type 0 (varint)
    varint(out, v);
}
fn zz(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

/// One MVT layer "cities" with point features (tile-local coordinates in
/// 0..4096) and a "name" string property.
pub fn mvt_points(points: &[(i64, i64, &str)]) -> Vec<u8> {
    let mut layer = Vec::new();
    field_varint(&mut layer, 15, 2); // version
    field_bytes(&mut layer, 1, b"cities"); // name
    for (i, (px, py, _)) in points.iter().enumerate() {
        let mut f = Vec::new();
        field_varint(&mut f, 1, i as u64 + 1); // id
        let mut tags = Vec::new();
        varint(&mut tags, 0); // key index 0 = "name"
        varint(&mut tags, i as u64); // value index i
        field_bytes(&mut f, 2, &tags);
        field_varint(&mut f, 3, 1); // POINT
        let mut geom = Vec::new();
        varint(&mut geom, (1 << 3) | 1); // MoveTo, count 1
        varint(&mut geom, zz(*px));
        varint(&mut geom, zz(*py));
        field_bytes(&mut f, 4, &geom);
        field_bytes(&mut layer, 2, &f);
    }
    field_bytes(&mut layer, 3, b"name"); // keys
    for (_, _, name) in points {
        let mut v = Vec::new();
        field_bytes(&mut v, 1, name.as_bytes()); // string_value
        field_bytes(&mut layer, 4, &v);
    }
    field_varint(&mut layer, 5, 4096); // extent
    let mut tile = Vec::new();
    field_bytes(&mut tile, 3, &layer);
    tile
}

pub fn gzip(b: &[u8]) -> Vec<u8> {
    let mut e = GzEncoder::new(Vec::new(), GzLevel::default());
    e.write_all(b).unwrap();
    e.finish().unwrap()
}

/// Approximate positions (lon, lat) of a few Algerian cities.
pub const CITIES: [(&str, f64, f64); 6] = [
    ("Alger", 3.0588, 36.7538),
    ("Oran", -0.6417, 35.6971),
    ("Constantine", 6.6147, 36.365),
    ("Annaba", 7.7667, 36.9),
    ("Ghardaïa", 3.6739, 32.4911),
    ("Tamanrasset", 5.5228, 22.785),
];

/// Lon/lat -> (tile x, tile y, pixel x, pixel y) at zoom z, extent 4096.
pub fn project(lon: f64, lat: f64, z: u8) -> (u32, u32, i64, i64) {
    let n = (1u64 << z) as f64;
    let fx = (lon + 180.0) / 360.0 * n;
    let r = lat.to_radians();
    let fy = (1.0 - (r.tan() + 1.0 / r.cos()).ln() / std::f64::consts::PI) / 2.0 * n;
    let (tx, ty) = (fx.floor() as u32, fy.floor() as u32);
    (tx, ty, ((fx - tx as f64) * 4096.0) as i64, ((fy - ty as f64) * 4096.0) as i64)
}

/// Writes `grid.pmtiles`, `grid-mb.mbtiles` (raster z0-3) and `cities.pmtiles`,
/// `cities-mb.mbtiles` (gzip MVT z0-6) into `dir`.
pub fn write_all(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut raster = Vec::new();
    for z in 0..=3u8 {
        for x in 0..(1u32 << z) {
            for y in 0..(1u32 << z) {
                raster.push((z, x, y, raster_tile(z, x, y)));
            }
        }
    }
    let mut w = Writer::new(TileFormat::Png, Compression::None);
    w.metadata = json!({"name": "Debug grid", "attribution": "generated"});
    for (z, x, y, d) in &raster {
        w.add(*z, *x, *y, d.clone());
    }
    w.write_to(&dir.join("grid.pmtiles"))?;
    mbtiles::write(
        &dir.join("grid-mb.mbtiles"),
        &[("name", "Debug grid (MBTiles)".into()), ("format", "png".into()), ("minzoom", "0".into()), ("maxzoom", "3".into())],
        &raster,
    )?;

    let mut vector: std::collections::BTreeMap<(u8, u32, u32), Vec<(i64, i64, &str)>> = Default::default();
    for z in 0..=6u8 {
        for (name, lon, lat) in CITIES {
            let (tx, ty, px, py) = project(lon, lat, z);
            vector.entry((z, tx, ty)).or_default().push((px, py, name));
        }
    }
    let tiles: Vec<(u8, u32, u32, Vec<u8>)> =
        vector.into_iter().map(|((z, x, y), pts)| (z, x, y, gzip(&mvt_points(&pts)))).collect();
    let layers = json!([{"id": "cities", "fields": {"name": "String"}, "minzoom": 0, "maxzoom": 6}]);
    let mut w = Writer::new(TileFormat::Mvt, Compression::Gzip);
    w.metadata = json!({"name": "Algerian cities (demo)", "vector_layers": layers});
    for (z, x, y, d) in &tiles {
        w.add(*z, *x, *y, d.clone());
    }
    w.write_to(&dir.join("cities.pmtiles"))?;
    mbtiles::write(
        &dir.join("cities-mb.mbtiles"),
        &[
            ("name", "Algerian cities (MBTiles)".into()),
            ("format", "pbf".into()),
            ("bounds", "-8.7,19,12,37.1".into()),
            ("center", "3,30,4".into()),
            ("json", json!({"vector_layers": layers}).to_string()),
        ],
        &tiles,
    )?;
    Ok(())
}
