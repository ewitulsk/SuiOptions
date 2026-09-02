//! Chain watchers.
//!
//! - **Spoke watcher**: polls the EVM endpoint's `OutboundMessage(bytes)`
//!   logs (spoke→hub wire messages), persists each as a pending queue row.
//! - **Hub watcher**: pages the Sui GraphQL event streams — hub
//!   `OutboundMessage` (hub→spoke queue rows), `SpokeWithdrawProcessed` /
//!   `SpokePayoutSettled` (payables book), `SpokeStateSynced` (fee pot +
//!   reconciliation), and the configured gate events (pause/risk/identity
//!   changes → immediate ConfigSync push).
//!
//! Both persist their scan positions in `watch_cursors`, so a restart
//! re-observes nothing it already ingested (and the unique lane
//! constraint suppresses anything it does).

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use tracing::{debug, info, warn};
use vault_messages::{decode, MsgType, Payload};

use crate::db::models::{direction, status, NewMessage};
use crate::db::repo::blocking;
use crate::engine::direction_for;
use crate::evm::SpokeChain;
use crate::hub::json_u64;
use crate::state::AppState;

const EVM_CURSOR: &str = "evm_blocks";

// ── pure parsing (unit-tested) ─────────────────────────────────────────

/// Decode one spoke→hub wire message into its lane identity.
pub fn parse_spoke_outbound(message: &[u8]) -> Result<(u64, u64, MsgType)> {
    let msg = decode(message).map_err(|e| anyhow!("wire decode: {e}"))?;
    let msg_type = msg.payload.msg_type();
    if direction_for(msg_type) != direction::SPOKE_TO_HUB {
        bail!("unexpected {msg_type:?} in a spoke outbound log");
    }
    let spoke_id = match &msg.payload {
        Payload::DepositNotice { spoke_id, .. }
        | Payload::WithdrawRequest { spoke_id, .. }
        | Payload::PayoutReceipt { spoke_id, .. }
        | Payload::StateSync { spoke_id, .. } => *spoke_id,
        _ => unreachable!("direction checked"),
    };
    Ok((spoke_id, msg.envelope.seq, msg_type))
}

/// A Move `vector<u8>` off the GraphQL json rendering: array of numbers,
/// base64 string, or 0x-hex string — accept all three.
pub fn json_bytes(v: &Value) -> Option<Vec<u8>> {
    if let Some(arr) = v.as_array() {
        return arr
            .iter()
            .map(|e| e.as_u64().and_then(|n| u8::try_from(n).ok()))
            .collect();
    }
    let s = v.as_str()?;
    if let Some(hexed) = s.strip_prefix("0x") {
        return hex::decode(hexed).ok();
    }
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

/// An EVM 20-byte address as the hub stores it: a 32-byte Sui `address`,
/// left-padded, lowercase 0x-hex.
pub fn pad_evm_address(addr: &str) -> String {
    let hexed = addr.trim_start_matches("0x").to_ascii_lowercase();
    format!("0x{}{}", "0".repeat(64usize.saturating_sub(hexed.len())), hexed)
}

fn norm_id(s: &str) -> String {
    let h = s.trim_start_matches("0x").to_ascii_lowercase();
    format!("0x{}{}", "0".repeat(64usize.saturating_sub(h.len())), h)
}

/// A parsed hub `OutboundMessage` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubOutbound {
    pub dst_app: String,
    pub seq: u64,
    pub msg_type: u8,
    pub bytes: Vec<u8>,
}

pub fn parse_hub_outbound(json: &Value) -> Result<HubOutbound> {
    let dst_app = json
        .get("dst_app")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("OutboundMessage missing dst_app"))?;
    Ok(HubOutbound {
        dst_app: norm_id(dst_app),
        seq: json
            .get("seq")
            .and_then(json_u64)
            .ok_or_else(|| anyhow!("OutboundMessage missing seq"))?,
        msg_type: json
            .get("msg_type")
            .and_then(json_u64)
            .and_then(|v| u8::try_from(v).ok())
            .ok_or_else(|| anyhow!("OutboundMessage missing msg_type"))?,
        bytes: json
            .get("bytes")
            .and_then(json_bytes)
            .ok_or_else(|| anyhow!("OutboundMessage bytes unparseable"))?,
    })
}

// ── spoke watcher ──────────────────────────────────────────────────────

pub struct SpokeWatcherParams {
    pub state: Arc<AppState>,
    pub spoke: Arc<dyn SpokeChain>,
    pub spoke_id: u64,
    pub start_block: Option<u64>,
    pub max_scan_blocks: u64,
    pub poll_interval: Duration,
}

pub fn spawn_spoke(p: SpokeWatcherParams) {
    tokio::spawn(async move { run_loop("spoke watcher", p.poll_interval, || spoke_tick(&p)).await });
}

async fn spoke_tick(p: &SpokeWatcherParams) -> Result<()> {
    let repo = p.state.repo.clone();
    let latest = p.spoke.latest_block().await?;
    let cursor = blocking(&repo, |r| r.cursor(EVM_CURSOR)).await?;
    let from = match cursor {
        Some(c) => c.parse::<u64>().context("bad evm cursor")? + 1,
        // Fresh DB: start at the configured block, else the current tip.
        None => p.start_block.unwrap_or(latest),
    };
    if from > latest {
        return Ok(());
    }
    let to = latest.min(from + p.max_scan_blocks - 1);
    let logs = p.spoke.outbound_logs(from, to).await?;
    for log in logs {
        match parse_spoke_outbound(&log.message) {
            Ok((spoke_id, seq, msg_type)) => {
                if spoke_id != p.spoke_id {
                    warn!(spoke_id, expected = p.spoke_id, "outbound log for a different spoke; skipping");
                    continue;
                }
                let new = NewMessage {
                    direction: direction::SPOKE_TO_HUB.to_string(),
                    spoke_id: spoke_id as i64,
                    seq: seq as i64,
                    msg_type: msg_type as u8 as i16,
                    message_hex: hex::encode(&log.message),
                    status: status::PENDING.to_string(),
                    observed_tx: Some(log.tx_hash.clone()),
                };
                let inserted = blocking(&repo, move |r| r.insert_message(new)).await?;
                if inserted {
                    info!(spoke_id, seq, ?msg_type, tx = %log.tx_hash, "spoke->hub message queued");
                } else {
                    debug!(spoke_id, seq, "duplicate spoke->hub message suppressed");
                }
            }
            Err(e) => {
                // A malformed log must not wedge the scan — record and move on.
                warn!(block = log.block, tx = %log.tx_hash, error = %format!("{e:#}"), "undecodable outbound log skipped");
            }
        }
    }
    let to_s = to.to_string();
    blocking(&repo, move |r| r.set_cursor(EVM_CURSOR, &to_s)).await?;
    Ok(())
}

// ── hub watcher ────────────────────────────────────────────────────────

pub struct HubWatcherParams {
    pub state: Arc<AppState>,
    pub events: sui_tx::events::EventClient,
    /// trading-vault package id (0x…), for event type names.
    pub pkg: String,
    pub vault_id: String,
    pub spoke_id: u64,
    /// The spoke vault address, hub-padded (matches `dst_app`).
    pub spoke_app: String,
    pub gate_event_types: Vec<String>,
    pub poll_interval: Duration,
}

pub fn spawn_hub(p: HubWatcherParams) {
    tokio::spawn(async move { run_loop("hub watcher", p.poll_interval, || hub_tick(&p)).await });
}

async fn hub_tick(p: &HubWatcherParams) -> Result<()> {
    let vault_id = norm_id(&p.vault_id);

    drain(p, "events::OutboundMessage", |p, json| {
        let out = parse_hub_outbound(json)?;
        if out.dst_app != p.spoke_app {
            return Ok(()); // another vault/spoke's lane
        }
        let new = NewMessage {
            direction: direction::HUB_TO_SPOKE.to_string(),
            spoke_id: p.spoke_id as i64,
            seq: out.seq as i64,
            msg_type: out.msg_type as i16,
            message_hex: hex::encode(&out.bytes),
            status: status::PENDING.to_string(),
            observed_tx: None,
        };
        let repo = p.state.repo.clone();
        if repo.insert_message(new)? {
            info!(seq = out.seq, msg_type = out.msg_type, "hub->spoke message queued");
        }
        Ok(())
    })
    .await?;

    drain(p, "events::SpokeWithdrawProcessed", |p, json| {
        if json.get("vault_id").and_then(Value::as_str).map(norm_id) != Some(vault_id.clone()) {
            return Ok(());
        }
        let request_seq = json.get("request_seq").and_then(json_u64).unwrap_or(0);
        let pay_units = json.get("pay_units").and_then(json_u64).unwrap_or(0);
        if pay_units > 0 {
            p.state.repo.upsert_payable(
                p.spoke_id as i64,
                request_seq as i64,
                pay_units.into(),
            )?;
        }
        Ok(())
    })
    .await?;

    drain(p, "events::SpokePayoutSettled", |p, json| {
        if json.get("vault_id").and_then(Value::as_str).map(norm_id) != Some(vault_id.clone()) {
            return Ok(());
        }
        let request_seq = json.get("request_seq").and_then(json_u64).unwrap_or(0);
        let unmatched = json.get("unmatched").and_then(json_u64).unwrap_or(0);
        if unmatched > 0 {
            warn!(request_seq, unmatched, "payout receipt exceeded hub books — reconciliation drift");
        }
        p.state.repo.settle_payable(p.spoke_id as i64, request_seq as i64)?;
        Ok(())
    })
    .await?;

    drain(p, "events::SpokeStateSynced", |p, json| {
        if json.get("vault_id").and_then(Value::as_str).map(norm_id) != Some(vault_id.clone()) {
            return Ok(());
        }
        let ts_ms = json.get("ts_ms").and_then(json_u64).unwrap_or(0);
        let fee_pot = json
            .get("fee_pot_balance")
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| v.to_string())
            })
            .and_then(|s| s.parse::<bigdecimal::BigDecimal>().ok())
            .unwrap_or_default();
        if json.get("divergent").and_then(Value::as_bool) == Some(true) {
            warn!(ts_ms, "spoke report diverges from hub books — reconciliation drift");
        }
        p.state
            .repo
            .upsert_lane_stats(p.spoke_id as i64, fee_pot, ts_ms as i64)?;
        Ok(())
    })
    .await?;

    // Gate events: contents don't matter — any new one means the spoke's
    // ConfigSync view may be stale.
    for ty in p.gate_event_types.clone() {
        let due = drain_count(p, &ty).await?;
        if due > 0 {
            info!(event = %ty, count = due, "hub gate event observed — ConfigSync push due");
            p.state.config_sync_due.store(true, Ordering::Relaxed);
        }
    }
    Ok(())
}

/// Page one event stream forward from its stored cursor, applying `f` to
/// each event's parsed json. Repo work happens inline on purpose: the
/// handler closures are quick row upserts.
async fn drain(
    p: &HubWatcherParams,
    type_suffix: &str,
    f: impl Fn(&HubWatcherParams, &Value) -> Result<()>,
) -> Result<()> {
    let event_type = format!("{}::{}", p.pkg, type_suffix);
    let cursor_name = format!("hub:{type_suffix}");
    let mut cursor = blocking(&p.state.repo, {
        let n = cursor_name.clone();
        move |r| r.cursor(&n)
    })
    .await?;
    for _ in 0..10 {
        let page = p
            .events
            .query_by_type(&event_type, cursor.as_deref(), 50, false)
            .await
            .with_context(|| format!("querying {event_type}"))?;
        for ev in &page.data {
            if let Err(e) = f(p, &ev.parsed_json) {
                warn!(event = %event_type, error = %format!("{e:#}"), "event handler failed; skipping event");
            }
        }
        if let Some(next) = page.next_cursor.clone() {
            let (n, v) = (cursor_name.clone(), next.clone());
            blocking(&p.state.repo, move |r| r.set_cursor(&n, &v)).await?;
            cursor = Some(next);
        }
        if !page.has_next_page {
            break;
        }
    }
    Ok(())
}

/// Like [`drain`] but only counts new events.
async fn drain_count(p: &HubWatcherParams, type_suffix: &str) -> Result<usize> {
    let count = std::sync::atomic::AtomicUsize::new(0);
    drain(p, type_suffix, |_, _| {
        count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    })
    .await?;
    Ok(count.into_inner())
}

// ── shared loop shell ──────────────────────────────────────────────────

pub async fn run_loop<F, Fut>(label: &'static str, interval: Duration, tick: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut consecutive_failures: u32 = 0;
    loop {
        ticker.tick().await;
        match tick().await {
            Ok(()) => consecutive_failures = 0,
            Err(e) => {
                consecutive_failures += 1;
                if consecutive_failures >= 5 {
                    tracing::error!(
                        alert_id = "vault-messenger-watch-failed",
                        label,
                        consecutive_failures,
                        error = %format!("{e:#}"),
                        "{label} failing repeatedly"
                    );
                } else {
                    warn!(label, error = %format!("{e:#}"), "{label} tick failed; retrying");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use vault_messages::{encode, Envelope, Message};

    fn wire(seq: u64, payload: Payload) -> Vec<u8> {
        encode(&Message {
            envelope: Envelope {
                src_chain_id: 0x101,
                dst_chain_id: 1,
                src_app: [2u8; 32],
                dst_app: [1u8; 32],
                seq,
            },
            payload,
        })
    }

    #[test]
    fn parses_spoke_outbound_via_vault_messages() {
        let bytes = wire(
            7,
            Payload::DepositNotice {
                spoke_id: 3,
                deposit_seq: 41,
                depositor: [0xd; 32],
                asset: 1,
                amount: 5,
                tranche: 2,
                ts_ms: 1,
            },
        );
        assert_eq!(
            parse_spoke_outbound(&bytes).unwrap(),
            (3, 7, MsgType::DepositNotice)
        );

        // Hub→spoke payloads must be rejected on the spoke stream.
        let ack = wire(9, Payload::DepositAck { deposit_seq: 41, accepted: true, shares: 10 });
        assert!(parse_spoke_outbound(&ack).is_err());

        // Truncated bytes fail decode, not panic.
        assert!(parse_spoke_outbound(&bytes[..bytes.len() - 1]).is_err());
    }

    /// Duplicate suppression rests on two layers: a re-observed message
    /// parses to the SAME lane key (spoke_id, seq) — so the DB's
    /// (direction, spoke_id, seq) unique insert is a no-op — and anything
    /// at/behind the confirmed watermark is gated off redelivery.
    #[test]
    fn duplicate_observations_map_to_one_lane_key() {
        let payload = Payload::PayoutReceipt { spoke_id: 3, request_seq: 9, amount: 55 };
        let first = wire(12, payload.clone());
        let second = wire(12, payload); // same message re-observed (rescan)
        assert_eq!(
            parse_spoke_outbound(&first).unwrap(),
            parse_spoke_outbound(&second).unwrap()
        );
        // And once seq 12 is confirmed, the gate refuses to redeliver it.
        assert_eq!(
            crate::engine::order_gate(12, 12),
            crate::engine::OrderGate::StaleDuplicate
        );
    }

    #[test]
    fn json_bytes_accepts_all_renderings() {
        assert_eq!(json_bytes(&json!([1, 2, 255])), Some(vec![1, 2, 255]));
        assert_eq!(json_bytes(&json!("0xdeadbeef")), Some(vec![0xde, 0xad, 0xbe, 0xef]));
        assert_eq!(json_bytes(&json!("3q2+7w==")), Some(vec![0xde, 0xad, 0xbe, 0xef]));
        assert_eq!(json_bytes(&json!([1, 300])), None);
        assert_eq!(json_bytes(&json!({})), None);
    }

    #[test]
    fn parses_hub_outbound_event_json() {
        let ev = json!({
            "endpoint": "abc::endpoint_relayer::RelayerEndpoint",
            "dst_chain_id": "257",
            "dst_app": "0x00000000000000000000000000000000000000000000000000000000000000aa",
            "seq": "4",
            "msg_type": 5,
            "bytes": [1, 2, 3],
        });
        let out = parse_hub_outbound(&ev).unwrap();
        assert_eq!(out.seq, 4);
        assert_eq!(out.msg_type, 5);
        assert_eq!(out.bytes, vec![1, 2, 3]);
        assert_eq!(out.dst_app, pad_evm_address("0xaa"));
        assert!(parse_hub_outbound(&json!({})).is_err());
    }

    #[test]
    fn pads_evm_addresses_to_hub_width() {
        assert_eq!(
            pad_evm_address("0xAB12000000000000000000000000000000000cd9"),
            "0x000000000000000000000000ab12000000000000000000000000000000000cd9"
        );
        assert_eq!(pad_evm_address("0xaa").len(), 66);
    }
}
