//! Concrete [`CommitmentProbe`]s: one per source-chain family, talking raw
//! JSON-RPC to a single RPC provider. The network-touching methods are thin; the
//! response interpretation lives in pure functions (`evm_find_log_block`,
//! `evm_finality`, `sui_events_contain_digest`) that are unit-tested directly.
//!
//! - **EVM** (`eth_getLogs` + `eth_blockNumber`): the Outbox's `MessageCommitted`
//!   event has the message hash as an indexed topic, so a `{address, [topic0,
//!   digest]}` filter is an exact lookup. Finality = confirmation depth.
//! - **Sui** (`suix_queryEvents`): query the package's `MessageCommitted` events
//!   and match `message_hash`. Sui events are only returned once finalized, so a
//!   match is `Final` (no pending state).

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use bridge_types::message::keccak256;
use bridge_types::Bytes32;
use serde_json::{json, Value};

use crate::config::SourceChainConfig;
use crate::verifier::{Commitment, CommitmentProbe};

/// Solidity: `MessageCommitted(bytes32 indexed messageHash, uint32, uint32, uint64, bytes32, bytes32, bytes)`.
const EVM_EVENT_SIG: &[u8] = b"MessageCommitted(bytes32,uint32,uint32,uint64,bytes32,bytes32,bytes)";
/// How many Sui event pages to scan before giving up (newest-first).
const SUI_MAX_PAGES: usize = 20;

/// Build the probes for one configured source chain (one probe per RPC url).
pub fn build_probes(sc: &SourceChainConfig) -> Result<Vec<Box<dyn CommitmentProbe>>> {
    let http = reqwest::Client::builder()
        .build()
        .context("building reqwest client")?;
    match sc.family.as_str() {
        "evm" => {
            let outbox_hex = sc
                .outbox_addr
                .as_ref()
                .ok_or_else(|| anyhow!("evm source {} needs outbox_addr", sc.internal_chain_id))?;
            let outbox = parse_evm_address(outbox_hex)?;
            Ok(sc
                .rpc_urls
                .iter()
                .map(|url| {
                    Box::new(EvmProbe::new(
                        http.clone(),
                        url.clone(),
                        outbox,
                        sc.confirmations,
                        sc.lookback_blocks,
                    )) as Box<dyn CommitmentProbe>
                })
                .collect())
        }
        "sui" => {
            let package = sc
                .package_id
                .as_ref()
                .ok_or_else(|| anyhow!("sui source {} needs package_id", sc.internal_chain_id))?;
            Ok(sc
                .rpc_urls
                .iter()
                .map(|url| {
                    Box::new(SuiProbe::new(http.clone(), url.clone(), package.clone()))
                        as Box<dyn CommitmentProbe>
                })
                .collect())
        }
        other => bail!("source chain {} has unknown family {other:?}", sc.internal_chain_id),
    }
}

fn parse_evm_address(s: &str) -> Result<[u8; 20]> {
    let bytes = hex::decode(s.trim_start_matches("0x")).context("decoding outbox_addr hex")?;
    bytes.try_into().map_err(|v: Vec<u8>| anyhow!("outbox_addr must be 20 bytes, got {}", v.len()))
}

fn host_label(url: &str) -> String {
    url.split("://").nth(1).unwrap_or(url).split('/').next().unwrap_or(url).to_string()
}

async fn rpc(http: &reqwest::Client, url: &str, method: &str, params: Value) -> Result<Value> {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let resp: Value = http
        .post(url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {method}"))?
        .error_for_status()?
        .json()
        .await
        .context("decoding JSON-RPC response")?;
    if let Some(err) = resp.get("error") {
        bail!("JSON-RPC error from {method}: {err}");
    }
    resp.get("result").cloned().ok_or_else(|| anyhow!("{method} response missing result"))
}

// --- EVM ---

pub struct EvmProbe {
    http: reqwest::Client,
    url: String,
    label: String,
    outbox: [u8; 20],
    confirmations: u64,
    /// How many blocks back from head to scan. Public RPCs cap `eth_getLogs`
    /// block ranges (an unbounded `fromBlock: 0x0` is rejected as "too large"),
    /// and a message being relayed is recent, so we only scan a bounded window.
    /// A commitment older than this reads as NotFound → fail-closed refusal.
    lookback_blocks: u64,
    topic0: Bytes32,
}

impl EvmProbe {
    pub fn new(
        http: reqwest::Client,
        url: String,
        outbox: [u8; 20],
        confirmations: u64,
        lookback_blocks: u64,
    ) -> Self {
        let label = host_label(&url);
        Self { http, label, url, outbox, confirmations, lookback_blocks, topic0: keccak256(EVM_EVENT_SIG) }
    }
}

#[async_trait]
impl CommitmentProbe for EvmProbe {
    fn label(&self) -> &str {
        &self.label
    }

    async fn check(&self, digest: &Bytes32) -> Result<Commitment> {
        let latest_hex = rpc(&self.http, &self.url, "eth_blockNumber", json!([])).await?;
        let latest = parse_hex_u64(&latest_hex).context("parsing eth_blockNumber")?;
        let from = latest.saturating_sub(self.lookback_blocks);
        let filter = json!({
            "address": format!("0x{}", hex::encode(self.outbox)),
            "topics": [format!("0x{}", hex::encode(self.topic0)), format!("0x{}", hex::encode(digest))],
            "fromBlock": format!("0x{from:x}"),
            "toBlock": "latest",
        });
        let logs = rpc(&self.http, &self.url, "eth_getLogs", json!([filter])).await?;
        let Some(block) = evm_find_log_block(&logs) else {
            return Ok(Commitment::NotFound);
        };
        Ok(evm_finality(block, latest, self.confirmations))
    }
}

/// Block number of the first log in an `eth_getLogs` result, or `None` if empty.
pub fn evm_find_log_block(result: &Value) -> Option<u64> {
    let first = result.as_array()?.first()?;
    parse_hex_u64(first.get("blockNumber")?).ok()
}

/// Confirmation-depth finality: a log at `block` is `Final` once at least
/// `confirmations` blocks sit on top of it (`latest - block >= confirmations`).
pub fn evm_finality(block: u64, latest: u64, confirmations: u64) -> Commitment {
    if latest.saturating_sub(block) >= confirmations {
        Commitment::Final
    } else {
        Commitment::Pending
    }
}

fn parse_hex_u64(v: &Value) -> Result<u64> {
    let s = v.as_str().ok_or_else(|| anyhow!("expected hex string, got {v}"))?;
    u64::from_str_radix(s.trim_start_matches("0x"), 16).context("parsing hex u64")
}

// --- Sui ---

pub struct SuiProbe {
    http: reqwest::Client,
    url: String,
    label: String,
    committed_type: String,
}

impl SuiProbe {
    pub fn new(http: reqwest::Client, url: String, package: String) -> Self {
        let label = host_label(&url);
        let committed_type = format!("{package}::events::MessageCommitted");
        Self { http, label, url, committed_type }
    }
}

#[async_trait]
impl CommitmentProbe for SuiProbe {
    fn label(&self) -> &str {
        &self.label
    }

    async fn check(&self, digest: &Bytes32) -> Result<Commitment> {
        let filter = json!({ "MoveEventType": self.committed_type });
        let mut cursor = Value::Null;
        for _ in 0..SUI_MAX_PAGES {
            // newest-first (descending) so recent commitments are found fast.
            let page = rpc(
                &self.http,
                &self.url,
                "suix_queryEvents",
                json!([filter, cursor, 50, true]),
            )
            .await?;
            if sui_events_contain_digest(&page, digest) {
                // Sui only returns finalized-checkpoint events → committed == final.
                return Ok(Commitment::Final);
            }
            if !page.get("hasNextPage").and_then(Value::as_bool).unwrap_or(false) {
                break;
            }
            cursor = page.get("nextCursor").cloned().unwrap_or(Value::Null);
        }
        Ok(Commitment::NotFound)
    }
}

/// Does any event in a `suix_queryEvents` page carry `message_hash == digest`?
pub fn sui_events_contain_digest(page: &Value, digest: &Bytes32) -> bool {
    let Some(data) = page.get("data").and_then(Value::as_array) else {
        return false;
    };
    data.iter().any(|ev| {
        ev.get("parsedJson")
            .and_then(|p| p.get("message_hash"))
            .and_then(json_bytes)
            .is_some_and(|b| b.as_slice() == digest.as_slice())
    })
}

/// Decode a Move `vector<u8>` JSON rendering: array-of-numbers or `0x`-hex.
fn json_bytes(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Array(items) => items.iter().map(|x| x.as_u64().and_then(|n| u8::try_from(n).ok())).collect(),
        Value::String(s) => hex::decode(s.trim_start_matches("0x")).ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evm_finality_thresholds() {
        assert_eq!(evm_finality(100, 111, 12), Commitment::Pending); // 11 on top
        assert_eq!(evm_finality(100, 112, 12), Commitment::Final); // 12 on top
        assert_eq!(evm_finality(100, 200, 12), Commitment::Final);
        assert_eq!(evm_finality(100, 100, 0), Commitment::Final); // 0-conf: any inclusion
    }

    #[test]
    fn evm_find_block_parses_first_log() {
        let logs = json!([{ "blockNumber": "0x2a", "data": "0x" }, { "blockNumber": "0x2b" }]);
        assert_eq!(evm_find_log_block(&logs), Some(42));
    }

    #[test]
    fn evm_find_block_empty_is_none() {
        assert_eq!(evm_find_log_block(&json!([])), None);
    }

    #[test]
    fn sui_matches_digest_array_form() {
        let digest = [0xab; 32];
        let page = json!({
            "data": [
                { "parsedJson": { "message_hash": vec![0x00u8; 32] } },
                { "parsedJson": { "message_hash": vec![0xabu8; 32] } },
            ],
            "hasNextPage": false,
        });
        assert!(sui_events_contain_digest(&page, &digest));
    }

    #[test]
    fn sui_matches_digest_hex_form() {
        let digest = [0xcd; 32];
        let page = json!({
            "data": [ { "parsedJson": { "message_hash": format!("0x{}", "cd".repeat(32)) } } ],
        });
        assert!(sui_events_contain_digest(&page, &digest));
    }

    #[test]
    fn sui_no_match_when_absent() {
        let page = json!({ "data": [ { "parsedJson": { "message_hash": vec![0x11u8; 32] } } ] });
        assert!(!sui_events_contain_digest(&page, &[0x22; 32]));
    }

    #[test]
    fn evm_event_topic0_is_stable() {
        // Guards against accidental event-signature drift.
        assert_eq!(
            hex::encode(keccak256(EVM_EVENT_SIG)),
            hex::encode(keccak256(b"MessageCommitted(bytes32,uint32,uint32,uint64,bytes32,bytes32,bytes)"))
        );
    }
}
