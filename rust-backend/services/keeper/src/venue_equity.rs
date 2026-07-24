//! Venue equity sources for the trading-vault equity-poster crank
//! (SO-299): where the keeper learns what an external account is worth
//! before stepping the on-chain `EquityBook` entry toward it.
//!
//! The keeper's wallet must be an admin-allowlisted poster
//! (`equity_oracle::add_poster`) — a denied post aborts E_NOT_POSTER (1)
//! and is classified as retry (alerting), not benign.
//!
//! Shipped impls are `Disabled` (no posting), `Fixed` (a per-vault
//! target map from keeper config `[external.equity_posts]` — an
//! operator/testing source), and [`Bluefin`] (SO-305: polls the venue's
//! public account endpoint for the FROST parent account's
//! `totalAccountValueE9`; configured via `[external.bluefin]`, default
//! off). The DeepBook-Margin manager reader is still a follow-up behind
//! the same trait.
//!
//! NOTE(SO-305): `[external.bluefin]` is parsed and the source is fully
//! implemented + tested here, but the construction site in
//! `trading_vault.rs` (`equity_posts.is_empty() → Disabled/Fixed`) is
//! owned by the crank work stream — selecting `Bluefin::spawn(...)` there
//! when `external.bluefin` is set is the one-line wiring left to it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use sui_types::base_types::{ObjectID, SuiAddress};
use tracing::warn;

/// Answers "what is this vault's external account worth right now?", in
/// deposit-asset units. `None` ⇒ no opinion, the keeper posts nothing.
pub trait VenueEquitySource: Send + Sync {
    fn equity_for(&self, vault_id: ObjectID, external_account: SuiAddress) -> Option<u64>;
}

/// Never posts.
pub struct Disabled;

impl VenueEquitySource for Disabled {
    fn equity_for(&self, _vault_id: ObjectID, _external_account: SuiAddress) -> Option<u64> {
        None
    }
}

/// Fixed per-vault targets from keeper config (`[external.equity_posts]`).
pub struct Fixed {
    targets: BTreeMap<ObjectID, u64>,
}

impl Fixed {
    pub fn new(targets: BTreeMap<ObjectID, u64>) -> Self {
        Self { targets }
    }
}

impl VenueEquitySource for Fixed {
    fn equity_for(&self, vault_id: ObjectID, _external_account: SuiAddress) -> Option<u64> {
        self.targets.get(&vault_id).copied()
    }
}

/// One vault's Bluefin identity: the FROST parent account address polled
/// for equity, and the vault deposit asset's decimals (Bluefin reports
/// E9 fixed-point; USDC vaults are 6).
#[derive(Debug, Clone)]
pub struct BluefinVenueAccount {
    pub account: SuiAddress,
    pub asset_decimals: u8,
}

/// Bluefin venue equity (SO-305): a background task polls
/// `GET {base_url}/api/v1/account?accountAddress=0x…` (public, no auth;
/// e.g. `https://api.sui-staging.bluefin.io` for the staging env) per
/// configured vault and caches `totalAccountValueE9` scaled to
/// deposit-asset units. `equity_for` reads the cache — never the network —
/// so the sync trait stays non-blocking; a mark older than `max_age`
/// yields `None` (the crank's own staleness alerting covers the gap).
pub struct Bluefin {
    accounts: Arc<BTreeMap<ObjectID, BluefinVenueAccount>>,
    cache: Arc<Mutex<BTreeMap<ObjectID, (u64, Instant)>>>,
    max_age: Duration,
}

impl Bluefin {
    /// Start the polling task on the current tokio runtime.
    pub fn spawn(
        base_url: String,
        accounts: BTreeMap<ObjectID, BluefinVenueAccount>,
        poll_interval: Duration,
        max_age: Duration,
    ) -> Self {
        let accounts = Arc::new(accounts);
        let cache: Arc<Mutex<BTreeMap<ObjectID, (u64, Instant)>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let task_accounts = accounts.clone();
        let task_cache = cache.clone();
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            let base_url = base_url.trim_end_matches('/').to_string();
            loop {
                for (vault, acct) in task_accounts.iter() {
                    match fetch_account_equity(&client, &base_url, acct).await {
                        Ok(equity) => {
                            task_cache
                                .lock()
                                .unwrap()
                                .insert(*vault, (equity, Instant::now()));
                        }
                        Err(e) => {
                            // Stale marks surface through the crank's
                            // equity_stale_alert_ms path; here we only log.
                            warn!(vault = %vault, account = %acct.account, error = %e,
                                "bluefin equity poll failed");
                        }
                    }
                }
                tokio::time::sleep(poll_interval).await;
            }
        });
        Self {
            accounts,
            cache,
            max_age,
        }
    }
}

impl VenueEquitySource for Bluefin {
    fn equity_for(&self, vault_id: ObjectID, external_account: SuiAddress) -> Option<u64> {
        // The on-chain external account must be the account we poll —
        // a mismatch means stale config, and posting would attest the
        // wrong book. No opinion in that case.
        let acct = self.accounts.get(&vault_id)?;
        if acct.account != external_account {
            warn!(vault = %vault_id, configured = %acct.account, onchain = %external_account,
                "bluefin equity: configured account does not match the vault's external account");
            return None;
        }
        let cache = self.cache.lock().unwrap();
        let (equity, at) = cache.get(&vault_id)?;
        if at.elapsed() > self.max_age {
            return None;
        }
        Some(*equity)
    }
}

async fn fetch_account_equity(
    client: &reqwest::Client,
    base_url: &str,
    acct: &BluefinVenueAccount,
) -> Result<u64> {
    let resp = client
        .get(format!("{base_url}/api/v1/account"))
        .query(&[("accountAddress", acct.account.to_string())])
        .send()
        .await
        .context("bluefin account request")?;
    let status = resp.status();
    let body = resp.text().await.context("bluefin account body")?;
    if !status.is_success() {
        return Err(anyhow!("bluefin account HTTP {status}: {body}"));
    }
    equity_from_account_json(&body, acct.asset_decimals)
}

/// `totalAccountValueE9` (effective balance + unrealized PnL − pending
/// funding, an E9 fixed-point decimal string) from Bluefin's account
/// response, scaled to deposit-asset units. A negative venue value clamps
/// to 0 — on-chain equity is a u64.
pub fn equity_from_account_json(body: &str, asset_decimals: u8) -> Result<u64> {
    let v: serde_json::Value = serde_json::from_str(body).context("bluefin account JSON")?;
    let raw = v
        .get("totalAccountValueE9")
        .ok_or_else(|| anyhow!("bluefin account response missing totalAccountValueE9"))?;
    // E9 values ship as decimal strings; tolerate a plain number too.
    let value_e9: i128 = match raw {
        serde_json::Value::String(s) => s
            .parse()
            .map_err(|_| anyhow!("totalAccountValueE9 {s:?} is not an integer"))?,
        serde_json::Value::Number(n) => n
            .as_i64()
            .ok_or_else(|| anyhow!("totalAccountValueE9 {n} is not an integer"))?
            as i128,
        other => return Err(anyhow!("totalAccountValueE9 has unexpected type: {other}")),
    };
    if asset_decimals > 9 {
        return Err(anyhow!(
            "asset_decimals {asset_decimals} > 9 is unsupported for an E9 venue value"
        ));
    }
    let scaled = value_e9.max(0) / 10i128.pow(u32::from(9 - asset_decimals));
    u64::try_from(scaled).context("scaled bluefin equity overflows u64")
}

/// One guardrail-respecting step from `previous` toward `target`: the
/// on-chain `post_equity` aborts (E_DELTA_TOO_LARGE) when
/// `delta * 10_000 > previous * max_delta_bps`, so the step is capped at
/// `floor(previous * max_delta_bps / 10_000)` and never overshoots the
/// target. A `previous` of zero is immovable (bps-of-zero) — callers
/// must skip and surface that admin `seed_equity` is required.
pub fn clamp_step(previous: u64, target: u64, max_delta_bps: u64) -> u64 {
    let max_delta = ((previous as u128) * (max_delta_bps as u128) / 10_000) as u64;
    if target > previous {
        target.min(previous.saturating_add(max_delta))
    } else {
        target.max(previous.saturating_sub(max_delta))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_step_bounds_each_direction() {
        // 20% cap: 1_000_000 may move at most ±200_000 per step.
        assert_eq!(clamp_step(1_000_000, 1_100_000, 2_000), 1_100_000); // within cap
        assert_eq!(clamp_step(1_000_000, 2_000_000, 2_000), 1_200_000); // capped up
        assert_eq!(clamp_step(1_000_000, 100, 2_000), 800_000); // capped down
        assert_eq!(clamp_step(1_000_000, 900_000, 2_000), 900_000); // within cap down
        assert_eq!(clamp_step(1_000_000, 1_000_000, 2_000), 1_000_000); // no-op
    }

    #[test]
    fn clamp_step_never_violates_the_onchain_guardrail() {
        // floor() rounding: the clamped delta always satisfies
        // delta * 10_000 <= previous * max_delta_bps.
        for (previous, target, bps) in [
            (3u64, 100u64, 2_500u64), // floor(3*2500/10000) = 0
            (7, 0, 3_333),
            (999_999, u64::MAX, 1),
            (u64::MAX / 2, u64::MAX, 10_000),
        ] {
            let stepped = clamp_step(previous, target, bps);
            let delta = stepped.abs_diff(previous);
            assert!(
                (delta as u128) * 10_000 <= (previous as u128) * (bps as u128),
                "guardrail violated: prev={previous} target={target} bps={bps} stepped={stepped}"
            );
        }
    }

    #[test]
    fn clamp_step_zero_previous_is_immovable() {
        assert_eq!(clamp_step(0, 5_000, 2_000), 0);
    }

    #[test]
    fn fixed_source_answers_only_mapped_vaults() {
        let vault = ObjectID::from_hex_literal("0xabc").unwrap();
        let other = ObjectID::from_hex_literal("0xdef").unwrap();
        let src = Fixed::new([(vault, 42u64)].into_iter().collect());
        let acct = SuiAddress::ZERO;
        assert_eq!(src.equity_for(vault, acct), Some(42));
        assert_eq!(src.equity_for(other, acct), None);
        assert_eq!(Disabled.equity_for(vault, acct), None);
    }

    #[test]
    fn bluefin_equity_json_scales_e9_to_asset_decimals() {
        // 1,234.567890123 (E9) → 1,234.567890 USDC (6dp).
        let body = r#"{"totalAccountValueE9":"1234567890123","crossAccountValueE9":"0"}"#;
        assert_eq!(equity_from_account_json(body, 6).unwrap(), 1_234_567_890);
        // 9dp asset keeps every digit; a plain JSON number also parses.
        assert_eq!(equity_from_account_json(body, 9).unwrap(), 1_234_567_890_123);
        let body = r#"{"totalAccountValueE9":1000000000}"#;
        assert_eq!(equity_from_account_json(body, 6).unwrap(), 1_000_000);
        // Negative venue value clamps to 0 (on-chain equity is a u64).
        let body = r#"{"totalAccountValueE9":"-5000000000"}"#;
        assert_eq!(equity_from_account_json(body, 6).unwrap(), 0);
        // Missing field / junk fail loudly.
        assert!(equity_from_account_json(r#"{"positions":[]}"#, 6).is_err());
        assert!(equity_from_account_json("not json", 6).is_err());
        assert!(equity_from_account_json(r#"{"totalAccountValueE9":"1"}"#, 10).is_err());
    }

    /// End-to-end against a mock Bluefin account endpoint: the polling task
    /// caches equity per configured vault, `equity_for` answers only the
    /// mapped vault + matching external account.
    #[tokio::test(flavor = "multi_thread")]
    async fn bluefin_source_polls_and_caches() {
        use axum::extract::Query;
        use axum::routing::get;
        use std::collections::HashMap;

        let parent =
            "0x00000000000000000000000000000000000000000000000000000000000000f0";
        let expected_addr = parent.to_string();
        let app = axum::Router::new().route(
            "/api/v1/account",
            get(move |Query(q): Query<HashMap<String, String>>| {
                let expected = expected_addr.clone();
                async move {
                    assert_eq!(q.get("accountAddress"), Some(&expected));
                    r#"{"totalAccountValueE9":"2500000000000","positions":[]}"#
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let vault = ObjectID::from_hex_literal("0xabc").unwrap();
        let other_vault = ObjectID::from_hex_literal("0xdef").unwrap();
        let account: SuiAddress = parent.parse().unwrap();
        let src = Bluefin::spawn(
            base_url,
            [(
                vault,
                BluefinVenueAccount {
                    account,
                    asset_decimals: 6,
                },
            )]
            .into_iter()
            .collect(),
            Duration::from_millis(50),
            Duration::from_secs(5),
        );

        // Wait for the first poll to land.
        let mut equity = None;
        for _ in 0..100 {
            equity = src.equity_for(vault, account);
            if equity.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(equity, Some(2_500_000_000), "2,500 USDC at 6dp");
        // Unmapped vault: no opinion.
        assert_eq!(src.equity_for(other_vault, account), None);
        // On-chain external account drifted from config: no opinion.
        assert_eq!(src.equity_for(vault, SuiAddress::ZERO), None);
    }
}
