use std::net::SocketAddr;
use std::path::PathBuf;

use tileserver::{fixtures, server, source};

const USAGE: &str = "usage:
  tileserver serve [--dir DIR] [--addr 127.0.0.1:3000] [--base-url URL] [--max-age SECONDS]
  tileserver fixtures DIR      write small demo archives into DIR";

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut dir = PathBuf::from("tiles");
    let mut addr: SocketAddr = "127.0.0.1:3000".parse().unwrap();
    let mut base_url = None;
    let mut max_age = 3600u32;
    match args.first().map(String::as_str) {
        Some("fixtures") => {
            let Some(d) = args.get(1) else { return eprintln!("{USAGE}") };
            match fixtures::write_all(&PathBuf::from(d)) {
                Ok(()) => println!("wrote demo archives to {d}"),
                Err(e) => {
                    eprintln!("tileserver: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("serve") | None => {}
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().unwrap_or_else(|| { eprintln!("{USAGE}"); std::process::exit(2) });
        match a.as_str() {
            "--dir" => dir = PathBuf::from(val()),
            "--addr" => addr = val().parse().unwrap_or_else(|e| { eprintln!("bad --addr: {e}"); std::process::exit(2) }),
            "--base-url" => base_url = Some(val()),
            "--max-age" => max_age = val().parse().unwrap_or(3600),
            _ => {
                eprintln!("{USAGE}");
                std::process::exit(2);
            }
        }
    }
    let (sources, errors) = match source::discover(&dir) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("tileserver: cannot read {}: {e}", dir.display());
            std::process::exit(1);
        }
    };
    for (p, e) in &errors {
        eprintln!("skipping {}: {e}", p.display());
    }
    for s in &sources {
        eprintln!("source {:<20} {} {} z{}-{}", s.id, s.kind(), s.format.extension(), s.min_zoom, s.max_zoom);
    }
    let app = server::router(server::AppState::new(sources, base_url, max_age));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap_or_else(|e| {
        eprintln!("tileserver: bind {addr}: {e}");
        std::process::exit(1)
    });
    eprintln!("listening on http://{addr}/");
    axum::serve(listener, app)
        .with_graceful_shutdown(async { let _ = tokio::signal::ctrl_c().await; })
        .await
        .unwrap();
}
