use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

use tileserver::fixtures;
use tileserver::server::{AppState, router};
use tileserver::source::discover;

struct Fx {
    _dir: tempfile::TempDir,
    app: axum::Router,
}

fn setup() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    fixtures::write_all(dir.path()).unwrap();
    std::fs::write(dir.path().join("broken.pmtiles"), b"not a pmtiles file").unwrap();
    std::fs::write(dir.path().join("readme.txt"), b"ignored").unwrap();
    let (sources, errors) = discover(dir.path()).unwrap();
    assert_eq!(errors.len(), 1, "broken file is reported, not fatal");
    assert_eq!(
        sources.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        ["cities", "cities-mb", "grid", "grid-mb"]
    );
    Fx { app: router(AppState::new(sources, None, 600)), _dir: dir }
}

async fn get(app: &axum::Router, uri: &str, headers: &[(&str, &str)]) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut req = Request::builder().uri(uri).header(header::HOST, "tiles.test");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = app.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = res.status();
    let h = res.headers().clone();
    let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, h, body)
}

#[tokio::test]
async fn raster_tile_headers_and_etag() {
    let fx = setup();
    let (s, h, body) = get(&fx.app, "/grid/2/1/3.png", &[]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h[header::CONTENT_TYPE], "image/png");
    assert!(h.get(header::CONTENT_ENCODING).is_none());
    assert_eq!(h[header::CACHE_CONTROL], "public, max-age=600");
    assert_eq!(h[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    assert!(body.starts_with(&[0x89, b'P', b'N', b'G']));
    assert_eq!(body, fixtures::raster_tile(2, 1, 3));
    let etag = h[header::ETAG].to_str().unwrap().to_string();
    assert!(etag.starts_with('"') && etag.len() == 18);

    let (s, h2, body) = get(&fx.app, "/grid/2/1/3.png", &[("if-none-match", &etag)]).await;
    assert_eq!(s, StatusCode::NOT_MODIFIED);
    assert!(body.is_empty());
    assert_eq!(h2[header::ETAG], etag.as_str());
    let (s, _, _) = get(&fx.app, "/grid/2/1/3.png", &[("if-none-match", "\"0000000000000000\"")]).await;
    assert_eq!(s, StatusCode::OK);
    // A different tile has a different ETag.
    let (_, h3, _) = get(&fx.app, "/grid/2/1/2.png", &[]).await;
    assert_ne!(h3[header::ETAG], etag.as_str());
}

#[tokio::test]
async fn vector_tiles_are_served_gzipped_or_inflated() {
    let fx = setup();
    // Alger at z6.
    let (tx, ty, _, _) = fixtures::project(3.0588, 36.7538, 6);
    let uri = format!("/cities/6/{tx}/{ty}.pbf");
    let (s, h, body) = get(&fx.app, &uri, &[("accept-encoding", "gzip, br")]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h[header::CONTENT_TYPE], "application/vnd.mapbox-vector-tile");
    assert_eq!(h[header::CONTENT_ENCODING], "gzip");
    assert_eq!(h[header::VARY], "Accept-Encoding");
    assert_eq!(&body[..2], &[0x1f, 0x8b]);
    let (s, h, body) = get(&fx.app, &uri, &[("accept-encoding", "identity")]).await;
    assert_eq!(s, StatusCode::OK);
    assert!(h.get(header::CONTENT_ENCODING).is_none());
    assert!(body.windows(5).any(|w| w == b"Alger"));
    let (s, _, _) = get(&fx.app, &format!("/cities/6/{tx}/{ty}.mvt"), &[]).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn empty_missing_and_bad_tiles() {
    let fx = setup();
    let cases = [
        ("/cities/6/0/0.pbf", StatusCode::NO_CONTENT), // inside the pyramid, no data
        ("/cities/7/0/0.pbf", StatusCode::NOT_FOUND),  // above maxzoom
        ("/nope/0/0/0.pbf", StatusCode::NOT_FOUND),
        ("/grid/2/4/0.png", StatusCode::BAD_REQUEST), // x out of range
        ("/grid/a/0/0.png", StatusCode::BAD_REQUEST),
        ("/grid/0/0/0.pbf", StatusCode::BAD_REQUEST), // wrong extension
        ("/grid/0/0/0", StatusCode::OK),             // extension optional
        ("/nope.json", StatusCode::NOT_FOUND),
        ("/health", StatusCode::OK),
        ("/", StatusCode::OK),
    ];
    for (uri, want) in cases {
        let (s, _, _) = get(&fx.app, uri, &[]).await;
        assert_eq!(s, want, "{uri}");
    }
}

#[tokio::test]
async fn tilejson_and_listing() {
    let fx = setup();
    let (s, _, body) = get(&fx.app, "/cities.json", &[]).await;
    assert_eq!(s, StatusCode::OK);
    let tj: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(tj["tilejson"], "3.0.0");
    assert_eq!(tj["tiles"][0], "http://tiles.test/cities/{z}/{x}/{y}.pbf");
    assert_eq!(tj["vector_layers"][0]["id"], "cities");
    assert_eq!((tj["minzoom"].as_u64(), tj["maxzoom"].as_u64()), (Some(0), Some(6)));
    let (_, _, body) = get(&fx.app, "/grid.json", &[("x-forwarded-proto", "https")]).await;
    let tj: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(tj["tiles"][0], "https://tiles.test/grid/{z}/{x}/{y}.png");
    assert!(tj.get("vector_layers").is_none());
    let (_, _, body) = get(&fx.app, "/sources", &[]).await;
    let list: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 4);
    assert_eq!(list[0]["kind"], "pmtiles");
}

#[test]
fn duplicate_ids_are_reported() {
    let dir = tempfile::tempdir().unwrap();
    fixtures::write_all(dir.path()).unwrap();
    std::fs::copy(dir.path().join("grid-mb.mbtiles"), dir.path().join("grid.mbtiles")).unwrap();
    let (sources, errors) = discover(dir.path()).unwrap();
    assert_eq!(sources.len(), 4);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].1.to_string().contains("duplicate"));
}

#[tokio::test]
async fn mbtiles_flip_rows_and_match_pmtiles() {
    // Serve only the MBTiles copies and compare with the PMTiles ones.
    let dir = tempfile::tempdir().unwrap();
    fixtures::write_all(dir.path()).unwrap();
    for f in ["grid.pmtiles", "cities.pmtiles"] {
        std::fs::remove_file(dir.path().join(f)).unwrap();
    }
    let (sources, errors) = discover(dir.path()).unwrap();
    assert!(errors.is_empty());
    assert!(sources.iter().all(|s| s.kind() == "mbtiles"));
    let app = router(AppState::new(sources, Some("https://maps.example.org/".into()), 60));
    for (z, x, y) in [(0, 0, 0), (1, 0, 1), (3, 5, 2), (3, 7, 7)] {
        let (s, _, body) = get(&app, &format!("/grid-mb/{z}/{x}/{y}.png"), &[]).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body, fixtures::raster_tile(z, x, y), "{z}/{x}/{y}");
    }
    let (_, h, _) = get(&app, "/cities-mb/0/0/0.pbf", &[("accept-encoding", "gzip")]).await;
    assert_eq!(h[header::CONTENT_ENCODING], "gzip", "gzip detected from magic bytes");
    let (_, _, body) = get(&app, "/cities-mb.json", &[]).await;
    let tj: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(tj["tiles"][0], "https://maps.example.org/cities-mb/{z}/{x}/{y}.pbf");
    assert_eq!(tj["bounds"][0], -8.7);
    assert_eq!(tj["center"][2], 4.0);
}

#[test]
fn png_encoder_produces_a_valid_signature_and_crc() {
    let p = fixtures::png(2, 1, &[255, 0, 0, 0, 255, 0]);
    assert_eq!(&p[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    assert_eq!(&p[p.len() - 12..], &[0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xae, 0x42, 0x60, 0x82]);
}
