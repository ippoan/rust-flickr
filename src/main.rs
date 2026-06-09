use axum::{routing::get, Json, Router};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn app() -> Router {
    Router::new().route("/healthz", get(healthz))
}

async fn healthz() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "service": "rust-flickr",
        "version": VERSION,
    }))
}

#[tokio::main]
async fn main() {
    // rust-ci.yml の smoke test (`<binary> --help`) が exit 0 で返るようにする
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        println!("rust-flickr {VERSION} — Flickr REST service (axum)");
        println!();
        println!("env:");
        println!("  PORT  listen port (default: 8080, Cloud Run が注入)");
        return;
    }

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .unwrap_or_else(|e| panic!("failed to bind 0.0.0.0:{port}: {e}"));

    println!("rust-flickr {VERSION} listening on 0.0.0.0:{port}");

    axum::serve(listener, app()).await.expect("server error");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn healthz_returns_ok() {
        let res = app()
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let body = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["service"], "rust-flickr");
        assert_eq!(v["version"], VERSION);
    }

    #[tokio::test]
    async fn unknown_path_returns_404() {
        let res = app()
            .oneshot(Request::get("/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
}
