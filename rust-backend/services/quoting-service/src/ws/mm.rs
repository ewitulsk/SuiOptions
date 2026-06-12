//! Connected market maker handler.
//!
//! 1. Auth: issue `AuthChallenge`, expect `AuthResponse`, verify against the
//!    pubkey the indexer holds for the claimed Account.
//! 2. On success, register an [`MmConnection`] in the [`MmRegistry`] so the
//!    RFQ orchestrator can broadcast to it.
//! 3. Read loop: route `Quote` and `Decline` frames to whichever RFQ matcher
//!    holds the matching `request_id`. Ignore frames after the auth phase
//!    that aren't routable.
//! 4. Write loop: drain the per-connection outbound mpsc into the WS sink.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tracing::{debug, info, trace, warn};

use protocol_types::messages::{
    AuthAckPayload, AuthChallengePayload, MmHelloPayload, MmToService, ServiceToMm,
};

use crate::state::{MmConnection, MmResponse};
use crate::ws::auth::{random_challenge, verify_challenge_response};
use crate::{AppState, Config};

pub async fn handle(
    ws: WebSocketStream<TcpStream>,
    _peer: SocketAddr,
    state: Arc<AppState>,
    _cfg: Arc<Config>,
    hello: MmHelloPayload,
) -> Result<()> {
    let _conn_gauge = super::ConnGauge::new("mm");
    let (mut sink, mut stream) = ws.split();

    // -- Auth -------------------------------------------------------------
    let challenge = random_challenge();
    let chal_frame = ServiceToMm::AuthChallenge {
        payload: AuthChallengePayload {
            challenge: challenge.clone(),
        },
    };
    sink.send(Message::Text(serde_json::to_string(&chal_frame)?))
        .await?;

    let next = stream
        .next()
        .await
        .ok_or_else(|| anyhow!("client closed before AuthResponse"))??;
    let text = msg_to_text(next)?;
    let resp: MmToService = serde_json::from_str(&text)?;
    let signature = match resp {
        MmToService::AuthResponse { payload } => payload.signature,
        other => return Err(anyhow!("expected AuthResponse, got {:?}", other)),
    };

    // The indexer's `(scheme, pubkey)` is authoritative; supplied must agree.
    // Fetched JIT; an indexer error or unknown account leaves both `None`/
    // empty, which `verify_challenge_response` treats as "scheme unknown"
    // (fail closed).
    let indexer_view = match state.indexer.account(hello.account_id).await {
        Ok(view) => view,
        Err(e) => {
            tracing::warn!(account = %hello.account_id, error = %e, "indexer account lookup failed during mm auth");
            None
        }
    };
    let indexer_scheme = indexer_view.as_ref().and_then(|a| a.signing_scheme);
    let indexer_key = indexer_view
        .as_ref()
        .map(|a| a.signing_pubkey.clone())
        .unwrap_or_default();
    if let Err(reason) = verify_challenge_response(
        indexer_scheme,
        &indexer_key,
        hello.signing_scheme,
        &hello.signing_pubkey,
        &challenge,
        &signature,
    ) {
        // Surface which branch tripped so the operator can diagnose without
        // grepping server logs. The message is safe to share (no secrets).
        let (code, message): (&str, String) = match reason {
            crate::ws::auth::AuthError::SchemeUnknown => (
                "auth_scheme_unknown",
                format!(
                    "indexer has not yet ingested an AccountCreated event for {} — \
                     check that the indexer is running against the current \
                     deployments.json packageId and has caught up to the \
                     checkpoint that created this Account",
                    hello.account_id
                ),
            ),
            crate::ws::auth::AuthError::PubkeyMismatch => (
                "auth_pubkey_mismatch",
                format!(
                    "Hello's (scheme, pubkey) does not match what is on chain \
                     for Account {} — verify mm-bot.toml signing_scheme + \
                     secrets.toml mm_bot.quote_key match the registered key",
                    hello.account_id
                ),
            ),
            crate::ws::auth::AuthError::SignatureInvalid => (
                "auth_signature_invalid",
                "challenge signature did not verify against the registered key"
                    .to_string(),
            ),
        };
        tracing::warn!(
            account = %hello.account_id,
            reason = ?reason,
            "mm auth rejected"
        );
        metrics::counter!("quoting_mm_auth_total", "outcome" => "failed").increment(1);
        sink.send(Message::Text(serde_json::to_string(&ServiceToMm::Error {
            request_id: None,
            payload: protocol_types::messages::ErrorPayload {
                code: code.into(),
                message,
            },
        })?))
        .await
        .ok();
        return Err(anyhow!(
            "mm auth failed for {} ({code})",
            hello.account_id
        ));
    }

    metrics::counter!("quoting_mm_auth_total", "outcome" => "ok").increment(1);
    let session_id = uuid::Uuid::new_v4().to_string();
    sink.send(Message::Text(serde_json::to_string(&ServiceToMm::AuthAck {
        payload: AuthAckPayload {
            session_id: session_id.clone(),
        },
    })?))
    .await?;
    info!(account = %hello.account_id, session_id, roles = ?hello.roles, "mm authed");

    // -- Register + start write loop --------------------------------------
    let (out_tx, mut out_rx) = mpsc::channel::<ServiceToMm>(64);
    let conn = MmConnection {
        account_id: hello.account_id,
        roles: Arc::new(RwLock::new(hello.roles.clone())),
        bulk_view: hello.bulk_view,
        tx: out_tx,
    };
    state.mms.insert(conn);

    // Write loop.
    let write_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let text = match serde_json::to_string(&msg) {
                Ok(t) => t,
                Err(e) => {
                    warn!(error = %e, "failed to encode outbound mm frame");
                    continue;
                }
            };
            metrics::counter!("quoting_ws_messages_total", "role" => "mm", "direction" => "out", "type" => out_type(&msg)).increment(1);
            if let Err(e) = sink.send(Message::Text(text)).await {
                debug!(error = %e, "mm sink closed");
                break;
            }
        }
    });

    // -- Read loop --------------------------------------------------------
    let read_state = Arc::clone(&state);
    let account_id = hello.account_id;
    let read_result: Result<()> = async {
        while let Some(frame) = stream.next().await {
            let frame = frame?;
            let text = match frame {
                Message::Text(t) => t,
                Message::Binary(b) => String::from_utf8(b)?,
                Message::Close(_) => break,
                _ => continue,
            };
            let msg: MmToService = match serde_json::from_str(&text) {
                Ok(m) => m,
                Err(e) => {
                    warn!(error = %e, "bad mm frame");
                    continue;
                }
            };
            metrics::counter!("quoting_ws_messages_total", "role" => "mm", "direction" => "in", "type" => in_type(&msg)).increment(1);
            match msg {
                MmToService::Quote { request_id, payload } => {
                    trace!(mm = %account_id, request_id, premium = payload.quote.premium, nonce = payload.quote.nonce, "received mm quote");
                    // Clone the sender out and drop the DashMap Ref before
                    // awaiting — holding a Ref across .await keeps the
                    // shard's read lock alive, which blocks concurrent
                    // inserts from orchestrate() on the same shard and
                    // can starve the tokio runtime under burst load.
                    let tx = read_state
                        .pending_rfqs
                        .get(&request_id)
                        .map(|e| e.value().clone());
                    if let Some(tx) = tx {
                        let _ = tx.send(MmResponse::Quote(account_id, payload)).await;
                    } else {
                        debug!(request_id, "quote arrived after rfq closed");
                    }
                }
                MmToService::Decline { request_id, .. } => {
                    trace!(mm = %account_id, request_id, "mm declined");
                    let tx = read_state
                        .pending_rfqs
                        .get(&request_id)
                        .map(|e| e.value().clone());
                    if let Some(tx) = tx {
                        let _ = tx.send(MmResponse::Decline(account_id)).await;
                    }
                }
                MmToService::BulkViewQuote { request_id, payload } => {
                    trace!(mm = %account_id, request_id, premiums = payload.premiums.len(), "received bulk-view quote");
                    // Clone the sender out before awaiting — same DashMap-Ref
                    // discipline as the signed Quote path above.
                    let tx = read_state
                        .pending_rfqs
                        .get(&request_id)
                        .map(|e| e.value().clone());
                    if let Some(tx) = tx {
                        let _ = tx.send(MmResponse::BulkView(account_id, payload)).await;
                    } else {
                        debug!(request_id, "bulk-view quote arrived after rfq closed");
                    }
                }
                MmToService::Pong => {}
                MmToService::Hello { .. } | MmToService::AuthResponse { .. } => {
                    debug!("ignored after-auth control frame");
                }
            }
        }
        Ok(())
    }
    .await;

    state.mms.remove(&account_id);
    write_task.abort();
    read_result
}

/// Variant name for the `type` label on `quoting_ws_messages_total`.
fn in_type(m: &MmToService) -> &'static str {
    match m {
        MmToService::Hello { .. } => "Hello",
        MmToService::AuthResponse { .. } => "AuthResponse",
        MmToService::Quote { .. } => "Quote",
        MmToService::BulkViewQuote { .. } => "BulkViewQuote",
        MmToService::Decline { .. } => "Decline",
        MmToService::Pong => "Pong",
    }
}

fn out_type(m: &ServiceToMm) -> &'static str {
    match m {
        ServiceToMm::AuthChallenge { .. } => "AuthChallenge",
        ServiceToMm::AuthAck { .. } => "AuthAck",
        ServiceToMm::RFQBroadcast { .. } => "RFQBroadcast",
        ServiceToMm::BulkViewRFQBroadcast { .. } => "BulkViewRFQBroadcast",
        ServiceToMm::AccountStateUpdate { .. } => "AccountStateUpdate",
        ServiceToMm::ReservationConfirmed { .. } => "ReservationConfirmed",
        ServiceToMm::ReservationReleased { .. } => "ReservationReleased",
        ServiceToMm::Error { .. } => "Error",
        ServiceToMm::Ping => "Ping",
    }
}

fn msg_to_text(m: Message) -> Result<String> {
    Ok(match m {
        Message::Text(t) => t,
        Message::Binary(b) => String::from_utf8(b)?,
        other => return Err(anyhow!("unexpected frame: {:?}", other)),
    })
}
