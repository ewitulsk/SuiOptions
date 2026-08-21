//! On-chain AdminCap ownership check (SO-422).
//!
//! The static `admin_addresses` list stays authoritative and dependency-free,
//! but admin caps move between wallets (redeploys re-mint them to the
//! deployer; handovers transfer them). Rather than editing config + rolling
//! the service on every move, login falls back to asking the chain: does the
//! signer currently own the core `admin::AdminCap`?
//!
//! Resolution is lazy and fail-closed. The cap TYPE (`{pkg}::admin::AdminCap`)
//! is derived from the core package id served by token-info (the only
//! deployments reader) and cached briefly so a contract redeploy is picked up
//! without a restart. Ownership itself is a single owned-objects GraphQL query
//! (the `sui_rpc.rs` pattern from api-service), re-asked on every login —
//! logins are rare and admin status must not outlive the cap by more than the
//! token TTL. Any error on this path means "not allowed via cap": the static
//! list keeps working when token-info or the GraphQL endpoint is down.

use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::info;

use token_info_client::TokenInfoClient;

/// How long a resolved cap type is trusted before re-asking token-info.
/// Long enough to keep login latency to one GraphQL round-trip in the steady
/// state, short enough that a redeploy's new package id lands promptly.
const CAP_TYPE_TTL: Duration = Duration::from_secs(300);

pub struct CapCheck {
    token_info: TokenInfoClient,
    http: reqwest::Client,
    graphql_url: String,
    cap_type: Mutex<Option<(String, Instant)>>,
}

impl CapCheck {
    pub fn new(token_info_url: &str, sui_graphql_url: &str) -> Self {
        Self {
            token_info: TokenInfoClient::new(token_info_url),
            http: reqwest::Client::new(),
            graphql_url: sui_graphql_url.trim_end_matches('/').to_string(),
            cap_type: Mutex::new(None),
        }
    }

    /// Whether `address` currently owns a core `admin::AdminCap`.
    pub async fn holds_admin_cap(&self, address: &str) -> Result<bool> {
        let cap_type = self.cap_type().await?;
        let body = json!({
            "query": "query($owner: SuiAddress!, $type: String!) {\
 address(address: $owner) { objects(filter: { type: $type }, first: 1) {\
 nodes { address } } } }",
            "variables": { "owner": address, "type": cap_type },
        });
        let resp = self
            .http
            .post(&self.graphql_url)
            .json(&body)
            .send()
            .await
            .context("sending owned-AdminCap query")?
            .error_for_status()
            .context("owned-AdminCap query returned an HTTP error")?;
        let parsed: Value = resp.json().await.context("decoding owned-AdminCap response")?;
        if let Some(errs) = parsed.get("errors") {
            return Err(anyhow!("owned-AdminCap query failed: {errs}"));
        }
        Ok(owns_any(&parsed))
    }

    /// The full cap type tag, resolved from token-info and cached for
    /// [`CAP_TYPE_TTL`].
    async fn cap_type(&self) -> Result<String> {
        let mut guard = self.cap_type.lock().await;
        if let Some((ty, at)) = guard.as_ref() {
            if at.elapsed() < CAP_TYPE_TTL {
                return Ok(ty.clone());
            }
        }
        let snapshot = self.token_info.fetch().await.context("fetching package id from token-info")?;
        let package = snapshot.package().context("core package id from token-info")?;
        let ty = format!("{package}::admin::AdminCap");
        info!(cap_type = %ty, "resolved core AdminCap type");
        *guard = Some((ty.clone(), Instant::now()));
        Ok(ty)
    }
}

/// Whether the owned-objects response carries at least one node. A null
/// `data.address` (unknown address) owns nothing.
fn owns_any(resp: &Value) -> bool {
    resp.pointer("/data/address/objects/nodes")
        .and_then(Value::as_array)
        .map(|nodes| !nodes.is_empty())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owns_any_with_a_node() {
        let resp = serde_json::json!({
            "data": { "address": { "objects": { "nodes": [ { "address": "0x1" } ] } } }
        });
        assert!(owns_any(&resp));
    }

    #[test]
    fn owns_any_empty_nodes() {
        let resp = serde_json::json!({
            "data": { "address": { "objects": { "nodes": [] } } }
        });
        assert!(!owns_any(&resp));
    }

    #[test]
    fn owns_any_null_address() {
        // Unknown addresses come back as a null wrapper, not an error.
        let resp = serde_json::json!({ "data": { "address": null } });
        assert!(!owns_any(&resp));
    }
}
