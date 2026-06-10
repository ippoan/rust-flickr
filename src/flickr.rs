//! Flickr API クライアント。
//!
//! endpoint を struct フィールドに持ち `with_endpoints` で注入できるようにする
//! (= wiremock でローカル HTTP サーバに差し替え可能、rust-alc-api の
//! 外部 API 開発フローに倣う)。

use std::collections::HashMap;

use crate::error::ApiError;
use crate::oauth1;

const REQUEST_TOKEN_URL: &str = "https://www.flickr.com/services/oauth/request_token";
const ACCESS_TOKEN_URL: &str = "https://www.flickr.com/services/oauth/access_token";
const AUTHORIZE_URL: &str = "https://www.flickr.com/services/oauth/authorize";
const REST_URL: &str = "https://www.flickr.com/services/rest/";
const UPLOAD_URL: &str = "https://up.flickr.com/services/upload/";

/// Flickr OAuth 1.0a 設定 (env から)
#[derive(Clone)]
pub struct FlickrConfig {
    pub consumer_key: String,
    pub consumer_secret: String,
    pub callback_url: String,
}

impl FlickrConfig {
    pub fn from_env() -> Option<Self> {
        let consumer_key = std::env::var("FLICKR_CONSUMER_KEY").ok()?;
        let consumer_secret = std::env::var("FLICKR_CONSUMER_SECRET").ok()?;
        let callback_url = std::env::var("FLICKR_CALLBACK_URL")
            .unwrap_or_else(|_| "https://test.mtamaramu.com/flickr/callback".to_string());
        Some(Self {
            consumer_key,
            consumer_secret,
            callback_url,
        })
    }
}

/// request token (一時トークン)
pub struct RequestToken {
    pub token: String,
    pub secret: String,
    pub authorization_url: String,
}

/// access token (永続トークン)
pub struct AccessToken {
    pub token: String,
    pub secret: String,
    pub user_nsid: String,
    pub username: String,
}

/// flickr.photos.getInfo の必要フィールド
#[derive(serde::Deserialize)]
pub struct PhotoInfo {
    pub id: String,
    pub server: String,
    pub secret: String,
}

#[derive(serde::Deserialize)]
struct GetInfoResponse {
    photo: Option<PhotoInfo>,
    stat: String,
}

#[derive(Clone)]
pub struct FlickrClient {
    http: reqwest::Client,
    config: FlickrConfig,
    request_token_url: String,
    access_token_url: String,
    authorize_url: String,
    rest_url: String,
    upload_url: String,
}

impl FlickrClient {
    pub fn new(config: FlickrConfig) -> Self {
        Self::with_endpoints(
            config,
            REQUEST_TOKEN_URL.to_string(),
            ACCESS_TOKEN_URL.to_string(),
            AUTHORIZE_URL.to_string(),
            REST_URL.to_string(),
        )
    }

    pub fn with_endpoints(
        config: FlickrConfig,
        request_token_url: String,
        access_token_url: String,
        authorize_url: String,
        rest_url: String,
    ) -> Self {
        Self {
            http: reqwest::Client::new(),
            config,
            request_token_url,
            access_token_url,
            authorize_url,
            rest_url,
            upload_url: UPLOAD_URL.to_string(),
        }
    }

    /// upload endpoint の差し替え (wiremock テスト用)
    #[cfg(test)]
    pub fn with_upload_url(mut self, url: impl Into<String>) -> Self {
        self.upload_url = url.into();
        self
    }

    /// OAuth 署名付き GET → form-encoded レスポンスをパース
    async fn signed_get_form(
        &self,
        url: &str,
        mut params: HashMap<String, String>,
        token_secret: Option<&str>,
        what: &str,
    ) -> Result<HashMap<String, String>, ApiError> {
        let signature = oauth1::sign(
            "GET",
            url,
            &params,
            &self.config.consumer_secret,
            token_secret,
        );
        params.insert("oauth_signature".to_string(), signature);
        let auth_header = oauth1::auth_header(&params);

        let response = self
            .http
            .get(url)
            .header("Authorization", format!("OAuth {auth_header}"))
            .send()
            .await
            .map_err(|e| ApiError::Upstream(format!("flickr {what} request failed: {e}")))?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            tracing::error!(%status, body, "flickr {what} failed");
            return Err(ApiError::Upstream(format!(
                "flickr {what} failed: {status} - {body}"
            )));
        }
        Ok(oauth1::parse_form(&body))
    }

    fn oauth_base_params(&self) -> HashMap<String, String> {
        let mut params = HashMap::new();
        params.insert(
            "oauth_consumer_key".to_string(),
            self.config.consumer_key.clone(),
        );
        params.insert("oauth_nonce".to_string(), oauth1::nonce());
        params.insert(
            "oauth_signature_method".to_string(),
            "HMAC-SHA1".to_string(),
        );
        params.insert("oauth_timestamp".to_string(), oauth1::timestamp());
        params.insert("oauth_version".to_string(), "1.0".to_string());
        params
    }

    /// request token を取得して認可 URL を組み立てる
    pub async fn get_request_token(&self) -> Result<RequestToken, ApiError> {
        let mut params = self.oauth_base_params();
        params.insert(
            "oauth_callback".to_string(),
            self.config.callback_url.clone(),
        );

        let form = self
            .signed_get_form(
                &self.request_token_url.clone(),
                params,
                None,
                "request_token",
            )
            .await?;

        let token = form.get("oauth_token").ok_or_else(|| {
            ApiError::Upstream("oauth_token not found in request_token response".to_string())
        })?;
        let secret = form.get("oauth_token_secret").ok_or_else(|| {
            ApiError::Upstream("oauth_token_secret not found in request_token response".to_string())
        })?;

        Ok(RequestToken {
            token: token.clone(),
            secret: secret.clone(),
            authorization_url: format!("{}?oauth_token={}&perms=write", self.authorize_url, token),
        })
    }

    /// verifier を access token に交換する
    pub async fn get_access_token(
        &self,
        oauth_token: &str,
        oauth_verifier: &str,
        request_token_secret: &str,
    ) -> Result<AccessToken, ApiError> {
        let mut params = self.oauth_base_params();
        params.insert("oauth_token".to_string(), oauth_token.to_string());
        params.insert("oauth_verifier".to_string(), oauth_verifier.to_string());

        let form = self
            .signed_get_form(
                &self.access_token_url.clone(),
                params,
                Some(request_token_secret),
                "access_token",
            )
            .await?;

        let token = form.get("oauth_token").ok_or_else(|| {
            ApiError::Upstream("oauth_token not found in access_token response".to_string())
        })?;
        let secret = form.get("oauth_token_secret").ok_or_else(|| {
            ApiError::Upstream("oauth_token_secret not found in access_token response".to_string())
        })?;

        Ok(AccessToken {
            token: token.clone(),
            secret: secret.clone(),
            user_nsid: form.get("user_nsid").cloned().unwrap_or_default(),
            username: form.get("username").cloned().unwrap_or_default(),
        })
    }

    /// flickr.photos.getInfo を OAuth 署名付きで呼ぶ。
    /// /import のループ用 — 失敗は呼び出し側で集計するため String エラーで返す
    /// (rust-logi `call_flickr_get_info` の移植)。
    pub async fn photos_get_info(
        &self,
        photo_id: &str,
        access_token: &str,
        access_token_secret: &str,
    ) -> Result<PhotoInfo, String> {
        let mut params = self.oauth_base_params();
        params.insert("oauth_token".to_string(), access_token.to_string());
        params.insert("method".to_string(), "flickr.photos.getInfo".to_string());
        params.insert("photo_id".to_string(), photo_id.to_string());
        params.insert("format".to_string(), "json".to_string());
        params.insert("nojsoncallback".to_string(), "1".to_string());

        let signature = oauth1::sign(
            "GET",
            &self.rest_url,
            &params,
            &self.config.consumer_secret,
            Some(access_token_secret),
        );
        params.insert("oauth_signature".to_string(), signature);

        // OAuth パラメータは Authorization ヘッダへ、API パラメータはクエリへ分離
        let oauth_keys = [
            "oauth_consumer_key",
            "oauth_nonce",
            "oauth_signature_method",
            "oauth_timestamp",
            "oauth_token",
            "oauth_version",
            "oauth_signature",
        ];
        let oauth_params: HashMap<String, String> = params
            .iter()
            .filter(|(k, _)| oauth_keys.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let auth_header = oauth1::auth_header(&oauth_params);
        let query_params: Vec<(&str, &str)> = params
            .iter()
            .filter(|(k, _)| !oauth_keys.contains(&k.as_str()))
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let response = self
            .http
            .get(&self.rest_url)
            .header("Authorization", format!("OAuth {auth_header}"))
            .query(&query_params)
            .send()
            .await
            .map_err(|e| format!("HTTP request failed for photo {photo_id}: {e}"))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(format!(
                "Flickr API error for photo {photo_id}: {status} - {body}"
            ));
        }

        let api_response: GetInfoResponse = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse Flickr response for photo {photo_id}: {e}"))?;

        if api_response.stat != "ok" {
            return Err(format!(
                "Flickr API returned stat={} for photo {photo_id}",
                api_response.stat
            ));
        }

        api_response
            .photo
            .ok_or_else(|| format!("No photo data in Flickr response for photo {photo_id}"))
    }

    /// 写真を Flickr Upload API にアップロードして photo id を返す (Refs #9)。
    /// OAuth1 署名は oauth/API パラメータのみが対象で photo バイナリは含めない
    /// (Flickr Upload API 仕様)。`tags=upBySytem` は hono-logi / rust-logi 互換
    /// (タイポも互換維持)。upload ループで per-file 集計するため失敗は String
    pub async fn upload_photo(
        &self,
        access_token: &str,
        access_token_secret: &str,
        title: &str,
        data: Vec<u8>,
    ) -> Result<String, String> {
        let mut params = self.oauth_base_params();
        params.insert("oauth_token".to_string(), access_token.to_string());
        params.insert("title".to_string(), title.to_string());
        params.insert("tags".to_string(), "upBySytem".to_string());

        let signature = oauth1::sign(
            "POST",
            &self.upload_url,
            &params,
            &self.config.consumer_secret,
            Some(access_token_secret),
        );
        params.insert("oauth_signature".to_string(), signature);

        let mut form = reqwest::multipart::Form::new();
        for (k, v) in &params {
            form = form.text(k.clone(), v.clone());
        }
        let part = reqwest::multipart::Part::bytes(data)
            .file_name(title.to_string())
            .mime_str("application/octet-stream")
            .map_err(|e| format!("multipart mime for {title}: {e}"))?;
        form = form.part("photo", part);

        let response = self
            .http
            .post(&self.upload_url)
            .multipart(form)
            .send()
            .await
            .map_err(|e| format!("flickr upload request failed for {title}: {e}"))?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            tracing::warn!(%status, body, title, "flickr upload failed");
            return Err(format!("flickr upload failed for {title}: {status}"));
        }

        parse_upload_photoid(&body).ok_or_else(|| {
            tracing::warn!(body, title, "flickr upload: photoid missing in response");
            format!("flickr upload: photoid not found in response for {title}")
        })
    }
}

/// Flickr Upload API レスポンス XML から `<photoid>…</photoid>` を抽出
fn parse_upload_photoid(xml: &str) -> Option<String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut in_photoid = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                if e.name().as_ref() == b"photoid" {
                    in_photoid = true;
                }
            }
            Ok(Event::Text(ref e)) => {
                if in_photoid {
                    return e.unescape().ok().map(|s| s.to_string());
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn test_config() -> FlickrConfig {
        FlickrConfig {
            consumer_key: "ck".to_string(),
            consumer_secret: "cs".to_string(),
            callback_url: "https://example.com/cb".to_string(),
        }
    }

    fn client_for(server: &MockServer) -> FlickrClient {
        FlickrClient::with_endpoints(
            test_config(),
            format!("{}/request_token", server.uri()),
            format!("{}/access_token", server.uri()),
            format!("{}/authorize", server.uri()),
            format!("{}/rest/", server.uri()),
        )
    }

    fn auth_header_of(req: &Request) -> String {
        req.headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn new_uses_prod_endpoints() {
        let c = FlickrClient::new(test_config());
        assert_eq!(c.request_token_url, REQUEST_TOKEN_URL);
        assert_eq!(c.access_token_url, ACCESS_TOKEN_URL);
        assert_eq!(c.authorize_url, AUTHORIZE_URL);
        assert_eq!(c.rest_url, REST_URL);
        assert_eq!(c.upload_url, UPLOAD_URL);
    }

    #[test]
    fn parse_upload_photoid_extracts_id() {
        assert_eq!(
            parse_upload_photoid(r#"<rsp stat="ok"><photoid>54321</photoid></rsp>"#),
            Some("54321".to_string())
        );
        assert_eq!(
            parse_upload_photoid(r#"<rsp stat="fail"><err code="5" msg="bad"/></rsp>"#),
            None
        );
        assert_eq!(parse_upload_photoid("not xml"), None);
    }

    #[tokio::test]
    async fn upload_photo_success_returns_photoid() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/upload/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<?xml version="1.0"?><rsp stat="ok"><photoid>9876</photoid></rsp>"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let client = client_for(&server).with_upload_url(format!("{}/upload/", server.uri()));
        let id = client
            .upload_photo("at", "ats", "Event20260101_000001.jpg", vec![1, 2, 3])
            .await
            .unwrap();
        assert_eq!(id, "9876");
    }

    #[tokio::test]
    async fn upload_photo_http_error_is_err_without_body_echo() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/upload/"))
            .respond_with(ResponseTemplate::new(500).set_body_string("secret internals"))
            .mount(&server)
            .await;

        let client = client_for(&server).with_upload_url(format!("{}/upload/", server.uri()));
        let err = client
            .upload_photo("at", "ats", "x.jpg", vec![0])
            .await
            .unwrap_err();
        // upstream の body は log のみに出し、Err 文字列に echo しない
        assert!(err.contains("flickr upload failed"));
        assert!(!err.contains("secret internals"));
    }

    #[tokio::test]
    async fn upload_photo_stat_fail_is_err() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/upload/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<rsp stat="fail"><err code="98" msg="Invalid auth token"/></rsp>"#,
            ))
            .mount(&server)
            .await;

        let client = client_for(&server).with_upload_url(format!("{}/upload/", server.uri()));
        let err = client
            .upload_photo("at", "ats", "x.jpg", vec![0])
            .await
            .unwrap_err();
        assert!(err.contains("photoid not found"));
    }

    #[tokio::test]
    async fn request_token_success() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/request_token"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "oauth_callback_confirmed=true&oauth_token=rt&oauth_token_secret=rts",
            ))
            .expect(1)
            .mount(&server)
            .await;

        let rt = client_for(&server).get_request_token().await.unwrap();
        assert_eq!(rt.token, "rt");
        assert_eq!(rt.secret, "rts");
        assert!(rt
            .authorization_url
            .ends_with("/authorize?oauth_token=rt&perms=write"));

        // OAuth ヘッダの形を検証 (callback / consumer_key / signature が入っている)
        let received = &server.received_requests().await.unwrap()[0];
        let auth = auth_header_of(received);
        assert!(auth.starts_with("OAuth "));
        assert!(auth.contains("oauth_consumer_key=\"ck\""));
        assert!(auth.contains("oauth_callback=\"https%3A%2F%2Fexample.com%2Fcb\""));
        assert!(auth.contains("oauth_signature=\""));
        assert!(auth.contains("oauth_signature_method=\"HMAC-SHA1\""));
    }

    #[tokio::test]
    async fn request_token_upstream_error_is_424_not_200() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/request_token"))
            .respond_with(
                ResponseTemplate::new(401).set_body_string("oauth_problem=consumer_key_rejected"),
            )
            .mount(&server)
            .await;

        let err = client_for(&server)
            .get_request_token()
            .await
            .err()
            .expect("expected error");
        match err {
            ApiError::Upstream(m) => {
                assert!(m.contains("401"));
                assert!(m.contains("consumer_key_rejected"));
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn request_token_missing_field_is_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/request_token"))
            .respond_with(ResponseTemplate::new(200).set_body_string("oauth_token=rt"))
            .mount(&server)
            .await;

        let err = client_for(&server)
            .get_request_token()
            .await
            .err()
            .expect("expected error");
        assert!(matches!(err, ApiError::Upstream(m) if m.contains("oauth_token_secret")));
    }

    #[tokio::test]
    async fn request_token_unreachable_is_upstream_error() {
        let config = test_config();
        let client = FlickrClient::with_endpoints(
            config,
            "http://127.0.0.1:1/request_token".to_string(),
            "http://127.0.0.1:1/access_token".to_string(),
            "http://127.0.0.1:1/authorize".to_string(),
            "http://127.0.0.1:1/rest/".to_string(),
        );
        let err = client
            .get_request_token()
            .await
            .err()
            .expect("expected error");
        assert!(matches!(err, ApiError::Upstream(m) if m.contains("request failed")));
    }

    #[tokio::test]
    async fn access_token_success() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/access_token"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "oauth_token=at&oauth_token_secret=ats&user_nsid=12345%40N00&username=tester",
            ))
            .expect(1)
            .mount(&server)
            .await;

        let at = client_for(&server)
            .get_access_token("rt", "verifier", "rts")
            .await
            .unwrap();
        assert_eq!(at.token, "at");
        assert_eq!(at.secret, "ats");
        // form 値は URL デコードせず raw のまま保存する (旧実装と同挙動)
        assert_eq!(at.user_nsid, "12345%40N00");
        assert_eq!(at.username, "tester");

        let received = &server.received_requests().await.unwrap()[0];
        let auth = auth_header_of(received);
        assert!(auth.contains("oauth_token=\"rt\""));
        assert!(auth.contains("oauth_verifier=\"verifier\""));
    }

    #[tokio::test]
    async fn access_token_nsid_username_optional() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/access_token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("oauth_token=at&oauth_token_secret=ats"),
            )
            .mount(&server)
            .await;

        let at = client_for(&server)
            .get_access_token("rt", "v", "rts")
            .await
            .unwrap();
        assert_eq!(at.user_nsid, "");
        assert_eq!(at.username, "");
    }

    #[tokio::test]
    async fn access_token_upstream_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/access_token"))
            .respond_with(ResponseTemplate::new(401).set_body_string("oauth_problem=token_expired"))
            .mount(&server)
            .await;

        let err = client_for(&server)
            .get_access_token("rt", "v", "rts")
            .await
            .err()
            .expect("expected error");
        assert!(matches!(err, ApiError::Upstream(m) if m.contains("token_expired")));
    }

    #[tokio::test]
    async fn photos_get_info_success_and_query_split() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "photo": {"id": "5050", "server": "65535", "secret": "abc123"},
                "stat": "ok"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let photo = client_for(&server)
            .photos_get_info("5050", "at", "ats")
            .await
            .unwrap();
        assert_eq!(photo.id, "5050");
        assert_eq!(photo.server, "65535");
        assert_eq!(photo.secret, "abc123");

        // API パラメータはクエリ、OAuth パラメータはヘッダに分離されている
        let received = &server.received_requests().await.unwrap()[0];
        let query = received.url.query().unwrap();
        assert!(query.contains("method=flickr.photos.getInfo"));
        assert!(query.contains("photo_id=5050"));
        assert!(query.contains("nojsoncallback=1"));
        assert!(!query.contains("oauth_signature"));
        let auth = auth_header_of(received);
        assert!(auth.contains("oauth_token=\"at\""));
        assert!(auth.contains("oauth_signature=\""));
        assert!(!auth.contains("photo_id"));
    }

    #[tokio::test]
    async fn photos_get_info_stat_fail() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "stat": "fail", "code": 1, "message": "Photo not found"
            })))
            .mount(&server)
            .await;

        let err = client_for(&server)
            .photos_get_info("404404", "at", "ats")
            .await
            .err()
            .expect("expected error");
        assert!(err.contains("stat=fail"));
        assert!(err.contains("404404"));
    }

    #[tokio::test]
    async fn photos_get_info_http_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/"))
            .respond_with(ResponseTemplate::new(500).set_body_string("oops"))
            .mount(&server)
            .await;

        let err = client_for(&server)
            .photos_get_info("1", "at", "ats")
            .await
            .err()
            .expect("expected error");
        assert!(err.contains("500"));
    }

    #[tokio::test]
    async fn photos_get_info_parse_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let err = client_for(&server)
            .photos_get_info("1", "at", "ats")
            .await
            .err()
            .expect("expected error");
        assert!(err.contains("parse"));
    }
}
