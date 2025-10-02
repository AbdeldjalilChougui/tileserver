//! Tile sources discovered in a directory.

use std::fmt;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::mbtiles::MbTiles;
use crate::pmtiles::{Compression, PmTiles};

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Sql(rusqlite::Error),
    Format(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io: {e}"),
            Error::Sql(e) => write!(f, "sqlite: {e}"),
            Error::Format(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Sql(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileFormat {
    Mvt,
    Png,
    Jpeg,
    Webp,
    Avif,
    Unknown,
}

impl TileFormat {
    pub fn content_type(self) -> &'static str {
        match self {
            TileFormat::Mvt => "application/vnd.mapbox-vector-tile",
            TileFormat::Png => "image/png",
            TileFormat::Jpeg => "image/jpeg",
            TileFormat::Webp => "image/webp",
            TileFormat::Avif => "image/avif",
            TileFormat::Unknown => "application/octet-stream",
        }
    }
    pub fn extension(self) -> &'static str {
        match self {
            TileFormat::Mvt => "pbf",
            TileFormat::Png => "png",
            TileFormat::Jpeg => "jpg",
            TileFormat::Webp => "webp",
            TileFormat::Avif => "avif",
            TileFormat::Unknown => "bin",
        }
    }
    /// Accepts the extensions clients commonly use for this format.
    pub fn accepts_extension(self, ext: &str) -> bool {
        match self {
            TileFormat::Mvt => matches!(ext, "pbf" | "mvt"),
            TileFormat::Jpeg => matches!(ext, "jpg" | "jpeg"),
            _ => ext == self.extension(),
        }
    }
    pub fn is_vector(self) -> bool {
        self == TileFormat::Mvt
    }
    pub fn from_name(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "pbf" | "mvt" => TileFormat::Mvt,
            "png" => TileFormat::Png,
            "jpg" | "jpeg" => TileFormat::Jpeg,
            "webp" => TileFormat::Webp,
            "avif" => TileFormat::Avif,
            _ => TileFormat::Unknown,
        }
    }
}

pub enum Backend {
    Pm(PmTiles),
    Mb(MbTiles),
}

/// One served tileset.
pub struct Source {
    pub id: String,
    pub path: PathBuf,
    pub format: TileFormat,
    /// How tiles are stored; sent as `Content-Encoding`.
    pub compression: Compression,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub bounds: Option<[f64; 4]>,
    pub center: Option<[f64; 3]>,
    pub name: String,
    pub attribution: Option<String>,
    pub vector_layers: Value,
    backend: Backend,
}

impl Source {
    pub fn open(path: &Path) -> Result<Self> {
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| Error::Format("bad file name".into()))?
            .to_string();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        match ext {
            "pmtiles" => {
                let pm = PmTiles::open(path)?;
                let h = pm.header.clone();
                let m = &pm.metadata;
                let world = h.bounds == [0.0, 0.0, 0.0, 0.0];
                Ok(Source {
                    name: m["name"].as_str().unwrap_or(&id).to_string(),
                    attribution: m["attribution"].as_str().map(String::from),
                    vector_layers: m.get("vector_layers").cloned().unwrap_or(json!([])),
                    id,
                    path: path.to_path_buf(),
                    format: h.tile_type,
                    compression: h.tile_compression,
                    min_zoom: h.min_zoom,
                    max_zoom: h.max_zoom,
                    bounds: (!world).then_some(h.bounds),
                    center: (!world).then_some([h.center[0], h.center[1], h.center_zoom as f64]),
                    backend: Backend::Pm(pm),
                })
            }
            "mbtiles" => {
                let mb = MbTiles::open(path)?;
                let meta = mb.metadata()?;
                Ok(Source {
                    name: meta.name.clone().unwrap_or_else(|| id.clone()),
                    attribution: meta.attribution.clone(),
                    vector_layers: meta.vector_layers.clone(),
                    id,
                    path: path.to_path_buf(),
                    format: meta.format,
                    compression: meta.compression,
                    min_zoom: meta.min_zoom,
                    max_zoom: meta.max_zoom,
                    bounds: meta.bounds,
                    center: meta.center,
                    backend: Backend::Mb(mb),
                })
            }
            _ => Err(Error::Format(format!("unsupported file type .{ext}"))),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self.backend {
            Backend::Pm(_) => "pmtiles",
            Backend::Mb(_) => "mbtiles",
        }
    }

    /// Stored tile bytes (still compressed if the archive compresses tiles).
    pub fn tile(&self, z: u8, x: u32, y: u32) -> Result<Option<Vec<u8>>> {
        match &self.backend {
            Backend::Pm(p) => p.tile(z, x, y),
            Backend::Mb(m) => m.tile(z, x, y),
        }
    }

    /// TileJSON 3.0.0 for this source; `base` is like `http://host:port`.
    pub fn tilejson(&self, base: &str) -> Value {
        let mut tj = json!({
            "tilejson": "3.0.0",
            "name": self.name,
            "scheme": "xyz",
            "tiles": [format!("{}/{}/{{z}}/{{x}}/{{y}}.{}", base.trim_end_matches('/'), self.id, self.format.extension())],
            "minzoom": self.min_zoom,
            "maxzoom": self.max_zoom,
            "format": self.format.extension(),
        });
        if let Some(b) = self.bounds {
            tj["bounds"] = json!(b);
        }
        if let Some(c) = self.center {
            tj["center"] = json!(c);
        }
        if let Some(a) = &self.attribution {
            tj["attribution"] = json!(a);
        }
        if self.format.is_vector() {
            tj["vector_layers"] = self.vector_layers.clone();
        }
        tj
    }
}

/// Opens every `.pmtiles` / `.mbtiles` file directly inside `dir`, sorted by
/// id. Files that fail to open are returned as errors next to the sources so
/// the server can log them and keep going.
pub fn discover(dir: &Path) -> Result<(Vec<Source>, Vec<(PathBuf, Error)>)> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("pmtiles" | "mbtiles")))
        .collect();
    paths.sort();
    let mut sources: Vec<Source> = Vec::new();
    let mut errors = Vec::new();
    for p in paths {
        match Source::open(&p) {
            Ok(s) if sources.iter().any(|o| o.id == s.id) => errors.push((
                p,
                Error::Format(format!("duplicate source id {:?}", s.id)),
            )),
            Ok(s) => sources.push(s),
            Err(e) => errors.push((p, e)),
        }
    }
    sources.sort_by(|a, b| a.id.cmp(&b.id));
    Ok((sources, errors))
}
