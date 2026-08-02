//! Event reads over GraphQL.
//!
//! gRPC has no events query — it is the one JSON-RPC capability the new API
//! does not replace (see `docs/sui-json-rpc-migration.md`). Everything that
//! used `event_api().query_events(..)` comes here instead.
//!
//! The shape deliberately mirrors the old `EventPage`/`SuiEvent` so call
//! sites keep their paging loops: a filter, an opaque cursor, a page size,
//! and a direction.
//!
//! Cursors are opaque base64 strings from the server (they are NOT the old
//! `EventID`), so anything that persists a cursor stores this string
//! verbatim and hands it back untouched.

use anyhow::{anyhow, Context, Result};
use move_core_types::language_storage::StructTag;
use serde_json::json;
use sui_types::base_types::SuiAddress;
use sui_types::digests::TransactionDigest;
use std::str::FromStr;

/// One Move event, with the fields the workspace actually reads.
#[derive(Debug, Clone)]
pub struct ChainEvent {
    pub tx_digest: TransactionDigest,
    /// Index of this event within its transaction.
    pub event_seq: u64,
    pub type_: StructTag,
    pub parsed_json: serde_json::Value,
    pub sender: SuiAddress,
    pub timestamp_ms: Option<u64>,
    pub transaction_module: String,
}

/// A page of events plus the cursor to continue from.
#[derive(Debug, Clone, Default)]
pub struct EventPage {
    pub data: Vec<ChainEvent>,
    pub next_cursor: Option<String>,
    pub has_next_page: bool,
}

/// GraphQL event reader bound to one endpoint.
#[derive(Clone)]
pub struct EventClient {
    http: reqwest::Client,
    url: String,
}

impl EventClient {
    pub fn new(url: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            url: url.to_owned(),
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Query events of one Move type.
    ///
    /// `descending` = newest first. The server always returns a page in
    /// ascending order, so the descending path takes the *last* page and
    /// reverses it — matching the old `query_events(.., descending=true)`.
    pub async fn query_by_type(
        &self,
        event_type: &str,
        cursor: Option<&str>,
        limit: u32,
        descending: bool,
    ) -> Result<EventPage> {
        self.query(json!({ "type": event_type }), cursor, limit, descending)
            .await
    }

    /// Query events emitted by one module (`<pkg>::<module>`).
    pub async fn query_by_module(
        &self,
        module: &str,
        cursor: Option<&str>,
        limit: u32,
        descending: bool,
    ) -> Result<EventPage> {
        self.query(json!({ "module": module }), cursor, limit, descending)
            .await
    }

    async fn query(
        &self,
        filter: serde_json::Value,
        cursor: Option<&str>,
        limit: u32,
        descending: bool,
    ) -> Result<EventPage> {
        // Descending walks backwards from the newest (`last`/`before`);
        // ascending walks forwards from the oldest (`first`/`after`).
        let query = if descending {
            r#"query($f: EventFilter, $n: Int!, $c: String) {
                 events(filter: $f, last: $n, before: $c) {
                   pageInfo { hasPreviousPage startCursor }
                   nodes { sequenceNumber timestamp transactionModule { name }
                           sender { address } transaction { digest }
                           contents { type { repr } json } }
                 }
               }"#
        } else {
            r#"query($f: EventFilter, $n: Int!, $c: String) {
                 events(filter: $f, first: $n, after: $c) {
                   pageInfo { hasNextPage endCursor }
                   nodes { sequenceNumber timestamp transactionModule { name }
                           sender { address } transaction { digest }
                           contents { type { repr } json } }
                 }
               }"#
        };

        let body = json!({
            "query": query,
            "variables": { "f": filter, "n": limit, "c": cursor },
        });

        let resp = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .context("sending GraphQL events query")?
            .error_for_status()
            .context("GraphQL events query returned an HTTP error")?;
        let body: serde_json::Value = resp
            .json()
            .await
            .context("decoding GraphQL events response")?;

        if let Some(errs) = body.get("errors") {
            return Err(anyhow!("GraphQL events query failed: {errs}"));
        }
        let events = body
            .pointer("/data/events")
            .ok_or_else(|| anyhow!("GraphQL events response missing data.events"))?;

        let mut data: Vec<ChainEvent> = events
            .get("nodes")
            .and_then(|n| n.as_array())
            .map(|nodes| nodes.iter().filter_map(parse_event).collect())
            .unwrap_or_default();

        let page_info = events.get("pageInfo");
        let (has_next, next_cursor) = if descending {
            // Newest-first: reverse the ascending page and continue
            // backwards from its first element.
            data.reverse();
            (
                page_info
                    .and_then(|p| p.get("hasPreviousPage"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                page_info
                    .and_then(|p| p.get("startCursor"))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
            )
        } else {
            (
                page_info
                    .and_then(|p| p.get("hasNextPage"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                page_info
                    .and_then(|p| p.get("endCursor"))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
            )
        };

        Ok(EventPage {
            data,
            next_cursor,
            has_next_page: has_next,
        })
    }
}

/// A node that does not parse is skipped rather than failing the page — a
/// single malformed event must not stall a watcher loop.
fn parse_event(node: &serde_json::Value) -> Option<ChainEvent> {
    let digest = node.pointer("/transaction/digest")?.as_str()?;
    let type_repr = node.pointer("/contents/type/repr")?.as_str()?;
    Some(ChainEvent {
        tx_digest: TransactionDigest::from_str(digest).ok()?,
        event_seq: node.get("sequenceNumber")?.as_u64()?,
        type_: sui_types::parse_sui_struct_tag(type_repr).ok()?,
        parsed_json: node
            .pointer("/contents/json")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        sender: node
            .pointer("/sender/address")
            .and_then(|v| v.as_str())
            .and_then(|s| SuiAddress::from_str(s).ok())
            .unwrap_or(SuiAddress::ZERO),
        timestamp_ms: node
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(parse_rfc3339_ms),
        transaction_module: node
            .pointer("/transactionModule/name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned(),
    })
}

fn parse_rfc3339_ms(s: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp_millis().max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_graphql_event_node() {
        let node = serde_json::json!({
            "sequenceNumber": 2,
            "timestamp": "2026-08-02T20:38:25.068Z",
            "sender": { "address": "0x921e2bd3432c784dce15b4073ba666d5067e6f8a41704eb63555235fec2e2e41" },
            "transactionModule": { "name": "block_scholes_store" },
            "transaction": { "digest": "BLkuVvzpzaYZFjNsSNJAU3XQHy2i57WfKr6GXwoGkS5z" },
            "contents": {
                "type": { "repr": "0x756ab217b8b7cbbe7a9e45a5cc385347cb43f74aac0102772336a24cf48ab9cb::block_scholes_store::BlockScholesBatchIngested" },
                "json": { "update_count": "1" }
            }
        });
        let e = parse_event(&node).expect("parses");
        assert_eq!(e.event_seq, 2);
        assert_eq!(e.transaction_module, "block_scholes_store");
        assert_eq!(e.type_.name.as_str(), "BlockScholesBatchIngested");
        assert_eq!(e.parsed_json["update_count"], "1");
        assert_eq!(e.timestamp_ms, Some(1785703105068));
    }

    #[test]
    fn malformed_node_is_skipped_not_fatal() {
        assert!(parse_event(&serde_json::json!({ "sequenceNumber": 1 })).is_none());
    }
}
