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

#[derive(Clone)]
pub struct FlickrClient {
    http: reqwest::Client,
    config: FlickrConfig,
    request_token_url: String,
    access_token_url: String,
    authorize_url: String,
}

impl FlickrClient {
    pub fn new(config: FlickrConfig) -> Self {
        Self::with_endpoints(
            config,
            REQUEST_TOKEN_URL.to_string(),
            ACCESS_TOKEN_URL.to_string(),
            AUTHORIZE_URL.to_string(),
        )
    }

    pub fn with_endpoints(
        config: FlickrConfig,
        request_token_url: String,
        access_token_url: String,
        authorize_url: String,
    ) -> Self {
        Self {
            http: reqwest::Client::new(),
            config,
            request_token_url,
            access_token_url,
            authorize_url,
        }
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
}
