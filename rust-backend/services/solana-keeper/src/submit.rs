//! Action → transaction submission, plus the error triage.
//!
//! Split in two layers so the litesvm integration test can drive the real
//! builders without an RPC:
//!
//! - **pure builders** (`build_*`): already-resolved inputs → instruction
//!   lists (plus the fresh Position keypair where the ix inits one);
//! - **async `execute`**: resolves the live inputs (auction state, FIFO
//!   head position) over RPC, posts the Pyth legs for oracle-gated cranks
//!   ([`crate::pyth_leg`]), and sends.
//!
//! Oracle-gated cranks (`select_bucket`, `open_rfq`, `open_swap_rfq`,
//! `settle_swap_rfq`, `finalize_round`) read the two `PriceUpdateV2`
//! accounts; the rest submit plain.
//!
//! Recipient token accounts (the winner's call/settlement ATA, the core
//! treasury's fee ATAs) are created **idempotently** in the same
//! transaction — anyone may create someone else's ATA, and a missing
//! destination must never wedge a permissionless crank.

use anyhow::{anyhow, Context, Result};
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer as _;

use auction_venue::state::Auction;
use options_vault::state::VaultPosition;
use solana_tx::{ix, pda, Classification, Program, SolanaClientWrapper};

use crate::discovery::DiscoveredVault;
use crate::planner::Action;
use crate::pyth_leg::{send_oracle_gated, PythPoster};
use crate::state::VaultView;

// ── pure instruction builders ──────────────────────────────────────────

/// `spl-associated-token-account` `CreateIdempotent` (discriminant 1):
/// creates `owner`'s ATA for `mint` unless it already exists. Hand-encoded
/// — the instruction-builder crate isn't in our tree, and the account
/// order is stable ABI.
pub fn create_ata_idempotent_ix(payer: &Pubkey, owner: &Pubkey, mint: &Pubkey) -> Instruction {
    Instruction {
        program_id: anchor_spl::associated_token::ID,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(pda::ata(owner, mint), false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(anchor_lang::system_program::ID, false),
            AccountMeta::new_readonly(anchor_spl::token::ID, false),
        ],
        data: vec![1],
    }
}

pub fn build_crank_redeem(
    cranker: &Pubkey,
    meta: &DiscoveredVault,
    bucket: &Pubkey,
    positions_head: u64,
    position: &Pubkey,
) -> Instruction {
    ix::crank_redeem(&ix::CrankRedeem {
        cranker: *cranker,
        vault: meta.vault,
        bucket: *bucket,
        positions_head,
        position: *position,
        bucket_underlying_vault: pda::ata(bucket, &meta.underlying_mint),
        bucket_settlement_vault: pda::ata(bucket, &meta.settlement_mint),
    })
}

/// `settle_rfq` needs a fresh Position keypair (co-signer) and the
/// winner's call-token destination. With no winner the venue takes the
/// refund path but still deserializes `call_dest` / `core_treasury_token`
/// as token accounts, so both are created idempotently up front (owner =
/// the standing recipient, or the cranker as a placeholder when unsold).
pub fn build_settle_rfq(
    cranker: &Pubkey,
    meta: &DiscoveredVault,
    auction: &Pubkey,
    best_token_recipient: Option<Pubkey>,
    positions_tail: u64,
    bucket: &Pubkey,
) -> (Vec<Instruction>, Keypair) {
    let position = Keypair::new();
    let call_mint = pda::call_mint(&options_core::ID, bucket);
    let recipient = best_token_recipient.unwrap_or(*cranker);
    let core_treasury = pda::treasury(&options_core::ID);
    let ixs = vec![
        create_ata_idempotent_ix(cranker, &recipient, &call_mint),
        create_ata_idempotent_ix(cranker, &core_treasury, &meta.settlement_mint),
        ix::settle_rfq(&ix::SettleRfq {
            cranker: *cranker,
            vault: meta.vault,
            auction: *auction,
            positions_tail,
            bucket: *bucket,
            position: position.pubkey(),
            bucket_underlying_vault: pda::ata(bucket, &meta.underlying_mint),
            call_mint,
            call_dest: pda::ata(&recipient, &call_mint),
            core_treasury_token: pda::ata(&core_treasury, &meta.settlement_mint),
        }),
    ];
    (ixs, position)
}

/// Recovery settle: refunds the standing bid (if any) to the bidder's
/// bid-mint ATA — the venue pins that exact address.
pub fn build_settle_rfq_expired(
    cranker: &Pubkey,
    meta: &DiscoveredVault,
    auction: &Pubkey,
    bucket: &Pubkey,
    best_bidder: Option<Pubkey>,
) -> Vec<Instruction> {
    let mut ixs = Vec::new();
    let bidder_refund = best_bidder.map(|bidder| {
        ixs.push(create_ata_idempotent_ix(cranker, &bidder, &meta.settlement_mint));
        pda::ata(&bidder, &meta.settlement_mint)
    });
    ixs.push(ix::settle_rfq_expired(
        cranker,
        &meta.vault,
        auction,
        bucket,
        bidder_refund,
    ));
    ixs
}

pub fn build_open_rfq(
    cranker: &Pubkey,
    meta: &DiscoveredVault,
    bucket: &Pubkey,
    auction_nonce: u64,
    slice_amount: u64,
    underlying_price: &Pubkey,
    settlement_price: &Pubkey,
) -> Instruction {
    ix::open_rfq(
        &ix::OpenRfq {
            cranker: *cranker,
            vault: meta.vault,
            bucket: *bucket,
            underlying_mint: meta.underlying_mint,
            settlement_mint: meta.settlement_mint,
            underlying_price: *underlying_price,
            settlement_price: *settlement_price,
            auction_nonce,
        },
        slice_amount,
    )
}

pub fn build_open_swap_rfq(
    cranker: &Pubkey,
    meta: &DiscoveredVault,
    auction_nonce: u64,
    amount_s: u64,
    underlying_price: &Pubkey,
    settlement_price: &Pubkey,
) -> Instruction {
    ix::open_swap_rfq(
        &ix::OpenRfq {
            cranker: *cranker,
            vault: meta.vault,
            // Ignored by open_swap_rfq (no bucket leg on a pure swap).
            bucket: meta.vault,
            underlying_mint: meta.underlying_mint,
            settlement_mint: meta.settlement_mint,
            underlying_price: *underlying_price,
            settlement_price: *settlement_price,
            auction_nonce,
        },
        amount_s,
    )
}

/// The keeper can't know at submit time whether the fresh-cross band
/// check will fill or veto, so when a standing bidder exists BOTH the
/// winner destination (escrowed settlement) and the refund ATA (their
/// underlying bid) are passed, created idempotently.
pub fn build_settle_swap_rfq(
    cranker: &Pubkey,
    meta: &DiscoveredVault,
    auction: &Pubkey,
    best_bidder: Option<Pubkey>,
    underlying_price: &Pubkey,
    settlement_price: &Pubkey,
) -> Vec<Instruction> {
    let mut ixs = Vec::new();
    let (winner_dest, bidder_refund) = match best_bidder {
        Some(bidder) => {
            ixs.push(create_ata_idempotent_ix(cranker, &bidder, &meta.settlement_mint));
            ixs.push(create_ata_idempotent_ix(cranker, &bidder, &meta.underlying_mint));
            (
                Some(pda::ata(&bidder, &meta.settlement_mint)),
                Some(pda::ata(&bidder, &meta.underlying_mint)),
            )
        }
        None => (None, None),
    };
    ixs.push(ix::settle_swap_rfq(&ix::SettleSwapRfq {
        cranker: *cranker,
        vault: meta.vault,
        auction: *auction,
        underlying_price: *underlying_price,
        settlement_price: *settlement_price,
        winner_dest,
        bidder_refund,
    }));
    ixs
}

pub fn build_finalize_round(
    cranker: &Pubkey,
    meta: &DiscoveredVault,
    round: u64,
    underlying_price: &Pubkey,
    settlement_price: &Pubkey,
) -> Vec<Instruction> {
    let core_treasury = pda::treasury(&options_core::ID);
    vec![
        // Fee destination: the treasury's underlying ATA must exist.
        create_ata_idempotent_ix(cranker, &core_treasury, &meta.underlying_mint),
        ix::finalize_round(&ix::FinalizeRound {
            cranker: *cranker,
            vault: meta.vault,
            round,
            core_treasury_token: pda::ata(&core_treasury, &meta.underlying_mint),
            underlying_price: *underlying_price,
            settlement_price: *settlement_price,
        }),
    ]
}

pub fn build_select_bucket(
    cranker: &Pubkey,
    meta: &DiscoveredVault,
    bucket: &Pubkey,
    underlying_price: &Pubkey,
    settlement_price: &Pubkey,
) -> Instruction {
    ix::select_bucket(cranker, &meta.vault, bucket, underlying_price, settlement_price)
}

// ── async execution over RPC ───────────────────────────────────────────

/// Everything needed to turn an [`Action`] for one vault into txs.
pub struct SubmitCtx<'a> {
    pub wrap: &'a SolanaClientWrapper,
    pub http: &'a reqwest::Client,
    pub poster: &'a mut PythPoster,
    pub meta: &'a DiscoveredVault,
}

impl SubmitCtx<'_> {
    fn cranker(&self) -> Pubkey {
        self.wrap.signer.pubkey()
    }

    /// The two persistent `PriceUpdateV2` accounts for this vault's feeds.
    fn price_accounts(&mut self) -> (Pubkey, Pubkey) {
        (
            self.poster.price_account(self.meta.underlying_feed),
            self.poster.price_account(self.meta.settlement_feed),
        )
    }

    async fn read_auction(&self, auction: &Pubkey) -> Result<Auction> {
        self.wrap
            .get_account_deserialized(auction)
            .await
            .with_context(|| format!("reading auction {auction}"))
    }

    /// Post fresh updates for both feeds, then send the crank (same tx
    /// when it fits, split otherwise).
    async fn send_gated(
        &mut self,
        crank_ixs: Vec<Instruction>,
        crank_signers: &[&Keypair],
        label: &str,
    ) -> Result<()> {
        let feeds = [self.meta.underlying_feed, self.meta.settlement_feed];
        let legs = self.poster.post_legs(self.http, &feeds).await?;
        send_oracle_gated(self.wrap, &legs, &crank_ixs, crank_signers, label).await
    }
}

/// Submit one action; callers only need the error for triage.
pub async fn execute(ctx: &mut SubmitCtx<'_>, view: &VaultView, action: &Action) -> Result<()> {
    let cranker = ctx.cranker();
    match action {
        Action::CrankRedeem { bucket } => {
            // The FIFO head's VaultPosition record carries the core
            // Position address.
            let vp_pda =
                pda::vault_position(&options_vault::ID, &ctx.meta.vault, view.positions_head);
            let vp: VaultPosition = ctx
                .wrap
                .get_account_deserialized(&vp_pda)
                .await
                .with_context(|| format!("reading vault position {}", view.positions_head))?;
            let ix = build_crank_redeem(&cranker, ctx.meta, bucket, view.positions_head, &vp.position);
            ctx.wrap.send_and_confirm(&[ix], &[], "vault::crank_redeem").await?;
        }
        Action::SettleRfq { auction, bucket } => {
            let live = ctx.read_auction(auction).await?;
            let (ixs, position) = build_settle_rfq(
                &cranker,
                ctx.meta,
                auction,
                live.best_token_recipient,
                view.positions_tail,
                bucket,
            );
            ctx.wrap.send_and_confirm(&ixs, &[&position], "vault::settle_rfq").await?;
        }
        Action::SettleRfqExpired { auction, bucket } => {
            let live = ctx.read_auction(auction).await?;
            let ixs = build_settle_rfq_expired(&cranker, ctx.meta, auction, bucket, live.best_bidder);
            ctx.wrap.send_and_confirm(&ixs, &[], "vault::settle_rfq_expired").await?;
        }
        Action::OpenRfq { bucket, slice_amount } => {
            let (u_price, s_price) = ctx.price_accounts();
            let ix = build_open_rfq(
                &cranker,
                ctx.meta,
                bucket,
                view.auction_nonce,
                *slice_amount,
                &u_price,
                &s_price,
            );
            ctx.send_gated(vec![ix], &[], "vault::open_rfq").await?;
        }
        Action::OpenSwapRfq { amount_s } => {
            let (u_price, s_price) = ctx.price_accounts();
            let ix = build_open_swap_rfq(
                &cranker,
                ctx.meta,
                view.auction_nonce,
                *amount_s,
                &u_price,
                &s_price,
            );
            ctx.send_gated(vec![ix], &[], "vault::open_swap_rfq").await?;
        }
        Action::SettleSwapRfq { auction } => {
            let live = ctx.read_auction(auction).await?;
            let (u_price, s_price) = ctx.price_accounts();
            let ixs = build_settle_swap_rfq(
                &cranker,
                ctx.meta,
                auction,
                live.best_bidder,
                &u_price,
                &s_price,
            );
            ctx.send_gated(ixs, &[], "vault::settle_swap_rfq").await?;
        }
        Action::FinalizeRound => {
            let (u_price, s_price) = ctx.price_accounts();
            let ixs = build_finalize_round(&cranker, ctx.meta, view.round, &u_price, &s_price);
            ctx.send_gated(ixs, &[], "vault::finalize_round").await?;
        }
        Action::SelectBucketNeeded | Action::Idle => {
            // Resolved by the tick loop before reaching here.
            return Err(anyhow!("{action:?} is not directly submittable"));
        }
    }
    tracing::info!(vault = %ctx.meta.vault, ?action, "action submitted");
    Ok(())
}

/// Submit `select_bucket` (the tick loop resolves the pick first).
pub async fn execute_select_bucket(ctx: &mut SubmitCtx<'_>, bucket: &Pubkey) -> Result<()> {
    let cranker = ctx.cranker();
    let (u_price, s_price) = ctx.price_accounts();
    let ix = build_select_bucket(&cranker, ctx.meta, bucket, &u_price, &s_price);
    ctx.send_gated(vec![ix], &[], "vault::select_bucket").await
}

// ── error triage ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Another keeper won the race or state moved under us — drop it,
    /// the next tick replans from fresh state.
    Benign,
    /// Transient (RPC / Hermes / stale oracle / blockhash) — next tick.
    Retry,
    /// Config or deployment bug — alert and halt this vault.
    Fatal,
}

/// Which of the three programs' error tables a failure text names. CPI
/// failures log `Program <id> failed` for the inner program; when no id
/// is present the keeper's own top-level program (options_vault) applies.
fn detect_program(text: &str) -> Program {
    // Scan for the LAST of our ids mentioned before a failure marker —
    // simple containment is enough: a venue/core id only appears in these
    // texts when its instruction ran (via CPI from the vault).
    let venue = auction_venue::ID.to_string();
    let core = options_core::ID.to_string();
    let vault = options_vault::ID.to_string();
    let pos = |needle: &str| text.rfind(&format!("Program {needle}"));
    let candidates = [
        (pos(&venue), Program::AuctionVenue),
        (pos(&core), Program::OptionsCore),
        (pos(&vault), Program::OptionsVault),
    ];
    candidates
        .into_iter()
        .filter_map(|(idx, p)| idx.map(|i| (i, p)))
        .max_by_key(|(i, _)| *i)
        .map(|(_, p)| p)
        .unwrap_or(Program::OptionsVault)
}

/// Classify a submission failure. Anchor custom codes go through
/// `solana-tx`'s per-program tables (no magic numbers — they're built
/// from the program crates' error enums); everything else — RPC
/// transport, expired blockhash, unhealthy node, Hermes — is Retry.
pub fn classify(err: &anyhow::Error) -> ErrorClass {
    let text = format!("{err:#}");
    if let Some(code) = solana_tx::extract_error_code(&text) {
        return match solana_tx::classify(detect_program(&text), code) {
            Classification::Benign => ErrorClass::Benign,
            Classification::Retry => ErrorClass::Retry,
            Classification::Fatal => ErrorClass::Fatal,
        };
    }
    ErrorClass::Retry
}

#[cfg(test)]
mod tests {
    use super::*;
    use auction_venue::error::VenueError;
    use options_vault::error::VaultError;
    use solana_tx::errors::{vault_code, venue_code};

    fn vault_revert(e: VaultError) -> anyhow::Error {
        anyhow!(
            "vault::crank failed: RPC response error -32002: Transaction simulation failed: \
             Error processing Instruction 1: custom program error: {:#x}",
            vault_code(e)
        )
    }

    #[test]
    fn classifies_vault_race_aborts_benign() {
        for e in [
            VaultError::WrongPhase,
            VaultError::BucketAlreadySelected,
            VaultError::RfqsOpen,
            VaultError::TooManyRfqs,
        ] {
            assert_eq!(classify(&vault_revert(e)), ErrorClass::Benign, "{e:?}");
        }
    }

    #[test]
    fn classifies_oracle_and_band_transients_retry() {
        for e in [
            VaultError::OraclePriceStale,
            VaultError::OracleConfidence,
            VaultError::StrikeOutOfBand,
            VaultError::SliceTooLarge,
        ] {
            assert_eq!(classify(&vault_revert(e)), ErrorClass::Retry, "{e:?}");
        }
    }

    #[test]
    fn classifies_config_families_fatal() {
        for e in [VaultError::OracleFeedMismatch, VaultError::ConfigInvalid] {
            assert_eq!(classify(&vault_revert(e)), ErrorClass::Fatal, "{e:?}");
        }
    }

    /// A CPI failure inside the venue names the venue program in the logs
    /// — its table applies, not the vault's.
    #[test]
    fn cpi_failures_classify_against_the_inner_program() {
        let text = format!(
            "vault::settle_rfq failed: Program {venue} invoke [2]\n\
             Program log: AnchorError occurred. Error Code: AuctionNotClosed. \
             Error Number: {code}. Error Message: auction not closed.\n\
             Program {venue} failed: custom program error: {code:#x}",
            venue = auction_venue::ID,
            code = venue_code(VenueError::AuctionNotClosed),
        );
        assert_eq!(classify(&anyhow!(text)), ErrorClass::Benign);

        let text = format!(
            "vault::settle_rfq failed: Program {venue} failed: custom program error: {:#x}",
            venue_code(VenueError::WrongSettleAuthority),
            venue = auction_venue::ID,
        );
        assert_eq!(classify(&anyhow!(text)), ErrorClass::Fatal);
    }

    /// Blockhash expiry, unhealthy nodes and transport errors carry no
    /// custom code — all Retry, per the keeper guide.
    #[test]
    fn non_code_failures_default_to_retry() {
        for msg in [
            "vault::open_rfq failed: Blockhash not found",
            "vault::open_rfq failed: Node is unhealthy",
            "fetching hermes update data: connection timed out",
            "vault::finalize_round failed: unable to confirm transaction",
        ] {
            assert_eq!(classify(&anyhow!(msg)), ErrorClass::Retry, "{msg}");
        }
    }

    #[test]
    fn unknown_custom_codes_default_to_retry() {
        let e = anyhow!("failed: custom program error: 0x1b57"); // 6999
        assert_eq!(classify(&e), ErrorClass::Retry);
    }

    #[test]
    fn ata_create_idempotent_shape() {
        let payer = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let ix = create_ata_idempotent_ix(&payer, &owner, &mint);
        assert_eq!(ix.program_id, anchor_spl::associated_token::ID);
        assert_eq!(ix.data, vec![1]);
        assert_eq!(ix.accounts.len(), 6);
        assert!(ix.accounts[0].is_signer && ix.accounts[0].is_writable);
        assert_eq!(ix.accounts[1].pubkey, pda::ata(&owner, &mint));
        assert!(ix.accounts[1].is_writable && !ix.accounts[1].is_signer);
        assert_eq!(ix.accounts[2].pubkey, owner);
        assert_eq!(ix.accounts[3].pubkey, mint);
    }
}
