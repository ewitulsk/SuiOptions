//! Dakota webhook receipt.
//!
//! Three things happen here, in order, and the order matters:
//!
//! 1. **Verify.** Dakota signs with Ed25519 (not HMAC) over
//!    `{timestamp}.{body}`. An unverified body is discarded without being
//!    parsed, let alone stored.
//! 2. **Extract.** We pull out ids, enums, amounts and assets — and nothing
//!    else. Dakota event payloads carry `sender_details.sender_account_name`
//!    and `sender_account_number`; storing the raw envelope (as the indexer
//!    does for chain events) would put bank details in our database.
//! 3. **Record.** Keyed on `X-Dakota-Event-ID`, so a redelivery is a no-op.
//!
//! Dakota does NOT guarantee ordering, so this table is a set of observations,
//! never a sequence. Anything that needs current state re-reads the resource.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use base64::Engine;
use chrono::{DateTime, TimeZone, Utc};
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::db::models::{NewLedgerEvent, NewWebhookError};
use crate::state::AppState;

/// Reject deliveries older than this. Dakota's own guidance is 5 minutes.
const MAX_SKEW_SECS: i64 = 300;

/// `POST /webhooks/dakota`.
///
/// Always answers 2xx once a delivery is verified — including when we cannot
/// make sense of the body. A non-2xx makes Dakota retry for 48 hours, and a
/// payload we failed to parse will fail identically on every retry; the
/// `webhook_errors` row is the durable record instead.
pub async fn receive(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let event_id = header(&headers, "x-dakota-event-id");
    let digest = sha256_hex(&body);

    if let Err(reason) = verify(&state.webhook_key, &headers, &body) {
        // Deliberately terse and 401: an unverified body is not ours to log or
        // store, and retrying will not help a forged or misconfigured sender.
        warn!(reason, event_id = event_id.as_deref().unwrap_or("-"), "rejected webhook");
        metrics::counter!("dakota_webhooks_total", "outcome" => "unverified").increment(1);
        let _ = state.repo.record_webhook_error(&NewWebhookError {
            event_id,
            reason: reason.to_string(),
            body_sha256: digest,
        });
        return StatusCode::UNAUTHORIZED;
    }

    let Some(event_id) = event_id else {
        metrics::counter!("dakota_webhooks_total", "outcome" => "no_event_id").increment(1);
        let _ = state.repo.record_webhook_error(&NewWebhookError {
            event_id: None,
            reason: "missing X-Dakota-Event-ID".into(),
            body_sha256: digest,
        });
        // Verified but unusable — no id means no idempotency key. Accept it so
        // Dakota stops retrying.
        return StatusCode::OK;
    };

    let parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            metrics::counter!("dakota_webhooks_total", "outcome" => "unparseable").increment(1);
            let _ = state.repo.record_webhook_error(&NewWebhookError {
                event_id: Some(event_id),
                reason: format!("body is not json: {e}"),
                body_sha256: digest,
            });
            return StatusCode::OK;
        }
    };

    let mut event = extract(&event_id, &parsed);
    // Events name the auto-account, not the customer; attribute it from our
    // own accounts table so per-customer totals are not all NULL.
    if event.dakota_customer_id.is_none() {
        if let Some(acct) = account_ref(&parsed) {
            event.dakota_customer_id = state.repo.account_owner(&acct).ok().flatten();
        }
    }
    match state.repo.record_event(&event) {
        Ok(true) => {
            metrics::counter!("dakota_webhooks_total", "outcome" => "recorded").increment(1);
            info!(event_id, event_type = %event.event_type, "webhook recorded");
        }
        Ok(false) => {
            metrics::counter!("dakota_webhooks_total", "outcome" => "duplicate").increment(1);
        }
        Err(e) => {
            // The database is down, not the payload's fault — 500 so Dakota
            // retries and we do not silently lose the event.
            warn!(event_id, error = %e, "recording webhook failed");
            metrics::counter!("dakota_webhooks_total", "outcome" => "db_error").increment(1);
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
    }
    StatusCode::OK
}

/// Ed25519 over `{timestamp}.{raw body}`, plus a freshness check.
fn verify(key: &VerifyingKey, headers: &HeaderMap, body: &[u8]) -> Result<(), &'static str> {
    let sig_b64 = header(headers, "x-webhook-signature").ok_or("missing signature header")?;
    let ts = header(headers, "x-webhook-timestamp").ok_or("missing timestamp header")?;

    let ts_num: i64 = ts.trim().parse().map_err(|_| "timestamp is not an integer")?;
    let age = Utc::now().timestamp() - ts_num;
    // Bounded on both sides: a far-future timestamp is as suspect as a stale
    // one, and would otherwise stay "fresh" forever.
    if age.abs() > MAX_SKEW_SECS {
        return Err("timestamp outside the accepted window");
    }

    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(sig_b64.trim())
        .map_err(|_| "signature is not base64")?;
    let sig_arr: [u8; 64] = sig_bytes.try_into().map_err(|_| "signature is not 64 bytes")?;
    let signature = Signature::from_bytes(&sig_arr);

    let mut signed = Vec::with_capacity(ts.len() + 1 + body.len());
    signed.extend_from_slice(ts.as_bytes());
    signed.push(b'.');
    signed.extend_from_slice(body);

    key.verify_strict(&signed, &signature)
        .map_err(|_| "signature does not verify")
}

/// Parse the environment's Ed25519 public key (64 hex chars).
pub fn parse_verifying_key(hex_key: &str) -> anyhow::Result<VerifyingKey> {
    let raw = hex::decode(hex_key.trim()).map_err(|e| anyhow::anyhow!("webhook key not hex: {e}"))?;
    let arr: [u8; 32] = raw
        .try_into()
        .map_err(|_| anyhow::anyhow!("webhook key must be 32 bytes"))?;
    VerifyingKey::from_bytes(&arr).map_err(|e| anyhow::anyhow!("invalid webhook key: {e}"))
}

/// Reduce a Dakota event to the non-identifying fields we keep.
///
/// Everything not named here is dropped, which is the point: the input holds
/// bank account numbers and legal names.
fn extract(event_id: &str, v: &serde_json::Value) -> NewLedgerEvent {
    let event_type = v
        .get("type")
        .or_else(|| v.get("event_type"))
        .and_then(|t| t.as_str())
        .unwrap_or("unknown")
        .to_string();

    let object = v
        .pointer("/data/object")
        .or_else(|| v.get("data"))
        .unwrap_or(v);

    let receipt = object.get("receipt");
    let (amount_minor, asset) = receipt
        .and_then(amount_from_receipt)
        .unwrap_or((None, None));

    NewLedgerEvent {
        event_id: event_id.to_string(),
        direction: Some(direction_for(&event_type, object).to_string()),
        resource_type: resource_type_for(&event_type),
        resource_id: str_field(object, "id"),
        // Events name the auto-account, not the customer. `resolve_customer`
        // fills this in from our own accounts table — without it every
        // per-customer total stays empty.
        dakota_customer_id: str_field(object, "customer_id")
            .or_else(|| str_field(object, "dakota_customer_id")),
        amount_minor,
        asset,
        exchange_rate: receipt.and_then(|r| str_field(r, "exchange_rate")),
        fee_minor: receipt.and_then(fee_from_receipt),
        status: str_field(object, "status"),
        occurred_at: v
            .get("created")
            .or_else(|| object.get("updated_at"))
            .and_then(|t| t.as_i64())
            .and_then(|secs| Utc.timestamp_opt(secs, 0).single()),
        event_type,
    }
}

/// Onramps bring value in; offramps send it out. A swap is neither — treating
/// it as both would double-count it in every total.
fn direction_for(event_type: &str, object: &serde_json::Value) -> &'static str {
    let kind = str_field(object, "type").unwrap_or_default();
    match kind.as_str() {
        "onramp" => "in",
        "offramp" => "out",
        "swap" => "transfer",
        _ if event_type.contains("deposit") => "in",
        _ => "transfer",
    }
}

fn resource_type_for(event_type: &str) -> Option<String> {
    let head = event_type.split('.').next()?;
    Some(head.to_string())
}

/// Pull `(minor units, asset)` from a receipt, preferring the output leg — the
/// amount actually delivered is the one worth reporting.
///
/// Dakota ships receipts in TWO shapes, and which one you get depends on where
/// you read it from:
///
/// - `GET /auto-transactions` nests them: `{"output":{"amount":"2","asset":"USDC"}}`
/// - `GET /events` and webhook deliveries flatten them:
///   `{"outgoing_amount":"2","output_currency":"USDC"}`
///
/// Handling only the nested form silently yields NULL amounts for every
/// webhook-sourced row, which is exactly how the ledger ends up full of events
/// that total to nothing.
fn amount_from_receipt(receipt: &serde_json::Value) -> Option<(Option<i64>, Option<String>)> {
    // Nested form.
    if let Some(leg) = receipt.get("output").or_else(|| receipt.get("input")) {
        if leg.is_object() {
            return Some((minor_units(leg.get("amount")), str_field(leg, "asset")));
        }
    }
    // Flat form.
    let amount = receipt
        .get("outgoing_amount")
        .or_else(|| receipt.get("converted_amount"))
        .or_else(|| receipt.get("initial_amount"));
    let asset = str_field(receipt, "output_currency")
        .or_else(|| str_field(receipt, "input_currency"));
    if amount.is_none() && asset.is_none() {
        return None;
    }
    Some((minor_units(amount), asset))
}

/// Dakota's fee field is an object in one shape and a bare decimal string in
/// the other.
fn fee_from_receipt(receipt: &serde_json::Value) -> Option<i64> {
    let fee = receipt.get("dakota_fee")?;
    if fee.is_object() {
        return minor_units(fee.get("amount"));
    }
    minor_units(Some(fee))
}

/// Decimal string -> integer minor units (cents / 1e-2).
///
/// Dakota sends amounts as decimal strings ("2", "1.50"). Storing them as
/// integers keeps the aggregate SUMs exact — floats would drift, and every
/// number here is money.
fn minor_units(v: Option<&serde_json::Value>) -> Option<i64> {
    let s = v?.as_str()?.trim();
    let (whole, frac) = match s.split_once('.') {
        Some((w, f)) => (w, f),
        None => (s, ""),
    };
    let negative = whole.starts_with('-');
    let whole_digits = whole.trim_start_matches(['-', '+']);
    if !whole_digits.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let units: i64 = if whole_digits.is_empty() { 0 } else { whole_digits.parse().ok()? };
    // Two decimal places, truncating anything finer.
    let mut cents_str = frac.chars().chain(std::iter::repeat('0')).take(2).collect::<String>();
    if cents_str.is_empty() {
        cents_str.push('0');
    }
    let cents: i64 = cents_str.parse().ok()?;
    let total = units.checked_mul(100)?.checked_add(cents)?;
    Some(if negative { -total } else { total })
}

fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)?.as_str().map(|s| s.to_string())
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name)?.to_str().ok().map(|s| s.to_string())
}

fn sha256_hex(body: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(body);
    hex::encode(h.finalize())
}

/// Exposed for the resync path, which builds ledger rows from `GET /events`
/// instead of from a delivery.
pub fn extract_for_resync(event_id: &str, v: &serde_json::Value) -> NewLedgerEvent {
    extract(event_id, v)
}

/// The auto-account an event refers to, if any.
///
/// Dakota events identify the account, not the customer, so this is the join
/// key callers use against our `accounts` table to attribute a transfer.
pub fn account_ref(v: &serde_json::Value) -> Option<String> {
    let object = v.pointer("/data/object").or_else(|| v.get("data")).unwrap_or(v);
    str_field(object, "auto_account_id").or_else(|| str_field(object, "account_id"))
}

#[allow(dead_code)]
fn _assert_datetime_type(_: Option<DateTime<Utc>>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn sample_event() -> serde_json::Value {
        // Trimmed from a real sandbox `GET /events` row. `sender_details` is
        // kept here on purpose: the test asserts we do NOT retain it.
        serde_json::json!({
            "api_version": "1.0.0",
            "created": 1785699115,
            "type": "transaction.auto.updated",
            "data": { "object": {
                "id": "3HNCPYPeEZCpScXWQbvFJwUeIxB",
                "auto_account_id": "3HNCN914HGh2Sr95XpcJBgMPLAT",
                "type": "onramp",
                "status": "processing",
                "receipt": {
                    "exchange_rate": "1",
                    "input":  { "amount": "2",    "asset": "USD" },
                    "output": { "amount": "1.50", "asset": "USDC" },
                    "dakota_fee": { "amount": "0.25", "asset": "USD" }
                },
                "sender_details": {
                    "sender_account_holder_name": "Sandbox Sender",
                    "sender_account_number": "9876543210"
                }
            }}
        })
    }

    #[test]
    fn extract_keeps_the_ledger_fields() {
        let e = extract("evt_1", &sample_event());
        assert_eq!(e.event_type, "transaction.auto.updated");
        assert_eq!(e.resource_type.as_deref(), Some("transaction"));
        assert_eq!(e.resource_id.as_deref(), Some("3HNCPYPeEZCpScXWQbvFJwUeIxB"));
        assert_eq!(e.status.as_deref(), Some("processing"));
        assert_eq!(e.direction.as_deref(), Some("in"), "onramp brings value in");
        // Output leg: 1.50 USDC -> 150 minor units.
        assert_eq!(e.amount_minor, Some(150));
        assert_eq!(e.asset.as_deref(), Some("USDC"));
        assert_eq!(e.fee_minor, Some(25));
        assert_eq!(e.exchange_rate.as_deref(), Some("1"));
        assert!(e.occurred_at.is_some());
    }

    #[test]
    fn extract_drops_sender_pii() {
        // The guarantee the whole design rests on: nothing identifying can
        // reach the database, because the row type has nowhere to put it.
        let e = extract("evt_1", &sample_event());
        let serialized = format!("{e:?}");
        assert!(!serialized.contains("Sandbox Sender"));
        assert!(!serialized.contains("9876543210"));
    }

    #[test]
    fn offramp_is_outbound_and_swap_is_neither() {
        let mut v = sample_event();
        v["data"]["object"]["type"] = serde_json::json!("offramp");
        assert_eq!(extract("e", &v).direction.as_deref(), Some("out"));

        v["data"]["object"]["type"] = serde_json::json!("swap");
        // Counting a swap as both in and out would double it in every total.
        assert_eq!(extract("e", &v).direction.as_deref(), Some("transfer"));
    }

    #[test]
    fn minor_units_parses_decimal_strings_exactly() {
        let f = |s: &str| minor_units(Some(&serde_json::json!(s)));
        assert_eq!(f("2"), Some(200));
        assert_eq!(f("1.50"), Some(150));
        assert_eq!(f("0.05"), Some(5));
        assert_eq!(f("0.5"), Some(50), "one decimal place is tenths, not hundredths");
        assert_eq!(f("1.999"), Some(199), "truncates below cents");
        assert_eq!(f("-1.25"), Some(-125));
        assert_eq!(f("1000000"), Some(100_000_000));
    }

    #[test]
    fn minor_units_rejects_garbage() {
        let f = |s: &str| minor_units(Some(&serde_json::json!(s)));
        assert_eq!(f("abc"), None);
        assert_eq!(f("1.2.3"), None);
        assert_eq!(f(""), Some(0));
        assert_eq!(minor_units(None), None);
        assert_eq!(minor_units(Some(&serde_json::json!(2.0))), None, "numbers are not strings here");
    }

    /// The shape `GET /events` and webhook deliveries actually use — flat,
    /// not nested. Captured verbatim from the sandbox.
    fn flat_receipt_event() -> serde_json::Value {
        serde_json::json!({
            "created": 1785699115,
            "type": "transaction.auto.updated",
            "data": { "object": {
                "id": "3HNCPYPeEZCpScXWQbvFJwUeIxB",
                "auto_account_id": "3HNCN914HGh2Sr95XpcJBgMPLAT",
                "fiat_rail": "ach",
                "status": "processing",
                "receipt": {
                    "client_fee": "0", "converted_amount": "2", "dakota_fee": "0.25",
                    "exchange_rate": "1", "external_fee": "0", "initial_amount": "2",
                    "input_currency": "USD", "outgoing_amount": "1.75",
                    "output_currency": "USDC", "subtotal_amount": "2"
                },
                "sender_details": {
                    "sender_account_holder_name": "Sandbox Sender",
                    "sender_account_number": "9876543210"
                }
            }}
        })
    }

    #[test]
    fn flat_receipts_are_parsed_too() {
        // Handling only the nested form leaves every webhook-sourced row with a
        // NULL amount, and the ledger totals to nothing.
        let e = extract("evt_flat", &flat_receipt_event());
        assert_eq!(e.amount_minor, Some(175), "outgoing_amount 1.75 -> 175");
        assert_eq!(e.asset.as_deref(), Some("USDC"));
        assert_eq!(e.fee_minor, Some(25), "bare-string dakota_fee 0.25 -> 25");
        assert_eq!(e.exchange_rate.as_deref(), Some("1"));
    }

    #[test]
    fn flat_receipt_drops_sender_pii() {
        let e = extract("evt_flat", &flat_receipt_event());
        let s = format!("{e:?}");
        assert!(!s.contains("Sandbox Sender") && !s.contains("9876543210"));
    }

    #[test]
    fn account_ref_finds_the_join_key() {
        // Events name the account, never the customer — this is what lets a
        // transfer be attributed to whoever owns it.
        assert_eq!(
            account_ref(&flat_receipt_event()).as_deref(),
            Some("3HNCN914HGh2Sr95XpcJBgMPLAT")
        );
        assert_eq!(account_ref(&serde_json::json!({})), None);
    }

    #[test]
    fn nested_and_flat_fees_agree() {
        let nested = serde_json::json!({ "dakota_fee": { "amount": "0.25", "asset": "USD" } });
        let flat = serde_json::json!({ "dakota_fee": "0.25" });
        assert_eq!(fee_from_receipt(&nested), Some(25));
        assert_eq!(fee_from_receipt(&flat), Some(25));
    }

    #[test]
    fn unknown_event_shape_still_yields_a_row() {
        // A schema change must not crash the receiver.
        let e = extract("evt_x", &serde_json::json!({}));
        assert_eq!(e.event_type, "unknown");
        assert!(e.amount_minor.is_none());
    }

    // ------------------------------------------------------- signature

    fn signed_delivery(key: &SigningKey, ts: i64, body: &[u8]) -> HeaderMap {
        let mut signed = Vec::new();
        signed.extend_from_slice(ts.to_string().as_bytes());
        signed.push(b'.');
        signed.extend_from_slice(body);
        let sig = key.sign(&signed);

        let mut h = HeaderMap::new();
        h.insert(
            "x-webhook-signature",
            base64::engine::general_purpose::STANDARD
                .encode(sig.to_bytes())
                .parse()
                .unwrap(),
        );
        h.insert("x-webhook-timestamp", ts.to_string().parse().unwrap());
        h
    }

    #[test]
    fn valid_signature_verifies() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let st = sk.verifying_key();
        let body = br#"{"type":"x"}"#;
        let h = signed_delivery(&sk, Utc::now().timestamp(), body);
        assert!(verify(&st, &h, body).is_ok());
    }

    #[test]
    fn tampered_body_is_rejected() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let st = sk.verifying_key();
        let h = signed_delivery(&sk, Utc::now().timestamp(), br#"{"amount":"1.00"}"#);
        assert!(verify(&st, &h, br#"{"amount":"9999.00"}"#).is_err());
    }

    #[test]
    fn wrong_key_is_rejected() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let other = SigningKey::from_bytes(&[9u8; 32]);
        let st = other.verifying_key();
        let body = br#"{"type":"x"}"#;
        let h = signed_delivery(&sk, Utc::now().timestamp(), body);
        assert!(verify(&st, &h, body).is_err());
    }

    #[test]
    fn stale_and_future_timestamps_are_rejected() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let st = sk.verifying_key();
        let body = br#"{"type":"x"}"#;

        let stale = signed_delivery(&sk, Utc::now().timestamp() - MAX_SKEW_SECS - 1, body);
        assert!(verify(&st, &stale, body).is_err(), "replay window must close");

        // A far-future stamp would otherwise stay valid indefinitely.
        let future = signed_delivery(&sk, Utc::now().timestamp() + MAX_SKEW_SECS + 1, body);
        assert!(verify(&st, &future, body).is_err());
    }

    #[test]
    fn missing_headers_are_rejected() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let st = sk.verifying_key();
        assert!(verify(&st, &HeaderMap::new(), b"{}").is_err());
    }

    #[test]
    fn sandbox_public_key_parses() {
        // The documented sandbox key — a typo here would fail every delivery.
        let k = parse_verifying_key(
            "7a2f771f3a7ac9ae2a95066df35dc0261d7ce354214736cc232d70b3c66f8a5f",
        );
        assert!(k.is_ok());
        assert!(parse_verifying_key("nothex").is_err());
        assert!(parse_verifying_key("aabb").is_err(), "must be 32 bytes");
    }
}
