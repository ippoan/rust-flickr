//! REST API の request / response 型。
//!
//! PR4 (Refs #1) で `#[derive(TS)]` を付けて TypeScript 型を生成する予定の
//! 型はこのファイルに集約する。

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// GET /oauth/url のレスポンス
#[derive(Serialize, Deserialize, TS)]
#[ts(export)]
pub struct OauthUrlResponse {
    pub authorization_url: String,
    pub request_token: String,
    pub request_token_secret: String,
}

/// POST /oauth/callback のリクエスト
#[derive(Serialize, Deserialize, TS)]
#[ts(export)]
pub struct OauthCallbackRequest {
    pub oauth_token: String,
    pub oauth_verifier: String,
    pub request_token_secret: String,
}

/// POST /oauth/callback のレスポンス。
/// access token / secret は DB に保存するのみで **echo しない**
/// (旧 gRPC `TokenResponse` はトークンを返していたが、front は使っておらず
/// レスポンス経由の値漏れを避けるため意図的に落とした)。
#[derive(Serialize, Deserialize, TS)]
#[ts(export)]
pub struct OauthCallbackResponse {
    pub user_nsid: String,
    pub username: String,
    pub saved: bool,
}

/// POST /import のリクエスト (body は省略可)
#[derive(Serialize, Deserialize, Default, TS)]
#[ts(export)]
pub struct ImportRequest {
    /// 1 回で処理する最大件数 (default 500)
    #[ts(optional, type = "number")]
    pub limit: Option<i64>,
}

/// 検証済み Flickr 写真 (flickr_photo へ登録した行)
#[derive(Serialize, Deserialize, TS)]
#[ts(export)]
pub struct FlickrPhoto {
    pub id: String,
    pub secret: String,
    pub server: String,
}

/// POST /import のレスポンス
#[derive(Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ImportResponse {
    pub imported_count: i32,
    pub errors_count: i32,
    pub remaining_count: i32,
    pub photos: Vec<FlickrPhoto>,
}
