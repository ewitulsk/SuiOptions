//! Guarded-launch protocol watches (SO-387).
//!
//! Two watch kinds on top of the wallet balance polls:
//!
//! - **Admin-change watch** — polls the protocol's single shared `Whitelist`
//!   (the standalone whitelist package) and diffs each of its four
//!   membership domains (options, exchange, vault_create, vault_lp) —
//!   members plus the `enabled` / `paused` flags — against the last poll.
//!   Any change fires `alert_id = "whitelist-changed"` (or
//!   `"protocol-paused"` for the pause flag) through the generic alert_id
//!   Grafana rule, with the domain in the `list` field. An alert nobody on
//!   the team caused means the admin key is acting without you — treat as
//!   an incident.
//! - **Drain watch** — polls configured shared objects and sums named
//!   top-level balance fields (e.g. a bucket's `underlying_balance`). While
//!   the value sits more than `drop_bps` below the max seen inside
//!   `window_secs`, fires `alert_id = "drain-suspected-<name>"` each poll.
//!   Only top-level struct fields are visible in the object JSON — vault and
//!   BalanceManager holdings live in dynamic fields and need the follow-up
//!   auto-discovery pass; buckets and similar flat escrows work today.

use std::collections::{BTreeSet, VecDeque};
use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::Value;
use sui_types::base_types::ObjectID;
use sui_tx::chain::ChainClient;
use tracing::{error, info, warn};

use crate::config::DrainWatch;

/// One membership domain's state as of one poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WlState {
    pub members: BTreeSet<String>,
    pub whitelist_enabled: bool,
    pub ingress_paused: bool,
}

/// The whitelist object's four domain fields, in on-chain order.
const DOMAINS: [&str; 4] = ["options", "exchange", "vault_create", "vault_lp"];

pub struct AdminWatch {
    whitelist: ObjectID,
    /// (domain, last seen state).
    domains: Vec<(&'static str, Option<WlState>)>,
}

impl AdminWatch {
    pub fn new(whitelist: ObjectID) -> Self {
        Self {
            whitelist,
            domains: DOMAINS.iter().map(|d| (*d, None)).collect(),
        }
    }

    pub async fn poll(&mut self, sui: &ChainClient) {
        let states = match fetch_wl_states(sui, self.whitelist).await {
            Ok(s) => s,
            Err(e) => {
                warn!(object = %self.whitelist, error = %e, "whitelist poll failed");
                metrics::counter!(
                    "balance_monitor_poll_errors_total",
                    "service" => "whitelist",
                )
                .increment(1);
                return;
            }
        };
        for ((list, last), state) in self.domains.iter_mut().zip(states) {
            let list: &'static str = list;
            metrics::gauge!("whitelist_members", "list" => list).set(state.members.len() as f64);
            metrics::gauge!("whitelist_enabled", "list" => list)
                .set(state.whitelist_enabled as u8 as f64);
            metrics::gauge!("ingress_paused", "list" => list)
                .set(state.ingress_paused as u8 as f64);

            match last {
                None => {
                    info!(
                        list,
                        members = state.members.len(),
                        enabled = state.whitelist_enabled,
                        paused = state.ingress_paused,
                        "whitelist baseline recorded"
                    );
                }
                Some(prev) if *prev != state => {
                    let added: Vec<_> = state.members.difference(&prev.members).cloned().collect();
                    let removed: Vec<_> =
                        prev.members.difference(&state.members).cloned().collect();
                    if !added.is_empty()
                        || !removed.is_empty()
                        || prev.whitelist_enabled != state.whitelist_enabled
                    {
                        error!(
                            alert_id = "whitelist-changed",
                            list,
                            added = ?added,
                            removed = ?removed,
                            enabled = state.whitelist_enabled,
                            "whitelist changed on-chain — expected only from our own admin action"
                        );
                    }
                    if prev.ingress_paused != state.ingress_paused {
                        error!(
                            alert_id = "protocol-paused",
                            list,
                            paused = state.ingress_paused,
                            "ingress pause flag flipped on-chain"
                        );
                    }
                }
                Some(_) => {}
            }
            *last = Some(state);
        }
    }
}

/// Per-domain states in [`DOMAINS`] order.
async fn fetch_wl_states(sui: &ChainClient, id: ObjectID) -> Result<Vec<WlState>> {
    let (_, json) = sui.get_object_json(id).await?;
    let json = json.context("object has no JSON rendering")?;
    DOMAINS
        .iter()
        .map(|domain| {
            let d = json
                .get(domain)
                .with_context(|| format!("no domain field {domain} in whitelist object"))?;
            let members = d
                .get("members")
                .and_then(|m| m.get("contents"))
                .and_then(Value::as_array)
                .with_context(|| format!("no {domain}.members.contents in whitelist object"))?
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            let flag = |name: &str| -> Result<bool> {
                d.get(name)
                    .and_then(Value::as_bool)
                    .with_context(|| format!("no bool field {domain}.{name} in whitelist object"))
            };
            Ok(WlState {
                members,
                whitelist_enabled: flag("enabled")?,
                ingress_paused: flag("paused")?,
            })
        })
        .collect()
}

pub struct DrainWatchState {
    watch: DrainWatch,
    object_id: ObjectID,
    /// (seen at, total) samples inside the rolling window.
    samples: VecDeque<(Instant, u64)>,
}

impl DrainWatchState {
    pub fn new(watch: DrainWatch) -> Result<Self> {
        let object_id = ObjectID::from_hex_literal(&watch.object_id)
            .with_context(|| format!("drain watch '{}': bad object_id", watch.name))?;
        Ok(Self {
            watch,
            object_id,
            samples: VecDeque::new(),
        })
    }

    pub async fn poll(&mut self, sui: &ChainClient) {
        let total = match self.fetch_total(sui).await {
            Ok(t) => t,
            Err(e) => {
                warn!(watch = %self.watch.name, error = %e, "drain watch poll failed");
                metrics::counter!(
                    "balance_monitor_poll_errors_total",
                    "service" => format!("drain-{}", self.watch.name),
                )
                .increment(1);
                return;
            }
        };

        metrics::gauge!("protocol_holdings", "watch" => self.watch.name.clone())
            .set(total as f64);

        let now = Instant::now();
        while let Some((t, _)) = self.samples.front() {
            if now.duration_since(*t).as_secs() > self.watch.window_secs {
                self.samples.pop_front();
            } else {
                break;
            }
        }
        let baseline = self.samples.iter().map(|(_, v)| *v).max().unwrap_or(total);
        self.samples.push_back((now, total));

        // Integer math: breach when total < baseline * (1 - drop_bps/10_000).
        let floor = baseline as u128 * (10_000 - self.watch.drop_bps as u128) / 10_000;
        if (total as u128) < floor {
            error!(
                alert_id = format!("drain-suspected-{}", self.watch.name),
                watch = %self.watch.name,
                object = %self.object_id,
                total,
                baseline,
                drop_bps = self.watch.drop_bps,
                window_secs = self.watch.window_secs,
                "holdings dropped past threshold inside window"
            );
        }
    }

    async fn fetch_total(&self, sui: &ChainClient) -> Result<u64> {
        let (_, json) = sui.get_object_json(self.object_id).await?;
        let json = json.context("object has no JSON rendering")?;
        let mut total: u64 = 0;
        for field in &self.watch.fields {
            let v = json
                .get(field)
                .with_context(|| format!("field '{field}' missing on object"))?;
            total = total.saturating_add(
                extract_amount(v).with_context(|| format!("field '{field}' not numeric"))?,
            );
        }
        Ok(total)
    }
}

/// A `Balance<T>` renders as a bare number, a numeric string, or
/// `{"value": <either>}` depending on renderer version — accept all three.
fn extract_amount(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse().ok(),
        Value::Object(o) => o.get("value").and_then(extract_amount),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_amount_accepts_all_renderings() {
        assert_eq!(extract_amount(&serde_json::json!(42)), Some(42));
        assert_eq!(extract_amount(&serde_json::json!("42")), Some(42));
        assert_eq!(extract_amount(&serde_json::json!({"value": "42"})), Some(42));
        assert_eq!(extract_amount(&serde_json::json!({"value": 42})), Some(42));
        assert_eq!(extract_amount(&serde_json::json!(null)), None);
    }
}
