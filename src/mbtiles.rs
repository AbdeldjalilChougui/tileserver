//! MBTiles 1.3 reader (and writer for fixtures): <https://github.com/mapbox/mbtiles-spec>.
//!
//! Tiles live in an SQLite `tiles` table with **TMS** row numbering (row 0
//! is the south edge), so XYZ `y` becomes `2^z - 1 - y`.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};

use crate::pmtiles::Compression;
use crate::source::{Result, TileFormat};

pub struct MbTiles {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub struct Metadata {
    pub name: Option<String>,
    pub format: TileFormat,
    pub compression: Compression,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub bounds: Option<[f64; 4]>,
    pub center: Option<[f64; 3]>,
    pub attribution: Option<String>,
    pub vector_layers: Value,
}

fn parse_floats<const N: usize>(s: &str) -> Option<[f64; N]> {
    let v: Vec<f64> = s.split(',').filter_map(|p| p.trim().parse().ok()).collect();
    v.try_into().ok()
}

impl MbTiles {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        // Fail early on files that are not MBTiles.
        conn.query_row("SELECT count(*) FROM metadata", [], |r| r.get::<_, i64>(0))?;
        Ok(MbTiles { conn: Mutex::new(conn) })
    }

    pub fn metadata(&self) -> Result<Metadata> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT name, value FROM metadata")?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        let get = |k: &str| rows.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let format = TileFormat::from_name(&get("format").unwrap_or_default());
        let json_meta: Value = get("json").and_then(|j| serde_json::from_str(&j).ok()).unwrap_or(json!({}));

        // Zoom range: metadata if present, otherwise from the tiles themselves.
        let (dmin, dmax): (Option<i64>, Option<i64>) = conn.query_row(
            "SELECT min(zoom_level), max(zoom_level) FROM tiles",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let min_zoom = get("minzoom").and_then(|v| v.parse().ok()).or(dmin.map(|v| v as u8)).unwrap_or(0);
        let max_zoom = get("maxzoom").and_then(|v| v.parse().ok()).or(dmax.map(|v| v as u8)).unwrap_or(0);

        // Vector MBTiles store gzip-compressed MVT by convention; check the
        // first tile's magic bytes rather than trusting it blindly.
        let first: Option<Vec<u8>> = conn
            .query_row("SELECT tile_data FROM tiles LIMIT 1", [], |r| r.get(0))
            .optional()?;
        let compression = match first {
            Some(b) if b.starts_with(&[0x1f, 0x8b]) => Compression::Gzip,
            _ => Compression::None,
        };
        Ok(Metadata {
            name: get("name"),
            format,
            compression,
            min_zoom,
            max_zoom,
            bounds: get("bounds").and_then(|b| parse_floats::<4>(&b)),
            center: get("center").and_then(|c| parse_floats::<3>(&c)),
            attribution: get("attribution"),
            vector_layers: json_meta.get("vector_layers").cloned().unwrap_or(json!([])),
        })
    }

    pub fn tile(&self, z: u8, x: u32, y: u32) -> Result<Option<Vec<u8>>> {
        let tms_y = (1u64 << z) - 1 - y as u64;
        let conn = self.conn.lock().unwrap();
        let data = conn
            .prepare_cached("SELECT tile_data FROM tiles WHERE zoom_level = ?1 AND tile_column = ?2 AND tile_row = ?3")?
            .query_row(params![z, x, tms_y as i64], |r| r.get(0))
            .optional()?;
        Ok(data)
    }
}

/// Creates a minimal MBTiles file (used by tests and `tileserver fixtures`).
pub fn write(path: &Path, metadata: &[(&str, String)], tiles: &[(u8, u32, u32, Vec<u8>)]) -> Result<()> {
    let _ = std::fs::remove_file(path);
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "CREATE TABLE metadata (name text, value text);
         CREATE TABLE tiles (zoom_level integer, tile_column integer, tile_row integer, tile_data blob);
         CREATE UNIQUE INDEX tile_index ON tiles (zoom_level, tile_column, tile_row);",
    )?;
    for (k, v) in metadata {
        conn.execute("INSERT INTO metadata VALUES (?1, ?2)", params![k, v])?;
    }
    for (z, x, y, data) in tiles {
        let tms_y = (1u64 << z) - 1 - *y as u64;
        conn.execute("INSERT INTO tiles VALUES (?1, ?2, ?3, ?4)", params![z, x, tms_y as i64, data])?;
    }
    Ok(())
}
