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

/// POST /sync のリクエスト (body は省略可、Refs #9)
#[derive(Serialize, Deserialize, Default, TS)]
#[ts(export)]
pub struct SyncRequest {
    /// 1 回で Flickr にアップロードする最大件数 (default 50)。
    /// 0 で upload を skip し cam_files の同期のみ行う。
    /// cf-flickr-proxy (edge 100s) 経由で呼ぶ場合は小さい値を渡すこと
    #[ts(optional, type = "number")]
    pub upload_limit: Option<i64>,
}

/// POST /sync のレスポンス (Refs #9)。
/// 旧 SyncCamFilesResponse との差: upload を同期実行に変えたので
/// `flickr_upload_started` (= 件数を投げただけ) ではなく実績
/// (`uploaded_count` / `upload_errors`) を返す
#[derive(Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SyncResponse {
    /// カメラ上で走査対象になった日付ディレクトリ数
    pub processed_dates: i32,
    /// 走査対象になった (日付, 時間) ディレクトリ数
    pub processed_hours: i32,
    /// cam_files に UPSERT した件数
    pub new_files: i32,
    /// flickr_tokens に access token があったか
    /// (false の場合 upload は skip される — 認可は GET /oauth/url から)
    pub flickr_token_present: bool,
    /// Flickr へのアップロード成功数
    pub uploaded_count: i32,
    /// アップロード失敗数 (個別失敗は処理継続、詳細は log)
    pub upload_errors: i32,
    /// flickr_id IS NULL で残っている件数 (次回 sync で続きから処理)
    pub remaining_unuploaded: i32,
    pub message: String,
}
