mod cam;
mod db;
mod error;
mod flickr;
mod oauth1;
mod routes;
mod types;

use cam::CamConfig;
use flickr::{FlickrClient, FlickrConfig};
use routes::AppState;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// env から AppState を構築する。
/// FLICKR_* / DATABASE_URL は **boot 時 optional** (secrets-inventory-gcp と同方式):
/// 未設定でも起動は成功し、該当 endpoint だけが 503 "not configured" を返す。
/// /healthz は常に動く = secret 配線 (PR5) 前でも deploy が落ちない。
fn state_from_env() -> AppState {
    let flickr = match FlickrConfig::from_env() {
        Some(config) => Some(FlickrClient::new(config)),
        None => {
            tracing::warn!(
                "FLICKR_CONSUMER_KEY/FLICKR_CONSUMER_SECRET not set — /oauth/* will return 503"
            );
            None
        }
    };

    let pool = match std::env::var("DATABASE_URL") {
        Ok(url) if !url.is_empty() => match db::lazy_pool(&url) {
            Ok(pool) => Some(pool),
            Err(e) => {
                tracing::error!("invalid DATABASE_URL (pool not created): {e}");
                None
            }
        },
        _ => {
            tracing::warn!("DATABASE_URL not set — DB-backed endpoints will return 503");
            None
        }
    };

    let cam = CamConfig::from_env();
    if cam.is_none() {
        tracing::warn!("CAM_* not fully set — /sync will return 503");
    }

    AppState { pool, flickr, cam }
}

#[tokio::main]
async fn main() {
    // rust-ci.yml の smoke test (`<binary> --help`) が exit 0 で返るようにする
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        println!("rust-flickr {VERSION} — Flickr REST service (axum)");
        println!();
        println!("env:");
        println!("  PORT                    listen port (default: 8080, Cloud Run が注入)");
        println!("  FLICKR_CONSUMER_KEY     Flickr OAuth consumer key");
        println!("  FLICKR_CONSUMER_SECRET  Flickr OAuth consumer secret");
        println!("  FLICKR_CALLBACK_URL     OAuth callback URL");
        println!("  DATABASE_URL            Supabase PostgreSQL (rust-logi と共有)");
        println!("  CAM_DIGEST_USER         camera digest auth user (POST /sync)");
        println!("  CAM_DIGEST_PASS         camera digest auth password");
        println!("  CAM_MACHINE_NAME        camera machine name");
        println!("  CAM_SDCARD_CGI          camera SD-card listing CGI base URL");
        println!("  CAM_MP4_CGI             camera mp4 download CGI base URL");
        println!("  CAM_JPG_CGI             camera jpg download CGI base URL");
        println!("  CAM_CF_ACCESS_CLIENT_ID     CF Access service token id (optional)");
        println!("  CAM_CF_ACCESS_CLIENT_SECRET CF Access service token secret (optional)");
        return;
    }

    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_target(false)
        .init();

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);

    let state = state_from_env();

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .unwrap_or_else(|e| panic!("failed to bind 0.0.0.0:{port}: {e}"));

    tracing::info!("rust-flickr {VERSION} listening on 0.0.0.0:{port}");

    axum::serve(listener, routes::app(state))
        .await
        .expect("server error");
}
