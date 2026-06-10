//! カメラ (SD カード CGI) クライアント。
//!
//! rust-logi `cam_files_service.rs` からの移植 (Refs #9)。カメラは
//! CF Access (Service Token) 越しに公開され、その内側で HTTP Digest 認証
//! (RFC 2617 / MD5) を要求する。SD カードの一覧は XML で返る:
//! ディレクトリは `<Dir name="20250323"/>`、ファイルは
//! `<Name>Event20250323_005902.jpg</Name>`。`_!` を含むファイル名は
//! カメラの一時ファイルなので除外する。

use std::collections::HashMap;

use md5::{Digest as _, Md5};
use quick_xml::events::Event;
use quick_xml::Reader;

use crate::error::ApiError;

/// カメラ設定 (env から)。rust-logi と同名の env を読む = Cloud Run の値を流用可
#[derive(Clone)]
pub struct CamConfig {
    pub digest_user: String,
    pub digest_pass: String,
    pub machine_name: String,
    pub sdcard_cgi: String,
    pub mp4_cgi: String,
    pub jpg_cgi: String,
    pub cf_access_client_id: Option<String>,
    pub cf_access_client_secret: Option<String>,
}

impl CamConfig {
    pub fn from_env() -> Option<Self> {
        Some(Self {
            digest_user: std::env::var("CAM_DIGEST_USER").ok()?,
            digest_pass: std::env::var("CAM_DIGEST_PASS").ok()?,
            machine_name: std::env::var("CAM_MACHINE_NAME").ok()?,
            sdcard_cgi: std::env::var("CAM_SDCARD_CGI").ok()?,
            mp4_cgi: std::env::var("CAM_MP4_CGI").ok()?,
            jpg_cgi: std::env::var("CAM_JPG_CGI").ok()?,
            cf_access_client_id: std::env::var("CAM_CF_ACCESS_CLIENT_ID").ok(),
            cf_access_client_secret: std::env::var("CAM_CF_ACCESS_CLIENT_SECRET").ok(),
        })
    }
}

const EVENT_DIR: &str = "/Event";

#[derive(Clone)]
pub struct CamClient {
    http: reqwest::Client,
    config: CamConfig,
}

impl CamClient {
    pub fn new(config: CamConfig) -> Self {
        Self {
            http: reqwest::Client::new(),
            config,
        }
    }

    pub fn machine_name(&self) -> &str {
        &self.config.machine_name
    }

    fn with_cf_access(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let builder = match &self.config.cf_access_client_id {
            Some(id) => builder.header("CF-Access-Client-Id", id),
            None => builder,
        };
        match &self.config.cf_access_client_secret {
            Some(secret) => builder.header("CF-Access-Client-Secret", secret),
            None => builder,
        }
    }

    /// CF Access ヘッダ + (401 なら) Digest 認証リトライ付き GET
    async fn fetch(&self, url: &str) -> Result<reqwest::Response, ApiError> {
        let response = self
            .with_cf_access(self.http.get(url))
            .send()
            .await
            .map_err(|e| ApiError::Upstream(format!("camera request failed for {url}: {e}")))?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            let www_auth = response
                .headers()
                .get("www-authenticate")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            if www_auth.contains("Digest") {
                let auth = digest_auth_header(
                    &self.config.digest_user,
                    &self.config.digest_pass,
                    "GET",
                    url,
                    &www_auth,
                );
                return self
                    .with_cf_access(self.http.get(url))
                    .header("Authorization", auth)
                    .send()
                    .await
                    .map_err(|e| {
                        ApiError::Upstream(format!("camera digest request failed for {url}: {e}"))
                    });
            }
        }
        Ok(response)
    }

    async fn fetch_xml(&self, url: &str) -> Result<String, ApiError> {
        let response = self.fetch(url).await?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            tracing::error!(%status, url, "camera listing failed");
            return Err(ApiError::Upstream(format!(
                "camera listing failed for {url}: {status}"
            )));
        }
        Ok(body)
    }

    /// SD カードの日付ディレクトリ一覧 (YYYYMMDD)
    pub async fn list_dates(&self) -> Result<Vec<String>, ApiError> {
        let url = format!(
            "{}{}{EVENT_DIR}",
            self.config.sdcard_cgi, self.config.machine_name
        );
        Ok(parse_dir_names(&self.fetch_xml(&url).await?))
    }

    /// 指定日付の時間ディレクトリ一覧
    pub async fn list_hours(&self, date: &str) -> Result<Vec<String>, ApiError> {
        let url = format!(
            "{}{}{EVENT_DIR}/{date}",
            self.config.sdcard_cgi, self.config.machine_name
        );
        Ok(parse_dir_names(&self.fetch_xml(&url).await?))
    }

    /// 指定 (日付, 時間) のファイル名一覧
    pub async fn list_file_names(&self, date: &str, hour: &str) -> Result<Vec<String>, ApiError> {
        let url = format!(
            "{}{}{EVENT_DIR}/{date}/{hour}",
            self.config.sdcard_cgi, self.config.machine_name
        );
        Ok(parse_file_names(&self.fetch_xml(&url).await?))
    }

    /// ファイル本体をダウンロード (mp4 / jpg で CGI を出し分け)。
    /// upload ループで per-file 集計するため失敗は String で返す
    pub async fn download(&self, name: &str, date: &str, hour: &str) -> Result<Vec<u8>, String> {
        let base = if name.contains(".mp4") {
            &self.config.mp4_cgi
        } else {
            &self.config.jpg_cgi
        };
        let url = format!(
            "{}{}{EVENT_DIR}/{date}/{hour}/{name}",
            base, self.config.machine_name
        );
        let response = self.fetch(&url).await.map_err(|e| format!("{e:?}"))?;
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if content_type != "application/octet-stream" {
            return Err(format!(
                "unexpected content type for {name}: {content_type}"
            ));
        }
        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| format!("failed to read {name}: {e}"))
    }
}

// ---- Digest 認証 (RFC 2617 / MD5) ----

/// www-authenticate ヘッダから Digest Authorization ヘッダ値を生成。
/// uri には rust-logi (hono-logi 由来) との互換で **full URL** を渡す
/// (RFC 的には request-path だがカメラ側はこれを受ける実績がある)
fn digest_auth_header(
    username: &str,
    password: &str,
    method: &str,
    uri: &str,
    www_auth: &str,
) -> String {
    let mut params = HashMap::new();
    for part in www_auth.split(',') {
        let part = part.trim();
        if let Some(eq_pos) = part.find('=') {
            let key = part[..eq_pos].trim().trim_start_matches("Digest ");
            let value = part[eq_pos + 1..].trim().trim_matches('"');
            params.insert(key.to_string(), value.to_string());
        }
    }
    let realm = params.get("realm").map(String::as_str).unwrap_or("");
    let nonce = params.get("nonce").map(String::as_str).unwrap_or("");
    let qop = params.get("qop").map(String::as_str);

    let nc = "00000001";
    let cnonce_full = uuid::Uuid::new_v4().to_string().replace('-', "");
    let cnonce = &cnonce_full[..13];

    let response = digest_response(
        username, password, method, uri, realm, nonce, qop, nc, cnonce,
    );

    let mut header = format!(
        "Digest username=\"{username}\", realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{uri}\", response=\"{response}\""
    );
    if let Some(qop_val) = qop {
        header.push_str(&format!(", qop={qop_val}, nc={nc}, cnonce=\"{cnonce}\""));
    }
    header
}

/// Digest response 値 (MD5)。既知ベクタでテストできるよう cnonce 等を引数で受ける
#[allow(clippy::too_many_arguments)]
fn digest_response(
    username: &str,
    password: &str,
    method: &str,
    uri: &str,
    realm: &str,
    nonce: &str,
    qop: Option<&str>,
    nc: &str,
    cnonce: &str,
) -> String {
    let ha1 = format!(
        "{:x}",
        Md5::digest(format!("{username}:{realm}:{password}"))
    );
    let ha2 = format!("{:x}", Md5::digest(format!("{method}:{uri}")));
    match qop {
        Some(qop_val) => format!(
            "{:x}",
            Md5::digest(format!("{ha1}:{nonce}:{nc}:{cnonce}:{qop_val}:{ha2}"))
        ),
        None => format!("{:x}", Md5::digest(format!("{ha1}:{nonce}:{ha2}"))),
    }
}

// ---- カメラ XML パース ----

/// `<Dir name="20250323"/>` の name 属性を抽出 (属性名の大文字小文字は不問)
pub(crate) fn parse_dir_names(xml_text: &str) -> Vec<String> {
    let mut reader = Reader::from_str(xml_text);
    let mut dirs = Vec::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(ref e)) | Ok(Event::Start(ref e)) => {
                if e.name().as_ref().eq_ignore_ascii_case(b"dir") {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref().eq_ignore_ascii_case(b"name") {
                            if let Ok(val) = String::from_utf8(attr.value.to_vec()) {
                                dirs.push(val);
                            }
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                tracing::warn!("camera XML parse error (Dir): {e}");
                break;
            }
            _ => {}
        }
        buf.clear();
    }
    dirs
}

/// `<Name>Event20250323_005902.jpg</Name>` のテキストを抽出。
/// `_!` を含むファイル名 (カメラの一時ファイル) はスキップ
pub(crate) fn parse_file_names(xml_text: &str) -> Vec<String> {
    let mut reader = Reader::from_str(xml_text);
    let mut files = Vec::new();
    let mut buf = Vec::new();
    let mut in_name = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                if e.name().as_ref().eq_ignore_ascii_case(b"name") {
                    in_name = true;
                }
            }
            Ok(Event::Text(ref e)) => {
                if in_name {
                    if let Ok(text) = e.unescape() {
                        let filename = text.to_string();
                        if !filename.contains("_!") {
                            files.push(filename);
                        }
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                if e.name().as_ref().eq_ignore_ascii_case(b"name") {
                    in_name = false;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                tracing::warn!("camera XML parse error (Name): {e}");
                break;
            }
            _ => {}
        }
        buf.clear();
    }
    files
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{header, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn test_config(base: &str) -> CamConfig {
        CamConfig {
            digest_user: "user".to_string(),
            digest_pass: "pass".to_string(),
            machine_name: "cam1".to_string(),
            sdcard_cgi: format!("{base}/sd/"),
            mp4_cgi: format!("{base}/mp4/"),
            jpg_cgi: format!("{base}/jpg/"),
            cf_access_client_id: Some("cf-id".to_string()),
            cf_access_client_secret: Some("cf-secret".to_string()),
        }
    }

    /// RFC 2617 §3.5 の既知ベクタ (qop=auth) で MD5 digest 計算をピン
    #[test]
    fn digest_response_rfc2617_known_vector() {
        let response = digest_response(
            "Mufasa",
            "Circle Of Life",
            "GET",
            "/dir/index.html",
            "testrealm@host.com",
            "dcd98b7102dd2f0e8b11d0f600bfb0c093",
            Some("auth"),
            "00000001",
            "0a4f113b",
        );
        assert_eq!(response, "6629fae49393a05397450978507c4ef1");
    }

    /// qop 無し (RFC 2069 互換) は ha1:nonce:ha2 の 3 要素
    #[test]
    fn digest_response_without_qop_differs() {
        let with_qop = digest_response(
            "u",
            "p",
            "GET",
            "/x",
            "r",
            "n",
            Some("auth"),
            "00000001",
            "cnonce",
        );
        let without_qop = digest_response("u", "p", "GET", "/x", "r", "n", None, "0", "0");
        assert_ne!(with_qop, without_qop);
        assert_eq!(without_qop.len(), 32);
    }

    #[test]
    fn digest_auth_header_includes_qop_fields_only_when_present() {
        let with_qop = digest_auth_header(
            "u",
            "p",
            "GET",
            "http://cam/x",
            r#"Digest realm="r", nonce="n", qop="auth""#,
        );
        assert!(with_qop.starts_with("Digest username=\"u\""));
        assert!(with_qop.contains("qop=auth"));
        assert!(with_qop.contains("nc=00000001"));
        assert!(with_qop.contains("cnonce=\""));

        let without_qop = digest_auth_header(
            "u",
            "p",
            "GET",
            "http://cam/x",
            r#"Digest realm="r", nonce="n""#,
        );
        assert!(!without_qop.contains("qop="));
        assert!(!without_qop.contains("cnonce"));
    }

    #[test]
    fn parse_dir_names_extracts_name_attribute_case_insensitive() {
        let xml = r#"<List><Dir name="20260101"/><Dir Name="20260102"/><Other name="x"/></List>"#;
        assert_eq!(parse_dir_names(xml), vec!["20260101", "20260102"]);
    }

    #[test]
    fn parse_file_names_skips_camera_temp_files() {
        let xml = r#"<List>
            <File><Name>Event20260101_000001.jpg</Name></File>
            <File><Name>Event20260101_!tmp.jpg</Name></File>
            <File><Name>Event20260101_000002.mp4</Name></File>
        </List>"#;
        assert_eq!(
            parse_file_names(xml),
            vec!["Event20260101_000001.jpg", "Event20260101_000002.mp4"]
        );
    }

    #[test]
    fn parse_handles_malformed_xml_without_panicking() {
        assert!(parse_dir_names("<unclosed").is_empty());
        assert!(parse_file_names("not xml at all").is_empty());
    }

    /// 401 Digest challenge → Authorization 付きリトライ → 200 の往復。
    /// CF Access Service Token ヘッダが両リクエストに付くことも検証する
    #[tokio::test]
    async fn fetch_retries_with_digest_on_401() {
        let server = MockServer::start().await;

        // Authorization 付き (= digest リトライ後) は 200
        Mock::given(method("GET"))
            .and(path("/sd/cam1/Event"))
            .and(header_exists("authorization"))
            .and(header("cf-access-client-id", "cf-id"))
            .and(header("cf-access-client-secret", "cf-secret"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"<List><Dir name="20260101"/></List>"#),
            )
            .mount(&server)
            .await;
        // Authorization 無し (= 初回) は 401 + Digest challenge
        Mock::given(method("GET"))
            .and(path("/sd/cam1/Event"))
            .respond_with(ResponseTemplate::new(401).insert_header(
                "www-authenticate",
                r#"Digest realm="cam", nonce="abc123", qop="auth""#,
            ))
            .mount(&server)
            .await;

        let client = CamClient::new(test_config(&server.uri()));
        let dates = client.list_dates().await.unwrap();
        assert_eq!(dates, vec!["20260101"]);
    }

    /// Digest ではない 401 (Basic 等) はリトライせずそのまま返す → 一覧は 424
    #[tokio::test]
    async fn listing_propagates_non_digest_401_as_upstream_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/sd/cam1/Event"))
            .respond_with(
                ResponseTemplate::new(401)
                    .insert_header("www-authenticate", r#"Basic realm="cam""#),
            )
            .mount(&server)
            .await;

        let client = CamClient::new(test_config(&server.uri()));
        let err = client.list_dates().await.unwrap_err();
        assert!(matches!(err, ApiError::Upstream(_)));
    }

    #[tokio::test]
    async fn list_hours_and_files_hit_nested_paths() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/sd/cam1/Event/20260101"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"<List><Dir name="120000"/></List>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/sd/cam1/Event/20260101/120000"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<List><File><Name>Event20260101_120000.jpg</Name></File></List>"#,
            ))
            .mount(&server)
            .await;

        let client = CamClient::new(test_config(&server.uri()));
        assert_eq!(client.list_hours("20260101").await.unwrap(), vec!["120000"]);
        assert_eq!(
            client.list_file_names("20260101", "120000").await.unwrap(),
            vec!["Event20260101_120000.jpg"]
        );
    }

    /// download は jpg / mp4 で CGI を出し分け、octet-stream 以外は拒否
    #[tokio::test]
    async fn download_selects_cgi_by_extension_and_checks_content_type() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/jpg/cam1/Event/20260101/120000/a.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/octet-stream")
                    .set_body_bytes(vec![1u8, 2, 3]),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/mp4/cam1/Event/20260101/120000/b.mp4"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/html")
                    .set_body_string("<html>login page</html>"),
            )
            .mount(&server)
            .await;

        let client = CamClient::new(test_config(&server.uri()));
        assert_eq!(
            client
                .download("a.jpg", "20260101", "120000")
                .await
                .unwrap(),
            vec![1u8, 2, 3]
        );
        let err = client
            .download("b.mp4", "20260101", "120000")
            .await
            .unwrap_err();
        assert!(err.contains("unexpected content type"));
    }
}
