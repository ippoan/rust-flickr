//! ルーティングとハンドラ。
//!
//! org は `X-Organization-Id` ヘッダで**明示必須** (UUID 形式)。
//! デフォルト org へのフォールバックは置かない (Refs #1 — 旧実装は
//! ヘッダ欠落時に固定 org に黙ってフォールバックし、RLS で token が
//! 見えず 0 件取り込みが続いた)。

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use sqlx::PgPool;

use crate::cam::{CamClient, CamConfig};
use crate::db;
use crate::error::ApiError;
use crate::flickr::FlickrClient;
use crate::types::{
    DayStat, FlickrPhoto, ImportRequest, ImportResponse, OauthCallbackRequest,
    OauthCallbackResponse, OauthUrlResponse, StatsResponse, SyncRequest, SyncResponse,
};

pub const ORGANIZATION_HEADER: &str = "x-organization-id";

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Default)]
pub struct AppState {
    pub pool: Option<PgPool>,
    pub flickr: Option<FlickrClient>,
    pub cam: Option<CamConfig>,
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

    fn cam(&self) -> Result<&CamConfig, ApiError> {
        self.cam.as_ref().ok_or(ApiError::NotConfigured(
            "CAM_DIGEST_USER/CAM_DIGEST_PASS/CAM_MACHINE_NAME/CAM_SDCARD_CGI/CAM_MP4_CGI/CAM_JPG_CGI",
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

/// SD カード上に実在する最古の日付 (= upload 対象の下限)。
/// 「巡回再開位置 (start_date) 以降」を下限にすると、巡回が最新日に追いついた
/// 時点で過去の未アップロード分が upload 対象から漏れる (#9 移行時に実測 —
/// remaining 11,643 件が一夜で 1,006 件に見えなくなった)。SD に実在する日付の
/// 最小値を下限にすれば、SD から消えた古い行への無限再試行も避けられる
fn min_sd_date(dates: &[String]) -> Option<String> {
    min_sd_dir(dates)
}

/// 数値として最小の dir 名を、入力文字列のフォーマットを保ったまま返す
/// (= "00" や "07" のような leading-zero hour 表記を `to_string()` で
/// 潰さないため。日付 8 桁 / 時間 2 桁 のいずれにも使える共通実装)。
fn min_sd_dir(dirs: &[String]) -> Option<String> {
    dirs.iter()
        .filter(|d| d.parse::<i64>().is_ok())
        .min_by_key(|d| d.parse::<i64>().unwrap_or(i64::MAX))
        .cloned()
}

pub fn app(state: AppState) -> Router {
    Router::new()
        // /health と /healthz は同一 handler。外形監視には /health を使うこと —
        // run.app / ghs (domain mapping) の Google フロントは `/healthz` を
        // インターセプトして汎用 404 を返すため、外から /healthz は見えない
        // (ippoan の他 Cloud Run service が /health 標準なのも同じ理由。
        //  Refs ippoan/cf-flickr-proxy#1 の cutover 検証)。
        .route("/health", get(healthz))
        .route("/healthz", get(healthz))
        .route("/oauth/url", get(oauth_url))
        .route("/oauth/callback", post(oauth_callback))
        .route("/import", post(import_photos))
        .route("/sync", post(sync_cam_files))
        .route("/stats", get(stats))
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

/// 未検証の cam_files.flickr_id を flickr.photos.getInfo で検証して
/// flickr_photo に登録する (旧 ImportFlickrPhotos の移植)。
///
/// 「黙って 200」禁止 (Refs #1):
/// - token 未登録 → 412 (旧実装はここで gRPC failed_precondition → HTTP 200 に化けていた)
/// - 個々の写真の検証失敗は errors_count に集計して処理は継続
/// - 未検証 0 件は正常系 (200, imported=0, remaining=0)
async fn import_photos(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<ImportRequest>>,
) -> Result<Json<ImportResponse>, ApiError> {
    let flickr = state.flickr()?;
    let pool = state.pool()?;
    let organization_id = require_organization(&headers)?;

    let req = body.map(|Json(r)| r).unwrap_or_default();
    let limit = match req.limit {
        None => 500,
        Some(n) if n > 0 => n,
        Some(_) => {
            return Err(ApiError::BadRequest(
                "limit must be a positive integer".to_string(),
            ))
        }
    };

    let mut conn = pool.acquire().await.map_err(ApiError::from)?;
    db::set_current_organization(&mut conn, &organization_id).await?;

    let token = db::get_flickr_token(&mut conn)
        .await?
        .ok_or(ApiError::NoToken)?;

    let unverified = db::list_unverified_flickr_ids(&mut conn, limit).await?;
    if unverified.is_empty() {
        tracing::info!(organization_id, "no unverified flickr photos");
        return Ok(Json(ImportResponse {
            imported_count: 0,
            errors_count: 0,
            remaining_count: 0,
            photos: vec![],
        }));
    }

    tracing::info!(
        organization_id,
        count = unverified.len(),
        "verifying flickr photos"
    );

    let mut photos = Vec::new();
    let mut errors_count = 0i32;

    for flickr_id in &unverified {
        match flickr
            .photos_get_info(flickr_id, &token.access_token, &token.access_token_secret)
            .await
        {
            Ok(photo) => match db::insert_flickr_photo(&mut conn, &organization_id, &photo).await {
                Ok(()) => photos.push(FlickrPhoto {
                    id: photo.id,
                    secret: photo.secret,
                    server: photo.server,
                }),
                Err(e) => {
                    tracing::warn!(flickr_id, ?e, "failed to insert flickr_photo");
                    errors_count += 1;
                }
            },
            Err(e) => {
                tracing::warn!(flickr_id, error = e, "failed to fetch flickr photo info");
                errors_count += 1;
            }
        }
    }

    let remaining = db::count_unverified(&mut conn).await?;
    let imported_count = photos.len() as i32;

    tracing::info!(
        organization_id,
        imported_count,
        errors_count,
        remaining,
        "import completed"
    );

    Ok(Json(ImportResponse {
        imported_count,
        errors_count,
        remaining_count: remaining as i32,
        photos,
    }))
}

/// カメラ SD カードの巡回 → cam_files UPSERT → Flickr アップロード
/// (旧 rust-logi SyncCamFiles の移植、Refs #9)。
///
/// 旧実装との差分:
/// - upload は `tokio::spawn` (background) ではなく**同期実行** — Cloud Run は
///   CPU throttling (default) のため response 返却後の background task は完走
///   しない。1 回の処理量は `upload_limit` で制御し、残りは次回 sync が拾う
/// - 失敗の明示 (Refs #1): cam env 未設定 503 / org ヘッダ無し 400 /
///   カメラの日付一覧が取れない 424 / DB 500。token 不在は sync 自体は成立
///   させ `flickr_token_present: false` で明示する (旧実装は info log のみ)
async fn sync_cam_files(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<SyncRequest>>,
) -> Result<Json<SyncResponse>, ApiError> {
    let cam_config = state.cam()?;
    let pool = state.pool()?;
    let organization_id = require_organization(&headers)?;

    let req = body.map(|Json(r)| r).unwrap_or_default();
    let upload_limit = match req.upload_limit {
        None => 50,
        Some(n) if n >= 0 => n,
        Some(_) => {
            return Err(ApiError::BadRequest(
                "upload_limit must be a non-negative integer".to_string(),
            ))
        }
    };

    let mut conn = pool.acquire().await.map_err(ApiError::from)?;
    db::set_current_organization(&mut conn, &organization_id).await?;

    // 1. 最終レコード → 再開位置 (RLS で現 org の行だけが見える)
    let (start_date, start_hour) =
        db::last_cam_file(&mut conn).await?.ok_or_else(|| {
            ApiError::BadRequest(
                "no existing cam_files rows for this organization; cannot determine sync start position".to_string(),
            )
        })?;
    tracing::info!(organization_id, start_date, start_hour, "sync started");

    let client = CamClient::new(cam_config.clone());

    // 2. 日付一覧 (取れなければ致命 = 424 をそのまま返す)
    let all_dates = client.list_dates().await?;
    let start_date_int: i64 = start_date.parse().unwrap_or(0);
    let dates: Vec<&String> = all_dates
        .iter()
        .filter(|d| d.parse::<i64>().unwrap_or(0) >= start_date_int)
        .collect();
    let processed_dates = dates.len() as i32;

    // 3. 時間一覧 (個別失敗は warn + continue — 旧実装互換)
    let start_hour_int: i64 = start_hour.parse().unwrap_or(0);
    let mut hours: Vec<(String, String)> = Vec::new();
    for date in &dates {
        match client.list_hours(date).await {
            Ok(hour_dirs) => {
                for hour in hour_dirs {
                    if date.as_str() == start_date {
                        if hour.parse::<i64>().unwrap_or(0) >= start_hour_int {
                            hours.push(((*date).clone(), hour));
                        }
                    } else {
                        hours.push(((*date).clone(), hour));
                    }
                }
            }
            Err(e) => tracing::warn!(date = date.as_str(), ?e, "failed to list hours"),
        }
    }
    let processed_hours = hours.len() as i32;

    // 4. ファイル一覧 → UPSERT
    let mut new_files = 0i32;
    for (date, hour) in &hours {
        match client.list_file_names(date, hour).await {
            Ok(filenames) => {
                for filename in filenames {
                    let file_type = if filename.contains(".mp4") {
                        "mp4"
                    } else {
                        "jpg"
                    };
                    match db::upsert_cam_file(
                        &mut conn,
                        &organization_id,
                        &filename,
                        date,
                        hour,
                        file_type,
                        client.machine_name(),
                    )
                    .await
                    {
                        Ok(()) => new_files += 1,
                        Err(e) => tracing::warn!(filename, ?e, "failed to upsert cam_file"),
                    }
                }
            }
            Err(e) => tracing::warn!(date, hour, ?e, "failed to list files"),
        }
    }

    // 5. Flickr アップロード (同期、upload_limit 件まで)。
    //    下限は SD に実在する最古日 (dates が空なら巡回再開位置に fallback)
    let upload_floor = min_sd_date(&all_dates).unwrap_or_else(|| start_date.clone());
    let token = db::get_flickr_token(&mut conn).await?;
    let flickr_token_present = token.is_some();
    let mut uploaded_count = 0i32;
    let mut upload_errors = 0i32;

    if upload_limit > 0 {
        match (state.flickr.as_ref(), token) {
            (Some(flickr), Some(token)) => {
                let unuploaded =
                    db::list_unuploaded_cam_files(&mut conn, &upload_floor, upload_limit).await?;
                for file in unuploaded {
                    let result = match client.download(&file.name, &file.date, &file.hour).await {
                        Ok(data) => {
                            flickr
                                .upload_photo(
                                    &token.access_token,
                                    &token.access_token_secret,
                                    &file.name,
                                    data,
                                )
                                .await
                        }
                        Err(e) => Err(e),
                    };
                    match result {
                        Ok(flickr_id) => {
                            match db::set_cam_file_flickr_id(&mut conn, &file.name, &flickr_id)
                                .await
                            {
                                Ok(()) => {
                                    uploaded_count += 1;
                                    tracing::info!(name = file.name, flickr_id, "flickr upload ok");
                                }
                                Err(e) => {
                                    upload_errors += 1;
                                    tracing::warn!(
                                        name = file.name,
                                        flickr_id,
                                        ?e,
                                        "uploaded but failed to record flickr_id"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            upload_errors += 1;
                            tracing::warn!(name = file.name, error = e, "flickr upload failed");
                        }
                    }
                }
            }
            _ => {
                tracing::info!(
                    organization_id,
                    flickr_token_present,
                    "flickr not configured or no access token — skipping uploads"
                );
            }
        }
    }

    let remaining_unuploaded =
        db::count_unuploaded_cam_files(&mut conn, &upload_floor).await? as i32;

    let message = format!(
        "Synced {processed_dates} dates, {processed_hours} hours, {new_files} files. \
         Uploaded {uploaded_count} to Flickr ({upload_errors} errors, {remaining_unuploaded} remaining)."
    );
    tracing::info!(organization_id, message, "sync completed");

    Ok(Json(SyncResponse {
        processed_dates,
        processed_hours,
        new_files,
        flickr_token_present,
        uploaded_count,
        upload_errors,
        remaining_unuploaded,
        message,
    }))
}

#[derive(serde::Deserialize, Default)]
struct StatsQuery {
    days: Option<i64>,
}

/// 撮影日別の登録 / Flickr upload / 検証の集計 (daily report 用、Refs #12)
async fn stats(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<StatsQuery>,
) -> Result<Json<StatsResponse>, ApiError> {
    let pool = state.pool()?;
    let organization_id = require_organization(&headers)?;
    let days = query.days.unwrap_or(7).clamp(1, 60);

    // 未アップロード残は SD に実在する範囲 (= upload floor 以降) だけを数える。
    // SD ローテーションで消えた古い行は flickr_id NULL のまま残り回収不能なので、
    // floor 無しの全期間 COUNT だと回収不能分まで残数に積み上がり「減らない」
    // ように見える (/sync が remaining_unuploaded に使う floor と同じ基準に揃える)。
    // floor は cam の (日付, 時間) 一覧から導く: 最古日付の最古時間まで絞ることで、
    // 「最古日付の途中 hour までしか SD に残っていない」ケースで日付粒度 floor が
    // 取りこぼす古い行 (= SD から消えた 永久ゴースト) を残数から除外する
    // (Refs #19 floor 日付粒度 → #24 hour 粒度に拡張)。
    // 日付は取れたが時間が取れない時は hour="00" に fallback (= 旧 #19 と同等動作、
    // リグレッションなし)。cam 未設定 / 日付一覧 到達不能時は全期間 COUNT に
    // fallback する (= daily report をカメラ障害で落とさない)。
    let upload_floor = stats_upload_floor(&state).await;

    let mut conn = pool.acquire().await.map_err(ApiError::from)?;
    db::set_current_organization(&mut conn, &organization_id).await?;

    let day_rows = db::day_stats(&mut conn, days).await?;
    let total_unuploaded = match upload_floor.as_ref() {
        Some((date, hour)) => db::count_unuploaded_cam_files_from(&mut conn, date, hour).await?,
        None => db::count_total_unuploaded(&mut conn).await?,
    };
    let total_unverified = db::count_unverified(&mut conn).await?;
    let oldest_unuploaded_date = db::oldest_unuploaded_date(&mut conn).await?;

    Ok(Json(StatsResponse {
        days: day_rows
            .into_iter()
            .map(|(date, files, uploaded, verified)| DayStat {
                date,
                files,
                uploaded,
                verified,
            })
            .collect(),
        total_unuploaded,
        total_unverified,
        oldest_unuploaded_date,
    }))
}

/// daily report (`GET /stats`) 用の upload floor (= SD カードに実在する最古
/// 日付の最古時間)。Refs #24: 旧 #19 では「日付」までしか絞らず、最古日付の
/// 途中 hour 以降だけが SD に残るケースで、その日付の 00:00〜floor_hour 前の
/// 行 (= SD ローテで消えた古い行) がゴーストとして count され続けた。本関数は
/// (date, hour) tuple を返し、SQL 側で 2 段絞りする。
/// 戻り値:
/// - `Some((date, hour))`: 通常経路 (cam OK)
/// - `None`: cam 未設定 / 日付一覧 到達不能 → 呼び出し側は全期間 COUNT に fallback
/// - hour 一覧だけ取れない時は `(date, "00")` を返す (= 旧 #19 と同等の日付粒度
///   floor で、リグレッションなし)
async fn stats_upload_floor(state: &AppState) -> Option<(String, String)> {
    let cam_config = state.cam.as_ref()?;
    let client = CamClient::new(cam_config.clone());
    let dates = match client.list_dates().await {
        Ok(dates) => dates,
        Err(e) => {
            tracing::warn!(
                ?e,
                "stats: failed to list cam dates; counting all unuploaded"
            );
            return None;
        }
    };
    let floor_date = min_sd_date(&dates)?;
    let floor_hour = match client.list_hours(&floor_date).await {
        Ok(hours) => min_sd_dir(&hours).unwrap_or_else(|| "00".to_string()),
        Err(e) => {
            tracing::warn!(
                ?e,
                date = %floor_date,
                "stats: failed to list cam hours; falling back to date-only floor"
            );
            "00".to_string()
        }
    };
    Some((floor_date, floor_hour))
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
            format!("{}/rest/", server.uri()),
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
    async fn health_alias_works_without_any_config() {
        // 外形監視用 alias (/healthz は Google フロントに食われるため)
        let (status, body) = send(
            AppState::default(),
            Request::get("/health").body(Body::empty()).unwrap(),
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
            cam: None,
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
            cam: None,
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
            cam: None,
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
            cam: None,
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
            cam: None,
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
            cam: None,
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
            cam: None,
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
            cam: None,
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

    #[tokio::test]
    async fn import_503_when_not_configured() {
        let (status, _) = send(
            AppState::default(),
            Request::post("/import").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    fn test_cam(server: &MockServer) -> crate::cam::CamConfig {
        crate::cam::CamConfig {
            digest_user: "u".to_string(),
            digest_pass: "p".to_string(),
            machine_name: "cam1".to_string(),
            sdcard_cgi: format!("{}/sd/", server.uri()),
            mp4_cgi: format!("{}/mp4/", server.uri()),
            jpg_cgi: format!("{}/jpg/", server.uri()),
            cf_access_client_id: None,
            cf_access_client_secret: None,
        }
    }

    #[test]
    fn min_sd_date_picks_numeric_minimum_and_skips_junk() {
        let dates = vec![
            "20260610".to_string(),
            "20260524".to_string(),
            "not-a-date".to_string(),
        ];
        assert_eq!(min_sd_date(&dates), Some("20260524".to_string()));
        assert_eq!(min_sd_date(&[]), None);
    }

    #[test]
    fn min_sd_dir_preserves_leading_zero_format() {
        // hour ディレクトリ ("00".."23") の最古を取る用途。"to_string()" で
        // potential 0-padding を潰さず、cam が返した string 表現をそのまま返す
        // (= SQL 側で cam_files.hour と等価比較するために必要)。
        let hours = vec!["14".to_string(), "09".to_string(), "22".to_string()];
        assert_eq!(min_sd_dir(&hours), Some("09".to_string()));

        // junk 混入は skip、空 vec は None
        let mixed = vec!["bad".to_string(), "07".to_string()];
        assert_eq!(min_sd_dir(&mixed), Some("07".to_string()));
        assert_eq!(min_sd_dir(&[]), None);
    }

    #[tokio::test]
    async fn stats_upload_floor_none_when_cam_unconfigured() {
        // cam 未設定 → floor 無し → 呼び出し側は全期間 COUNT に fallback
        let state = AppState {
            pool: None,
            flickr: None,
            cam: None,
        };
        assert_eq!(stats_upload_floor(&state).await, None);
    }

    #[tokio::test]
    async fn stats_upload_floor_returns_min_sd_date_and_hour() {
        use wiremock::matchers::header_exists;
        let server = MockServer::start().await;
        // mount 順は specific (auth 必須) を先に — wiremock は最初にマッチした
        // mock を返すので、catch-all (no auth) を先に置くと auth ありリトライも
        // 401 で吸われてしまう。
        Mock::given(method("GET"))
            .and(path("/sd/cam1/Event"))
            .and(header_exists("authorization"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"<List><Dir name="20260610"/><Dir name="20260524"/></List>"#,
                ),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/sd/cam1/Event"))
            .respond_with(ResponseTemplate::new(401).insert_header(
                "www-authenticate",
                r#"Digest realm="cam", nonce="abc123", qop="auth""#,
            ))
            .mount(&server)
            .await;
        // 最古日 20260524 の hour 一覧: 14, 09, 22 のうち 09 が最小。
        // SD の最古 hour ディレクトリより前の (= ローテ済) 行を残数から除外できることを担保。
        Mock::given(method("GET"))
            .and(path("/sd/cam1/Event/20260524"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<List><Dir name="14"/><Dir name="09"/><Dir name="22"/></List>"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/sd/cam1/Event/20260524"))
            .respond_with(ResponseTemplate::new(401).insert_header(
                "www-authenticate",
                r#"Digest realm="cam", nonce="def456", qop="auth""#,
            ))
            .mount(&server)
            .await;
        let state = AppState {
            pool: None,
            flickr: None,
            cam: Some(test_cam(&server)),
        };
        assert_eq!(
            stats_upload_floor(&state).await,
            Some(("20260524".to_string(), "09".to_string()))
        );
    }

    #[tokio::test]
    async fn stats_upload_floor_falls_back_to_hour_zero_on_list_hours_error() {
        // 日付は取れたが時間一覧で 5xx → hour="00" にフォールバック (=旧 #19 と
        // 同等の日付粒度 floor、リグレッションなし)。
        use wiremock::matchers::header_exists;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/sd/cam1/Event"))
            .and(header_exists("authorization"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"<List><Dir name="20260524"/></List>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/sd/cam1/Event"))
            .respond_with(ResponseTemplate::new(401).insert_header(
                "www-authenticate",
                r#"Digest realm="cam", nonce="abc123", qop="auth""#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/sd/cam1/Event/20260524"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let state = AppState {
            pool: None,
            flickr: None,
            cam: Some(test_cam(&server)),
        };
        assert_eq!(
            stats_upload_floor(&state).await,
            Some(("20260524".to_string(), "00".to_string()))
        );
    }

    #[tokio::test]
    async fn stats_upload_floor_falls_back_to_hour_zero_on_empty_hours() {
        // list_hours が空配列を返した時も hour="00" にフォールバック (= 日付
        // ディレクトリは残っているが中身の hour が全消失したレアケース)。
        use wiremock::matchers::header_exists;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/sd/cam1/Event"))
            .and(header_exists("authorization"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"<List><Dir name="20260524"/></List>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/sd/cam1/Event"))
            .respond_with(ResponseTemplate::new(401).insert_header(
                "www-authenticate",
                r#"Digest realm="cam", nonce="abc123", qop="auth""#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/sd/cam1/Event/20260524"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"<List/>"#))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/sd/cam1/Event/20260524"))
            .respond_with(ResponseTemplate::new(401).insert_header(
                "www-authenticate",
                r#"Digest realm="cam", nonce="def456", qop="auth""#,
            ))
            .mount(&server)
            .await;
        let state = AppState {
            pool: None,
            flickr: None,
            cam: Some(test_cam(&server)),
        };
        assert_eq!(
            stats_upload_floor(&state).await,
            Some(("20260524".to_string(), "00".to_string()))
        );
    }

    #[tokio::test]
    async fn stats_upload_floor_none_on_cam_error() {
        // Digest ではない 401 → Upstream error → floor 無し (warn して fallback)
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/sd/cam1/Event"))
            .respond_with(
                ResponseTemplate::new(401)
                    .insert_header("www-authenticate", r#"Basic realm="cam""#),
            )
            .mount(&server)
            .await;
        let state = AppState {
            pool: None,
            flickr: None,
            cam: Some(test_cam(&server)),
        };
        assert_eq!(stats_upload_floor(&state).await, None);
    }

    #[tokio::test]
    async fn stats_503_when_db_not_configured() {
        let (status, _) = send(
            AppState::default(),
            Request::get("/stats").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn stats_400_without_org_header() {
        let state = AppState {
            pool: Some(dead_pool()),
            flickr: None,
            cam: None,
        };
        let (status, _) = send(state, Request::get("/stats").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn stats_500_when_db_unreachable_without_detail_echo() {
        let state = AppState {
            pool: Some(dead_pool()),
            flickr: None,
            cam: None,
        };
        let (status, body) = send(
            state,
            Request::get("/stats?days=3")
                .header(ORGANIZATION_HEADER, ORG)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["message"], "internal error");
    }

    #[tokio::test]
    async fn sync_503_when_cam_not_configured() {
        let (status, body) = send(
            AppState::default(),
            Request::post("/sync").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body["message"].as_str().unwrap().contains("CAM_"));
    }

    #[tokio::test]
    async fn sync_400_without_org_header() {
        let server = MockServer::start().await;
        let state = AppState {
            pool: Some(dead_pool()),
            flickr: None,
            cam: Some(test_cam(&server)),
        };
        let (status, _) = send(state, Request::post("/sync").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn sync_400_with_negative_upload_limit() {
        let server = MockServer::start().await;
        let state = AppState {
            pool: Some(dead_pool()),
            flickr: None,
            cam: Some(test_cam(&server)),
        };
        let (status, body) = send(
            state,
            Request::post("/sync")
                .header(ORGANIZATION_HEADER, ORG)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"upload_limit":-1}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["message"].as_str().unwrap().contains("upload_limit"));
    }

    #[tokio::test]
    async fn sync_500_when_db_unreachable() {
        let server = MockServer::start().await;
        let state = AppState {
            pool: Some(dead_pool()),
            flickr: None,
            cam: Some(test_cam(&server)),
        };
        let (status, body) = send(
            state,
            Request::post("/sync")
                .header(ORGANIZATION_HEADER, ORG)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        // 接続文字列等の内部詳細を echo しない
        assert_eq!(body["message"], "internal error");
    }

    #[tokio::test]
    async fn import_400_without_org_header() {
        let server = MockServer::start().await;
        let state = AppState {
            pool: Some(dead_pool()),
            flickr: Some(test_flickr(&server)),
            cam: None,
        };
        let (status, _) = send(state, Request::post("/import").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn import_400_with_non_positive_limit() {
        let server = MockServer::start().await;
        let state = AppState {
            pool: Some(dead_pool()),
            flickr: Some(test_flickr(&server)),
            cam: None,
        };
        let (status, body) = send(
            state,
            Request::post("/import")
                .header(ORGANIZATION_HEADER, ORG)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"limit":0}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["message"].as_str().unwrap().contains("positive"));
    }

    #[tokio::test]
    async fn import_500_when_db_unreachable() {
        // org/limit バリデーション通過後、DB 接続で落ちる → 500 (黙って 200 にならない)
        let server = MockServer::start().await;
        let state = AppState {
            pool: Some(dead_pool()),
            flickr: Some(test_flickr(&server)),
            cam: None,
        };
        let (status, body) = send(
            state,
            Request::post("/import")
                .header(ORGANIZATION_HEADER, ORG)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["message"], "internal error");
    }
}
