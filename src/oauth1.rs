//! OAuth 1.0a 署名ヘルパ (pure 関数のみ、I/O なし)。
//!
//! rust-logi `flickr_service.rs` の `generate_signature` / `percent_encode` を
//! 移植したもの。HMAC-SHA1 は ring ではなく pure Rust の hmac + sha1 を使う。

use std::collections::HashMap;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha1::Sha1;

/// パーセントエンコード (RFC 5849 §3.6)
pub fn percent_encode(s: &str) -> String {
    let mut result = String::new();
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                result.push(byte as char);
            }
            _ => {
                result.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    result
}

/// OAuth 1.0a 署名 (HMAC-SHA1) を生成
pub fn sign(
    method: &str,
    url: &str,
    params: &HashMap<String, String>,
    consumer_secret: &str,
    token_secret: Option<&str>,
) -> String {
    let mut sorted_params: Vec<(&String, &String)> = params.iter().collect();
    sorted_params.sort_by(|a, b| a.0.cmp(b.0));

    let param_string: String = sorted_params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    let signature_base = format!(
        "{}&{}&{}",
        method.to_uppercase(),
        percent_encode(url),
        percent_encode(&param_string)
    );

    let signing_key = format!(
        "{}&{}",
        percent_encode(consumer_secret),
        token_secret.map(percent_encode).unwrap_or_default()
    );

    let mut mac =
        Hmac::<Sha1>::new_from_slice(signing_key.as_bytes()).expect("HMAC accepts any key length");
    mac.update(signature_base.as_bytes());
    BASE64.encode(mac.finalize().into_bytes())
}

/// ノンス生成 (UUIDv4 のハイフン抜き)
pub fn nonce() -> String {
    uuid::Uuid::new_v4().to_string().replace('-', "")
}

/// UNIX 秒タイムスタンプ
pub fn timestamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before UNIX_EPOCH")
        .as_secs()
        .to_string()
}

/// `Authorization: OAuth ...` ヘッダ値 (の OAuth 以降) を構築
pub fn auth_header(params: &HashMap<String, String>) -> String {
    let mut sorted: Vec<(&String, &String)> = params.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    sorted
        .iter()
        .map(|(k, v)| format!("{}=\"{}\"", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `k1=v1&k2=v2` 形式の form-encoded レスポンスをパース
pub fn parse_form(body: &str) -> HashMap<String, String> {
    body.split('&')
        .filter_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            Some((parts.next()?.to_string(), parts.next()?.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encode_unreserved_passthrough() {
        assert_eq!(percent_encode("AZaz09-._~"), "AZaz09-._~");
    }

    #[test]
    fn percent_encode_reserved_and_utf8() {
        assert_eq!(percent_encode("a b&c"), "a%20b%26c");
        assert_eq!(
            percent_encode("Ladies + Gentlemen"),
            "Ladies%20%2B%20Gentlemen"
        );
        // UTF-8 マルチバイトは byte 単位でエンコード
        assert_eq!(percent_encode("あ"), "%E3%81%82");
    }

    /// 既知ベクトル: Twitter "Creating a signature" docs と同系の入力。
    /// 期待値は python oauthlib 3.2.2 (RFC 5849 実装) で独立計算した値
    /// (= sort → base string → signing key → HMAC-SHA1 → base64 の全体をピン)。
    #[test]
    fn sign_known_vector() {
        let mut params = HashMap::new();
        params.insert(
            "status".to_string(),
            "Hello Ladies + Gentlemen, a signed OAuth request!".to_string(),
        );
        params.insert("include_entities".to_string(), "true".to_string());
        params.insert(
            "oauth_consumer_key".to_string(),
            "xvz1evFS4wEEPTGEFPHBog".to_string(),
        );
        params.insert(
            "oauth_nonce".to_string(),
            "kYjzVBB8Y0ZFabxSWbWovY3uYSQ2pTgmZeNu2VS4cg".to_string(),
        );
        params.insert(
            "oauth_signature_method".to_string(),
            "HMAC-SHA1".to_string(),
        );
        params.insert("oauth_timestamp".to_string(), "1318622958".to_string());
        params.insert(
            "oauth_token".to_string(),
            "370773112-GmHxMAgYyLbNEtIKZeRNFsMKPR9EyMZeS9weJAEb".to_string(),
        );
        params.insert("oauth_version".to_string(), "1.0".to_string());

        let signature = sign(
            "POST",
            "https://api.twitter.com/1.1/statuses/update.json",
            &params,
            "kAcSOqF21Fu85e7zjz7ZN2U4ZRhfV3WpwPAoE3Z7kBw",
            Some("LswwdoUaIvS8ltyTt5jkRh4J50vUPVVHtR2YPi5kE"),
        );
        assert_eq!(signature, "hCtSmYh+iHYCEqBWrE7C7hYmtUk=");
    }

    #[test]
    fn sign_without_token_secret_uses_empty_suffix() {
        let mut params = HashMap::new();
        params.insert("oauth_consumer_key".to_string(), "key".to_string());
        let with_none = sign("GET", "https://example.com/", &params, "secret", None);
        let with_empty = sign("GET", "https://example.com/", &params, "secret", Some(""));
        assert_eq!(with_none, with_empty);
    }

    #[test]
    fn nonce_is_unique_and_hyphenless() {
        let a = nonce();
        let b = nonce();
        assert_ne!(a, b);
        assert!(!a.contains('-'));
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn timestamp_is_numeric() {
        let ts: u64 = timestamp().parse().unwrap();
        assert!(ts > 1_700_000_000);
    }

    #[test]
    fn auth_header_sorted_and_quoted() {
        let mut params = HashMap::new();
        params.insert("b".to_string(), "2 2".to_string());
        params.insert("a".to_string(), "1".to_string());
        assert_eq!(auth_header(&params), "a=\"1\", b=\"2%202\"");
    }

    #[test]
    fn parse_form_basic() {
        let m = parse_form("oauth_token=abc&oauth_token_secret=def&ok=true");
        assert_eq!(m["oauth_token"], "abc");
        assert_eq!(m["oauth_token_secret"], "def");
        assert_eq!(m["ok"], "true");
    }

    #[test]
    fn parse_form_skips_malformed_pairs() {
        let m = parse_form("novalue&k=v");
        assert_eq!(m.len(), 1);
        assert_eq!(m["k"], "v");
    }
}
