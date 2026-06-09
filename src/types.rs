//! REST API の request / response 型。
//!
//! PR4 (Refs #1) で `#[derive(TS)]` を付けて TypeScript 型を生成する予定の
//! 型はこのファイルに集約する。

use serde::{Deserialize, Serialize};

/// GET /oauth/url のレスポンス
#[derive(Serialize, Deserialize)]
pub struct OauthUrlResponse {
    pub authorization_url: String,
    pub request_token: String,
    pub request_token_secret: String,
}

/// POST /oauth/callback のリクエスト
#[derive(Serialize, Deserialize)]
pub struct OauthCallbackRequest {
    pub oauth_token: String,
    pub oauth_verifier: String,
    pub request_token_secret: String,
}

/// POST /oauth/callback のレスポンス。
/// access token / secret は DB に保存するのみで **echo しない**
/// (旧 gRPC `TokenResponse` はトークンを返していたが、front は使っておらず
/// レスポンス経由の値漏れを避けるため意図的に落とした)。
#[derive(Serialize, Deserialize)]
pub struct OauthCallbackResponse {
    pub user_nsid: String,
    pub username: String,
    pub saved: bool,
}
