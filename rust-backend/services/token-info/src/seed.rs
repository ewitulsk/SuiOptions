//! Dev/staging auto-seed: populate the catalog from `deployments.json`
//! testTokens so local and staging flows have a supported-token set with zero
//! manual setup.
//!
//! Idempotent and non-destructive: each token is inserted only if its coin
//! type isn't already present (`seed_if_absent`), so a re-seed never clobbers
//! a manual edit or a prior seed.

use anyhow::Result;
use tracing::{info, warn};

use deployments::NetworkDeployment;

use crate::db::models::UpsertToken;
use crate::db::Repo;

/// Seed every testToken into the catalog. `decimals` and `pyth_feed_id` are
/// taken from the matching `token_info` entry when present (it's the pricing
/// source of truth), else from the testToken record itself.
pub fn auto_seed(repo: &Repo, dep: &NetworkDeployment) -> Result<()> {
    let Some(test_tokens) = dep.package_info.test_tokens.as_ref() else {
        warn!("auto-seed requested but deployments.json has no testTokens; nothing to seed");
        return Ok(());
    };

    let mut inserted = 0usize;
    for (symbol, tt) in &test_tokens.tokens {
        let spec = dep.token_info.get(&symbol.to_ascii_uppercase());
        let decimals = spec.map(|s| s.decimals).unwrap_or(tt.decimals);
        let pyth_feed_id = spec.and_then(|s| s.pyth_feed_id.clone());

        let row = UpsertToken {
            coin_type: tt.coin_type.clone(),
            ticker: symbol.clone(),
            name: symbol.clone(),
            logo_uri: None,
            decimals: decimals as i16,
            pyth_feed_id,
            enabled: true,
            source: "seed".to_string(),
        };
        if repo.seed_if_absent(row)? {
            inserted += 1;
        }
    }
    info!(
        seeded = inserted,
        total = test_tokens.tokens.len(),
        "auto-seeded catalog from testTokens"
    );
    Ok(())
}
