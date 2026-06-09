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
