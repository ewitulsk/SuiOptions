//! Real Sui source watcher over raw JSON-RPC (`suix_queryEvents`) via reqwest.
//!
//! We talk JSON-RPC directly rather than through `sui-sdk` because the SDK's
//! jsonrpsee HTTP client stalls on the `rpc.discover` handshake against
//! proxied public fullnodes; a plain reqwest POST is lighter and reliable.
//!
//! Sui has fast deterministic finality — events returned by a fullnode are from
//! finalized checkpoints — so no extra confirmation gate is needed (§4).

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use bridge_types::CrossChainMessage;
use serde_json::{json, Value};
use tracing::warn;

use crate::event::decode_message_committed;
use crate::relay::SourceWatcher;

const EVENTS_MODULE: &str = "events";
const COMMITTED_SUFFIX: &str = "::events::MessageCommitted";

pub struct SuiSourceWatcher {
    http: reqwest::Client,
    rpc_url: String,
    package: String,
    /// JSON-RPC `EventID` cursor ({txDigest, eventSeq}) or null for the start.
    cursor: Value,
    page_limit: u64,
}

impl SuiSourceWatcher {
    /// Build the watcher and sanity-check the RPC (fails fast on a bad URL).
    pub async fn connect(rpc_url: &str, package: &str) -> Result<Self> {
        // Bind egress to IPv4: some hosts' IPv6 path to proxied fullnodes
        // black-holes the response (connects, then never replies).
        let http = reqwest::Client::builder()
            .local_address(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
            .http1_only()
            .build()
            .context("building reqwest client")?;
        let watcher = Self {
            http,
            rpc_url: rpc_url.to_string(),
            package: package.to_string(),
            cursor: Value::Null,
            page_limit: 50,
        };
        let id: String = watcher
            .rpc("sui_getChainIdentifier", json!([]))
            .await
            .context("sui_getChainIdentifier")?
            .as_str()
            .unwrap_or_default()
            .to_string();
        tracing::info!(rpc_url, chain_identifier = %id, "connected to Sui RPC");
        Ok(watcher)
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let resp: Value = self
            .http
            .post(&self.rpc_url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {method}"))?
            .error_for_status()?
            .json()
            .await
            .context("decoding JSON-RPC response")?;
        if let Some(err) = resp.get("error") {
            return Err(anyhow!("JSON-RPC error from {method}: {err}"));
        }
        resp.get("result")
            .cloned()
            .ok_or_else(|| anyhow!("JSON-RPC response missing result for {method}"))
    }
}

#[async_trait]
impl SourceWatcher for SuiSourceWatcher {
    async fn poll(&mut self) -> Result<Vec<CrossChainMessage>> {
        let mut out = Vec::new();
        loop {
            // MoveEventModule matches by the module that DEFINES the event type
            // (`events`), regardless of which module emitted it. MoveModule would
            // match the emitting module instead and miss these events.
            let filter = json!({ "MoveEventModule": { "package": self.package, "module": EVENTS_MODULE } });
            let params = json!([filter, self.cursor, self.page_limit, false]);
            let result = self.rpc("suix_queryEvents", params).await?;

            for ev in result["data"].as_array().cloned().unwrap_or_default() {
                let ty = ev["type"].as_str().unwrap_or_default();
                if ty.ends_with(COMMITTED_SUFFIX) {
                    match decode_message_committed(&ev["parsedJson"]) {
                        Ok(m) => out.push(m),
                        Err(e) => {
                            warn!(error = %e, "skipping undecodable MessageCommitted event")
                        }
                    }
                }
            }

            // Advance the cursor so the next poll only sees newer events.
            self.cursor = result["nextCursor"].clone();
            if !result["hasNextPage"].as_bool().unwrap_or(false) {
                break;
            }
        }
        Ok(out)
    }
}
