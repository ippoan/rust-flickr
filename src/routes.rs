//! ルーティングとハンドラ。
//!
//! org は `X-Organization-Id` ヘッダで**明示必須** (UUID 形式)。
//! デフォルト org へのフォールバックは置かない (Refs #1 — 旧実装は
//! ヘッダ欠落時に固定 org に黙ってフォールバックし、RLS で token が
//! 見えず 0 件取り込みが続いた)。

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use sqlx::PgPool;

use crate::db;
use crate::error::ApiError;
use crate::flickr::FlickrClient;
use crate::types::{OauthCallbackRequest, OauthCallbackResponse, OauthUrlResponse};

pub const ORGANIZATION_HEADER: &str = "x-organization-id";

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Default)]
pub struct AppState {
    pub pool: Option<PgPool>,
    pub flickr: Option<FlickrClient>,
}

impl AppState {
    fn pool(&self) -> Result<&PgPool, ApiError> {
        self.pool
            .as_ref()
            .ok_or(ApiError::NotConfigured("DATABASE_URL"))
    }

    fn flickr(&self) -> Result<&FlickrClient, ApiError> {
        self.flickr.as_ref().ok_or(ApiError::NotConfigured(
            "FLICKR_CONSUMER_KEY/FLICKR_CONSUMER_SECRET",
        ))
    }
}

/// `X-Organization-Id` ヘッダから org を取り出す (UUID 形式を強制)
fn require_organization(headers: &HeaderMap) -> Result<String, ApiError> {
    let raw = headers
        .get(ORGANIZATION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ApiError::BadRequest(format!(
                "missing required header: {ORGANIZATION_HEADER} (organization UUID)"
            ))
        })?;
    let parsed = uuid::Uuid::parse_str(raw)
        .map_err(|_| ApiError::BadRequest(format!("invalid {ORGANIZATION_HEADER}: not a UUID")))?;
    Ok(parsed.to_string())
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/oauth/url", get(oauth_url))
        .route("/oauth/callback", post(oauth_callback))
        .with_state(state)
}

async fn healthz() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "service": "rust-flickr",
        "version": VERSION,
    }))
}

/// OAuth1.0a request token を取得して認可 URL を返す
async fn oauth_url(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<OauthUrlResponse>, ApiError> {
    let flickr = state.flickr()?;
    let pool = state.pool()?;
    let organization_id = require_organization(&headers)?;

    let request_token = flickr.get_request_token().await?;

    let mut conn = pool.acquire().await.map_err(ApiError::from)?;
    db::set_current_organization(&mut conn, &organization_id).await?;
    db::insert_oauth_session(
        &mut conn,
        &organization_id,
        &request_token.token,
        &request_token.secret,
    )
    .await?;

    tracing::info!(organization_id, "issued flickr authorization url");

    Ok(Json(OauthUrlResponse {
        authorization_url: request_token.authorization_url,
        request_token: request_token.token,
        request_token_secret: request_token.secret,
    }))
}

/// verifier を access token に交換して flickr_tokens に UPSERT する
async fn oauth_callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<OauthCallbackRequest>,
) -> Result<Json<OauthCallbackResponse>, ApiError> {
    let flickr = state.flickr()?;
    let pool = state.pool()?;
    let organization_id = require_organization(&headers)?;

    if req.oauth_token.is_empty() || req.oauth_verifier.is_empty() {
        return Err(ApiError::BadRequest(
            "oauth_token and oauth_verifier must be non-empty".to_string(),
        ));
    }

    let access_token = flickr
        .get_access_token(
            &req.oauth_token,
            &req.oauth_verifier,
            &req.request_token_secret,
        )
        .await?;

    let mut conn = pool.acquire().await.map_err(ApiError::from)?;
    db::set_current_organization(&mut conn, &organization_id).await?;
    db::upsert_token(&mut conn, &organization_id, &access_token).await?;
    db::delete_oauth_session(&mut conn, &req.oauth_token).await;

    tracing::info!(
        organization_id,
        username = access_token.username,
        "saved flickr access token"
    );

    Ok(Json(OauthCallbackResponse {
        user_nsid: access_token.user_nsid,
        username: access_token.username,
        saved: true,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::flickr::{FlickrClient, FlickrConfig};

    const ORG: &str = "11111111-1111-1111-1111-111111111111";

    fn test_flickr(server: &MockServer) -> FlickrClient {
        FlickrClient::with_endpoints(
            FlickrConfig {
                consumer_key: "ck".to_string(),
                consumer_secret: "cs".to_string(),
                callback_url: "https://example.com/cb".to_string(),
            },
            format!("{}/request_token", server.uri()),
            format!("{}/access_token", server.uri()),
            format!("{}/authorize", server.uri()),
        )
    }

    /// 接続即失敗する lazy pool (DB エラー経路のテスト用)。
    /// acquire_timeout を 1s に絞ってテストを高速化する
    fn dead_pool() -> PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_secs(1))
            .connect_lazy("postgres://nouser@127.0.0.1:1/nodb")
            .unwrap()
    }

    async fn send(state: AppState, request: Request<Body>) -> (StatusCode, serde_json::Value) {
        let res = app(state).oneshot(request).await.unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), 65536).await.unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::json!(null));
        (status, json)
    }

    #[tokio::test]
    async fn healthz_works_without_any_config() {
        let (status, body) = send(
            AppState::default(),
            Request::get("/healthz").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["service"], "rust-flickr");
    }

    #[tokio::test]
    async fn oauth_url_503_when_flickr_not_configured() {
        let (status, body) = send(
            AppState::default(),
            Request::get("/oauth/url").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body["message"]
            .as_str()
            .unwrap()
            .contains("FLICKR_CONSUMER_KEY"));
    }

    #[tokio::test]
    async fn oauth_url_503_when_db_not_configured() {
        let server = MockServer::start().await;
        let state = AppState {
            pool: None,
            flickr: Some(test_flickr(&server)),
        };
        let (status, body) = send(
            state,
            Request::get("/oauth/url").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body["message"].as_str().unwrap().contains("DATABASE_URL"));
    }

    #[tokio::test]
    async fn oauth_url_400_without_org_header() {
        let server = MockServer::start().await;
        let state = AppState {
            pool: Some(dead_pool()),
            flickr: Some(test_flickr(&server)),
        };
        let (status, body) = send(
            state,
            Request::get("/oauth/url").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["message"]
            .as_str()
            .unwrap()
            .contains("x-organization-id"));
    }

    #[tokio::test]
    async fn oauth_url_400_with_non_uuid_org() {
        let server = MockServer::start().await;
        let state = AppState {
            pool: Some(dead_pool()),
            flickr: Some(test_flickr(&server)),
        };
        let (status, body) = send(
            state,
            Request::get("/oauth/url")
                .header(ORGANIZATION_HEADER, "default-org")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["message"].as_str().unwrap().contains("not a UUID"));
    }

    #[tokio::test]
    async fn oauth_url_424_when_flickr_rejects() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/request_token"))
            .respond_with(ResponseTemplate::new(401).set_body_string("oauth_problem=rejected"))
            .mount(&server)
            .await;
        let state = AppState {
            pool: Some(dead_pool()),
            flickr: Some(test_flickr(&server)),
        };
        let (status, _) = send(
            state,
            Request::get("/oauth/url")
                .header(ORGANIZATION_HEADER, ORG)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::FAILED_DEPENDENCY);
    }

    #[tokio::test]
    async fn oauth_url_500_when_db_unreachable() {
        // Flickr は成功、DB 保存で落ちる → 500 internal (詳細 echo なし)
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/request_token"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "oauth_callback_confirmed=true&oauth_token=rt&oauth_token_secret=rts",
            ))
            .mount(&server)
            .await;
        let state = AppState {
            pool: Some(dead_pool()),
            flickr: Some(test_flickr(&server)),
        };
        let (status, body) = send(
            state,
            Request::get("/oauth/url")
                .header(ORGANIZATION_HEADER, ORG)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["message"], "internal error");
    }

    #[tokio::test]
    async fn oauth_callback_400_with_empty_fields() {
        let server = MockServer::start().await;
        let state = AppState {
            pool: Some(dead_pool()),
            flickr: Some(test_flickr(&server)),
        };
        let (status, _) = send(
            state,
            Request::post("/oauth/callback")
                .header(ORGANIZATION_HEADER, ORG)
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"oauth_token":"","oauth_verifier":"","request_token_secret":""}"#,
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn oauth_callback_424_when_flickr_rejects_verifier() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/access_token"))
            .respond_with(ResponseTemplate::new(401).set_body_string("oauth_problem=token_expired"))
            .mount(&server)
            .await;
        let state = AppState {
            pool: Some(dead_pool()),
            flickr: Some(test_flickr(&server)),
        };
        let (status, _) = send(
            state,
            Request::post("/oauth/callback")
                .header(ORGANIZATION_HEADER, ORG)
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"oauth_token":"rt","oauth_verifier":"v","request_token_secret":"rts"}"#,
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::FAILED_DEPENDENCY);
    }

    #[tokio::test]
    async fn oauth_callback_500_when_db_unreachable_and_no_token_echo() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/access_token"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "oauth_token=secret-at&oauth_token_secret=secret-ats&user_nsid=u&username=n",
            ))
            .mount(&server)
            .await;
        let state = AppState {
            pool: Some(dead_pool()),
            flickr: Some(test_flickr(&server)),
        };
        let (status, body) = send(
            state,
            Request::post("/oauth/callback")
                .header(ORGANIZATION_HEADER, ORG)
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"oauth_token":"rt","oauth_verifier":"v","request_token_secret":"rts"}"#,
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        // access token がレスポンスに漏れないこと
        assert!(!body.to_string().contains("secret-at"));
    }
}
