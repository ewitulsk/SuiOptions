//! Chain-state reads: build a [`VaultView`] (and the open-auction
//! [`RfqView`]s / [`SwapRfqView`]s) per discovered vault each tick.
//!
//! Unlike the Sui twin's JSON-RPC field parsing, accounts deserialize
//! through the **program crates' own structs** (anchor
//! `AccountDeserialize` — exact layout, zero drift). Balances live in the
//! vault's PDA-seeded SPL token accounts, so a view is one `Vault` read
//! plus four token-account reads.

use anchor_lang::AccountDeserialize;
use anchor_spl::token::TokenAccount;
use anyhow::{anyhow, Context, Result};
use solana_sdk::pubkey::Pubkey;

use auction_venue::state::Auction;
use options_vault::state::{Phase, Vault, VaultConfig};
use solana_tx::pda;
use solana_tx::SolanaClientWrapper;

/// Everything the planner needs from one vault, snapshotted per tick.
#[derive(Debug, Clone, PartialEq)]
pub struct VaultView {
    pub round: u64,
    pub settling: bool,
    pub current_bucket: Option<Pubkey>,
    /// 0 ⇒ no bucket was selected this round.
    pub current_expiry_ms: u64,
    pub selling_ends_ms: u64,
    pub open_rfqs: u64,
    /// Coupled proceeds-swap auctions not yet settled.
    pub open_swap_rfqs: u64,
    /// positions_tail − positions_head.
    pub pending_positions: u64,
    /// FIFO cursors — seed the VaultPosition PDAs for redeem/settle.
    pub positions_head: u64,
    pub positions_tail: u64,
    /// Seeds the next CPI-created auction PDA (open_rfq / open_swap_rfq).
    pub auction_nonce: u64,
    /// Balance of the `deployable` token account (underlying).
    pub deployable: u64,
    /// Balance of the `proceeds` token account (settlement).
    pub proceeds_settlement: u64,
    /// Balance of the `pending` token account (queued deposits).
    pub pending_deposits: u64,
    /// Balance of the `queued_shares` token account.
    pub queued_withdraw_shares: u64,
    /// The exact on-chain config (pinned feeds, bands, decimals, …).
    pub config: VaultConfig,
}

/// One live vault-coupled call auction (account still exists ⇒ unsettled).
#[derive(Debug, Clone, PartialEq)]
pub struct RfqView {
    pub auction: Pubkey,
    pub bucket: Pubkey,
    pub deadline_ms: u64,
    pub amount: u64,
}

/// One live vault-coupled proceeds-swap auction.
#[derive(Debug, Clone, PartialEq)]
pub struct SwapRfqView {
    pub auction: Pubkey,
    pub deadline_ms: u64,
    /// Settlement escrowed (for logging/metrics).
    pub amount_s: u64,
}

/// Pure assembly of a view from already-read parts — shared by the RPC
/// path below and the litesvm integration test (which reads accounts
/// straight out of the SVM).
pub fn view_from_parts(
    vault: &Vault,
    deployable: u64,
    proceeds: u64,
    pending: u64,
    queued_shares: u64,
) -> Result<VaultView> {
    Ok(VaultView {
        round: vault.round,
        settling: vault.phase == Phase::Settling,
        current_bucket: vault.current_bucket,
        current_expiry_ms: vault.current_expiry_ms,
        selling_ends_ms: vault.selling_ends_ms,
        open_rfqs: vault.open_rfqs,
        open_swap_rfqs: vault.open_swap_rfqs,
        pending_positions: vault
            .positions_tail
            .checked_sub(vault.positions_head)
            .ok_or_else(|| {
                anyhow!(
                    "positions_head {} > tail {}",
                    vault.positions_head,
                    vault.positions_tail
                )
            })?,
        positions_head: vault.positions_head,
        positions_tail: vault.positions_tail,
        auction_nonce: vault.auction_nonce,
        deployable,
        proceeds_settlement: proceeds,
        pending_deposits: pending,
        queued_withdraw_shares: queued_shares,
        config: vault.config,
    })
}

/// Deserialize an SPL token-account balance out of raw account data.
pub fn token_balance(data: &[u8]) -> Result<u64> {
    let acc = TokenAccount::try_deserialize(&mut &data[..])
        .map_err(|e| anyhow!("deserializing token account: {e}"))?;
    Ok(acc.amount)
}

async fn fetch_token_balance(wrap: &SolanaClientWrapper, key: &Pubkey) -> Result<u64> {
    let acc: TokenAccount = wrap.get_account_deserialized(key).await?;
    Ok(acc.amount)
}

/// Read the live vault (the `Vault` account + its four balance-carrying
/// token PDAs) into a [`VaultView`].
pub async fn fetch_vault_view(wrap: &SolanaClientWrapper, vault_pk: &Pubkey) -> Result<VaultView> {
    let vp = options_vault::ID;
    let vault: Vault = wrap
        .get_account_deserialized(vault_pk)
        .await
        .with_context(|| format!("reading vault {vault_pk}"))?;
    let deployable = fetch_token_balance(wrap, &pda::vault_deployable(&vp, vault_pk)).await?;
    let proceeds = fetch_token_balance(wrap, &pda::vault_proceeds(&vp, vault_pk)).await?;
    let pending = fetch_token_balance(wrap, &pda::vault_pending(&vp, vault_pk)).await?;
    let queued = fetch_token_balance(wrap, &pda::vault_queued_shares(&vp, vault_pk)).await?;
    view_from_parts(&vault, deployable, proceeds, pending, queued)
        .with_context(|| format!("assembling view for vault {vault_pk}"))
}

/// Discover the vault's live coupled auctions: the indexer's `auctions`
/// view (status open, creator = vault) yields candidates, then each
/// account is read live — still existing ⇒ still open, and the live read
/// carries the anti-snipe-extended deadline plus the standing bidder.
/// Stateless, restart- and race-safe by construction.
pub async fn discover_open_auctions(
    indexer: &solana_indexer_graphql::IndexerClient,
    wrap: &SolanaClientWrapper,
    vault_pk: &Pubkey,
) -> Result<(Vec<RfqView>, Vec<SwapRfqView>)> {
    let rows = indexer
        .auctions(Some("open"), None, None, Some(&vault_pk.to_string()))
        .await
        .context("querying open auctions from the indexer")?;
    let mut rfqs = Vec::new();
    let mut swaps = Vec::new();
    for row in rows {
        let key: Pubkey = row
            .auction_id
            .parse()
            .with_context(|| format!("parsing auction id {:?}", row.auction_id))?;
        // Live read: a settled auction's account is closed — skip (the
        // indexer view is a tick behind).
        let Ok(account) = wrap.client.get_account(&key).await else {
            continue;
        };
        let Ok(auction) = Auction::try_deserialize(&mut account.data.as_slice()) else {
            continue;
        };
        match row.mode.as_str() {
            "covered_call" => rfqs.push(RfqView {
                auction: key,
                bucket: auction.bucket,
                deadline_ms: auction.deadline_ms,
                amount: auction.amount,
            }),
            "swap" => swaps.push(SwapRfqView {
                auction: key,
                deadline_ms: auction.deadline_ms,
                amount_s: auction.amount,
            }),
            other => {
                // cash_secured_put auctions can't originate from the vault.
                tracing::warn!(auction = %key, mode = other, "unexpected vault-coupled auction mode");
            }
        }
    }
    Ok((rfqs, swaps))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn test_config() -> VaultConfig {
        VaultConfig {
            mgmt_fee_bps_annual: 200,
            perf_fee_bps: 2_000,
            round_ms: 604_800_000,
            selling_window_ms: 43_200_000,
            min_strike_bps_over_spot: 300,
            max_strike_bps_over_spot: 6_000,
            min_expiry_lead_ms: 3 * 86_400_000,
            max_expiry_lead_ms: 9 * 86_400_000,
            min_reserve_premium_bps: 10,
            max_slice_amount: u64::MAX,
            max_open_rfqs: 4,
            rfq_duration_ms: 600_000,
            rfq_snipe_window_ms: 60_000,
            rfq_snipe_extension_ms: 120_000,
            rfq_max_extension_ms: 600_000,
            rfq_min_increment_bps: 500,
            hold_premium_in_settlement: false,
            max_swap_slippage_bps: 100,
            underlying_feed_id: [1u8; 32],
            settlement_feed_id: [2u8; 32],
            max_price_age_secs: 3_600,
            max_conf_bps: 500,
            underlying_decimals: 9,
            settlement_decimals: 6,
        }
    }

    fn vault_fixture(phase: Phase) -> Vault {
        Vault {
            admin: Pubkey::new_unique(),
            underlying_mint: Pubkey::new_unique(),
            settlement_mint: Pubkey::new_unique(),
            share_mint: Pubkey::new_unique(),
            config: test_config(),
            pending_config: None,
            round: 3,
            phase,
            current_bucket: Some(Pubkey::new_unique()),
            current_expiry_ms: 1_700_000_000_000,
            selling_ends_ms: 1_699_990_000_000,
            open_rfqs: 1,
            open_swap_rfqs: 0,
            positions_head: 2,
            positions_tail: 4,
            round_premium_collected: 0,
            round_swap_settlement_out: 0,
            round_swap_underlying_in: 0,
            paused_deposits: false,
            auction_nonce: 7,
            salt: 0,
            bump: 255,
        }
    }

    #[test]
    fn assembles_view_from_parts() {
        let v = vault_fixture(Phase::Active);
        let view = view_from_parts(&v, 5_000_000_000, 123_456, 777, 88).unwrap();
        assert!(!view.settling);
        assert_eq!(view.round, 3);
        assert_eq!(view.pending_positions, 2);
        assert_eq!(view.positions_head, 2);
        assert_eq!(view.positions_tail, 4);
        assert_eq!(view.auction_nonce, 7);
        assert_eq!(view.deployable, 5_000_000_000);
        assert_eq!(view.proceeds_settlement, 123_456);
        assert_eq!(view.pending_deposits, 777);
        assert_eq!(view.queued_withdraw_shares, 88);
        assert_eq!(view.config.underlying_feed_id, [1u8; 32]);
        assert_eq!(view.config.underlying_decimals, 9);

        let view = view_from_parts(&vault_fixture(Phase::Settling), 0, 0, 0, 0).unwrap();
        assert!(view.settling);
    }

    #[test]
    fn rejects_inverted_fifo_cursors() {
        let mut v = vault_fixture(Phase::Active);
        v.positions_head = 9; // head > tail
        assert!(view_from_parts(&v, 0, 0, 0, 0).is_err());
    }
}
