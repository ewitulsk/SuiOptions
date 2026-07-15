//! OAuth 1.0a request signing (RFC 5849, HMAC-SHA1) — the scheme Twitter's
//! v2 user-context endpoints require. Hand-rolled on hmac/sha1 rather than
//! pulling a signing crate: the surface we need is one function.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use hmac::{Hmac, Mac};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use sha1::Sha1;

use crate::secrets::TwitterAccount;

/// RFC 3986 unreserved characters stay literal; everything else is encoded.
const OAUTH_ENCODE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

fn encode(s: &str) -> String {
    utf8_percent_encode(s, OAUTH_ENCODE).to_string()
}

/// Build the `Authorization: OAuth …` header value for a request.
///
/// `extra_params` are the request's query/form parameters, which RFC 5849
/// folds into the signature base string. A JSON body (Twitter v2) contributes
/// none, so callers pass an empty map.
pub fn authorization_header(
    creds: &TwitterAccount,
    method: &str,
    url: &str,
    extra_params: &BTreeMap<String, String>,
) -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs()
        .to_string();
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    header_with(creds, method, url, extra_params, &nonce, &timestamp)
}

fn header_with(
    creds: &TwitterAccount,
    method: &str,
    url: &str,
    extra_params: &BTreeMap<String, String>,
    nonce: &str,
    timestamp: &str,
) -> String {
    let oauth_params: [(&str, &str); 6] = [
        ("oauth_consumer_key", &creds.api_key),
        ("oauth_nonce", nonce),
        ("oauth_signature_method", "HMAC-SHA1"),
        ("oauth_timestamp", timestamp),
        ("oauth_token", &creds.access_token),
        ("oauth_version", "1.0"),
    ];

    // Parameter string: every param (oauth + request), percent-encoded,
    // sorted by encoded key.
    let mut all: BTreeMap<String, String> = extra_params
        .iter()
        .map(|(k, v)| (encode(k), encode(v)))
        .collect();
    for (k, v) in oauth_params {
        all.insert(encode(k), encode(v));
    }
    let param_string = all
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");

    let base_string = format!(
        "{}&{}&{}",
        method.to_uppercase(),
        encode(url),
        encode(&param_string)
    );
    let signing_key = format!(
        "{}&{}",
        encode(&creds.api_key_secret),
        encode(&creds.access_token_secret)
    );

    let mut mac =
        Hmac::<Sha1>::new_from_slice(signing_key.as_bytes()).expect("hmac accepts any key length");
    mac.update(base_string.as_bytes());
    let signature = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

    let mut header_params: Vec<(&str, String)> = oauth_params
        .iter()
        .map(|(k, v)| (*k, v.to_string()))
        .collect();
    header_params.push(("oauth_signature", signature));
    header_params.sort_by(|a, b| a.0.cmp(b.0));

    let fields = header_params
        .iter()
        .map(|(k, v)| format!("{}=\"{}\"", k, encode(v)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("OAuth {fields}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worked example from Twitter's "Creating a signature" docs.
    #[test]
    fn matches_twitter_documented_signature() {
        let creds = TwitterAccount {
            api_key: "xvz1evFS4wEEPTGEFPHBog".into(),
            api_key_secret: "kAcSOqF21Fu85e7zjz7ZN2U4ZRhfV3WpwPAoE3Z7kBw".into(),
            access_token: "370773112-GmHxMAgYyLbNEtIKZeRNFsMKPR9EyMZeS9weJAEb".into(),
            access_token_secret: "LswwdoUaIvS8ltyTt5jkRh4J50vUPVVHtR2YPi5kE".into(),
        };
        let mut params = BTreeMap::new();
        params.insert("include_entities".to_string(), "true".to_string());
        params.insert(
            "status".to_string(),
            "Hello Ladies + Gentlemen, a signed OAuth request!".to_string(),
        );

        let header = header_with(
            &creds,
            "post",
            "https://api.twitter.com/1.1/statuses/update.json",
            &params,
            "kYjzVBB8Y0ZFabxSWbWovY3uYSQ2pTgmZeNu2VS4cg",
            "1318622958",
        );

        // Expected signature: hCtSmYh+iHYCEqBWrE7C7hYmtUk= (percent-encoded
        // in the header).
        assert!(
            header.contains("oauth_signature=\"hCtSmYh%2BiHYCEqBWrE7C7hYmtUk%3D\""),
            "unexpected header: {header}"
        );
        assert!(header.starts_with("OAuth "));
        assert!(header.contains("oauth_consumer_key=\"xvz1evFS4wEEPTGEFPHBog\""));
    }

    #[test]
    fn encodes_reserved_characters() {
        assert_eq!(encode("Hello Ladies + Gentlemen"), "Hello%20Ladies%20%2B%20Gentlemen");
        assert_eq!(encode("a-b._~c"), "a-b._~c");
    }
}
