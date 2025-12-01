# tileserver

A small tile server in Rust (axum) for **PMTiles** and **MBTiles** archives.
Put files in a directory and the server exposes each one as XYZ tiles plus
TileJSON, with correct content headers, ETags, and a MapLibre preview page.

## Install

```sh
cargo install tileserver --locked
tileserver fixtures tiles && tileserver serve --dir tiles
```

## Quick start

```sh
cargo run --release -- fixtures tiles            # generate 4 small demo archives
cargo run --release -- serve --dir tiles         # http://127.0.0.1:3000/
cargo test
```

```
tileserver serve [--dir DIR] [--addr 127.0.0.1:3000] [--base-url URL] [--max-age SECONDS]
tileserver fixtures DIR
```

Every `*.pmtiles` and `*.mbtiles` file directly inside `--dir` becomes a
source named after the file (`roads.pmtiles` → `roads`). Unreadable files
and duplicate ids are logged and skipped. They do not stop the server.

| Endpoint | |
|---|---|
| `GET /` | preview: source list, auto-styled vector layers or raster, click for properties (`?debug` shows tile boundaries) |
| `GET /sources` | JSON list of sources (kind, format, zooms, bounds, TileJSON URL) |
| `GET /{id}.json` | TileJSON 3.0.0 (`vector_layers` for vector sources) |
| `GET /{id}/{z}/{x}/{y}[.ext]` | the tile; the extension is optional but must match (`pbf`/`mvt`, `png`, `jpg`, `webp`, `avif`) |
| `GET /health` | `ok` |

## Headers and status codes

- `Content-Type`: `application/vnd.mapbox-vector-tile`, `image/png`, and so on, from the archive's tile type.
- `Content-Encoding: gzip` (or `br`/`zstd`) when the archive stores compressed tiles. Tiles are served as
  stored, with no recompression. If a client does not accept gzip, the tile is inflated on the fly.
  `Vary: Accept-Encoding` is always set.
- `ETag` is a strong validator, an FNV-1a hash of the stored bytes. `If-None-Match` → `304 Not Modified`.
- `Cache-Control: public, max-age=N` (default 3600). `Access-Control-Allow-Origin: *`.
- `204 No Content` for a tile inside the zoom range that the archive does not contain (MapLibre treats that as
  empty). `404` for unknown sources or zooms outside the range. `400` for invalid coordinates or a wrong
  extension.
- TileJSON URLs use `--base-url`, or else the `Host` header and `X-Forwarded-Proto`.

## How it works

**PMTiles v3** (`src/pmtiles.rs`, written from the spec). The reader parses the
127-byte header, then the gzip-compressed root directory and JSON metadata.
A tile id is the count of tiles in lower zooms plus the tile's position on a
Hilbert curve. The reader binary-searches the directory for the last entry
≤ id. Entries with `run_length > 0` point into tile data, and `run_length == 0`
points to a leaf directory. Leaves are cached. File reads use `pread`, so one
open file serves concurrent requests without locking.

The same module has a **writer**. It deduplicates identical tiles,
run-length encodes consecutive repeats, and splits into leaf directories when
the root gets big. The tests and the `fixtures` command use it.

**MBTiles** (`src/mbtiles.rs`). The SQLite file is opened read-only through
`rusqlite` (bundled SQLite). XYZ `y` is flipped to TMS `2^z − 1 − y`.
Metadata (`format`, `bounds`, `center`, `minzoom`/`maxzoom`, `json` →
`vector_layers`) feeds TileJSON. Gzip is detected from the first tile's magic
bytes instead of being assumed.

**HTTP** (`src/server.rs`). axum 0.8. Tile reads run in `spawn_blocking`,
since both backends do blocking I/O.

**Fixtures** (`src/fixtures.rs`). There is no external data. A tiny PNG
encoder draws a debug grid (z0–3) with each tile's z/x/y as bar counts. A
hand-written MVT encoder produces point tiles (z0–6) for six Algerian cities.
Both are written as PMTiles and MBTiles (`*-mb.mbtiles`).

## Tests

`cargo test` runs 13 integration tests on freshly generated fixtures in a temp dir:

- **PMTiles:**
  - tile ids against the spec's values (0, 1, 2, 3, 4, 5, 21, 84, and z20)
  - id ↔ zxy round trips over z0–6 plus deep zooms
  - directory encode/decode, including the contiguous-offset shorthand, dedup and leaf pointers
  - entry lookup inside runs and gaps, and header round trip and version check
  - writer → reader over z0–4 with deduplicated "ocean" tiles, both root-only and forced leaf directories
- **HTTP:**
  - raster headers and body
  - ETag and 304 (and no 304 on a mismatch)
  - gzip pass-through vs inflation by `Accept-Encoding`, and `.mvt` alias
  - 204/404/400 cases, TileJSON (including `X-Forwarded-Proto` and `--base-url`), and the source list
  - MBTiles row flipping, checked tile by tile against the generated PNGs
  - duplicate id and broken file reporting, and the PNG CRC

## Status and limitations

- Sources are discovered at startup. Restart to pick up new files.
- There is no HTTP range/remote PMTiles (local files only) and no brotli/zstd *internal* (directory)
  compression. Tile-level compression of any kind is passed through.
- The leaf-directory cache is a simple bounded map, not an LRU.
- The preview page loads MapLibre from unpkg, OSM raster tiles as a backdrop and glyphs from
  demotiles.maplibre.org for labels.

## License

MIT, see [LICENSE](LICENSE). The generated fixtures contain no third-party
data (city positions are approximate, typed by hand). The preview backdrop is
© OpenStreetMap contributors (ODbL).
