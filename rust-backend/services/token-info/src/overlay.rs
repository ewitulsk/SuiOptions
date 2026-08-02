//! Dev/staging test-token overlay.
//!
//! Instead of persisting the `deployments.json` test tokens into Postgres, we
//! derive them once at boot and merge them into the `/tokens` response at read
//! time (see `handlers::tokens`). This keeps the DB to durable, operator-managed
//! tokens only, and the overlay always tracks `deployments.json` exactly.
//!
//! `decimals`/`pyth_feed_id` come from the deployment's `token_info` block (the
//! pricing source of truth) falling back to the testToken record; `name`/`logo`
//! come from the `[seed_meta.<TICKER>]` config map. Built only on dev/staging.

use std::collections::BTreeMap;

use deployments::NetworkDeployment;
use token_info_client::SupportedToken;
use tracing::info;

use crate::config::SeedMeta;

/// Build the read-time overlay. Returns empty when `enabled` is false (prod) or
/// the deployment has no testTokens (mainnet).
pub fn build(
    dep: &NetworkDeployment,
    seed_meta: &BTreeMap<String, SeedMeta>,
    enabled: bool,
) -> Vec<SupportedToken> {
    if !enabled {
        return Vec::new();
    }
    let Some(tt) = dep.package_info.test_tokens.as_ref() else {
        return Vec::new();
    };

    let overlay: Vec<SupportedToken> = tt
        .tokens
        .iter()
        .map(|(symbol, t)| {
            let spec = dep.token_info.get(&symbol.to_ascii_uppercase());
            let decimals = spec.map(|s| s.decimals).unwrap_or(t.decimals);
            let pyth_feed_id = spec.and_then(|s| s.pyth_feed_id.clone());
            let switchboard_feed_id = spec.and_then(|s| s.switchboard_feed_id.clone());
            // The `config` crate lowercases all TOML keys, so `[seed_meta.TBTC]`
            // arrives as `tbtc`. Look up case-insensitively (lowercase first).
            let meta = seed_meta
                .get(&symbol.to_ascii_lowercase())
                .or_else(|| seed_meta.get(symbol))
                .or_else(|| seed_meta.get(&symbol.to_ascii_uppercase()));
            SupportedToken {
                coin_type: t.coin_type.clone(),
                ticker: symbol.clone(),
                name: meta
                    .and_then(|m| m.name.clone())
                    .unwrap_or_else(|| symbol.clone()),
                logo_uri: meta.and_then(|m| m.logo_uri.clone()),
                decimals,
                pyth_feed_id,
                switchboard_feed_id,
                enabled: true,
            }
        })
        .collect();

    info!(count = overlay.len(), "built dev/staging test-token overlay");
    overlay
}
