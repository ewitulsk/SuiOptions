//! Slack slash-command webhook (`POST /slack/command`).
//!
//! Slack signs every request with the app's signing secret
//! (`v0=hex(hmac_sha256("v0:<ts>:<body>"))`); the command payload is a form
//! body. The handler acks within Slack's 3s window with an ephemeral message
//! and delivers the tweet result through `response_url`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use tracing::warn;

use crate::commands;
use crate::state::AppState;

/// Reject requests whose timestamp strays this far from now (replay guard,
/// per Slack's verification guide).
const MAX_CLOCK_SKEW_SECS: u64 = 60 * 5;

pub async fn command(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs();
    if let Err(reason) = verify_signature(&state.slack_signing_secret, &headers, &body, now) {
        warn!(reason, "rejected slack request");
        return (StatusCode::UNAUTHORIZED, "invalid signature").into_response();
    }

    let form: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
    let user_id = form.get("user_id").cloned().unwrap_or_default();
    let text = form.get("text").cloned().unwrap_or_default();
    let response_url = form.get("response_url").cloned().unwrap_or_default();

    if !state.slack_allowed_user_ids.iter().any(|u| u == &user_id) {
        warn!(user_id, "slack user not on the allow list");
        return ephemeral("You're not on the allow list for posting tweets.");
    }

    let Some((account, tweet_text)) = commands::parse_tweet_args(&text) else {
        return ephemeral(&commands::usage(&state).await);
    };
    let (account, tweet_text) = (account.to_string(), tweet_text.to_string());

    // Ack now; post + report through response_url from a background task.
    let task_state = state.clone();
    tokio::spawn(async move {
        let message = commands::run_tweet(&task_state, &user_id, &account, &tweet_text).await;
        let body = serde_json::json!({ "response_type": "in_channel", "text": message });
        let send = |headers| {
            task_state
                .http
                .post(&response_url)
                .headers(headers)
                .json(&body)
                .send()
        };
        match observability::client::instrumented("slack", "POST response_url", send).await {
            Ok(resp) if !resp.status().is_success() => {
                warn!(status = %resp.status(), "slack response_url post failed");
            }
            Err(e) => warn!(error = %e, "slack response_url post failed"),
            _ => {}
        }
    });

    ephemeral("Posting tweet…")
}

fn ephemeral(text: &str) -> Response {
    Json(serde_json::json!({ "response_type": "ephemeral", "text": text })).into_response()
}

/// Check `X-Slack-Signature` over `v0:<ts>:<body>` (constant-time compare)
/// and bound the timestamp skew.
fn verify_signature(
    signing_secret: &str,
    headers: &HeaderMap,
    body: &[u8],
    now_unix: u64,
) -> Result<(), &'static str> {
    let timestamp = headers
        .get("x-slack-request-timestamp")
        .and_then(|v| v.to_str().ok())
        .ok_or("missing timestamp header")?;
    let ts: u64 = timestamp.parse().map_err(|_| "bad timestamp")?;
    if ts.abs_diff(now_unix) > MAX_CLOCK_SKEW_SECS {
        return Err("stale timestamp");
    }

    let signature = headers
        .get("x-slack-signature")
        .and_then(|v| v.to_str().ok())
        .ok_or("missing signature header")?;
    let sig_hex = signature.strip_prefix("v0=").ok_or("bad signature format")?;
    let sig = hex::decode(sig_hex).map_err(|_| "bad signature hex")?;

    let mut mac = Hmac::<Sha256>::new_from_slice(signing_secret.as_bytes())
        .expect("hmac accepts any key length");
    mac.update(b"v0:");
    mac.update(timestamp.as_bytes());
    mac.update(b":");
    mac.update(body);
    mac.verify_slice(&sig).map_err(|_| "signature mismatch")
}

#[cfg(test)]
mod tests {
    use super::*;

    // The worked example from Slack's "Verifying requests" docs.
    const SECRET: &str = "8f742231b10e8888abcd99yyyzzz85a5";
    const TS: &str = "1531420618";
    const BODY: &[u8] = b"token=xyzz0WbapA4vBCDEFasx0q6G&team_id=T1DC2JH3J&team_domain=testteamnow&channel_id=G8PSS9T3V&channel_name=foobar&user_id=U2CERLKJA&user_name=roadrunner&command=%2Fwebhook-collect&text=&response_url=https%3A%2F%2Fhooks.slack.com%2Fcommands%2FT1DC2JH3J%2F397700885554%2F96rGlfmibIGlgcZRskXaIFfN&trigger_id=398738663015.47445629121.803a0bc887a14d10d2c447fce8b6703c";
    const SIG: &str = "v0=a2114d57b48eac39b9ad189dd8316235a7b4a8d21a10bd27519666489c69b503";

    fn headers(ts: &str, sig: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-slack-request-timestamp", ts.parse().unwrap());
        h.insert("x-slack-signature", sig.parse().unwrap());
        h
    }

    #[test]
    fn accepts_documented_slack_example() {
        let now = TS.parse::<u64>().unwrap() + 30;
        assert_eq!(verify_signature(SECRET, &headers(TS, SIG), BODY, now), Ok(()));
    }

    #[test]
    fn rejects_tampered_body() {
        let now = TS.parse::<u64>().unwrap() + 30;
        assert!(verify_signature(SECRET, &headers(TS, SIG), b"text=evil", now).is_err());
    }

    #[test]
    fn rejects_stale_timestamp() {
        let now = TS.parse::<u64>().unwrap() + MAX_CLOCK_SKEW_SECS + 1;
        assert_eq!(
            verify_signature(SECRET, &headers(TS, SIG), BODY, now),
            Err("stale timestamp")
        );
    }
}
