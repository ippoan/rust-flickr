//! DB アクセス層 (rust-logi と同一 DB を共有 — 案A、Refs #1)。
//!
//! テーブル (`flickr_tokens` / `flickr_oauth_sessions` / `flickr_photo`) と
//! RLS 関数 `set_current_organization()` は rust-logi の migration が所有する。
//! 本サービスは migration を持たず、既存スキーマに対して読み書きだけ行う。

use sqlx::postgres::PgPoolOptions;
use sqlx::{PgConnection, PgPool};

use crate::error::ApiError;
use crate::flickr::AccessToken;

/// DATABASE_URL から lazy pool を作る (boot 時に DB 接続を要求しない)
pub fn lazy_pool(database_url: &str) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect_lazy(database_url)
}

/// RLS の現 organization を設定する。**必ず**各リクエストの接続取得直後に呼ぶ。
/// org はヘッダから明示で受けたものだけを渡す (デフォルト org への
/// 暗黙フォールバック禁止 — 3/20 の「黙って 0 件」の根治、Refs #1)。
pub async fn set_current_organization(
    conn: &mut PgConnection,
    organization_id: &str,
) -> Result<(), ApiError> {
    sqlx::query("SELECT set_current_organization($1)")
        .bind(organization_id)
        .execute(conn)
        .await?;
    Ok(())
}

/// OAuth セッション (request token) を保存
pub async fn insert_oauth_session(
    conn: &mut PgConnection,
    organization_id: &str,
    request_token: &str,
    request_token_secret: &str,
) -> Result<(), ApiError> {
    sqlx::query(
        r#"
        INSERT INTO flickr_oauth_sessions (organization_id, request_token, request_token_secret)
        VALUES ($1::uuid, $2, $3)
        "#,
    )
    .bind(organization_id)
    .bind(request_token)
    .bind(request_token_secret)
    .execute(conn)
    .await?;
    Ok(())
}

/// access token を UPSERT (organization 単位で 1 行)
pub async fn upsert_token(
    conn: &mut PgConnection,
    organization_id: &str,
    token: &AccessToken,
) -> Result<(), ApiError> {
    sqlx::query(
        r#"
        INSERT INTO flickr_tokens (organization_id, access_token, access_token_secret, user_nsid, username)
        VALUES ($1::uuid, $2, $3, $4, $5)
        ON CONFLICT (organization_id) DO UPDATE SET
            access_token = EXCLUDED.access_token,
            access_token_secret = EXCLUDED.access_token_secret,
            user_nsid = EXCLUDED.user_nsid,
            username = EXCLUDED.username,
            updated_at = NOW()
        "#,
    )
    .bind(organization_id)
    .bind(&token.token)
    .bind(&token.secret)
    .bind(&token.user_nsid)
    .bind(&token.username)
    .execute(conn)
    .await?;
    Ok(())
}

/// 使用済み OAuth セッションを削除 (失敗は無視して良い後始末)
pub async fn delete_oauth_session(conn: &mut PgConnection, request_token: &str) {
    let _ = sqlx::query("DELETE FROM flickr_oauth_sessions WHERE request_token = $1")
        .bind(request_token)
        .execute(conn)
        .await;
}

/// flickr_tokens の access token (RLS で現 org の行だけ見える)
pub struct FlickrTokenRow {
    pub access_token: String,
    pub access_token_secret: String,
}

/// 現 org の access token を取得 (無ければ None = 呼び出し側で 412)
pub async fn get_flickr_token(conn: &mut PgConnection) -> Result<Option<FlickrTokenRow>, ApiError> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT access_token, access_token_secret FROM flickr_tokens LIMIT 1")
            .fetch_optional(conn)
            .await?;
    Ok(
        row.map(|(access_token, access_token_secret)| FlickrTokenRow {
            access_token,
            access_token_secret,
        }),
    )
}

/// 未検証の cam_files.flickr_id を最大 limit 件取得
pub async fn list_unverified_flickr_ids(
    conn: &mut PgConnection,
    limit: i64,
) -> Result<Vec<String>, ApiError> {
    let rows: Vec<(String,)> = sqlx::query_as(
        r#"
        SELECT cf.flickr_id
        FROM cam_files cf
        LEFT JOIN flickr_photo fp ON cf.flickr_id = fp.id AND cf.organization_id = fp.organization_id
        WHERE cf.flickr_id IS NOT NULL AND fp.id IS NULL
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// 検証済み写真を flickr_photo に登録 (冪等: ON CONFLICT DO NOTHING)
pub async fn insert_flickr_photo(
    conn: &mut PgConnection,
    organization_id: &str,
    photo: &crate::flickr::PhotoInfo,
) -> Result<(), ApiError> {
    sqlx::query(
        r#"
        INSERT INTO flickr_photo (id, organization_id, secret, server)
        VALUES ($1, $2::uuid, $3, $4)
        ON CONFLICT (organization_id, id) DO NOTHING
        "#,
    )
    .bind(&photo.id)
    .bind(organization_id)
    .bind(&photo.secret)
    .bind(&photo.server)
    .execute(conn)
    .await?;
    Ok(())
}

/// 未検証の残数
pub async fn count_unverified(conn: &mut PgConnection) -> Result<i64, ApiError> {
    let (count,): (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(*)
        FROM cam_files cf
        LEFT JOIN flickr_photo fp ON cf.flickr_id = fp.id AND cf.organization_id = fp.organization_id
        WHERE cf.flickr_id IS NOT NULL AND fp.id IS NULL
        "#,
    )
    .fetch_one(conn)
    .await?;
    Ok(count)
}

// ---- cam_files 同期 (POST /sync, Refs #9) ----

/// cam_files の 1 行 (sync / upload ループで使用)
pub struct CamFileRow {
    pub name: String,
    pub date: String,
    pub hour: String,
}

/// 最終レコードの (date, hour) — sync の再開位置 (rust-logi 互換: name 降順)
pub async fn last_cam_file(conn: &mut PgConnection) -> Result<Option<(String, String)>, ApiError> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT date, hour FROM cam_files ORDER BY name DESC LIMIT 1")
            .fetch_optional(conn)
            .await?;
    Ok(row)
}

/// カメラ上のファイルを cam_files に UPSERT
pub async fn upsert_cam_file(
    conn: &mut PgConnection,
    organization_id: &str,
    name: &str,
    date: &str,
    hour: &str,
    file_type: &str,
    cam: &str,
) -> Result<(), ApiError> {
    sqlx::query(
        r#"
        INSERT INTO cam_files (name, organization_id, date, hour, type, cam)
        VALUES ($1, $2::uuid, $3, $4, $5, $6)
        ON CONFLICT (organization_id, name) DO UPDATE SET
            date = EXCLUDED.date, hour = EXCLUDED.hour,
            type = EXCLUDED.type, cam = EXCLUDED.cam
        "#,
    )
    .bind(name)
    .bind(organization_id)
    .bind(date)
    .bind(hour)
    .bind(file_type)
    .bind(cam)
    .execute(conn)
    .await?;
    Ok(())
}

/// Flickr 未アップロード (flickr_id IS NULL) のファイルを古い順に取得
pub async fn list_unuploaded_cam_files(
    conn: &mut PgConnection,
    start_date: &str,
    limit: i64,
) -> Result<Vec<CamFileRow>, ApiError> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        r#"
        SELECT name, date, hour FROM cam_files
        WHERE date >= $1 AND flickr_id IS NULL
        ORDER BY name
        LIMIT $2
        "#,
    )
    .bind(start_date)
    .bind(limit)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(name, date, hour)| CamFileRow { name, date, hour })
        .collect())
}

/// Flickr 未アップロードの残数
pub async fn count_unuploaded_cam_files(
    conn: &mut PgConnection,
    start_date: &str,
) -> Result<i64, ApiError> {
    let (count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM cam_files WHERE date >= $1 AND flickr_id IS NULL")
            .bind(start_date)
            .fetch_one(conn)
            .await?;
    Ok(count)
}

/// Flickr 未アップロードの残数 ((date, hour) 粒度 floor 版、Refs #24)。
/// SD に実在する最古日付の途中 hour までしか残っていないケースで、その hour
/// 未満の (= SD から既に消えた) 古い行が `count_unuploaded_cam_files` の日付
/// 粒度 floor では除外できず「永久ゴースト」として残数に積まれ続ける問題の
/// 修正。条件式は (date > floor_date) OR (date == floor_date AND hour >= floor_hour)。
pub async fn count_unuploaded_cam_files_from(
    conn: &mut PgConnection,
    floor_date: &str,
    floor_hour: &str,
) -> Result<i64, ApiError> {
    let (count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM cam_files \
         WHERE flickr_id IS NULL \
           AND (date > $1 OR (date = $1 AND hour >= $2))",
    )
    .bind(floor_date)
    .bind(floor_hour)
    .fetch_one(conn)
    .await?;
    Ok(count)
}

/// アップロード成功した flickr_id を記録
pub async fn set_cam_file_flickr_id(
    conn: &mut PgConnection,
    name: &str,
    flickr_id: &str,
) -> Result<(), ApiError> {
    sqlx::query("UPDATE cam_files SET flickr_id = $1 WHERE name = $2")
        .bind(flickr_id)
        .bind(name)
        .execute(conn)
        .await?;
    Ok(())
}

// ---- 日次集計 (GET /stats, Refs #12) ----

/// 撮影日別の (date, files, uploaded, verified) を新しい順に最大 limit 日分
pub async fn day_stats(
    conn: &mut PgConnection,
    limit: i64,
) -> Result<Vec<(String, i64, i64, i64)>, ApiError> {
    Ok(sqlx::query_as(
        r#"
        SELECT cf.date,
               count(*)::bigint AS files,
               count(cf.flickr_id)::bigint AS uploaded,
               count(fp.id)::bigint AS verified
        FROM cam_files cf
        LEFT JOIN flickr_photo fp ON cf.flickr_id = fp.id AND cf.organization_id = fp.organization_id
        GROUP BY cf.date
        ORDER BY cf.date DESC
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(conn)
    .await?)
}

/// 全期間の flickr_id IS NULL 残数 (= 未アップロード)
pub async fn count_total_unuploaded(conn: &mut PgConnection) -> Result<i64, ApiError> {
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM cam_files WHERE flickr_id IS NULL")
        .fetch_one(conn)
        .await?;
    Ok(count)
}

/// 最古の未アップロード撮影日 (backfill 進捗の可視化用、Refs #12)
pub async fn oldest_unuploaded_date(conn: &mut PgConnection) -> Result<Option<String>, ApiError> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT min(date) FROM cam_files WHERE flickr_id IS NULL")
            .fetch_optional(conn)
            .await?;
    Ok(row.and_then(|(d,)| d))
}
