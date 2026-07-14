//! Vault auto-discovery: the keeper finds its vaults instead of being
//! configured with them. As soon as a vault is created on chain, the
//! solana-indexer materializes it (`vaults` view) and the next keeper
//! tick picks it up — no config edit, no redeploy.
//!
//! Everything per-vault comes from the vault's own chain account
//! (authoritative — the pinned feed ids are what `oracle::spot_cross`
//! enforces): mints, feeds, pair decimals. On Solana there is no
//! PriceInfoObject table to walk — the keeper posts its own
//! `PriceUpdateV2` accounts (see `pyth_leg.rs`), so discovery is a single
//! account read.

use anyhow::{Context, Result};
use solana_sdk::pubkey::Pubkey;

use options_vault::state::Vault;
use pyth_client::types::PriceFeedId;
use solana_tx::SolanaClientWrapper;

/// One vault the keeper cranks, fully resolved from chain state at
/// discovery time. Everything here is immutable for the vault's life
/// (mints, feeds and decimals are pinned in `VaultConfig`).
#[derive(Debug, Clone)]
pub struct DiscoveredVault {
    pub vault: Pubkey,
    pub underlying_mint: Pubkey,
    pub settlement_mint: Pubkey,
    pub underlying_decimals: u8,
    pub settlement_decimals: u8,
    pub underlying_feed: PriceFeedId,
    pub settlement_feed: PriceFeedId,
}

/// Fully resolve one indexer vault row into a crankable
/// [`DiscoveredVault`] by reading its chain account.
pub async fn resolve_vault(
    wrap: &SolanaClientWrapper,
    vault_pk: &Pubkey,
) -> Result<DiscoveredVault> {
    let vault: Vault = wrap
        .get_account_deserialized(vault_pk)
        .await
        .with_context(|| format!("reading vault {vault_pk} for discovery"))?;
    Ok(DiscoveredVault {
        vault: *vault_pk,
        underlying_mint: vault.underlying_mint,
        settlement_mint: vault.settlement_mint,
        underlying_decimals: vault.config.underlying_decimals,
        settlement_decimals: vault.config.settlement_decimals,
        underlying_feed: PriceFeedId(vault.config.underlying_feed_id),
        settlement_feed: PriceFeedId(vault.config.settlement_feed_id),
    })
}
