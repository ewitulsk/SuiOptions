//! Discord interactions endpoint (`POST /discord/interactions`).
//!
//! Same webhook shape as social-bot's: Discord signs every interaction with
//! the application's Ed25519 key over `<timestamp><body>` and requires the
//! PING handshake plus an ack within 3s. The handler defers, fetches from
//! engagement-service in a background task, and edits the deferred response
//! through the interaction webhook.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde_json::{json, Value};
use tracing::warn;

use crate::commands;
use crate::state::AppState;

const DISCORD_API: &str = "https://discord.com/api/v10";

// Interaction types / callback types, from Discord's API reference.
const PING: u64 = 1;
const APPLICATION_COMMAND: u64 = 2;
const PONG: u64 = 1;
const CHANNEL_MESSAGE: u64 = 4;
const DEFERRED_CHANNEL_MESSAGE: u64 = 5;
const EPHEMERAL: u64 = 1 << 6;

pub async fn interactions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(reason) = verify_signature(&state.discord_verify_key, &headers, &body) {
        warn!(reason, "rejected discord request");
        return (StatusCode::UNAUTHORIZED, "invalid signature").into_response();
    }

    let Ok(interaction) = serde_json::from_slice::<Value>(&body) else {
        return (StatusCode::BAD_REQUEST, "bad json").into_response();
    };

    match interaction["type"].as_u64() {
        Some(PING) => Json(json!({ "type": PONG })).into_response(),
        Some(APPLICATION_COMMAND) => command(state, &interaction).await,
        _ => (StatusCode::BAD_REQUEST, "unsupported interaction type").into_response(),
    }
}

async fn command(state: Arc<AppState>, interaction: &Value) -> Response {
    let option = |name: &str| -> Option<Value> {
        interaction["data"]["options"]
            .as_array()
            .and_then(|opts| opts.iter().find(|o| o["name"].as_str() == Some(name)))
            .map(|o| o["value"].clone())
    };

    // Both commands are read-only, so there is no allow list.
    enum Cmd {
        Leaderboard { count: Option<usize> },
        Points { handle: String },
    }
    let cmd = match interaction["data"]["name"].as_str() {
        Some("leaderboard") => Cmd::Leaderboard {
            count: option("count").and_then(|v| v.as_u64()).map(|n| n as usize),
        },
        Some("points") => match option("handle").and_then(|v| v.as_str().map(str::to_string)) {
            Some(handle) if !handle.trim().is_empty() => Cmd::Points {
                handle: handle.trim().to_string(),
            },
            _ => return ephemeral_message("Usage: /points <twitter-handle>"),
        },
        _ => return ephemeral_message("Unknown command."),
    };

    let application_id = interaction["application_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let token = interaction["token"].as_str().unwrap_or_default().to_string();

    // Defer now; fetch + edit the deferred response from a background task.
    let task_state = state.clone();
    tokio::spawn(async move {
        let message = match cmd {
            Cmd::Leaderboard { count } => commands::run_leaderboard(&task_state, count).await,
            Cmd::Points { handle } => commands::run_points(&task_state, &handle).await,
        };
        let url = format!("{DISCORD_API}/webhooks/{application_id}/{token}/messages/@original");
        let body = json!({ "content": message });
        let send = |headers| {
            task_state
                .http
                .patch(&url)
                .headers(headers)
                .json(&body)
                .send()
        };
        match observability::client::instrumented("discord", "PATCH webhook", send).await {
            Ok(resp) if !resp.status().is_success() => {
                warn!(status = %resp.status(), "discord follow-up failed");
            }
            Err(e) => warn!(error = %e, "discord follow-up failed"),
            _ => {}
        }
    });

    Json(json!({ "type": DEFERRED_CHANNEL_MESSAGE })).into_response()
}

fn ephemeral_message(text: &str) -> Response {
    Json(json!({
        "type": CHANNEL_MESSAGE,
        "data": { "content": text, "flags": EPHEMERAL },
    }))
    .into_response()
}

/// Check `X-Signature-Ed25519` over `<timestamp><body>`.
fn verify_signature(
    key: &VerifyingKey,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<(), &'static str> {
    let timestamp = headers
        .get("x-signature-timestamp")
        .and_then(|v| v.to_str().ok())
        .ok_or("missing timestamp header")?;
    let sig_hex = headers
        .get("x-signature-ed25519")
        .and_then(|v| v.to_str().ok())
        .ok_or("missing signature header")?;
    let sig_bytes: [u8; 64] = hex::decode(sig_hex)
        .map_err(|_| "bad signature hex")?
        .try_into()
        .map_err(|_| "bad signature length")?;
    let signature = Signature::from_bytes(&sig_bytes);

    let mut message = Vec::with_capacity(timestamp.len() + body.len());
    message.extend_from_slice(timestamp.as_bytes());
    message.extend_from_slice(body);
    key.verify(&message, &signature)
        .map_err(|_| "signature mismatch")
}

/// Parse the application public key from the Discord developer portal (hex).
pub fn parse_public_key(hex_key: &str) -> anyhow::Result<VerifyingKey> {
    let bytes: [u8; 32] = hex::decode(hex_key.trim())
        .map_err(|e| anyhow::anyhow!("discord public key is not hex: {e}"))?
        .try_into()
        .map_err(|_| anyhow::anyhow!("discord public key must be 32 bytes"))?;
    VerifyingKey::from_bytes(&bytes).map_err(|e| anyhow::anyhow!("bad discord public key: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn signed_headers(key: &SigningKey, timestamp: &str, body: &[u8]) -> HeaderMap {
        let mut message = timestamp.as_bytes().to_vec();
        message.extend_from_slice(body);
        let sig = key.sign(&message);
        let mut h = HeaderMap::new();
        h.insert("x-signature-timestamp", timestamp.parse().unwrap());
        h.insert(
            "x-signature-ed25519",
            hex::encode(sig.to_bytes()).parse().unwrap(),
        );
        h
    }

    #[test]
    fn accepts_valid_signature_and_rejects_tampering() {
        let signing = SigningKey::generate(&mut rand::rngs::OsRng);
        let verifying = signing.verifying_key();
        let body = br#"{"type":1}"#;

        let headers = signed_headers(&signing, "1700000000", body);
        assert_eq!(verify_signature(&verifying, &headers, body), Ok(()));
        assert!(verify_signature(&verifying, &headers, br#"{"type":2}"#).is_err());
    }

    #[test]
    fn parses_portal_hex_key() {
        let signing = SigningKey::generate(&mut rand::rngs::OsRng);
        let hex_key = hex::encode(signing.verifying_key().to_bytes());
        let parsed = parse_public_key(&hex_key).unwrap();
        assert_eq!(parsed, signing.verifying_key());
        assert!(parse_public_key("nothex").is_err());
    }
}
