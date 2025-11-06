//! HTTP layer (axum).

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use serde_json::json;

use crate::source::Source;

const INDEX_HTML: &str = include_str!("../assets/index.html");

#[derive(Clone)]
pub struct AppState {
    pub sources: Arc<BTreeMap<String, Arc<Source>>>,
    /// Public base URL for TileJSON; derived from the Host header when None.
    pub base_url: Option<String>,
    pub max_age: u32,
}

impl AppState {
    pub fn new(sources: Vec<Source>, base_url: Option<String>, max_age: u32) -> Self {
        AppState {
            sources: Arc::new(sources.into_iter().map(|s| (s.id.clone(), Arc::new(s))).collect()),
            base_url,
            max_age,
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        .route("/health", get(|| async { "ok\n" }))
        .route("/sources", get(list_sources))
        .route("/{file}", get(tilejson))
        .route("/{id}/{z}/{x}/{y}", get(tile))
        .layer(axum::middleware::map_response(|mut r: Response| async move {
            r.headers_mut().insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
            r
        }))
        .with_state(state)
}

fn base_url(state: &AppState, headers: &HeaderMap) -> String {
    if let Some(b) = &state.base_url {
        return b.trim_end_matches('/').to_string();
    }
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("localhost");
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|h| h.to_str().ok())
        .filter(|p| *p == "https" || *p == "http")
        .unwrap_or("http");
    format!("{proto}://{host}")
}

async fn list_sources(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let base = base_url(&state, &headers);
    let list: Vec<_> = state
        .sources
        .values()
        .map(|s| {
            json!({
                "id": s.id, "name": s.name, "kind": s.kind(), "format": s.format.extension(),
                "vector": s.format.is_vector(), "minzoom": s.min_zoom, "maxzoom": s.max_zoom,
                "bounds": s.bounds, "tilejson": format!("{base}/{}.json", s.id),
            })
        })
        .collect();
    axum::Json(list).into_response()
}

async fn tilejson(State(state): State<AppState>, Path(file): Path<String>, headers: HeaderMap) -> Response {
    let Some(id) = file.strip_suffix(".json") else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    match state.sources.get(id) {
        Some(s) => axum::Json(s.tilejson(&base_url(&state, &headers))).into_response(),
        None => (StatusCode::NOT_FOUND, "unknown source").into_response(),
    }
}

/// FNV-1a 64-bit, used for strong ETags over the stored tile bytes.
pub fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn parse_tile(z: &str, x: &str, y_ext: &str) -> Option<(u8, u32, u32, Option<String>)> {
    let (y, ext) = match y_ext.split_once('.') {
        Some((y, e)) => (y, Some(e.to_ascii_lowercase())),
        None => (y_ext, None),
    };
    let z: u8 = z.parse().ok()?;
    if z > 30 {
        return None;
    }
    let (x, y): (u32, u32) = (x.parse().ok()?, y.parse().ok()?);
    let n = 1u64 << z;
    ((x as u64) < n && (y as u64) < n).then_some((z, x, y, ext))
}

fn accepts(headers: &HeaderMap, encoding: &str) -> bool {
    headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|t| {
            let mut parts = t.trim().split(';');
            let name = parts.next().unwrap_or("").trim();
            let q0 = parts.any(|p| p.trim().replace(' ', "") == "q=0");
            (name == encoding || name == "*") && !q0
        })
}

async fn tile(
    State(state): State<AppState>,
    Path((id, z, x, y)): Path<(String, String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let Some(src) = state.sources.get(&id).cloned() else {
        return (StatusCode::NOT_FOUND, "unknown source").into_response();
    };
    let Some((z, x, y, ext)) = parse_tile(&z, &x, &y) else {
        return (StatusCode::BAD_REQUEST, "bad tile coordinates").into_response();
    };
    if let Some(ext) = &ext
        && !src.format.accepts_extension(ext) {
            return (StatusCode::BAD_REQUEST, format!("this source serves .{}", src.format.extension())).into_response();
        }
    if z < src.min_zoom || z > src.max_zoom {
        return (StatusCode::NOT_FOUND, "zoom out of range").into_response();
    }
    let s2 = src.clone();
    let data = match tokio::task::spawn_blocking(move || s2.tile(z, x, y)).await {
        Ok(Ok(d)) => d,
        Ok(Err(e)) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "tile task failed").into_response(),
    };
    let Some(mut data) = data else {
        // Inside the pyramid but not stored: an empty tile, cacheable.
        let mut r = StatusCode::NO_CONTENT.into_response();
        r.headers_mut().insert(header::CACHE_CONTROL, cache_control(state.max_age));
        return r;
    };

    let etag = format!("\"{:016x}\"", fnv1a(&data));
    let mut encoding = src.compression.content_encoding();
    if encoding == Some("gzip") && !accepts(&headers, "gzip") {
        // Rare today, but be correct: inflate for clients that cannot.
        let mut out = Vec::new();
        if flate2::read::GzDecoder::new(&data[..]).read_to_end(&mut out).is_ok() {
            data = out;
            encoding = None;
        }
    }
    let etag_hv = HeaderValue::from_str(&etag).unwrap();
    let mut h = HeaderMap::new();
    h.insert(header::ETAG, etag_hv);
    h.insert(header::CACHE_CONTROL, cache_control(state.max_age));
    h.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    if let Some(inm) = headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok())
        && inm.split(',').any(|t| t.trim() == etag || t.trim() == "*" || t.trim().trim_start_matches("W/") == etag) {
            return (StatusCode::NOT_MODIFIED, h).into_response();
        }
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(src.format.content_type()));
    if let Some(enc) = encoding {
        h.insert(header::CONTENT_ENCODING, HeaderValue::from_static(enc));
    }
    (StatusCode::OK, h, data).into_response()
}

fn cache_control(max_age: u32) -> HeaderValue {
    HeaderValue::from_str(&format!("public, max-age={max_age}")).unwrap()
}
