//! Instruction builders for the three programs, using the crates'
//! generated `instruction` (data) and `accounts` (metas) modules — the
//! Anchor analog of sui-tx's PTB builders.
//!
//! Program ids come from the crates' `declare_id!` (deploy-stable; the
//! Anchor runtime rejects anything else). Every event-emitting instruction
//! carries the event-cpi pair (`event_authority` + `program`) — the
//! generated client structs declare them, we derive them here. PDAs that
//! are pure functions of the passed accounts (config, treasury, per-bucket
//! vault ATAs, nonce records, …) are derived internally via [`crate::pda`];
//! everything else is an explicit argument.

use anchor_lang::{InstructionData, ToAccountMetas};
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;

use options_core::quote::{FlowKind, Quote};
use options_vault::state::VaultConfig;

use crate::pda;

fn instructions_sysvar_id() -> Pubkey {
    solana_sdk_ids::sysvar::instructions::ID
}

// ─────────────────────────── options_core ───────────────────────────

/// `initialize`: create Config + Treasury; `admin` becomes the admin.
pub fn initialize(admin: &Pubkey) -> Instruction {
    let core = options_core::ID;
    Instruction::new_with_bytes(
        core,
        &options_core::instruction::Initialize {}.data(),
        options_core::accounts::Initialize {
            admin: *admin,
            config: pda::config(&core),
            treasury: pda::treasury(&core),
            system_program: anchor_lang::system_program::ID,
            event_authority: pda::event_authority(&core),
            program: core,
        }
        .to_account_metas(None),
    )
}

/// `create_account`: init the MmAccount PDA for `(owner, salt)`.
pub fn create_account(
    owner: &Pubkey,
    salt: u64,
    signing_scheme: u8,
    signing_pubkey: Vec<u8>,
) -> Instruction {
    let core = options_core::ID;
    Instruction::new_with_bytes(
        core,
        &options_core::instruction::CreateAccount {
            salt,
            signing_scheme,
            signing_pubkey,
        }
        .data(),
        options_core::accounts::CreateAccount {
            owner: *owner,
            mm_account: pda::mm_account(&core, owner, salt),
            system_program: anchor_lang::system_program::ID,
            event_authority: pda::event_authority(&core),
            program: core,
        }
        .to_account_metas(None),
    )
}

/// `account_deposit`: move `amount` of `mint` from `from_token` into the
/// MM account's ATA (created if needed, depositor pays).
pub fn account_deposit(
    depositor: &Pubkey,
    mm_account: &Pubkey,
    mint: &Pubkey,
    from_token: &Pubkey,
    amount: u64,
) -> Instruction {
    let core = options_core::ID;
    Instruction::new_with_bytes(
        core,
        &options_core::instruction::AccountDeposit { amount }.data(),
        options_core::accounts::DepositToAccount {
            depositor: *depositor,
            mm_account: *mm_account,
            mint: *mint,
            from_token: *from_token,
            account_token: pda::ata(mm_account, mint),
            token_program: anchor_spl::token::ID,
            associated_token_program: anchor_spl::associated_token::ID,
            system_program: anchor_lang::system_program::ID,
            event_authority: pda::event_authority(&core),
            program: core,
        }
        .to_account_metas(None),
    )
}

/// `account_withdraw`: owner-only withdrawal from the MM account's ATA.
pub fn account_withdraw(
    owner: &Pubkey,
    mm_account: &Pubkey,
    mint: &Pubkey,
    to_token: &Pubkey,
    amount: u64,
) -> Instruction {
    let core = options_core::ID;
    Instruction::new_with_bytes(
        core,
        &options_core::instruction::AccountWithdraw { amount }.data(),
        options_core::accounts::WithdrawFromAccount {
            owner: *owner,
            mm_account: *mm_account,
            mint: *mint,
            account_token: pda::ata(mm_account, mint),
            to_token: *to_token,
            token_program: anchor_spl::token::ID,
            event_authority: pda::event_authority(&core),
            program: core,
        }
        .to_account_metas(None),
    )
}

/// `create_bucket` (admin): covered-call bucket + call mint + both vaults.
pub fn create_bucket(
    admin: &Pubkey,
    underlying_mint: &Pubkey,
    settlement_mint: &Pubkey,
    salt: u64,
    expiry_ms: u64,
    strike: u128,
    strike_scale: u8,
) -> Instruction {
    let core = options_core::ID;
    let bucket = pda::bucket(&core, underlying_mint, settlement_mint, salt);
    Instruction::new_with_bytes(
        core,
        &options_core::instruction::CreateBucket {
            salt,
            expiry_ms,
            strike,
            strike_scale,
        }
        .data(),
        options_core::accounts::CreateBucket {
            admin: *admin,
            config: pda::config(&core),
            underlying_mint: *underlying_mint,
            settlement_mint: *settlement_mint,
            bucket,
            call_mint: pda::call_mint(&core, &bucket),
            underlying_vault: pda::ata(&bucket, underlying_mint),
            settlement_vault: pda::ata(&bucket, settlement_mint),
            token_program: anchor_spl::token::ID,
            associated_token_program: anchor_spl::associated_token::ID,
            system_program: anchor_lang::system_program::ID,
            event_authority: pda::event_authority(&core),
            program: core,
        }
        .to_account_metas(None),
    )
}

/// `create_put_bucket` (admin): cash-secured-put twin of [`create_bucket`].
pub fn create_put_bucket(
    admin: &Pubkey,
    underlying_mint: &Pubkey,
    settlement_mint: &Pubkey,
    salt: u64,
    expiry_ms: u64,
    strike: u128,
    strike_scale: u8,
) -> Instruction {
    let core = options_core::ID;
    let bucket = pda::put_bucket(&core, underlying_mint, settlement_mint, salt);
    Instruction::new_with_bytes(
        core,
        &options_core::instruction::CreatePutBucket {
            salt,
            expiry_ms,
            strike,
            strike_scale,
        }
        .data(),
        options_core::accounts::CreatePutBucket {
            admin: *admin,
            config: pda::config(&core),
            underlying_mint: *underlying_mint,
            settlement_mint: *settlement_mint,
            bucket,
            put_mint: pda::put_mint(&core, &bucket),
            underlying_vault: pda::ata(&bucket, underlying_mint),
            settlement_vault: pda::ata(&bucket, settlement_mint),
            token_program: anchor_spl::token::ID,
            associated_token_program: anchor_spl::associated_token::ID,
            system_program: anchor_lang::system_program::ID,
            event_authority: pda::event_authority(&core),
            program: core,
        }
        .to_account_metas(None),
    )
}

/// Accounts for [`write_collateralized`]. `position` is a fresh keypair's
/// pubkey — it must also sign the transaction (core `init`s it).
pub struct WriteCollateralized {
    pub payer: Pubkey,
    pub writer: Pubkey,
    pub bucket: Pubkey,
    pub underlying_mint: Pubkey,
    pub position: Pubkey,
    /// Writer's underlying source token account.
    pub writer_underlying: Pubkey,
    /// Destination of the minted call coins (any call-mint token account).
    pub call_dest: Pubkey,
}

/// `write_collateralized`: self-write — deposit underlying, mint calls.
pub fn write_collateralized(
    accounts: &WriteCollateralized,
    amount: u64,
    position_owner: Pubkey,
) -> Instruction {
    let core = options_core::ID;
    Instruction::new_with_bytes(
        core,
        &options_core::instruction::WriteCollateralized {
            amount,
            position_owner,
        }
        .data(),
        options_core::accounts::WriteCollateralized {
            payer: accounts.payer,
            writer: accounts.writer,
            bucket: accounts.bucket,
            position: accounts.position,
            writer_underlying: accounts.writer_underlying,
            underlying_vault: pda::ata(&accounts.bucket, &accounts.underlying_mint),
            call_mint: pda::call_mint(&core, &accounts.bucket),
            call_dest: accounts.call_dest,
            token_program: anchor_spl::token::ID,
            system_program: anchor_lang::system_program::ID,
            event_authority: pda::event_authority(&core),
            program: core,
        }
        .to_account_metas(None),
    )
}

/// Accounts for [`exercise`].
pub struct Exercise {
    pub exerciser: Pubkey,
    pub bucket: Pubkey,
    pub underlying_mint: Pubkey,
    pub settlement_mint: Pubkey,
    /// Exerciser's call-coin source (burned).
    pub exerciser_call: Pubkey,
    /// Exerciser's settlement source (pays `amount × strike`).
    pub exerciser_settlement: Pubkey,
    /// Exerciser's underlying destination.
    pub exerciser_underlying: Pubkey,
}

/// `exercise`: burn calls, pay strike, receive underlying.
pub fn exercise(accounts: &Exercise, amount: u64) -> Instruction {
    let core = options_core::ID;
    Instruction::new_with_bytes(
        core,
        &options_core::instruction::Exercise { amount }.data(),
        options_core::accounts::Exercise {
            exerciser: accounts.exerciser,
            bucket: accounts.bucket,
            call_mint: pda::call_mint(&core, &accounts.bucket),
            exerciser_call: accounts.exerciser_call,
            exerciser_settlement: accounts.exerciser_settlement,
            exerciser_underlying: accounts.exerciser_underlying,
            underlying_vault: pda::ata(&accounts.bucket, &accounts.underlying_mint),
            settlement_vault: pda::ata(&accounts.bucket, &accounts.settlement_mint),
            token_program: anchor_spl::token::ID,
            event_authority: pda::event_authority(&core),
            program: core,
        }
        .to_account_metas(None),
    )
}

/// Accounts for [`redeem_position`].
pub struct RedeemPosition {
    pub redeemer: Pubkey,
    pub bucket: Pubkey,
    pub underlying_mint: Pubkey,
    pub settlement_mint: Pubkey,
    pub position: Pubkey,
    pub redeemer_underlying: Pubkey,
    pub redeemer_settlement: Pubkey,
}

/// `redeem_position`: writer collects the mixed underlying/settlement
/// outcome after expiry; the Position closes, rent to the redeemer.
pub fn redeem_position(accounts: &RedeemPosition) -> Instruction {
    let core = options_core::ID;
    Instruction::new_with_bytes(
        core,
        &options_core::instruction::RedeemPosition {}.data(),
        options_core::accounts::RedeemPosition {
            redeemer: accounts.redeemer,
            bucket: accounts.bucket,
            position: accounts.position,
            redeemer_underlying: accounts.redeemer_underlying,
            redeemer_settlement: accounts.redeemer_settlement,
            underlying_vault: pda::ata(&accounts.bucket, &accounts.underlying_mint),
            settlement_vault: pda::ata(&accounts.bucket, &accounts.settlement_mint),
            token_program: anchor_spl::token::ID,
            event_authority: pda::event_authority(&core),
            program: core,
        }
        .to_account_metas(None),
    )
}

/// Accounts for [`execute_write`]. `position` is a fresh keypair pubkey
/// (co-signer). Exactly one of `mm_underlying` (Trader flow) /
/// `executor_underlying` (Writer flow) is `Some`, matching the handler.
pub struct ExecuteWrite {
    pub executor: Pubkey,
    pub bucket: Pubkey,
    pub underlying_mint: Pubkey,
    pub settlement_mint: Pubkey,
    /// Destination of the minted call coins.
    pub call_dest: Pubkey,
    pub mm_account: Pubkey,
    /// Trader flow only: MM account's underlying ATA (collateral source).
    pub mm_underlying: Option<Pubkey>,
    /// Writer flow only: executor's underlying source.
    pub executor_underlying: Option<Pubkey>,
    /// Writer flow: net-premium destination. Trader flow: premium source.
    pub executor_settlement: Pubkey,
    pub position: Pubkey,
}

/// `execute_write`: execute an MM-signed quote. The transaction must also
/// carry the Ed25519 precompile instruction at `sig_ix_index` (see
/// [`crate::quote::ed25519_verify_ix`]).
pub fn execute_write(
    accounts: &ExecuteWrite,
    quote: Quote,
    flow: FlowKind,
    position_recipient: Pubkey,
    sig_ix_index: u8,
) -> Instruction {
    let core = options_core::ID;
    let treasury = pda::treasury(&core);
    let nonce_record = pda::nonce_record(&core, &accounts.mm_account, quote.nonce);
    Instruction::new_with_bytes(
        core,
        &options_core::instruction::ExecuteWrite {
            quote,
            flow,
            position_recipient,
            sig_ix_index,
        }
        .data(),
        options_core::accounts::ExecuteWrite {
            executor: accounts.executor,
            config: pda::config(&core),
            treasury,
            bucket: accounts.bucket,
            settlement_mint: accounts.settlement_mint,
            underlying_vault: pda::ata(&accounts.bucket, &accounts.underlying_mint),
            call_mint: pda::call_mint(&core, &accounts.bucket),
            call_dest: accounts.call_dest,
            mm_account: accounts.mm_account,
            mm_settlement: pda::ata(&accounts.mm_account, &accounts.settlement_mint),
            mm_underlying: accounts.mm_underlying,
            executor_underlying: accounts.executor_underlying,
            executor_settlement: accounts.executor_settlement,
            treasury_settlement: pda::ata(&treasury, &accounts.settlement_mint),
            position: accounts.position,
            nonce_record,
            instructions_sysvar: instructions_sysvar_id(),
            token_program: anchor_spl::token::ID,
            associated_token_program: anchor_spl::associated_token::ID,
            system_program: anchor_lang::system_program::ID,
            event_authority: pda::event_authority(&core),
            program: core,
        }
        .to_account_metas(None),
    )
}

// ─────────────────────────── auction_venue ───────────────────────────

/// `bid`: escrowed ascending bid. `previous_bidder_refund` is the standing
/// best bidder's bid-mint ATA — required whenever a best bid exists.
pub fn bid(
    bidder: &Pubkey,
    auction: &Pubkey,
    bidder_source: &Pubkey,
    previous_bidder_refund: Option<Pubkey>,
    amount: u64,
    token_recipient: Pubkey,
) -> Instruction {
    let venue = auction_venue::ID;
    Instruction::new_with_bytes(
        venue,
        &auction_venue::instruction::Bid {
            amount,
            token_recipient,
        }
        .data(),
        auction_venue::accounts::Bid {
            bidder: *bidder,
            auction: *auction,
            bid_vault: pda::bid_vault(&venue, auction),
            bidder_source: *bidder_source,
            previous_bidder_refund,
            token_program: anchor_spl::token::ID,
            event_authority: pda::event_authority(&venue),
            program: venue,
        }
        .to_account_metas(None),
    )
}

/// Accounts for [`settle_call`] / [`settle_put`] — the venue's option
/// adapters CPI options_core, so the core pass-through accounts ride
/// along. `position` is a fresh keypair pubkey (co-signer).
pub struct SettleOption {
    pub cranker: Pubkey,
    /// Rent destination for the closed auction accounts (auction.creator).
    pub creator_wallet: Pubkey,
    pub auction: Pubkey,
    /// The coupled settle authority (must sign) — `None` for
    /// permissionless auctions.
    pub authority: Option<Pubkey>,
    pub proceeds_token: Pubkey,
    pub refund_token: Pubkey,
    pub bucket: Pubkey,
    pub position: Pubkey,
    /// Calls: the bucket's underlying vault. Puts: its settlement vault.
    pub bucket_vault: Pubkey,
    /// Calls: the bucket's call mint. Puts: its put mint.
    pub option_mint: Pubkey,
    /// Winner's option-coin destination.
    pub option_dest: Pubkey,
    /// Core treasury's ATA for the bid mint (fee destination).
    pub core_treasury_token: Pubkey,
}

/// `settle_call`: settle a covered-call auction (CPIs
/// `options_core::write_collateralized` when there is a winner).
pub fn settle_call(accounts: &SettleOption) -> Instruction {
    let venue = auction_venue::ID;
    let core = options_core::ID;
    Instruction::new_with_bytes(
        venue,
        &auction_venue::instruction::SettleCall {}.data(),
        auction_venue::accounts::SettleCall {
            cranker: accounts.cranker,
            creator_wallet: accounts.creator_wallet,
            auction: accounts.auction,
            escrow_vault: pda::escrow_vault(&venue, &accounts.auction),
            bid_vault: pda::bid_vault(&venue, &accounts.auction),
            authority: accounts.authority,
            proceeds_token: accounts.proceeds_token,
            refund_token: accounts.refund_token,
            bucket: accounts.bucket,
            position: accounts.position,
            underlying_vault: accounts.bucket_vault,
            call_mint: accounts.option_mint,
            call_dest: accounts.option_dest,
            core_config: pda::config(&core),
            core_treasury_token: accounts.core_treasury_token,
            core_event_authority_acc: pda::event_authority(&core),
            core_program: core,
            token_program: anchor_spl::token::ID,
            system_program: anchor_lang::system_program::ID,
            event_authority: pda::event_authority(&venue),
            program: venue,
        }
        .to_account_metas(None),
    )
}

/// `settle_put`: settle a cash-secured-put auction.
pub fn settle_put(accounts: &SettleOption) -> Instruction {
    let venue = auction_venue::ID;
    let core = options_core::ID;
    Instruction::new_with_bytes(
        venue,
        &auction_venue::instruction::SettlePut {}.data(),
        auction_venue::accounts::SettlePut {
            cranker: accounts.cranker,
            creator_wallet: accounts.creator_wallet,
            auction: accounts.auction,
            escrow_vault: pda::escrow_vault(&venue, &accounts.auction),
            bid_vault: pda::bid_vault(&venue, &accounts.auction),
            authority: accounts.authority,
            proceeds_token: accounts.proceeds_token,
            refund_token: accounts.refund_token,
            bucket: accounts.bucket,
            position: accounts.position,
            settlement_vault: accounts.bucket_vault,
            put_mint: accounts.option_mint,
            put_dest: accounts.option_dest,
            core_config: pda::config(&core),
            core_treasury_token: accounts.core_treasury_token,
            core_event_authority_acc: pda::event_authority(&core),
            core_program: core,
            token_program: anchor_spl::token::ID,
            system_program: anchor_lang::system_program::ID,
            event_authority: pda::event_authority(&venue),
            program: venue,
        }
        .to_account_metas(None),
    )
}

/// Accounts for [`settle_swap`].
pub struct SettleSwap {
    pub cranker: Pubkey,
    pub creator_wallet: Pubkey,
    pub auction: Pubkey,
    pub authority: Option<Pubkey>,
    /// Winner's escrow destination (fill path).
    pub winner_dest: Option<Pubkey>,
    /// Standing bidder's refund ATA (force_refund path).
    pub bidder_refund: Option<Pubkey>,
    pub proceeds_token: Pubkey,
    pub refund_token: Pubkey,
}

/// `settle_swap`: settle a pure token-for-token auction.
pub fn settle_swap(accounts: &SettleSwap, force_refund: bool) -> Instruction {
    let venue = auction_venue::ID;
    Instruction::new_with_bytes(
        venue,
        &auction_venue::instruction::SettleSwap { force_refund }.data(),
        auction_venue::accounts::SettleSwap {
            cranker: accounts.cranker,
            creator_wallet: accounts.creator_wallet,
            auction: accounts.auction,
            escrow_vault: pda::escrow_vault(&venue, &accounts.auction),
            bid_vault: pda::bid_vault(&venue, &accounts.auction),
            authority: accounts.authority,
            winner_dest: accounts.winner_dest,
            bidder_refund: accounts.bidder_refund,
            proceeds_token: accounts.proceeds_token,
            refund_token: accounts.refund_token,
            token_program: anchor_spl::token::ID,
            event_authority: pda::event_authority(&venue),
            program: venue,
        }
        .to_account_metas(None),
    )
}

/// Accounts for [`settle_expired`].
pub struct SettleExpired {
    pub cranker: Pubkey,
    pub creator_wallet: Pubkey,
    pub auction: Pubkey,
    pub authority: Option<Pubkey>,
    pub bucket: Pubkey,
    /// Standing bidder's refund ATA, when a best bid exists.
    pub bidder_refund: Option<Pubkey>,
    pub refund_token: Pubkey,
}

/// `settle_expired`: recover escrow + refund the standing bid after the
/// bucket expired under an unsettled auction.
pub fn settle_expired(accounts: &SettleExpired) -> Instruction {
    let venue = auction_venue::ID;
    Instruction::new_with_bytes(
        venue,
        &auction_venue::instruction::SettleExpired {}.data(),
        auction_venue::accounts::SettleExpired {
            cranker: accounts.cranker,
            creator_wallet: accounts.creator_wallet,
            auction: accounts.auction,
            escrow_vault: pda::escrow_vault(&venue, &accounts.auction),
            bid_vault: pda::bid_vault(&venue, &accounts.auction),
            authority: accounts.authority,
            bucket: accounts.bucket,
            bidder_refund: accounts.bidder_refund,
            refund_token: accounts.refund_token,
            token_program: anchor_spl::token::ID,
            event_authority: pda::event_authority(&venue),
            program: venue,
        }
        .to_account_metas(None),
    )
}

// ─────────────────────────── options_vault ───────────────────────────

/// `create_vault` (admin): vault PDA + share mint + the six PDA-seeded
/// token accounts.
pub fn create_vault(
    admin: &Pubkey,
    underlying_mint: &Pubkey,
    settlement_mint: &Pubkey,
    salt: u64,
    config: VaultConfig,
) -> Instruction {
    let vp = options_vault::ID;
    let vault = pda::vault(&vp, underlying_mint, settlement_mint, salt);
    Instruction::new_with_bytes(
        vp,
        &options_vault::instruction::CreateVault { salt, config }.data(),
        options_vault::accounts::CreateVault {
            admin: *admin,
            underlying_mint: *underlying_mint,
            settlement_mint: *settlement_mint,
            vault,
            share_mint: pda::share_mint(&vp, &vault),
            deployable: pda::vault_deployable(&vp, &vault),
            pending: pda::vault_pending(&vp, &vault),
            proceeds: pda::vault_proceeds(&vp, &vault),
            withdrawal_pool: pda::vault_withdrawal_pool(&vp, &vault),
            claimable_shares: pda::vault_claimable_shares(&vp, &vault),
            queued_shares: pda::vault_queued_shares(&vp, &vault),
            token_program: anchor_spl::token::ID,
            system_program: anchor_lang::system_program::ID,
            event_authority: pda::event_authority(&vp),
            program: vp,
        }
        .to_account_metas(None),
    )
}

/// `select_bucket` (crank): pin this round's bucket, oracle-banded.
pub fn select_bucket(
    cranker: &Pubkey,
    vault: &Pubkey,
    bucket: &Pubkey,
    underlying_price: &Pubkey,
    settlement_price: &Pubkey,
) -> Instruction {
    let vp = options_vault::ID;
    Instruction::new_with_bytes(
        vp,
        &options_vault::instruction::SelectBucket {}.data(),
        options_vault::accounts::SelectBucket {
            cranker: *cranker,
            vault: *vault,
            bucket: *bucket,
            underlying_price: *underlying_price,
            settlement_price: *settlement_price,
            event_authority: pda::event_authority(&vp),
            program: vp,
        }
        .to_account_metas(None),
    )
}

/// Accounts for [`crank_redeem`].
pub struct CrankRedeem {
    pub cranker: Pubkey,
    pub vault: Pubkey,
    pub bucket: Pubkey,
    /// `vault.positions_head` — seeds the FIFO-head VaultPosition PDA.
    pub positions_head: u64,
    /// The core Position at the FIFO head (from the VaultPosition record).
    pub position: Pubkey,
    pub bucket_underlying_vault: Pubkey,
    pub bucket_settlement_vault: Pubkey,
}

/// `crank_redeem`: redeem the FIFO-head position back into the vault.
pub fn crank_redeem(accounts: &CrankRedeem) -> Instruction {
    let vp = options_vault::ID;
    let core = options_core::ID;
    Instruction::new_with_bytes(
        vp,
        &options_vault::instruction::CrankRedeem {}.data(),
        options_vault::accounts::CrankRedeem {
            cranker: accounts.cranker,
            vault: accounts.vault,
            bucket: accounts.bucket,
            vault_position: pda::vault_position(&vp, &accounts.vault, accounts.positions_head),
            position: accounts.position,
            deployable: pda::vault_deployable(&vp, &accounts.vault),
            proceeds: pda::vault_proceeds(&vp, &accounts.vault),
            bucket_underlying_vault: accounts.bucket_underlying_vault,
            bucket_settlement_vault: accounts.bucket_settlement_vault,
            core_event_authority: pda::event_authority(&core),
            core_program: core,
            token_program: anchor_spl::token::ID,
            event_authority: pda::event_authority(&vp),
            program: vp,
        }
        .to_account_metas(None),
    )
}

/// Accounts for [`open_rfq`] / [`open_swap_rfq`].
pub struct OpenRfq {
    pub cranker: Pubkey,
    pub vault: Pubkey,
    /// The selected bucket — ignored by `open_swap_rfq`.
    pub bucket: Pubkey,
    pub underlying_mint: Pubkey,
    pub settlement_mint: Pubkey,
    pub underlying_price: Pubkey,
    pub settlement_price: Pubkey,
    /// `vault.auction_nonce` — seeds the CPI-created auction PDA.
    pub auction_nonce: u64,
}

/// `open_rfq` (crank): open a coupled covered-call slice auction.
pub fn open_rfq(accounts: &OpenRfq, slice_amount: u64) -> Instruction {
    let vp = options_vault::ID;
    let venue = auction_venue::ID;
    let auction = pda::auction(&venue, &accounts.vault, accounts.auction_nonce);
    Instruction::new_with_bytes(
        vp,
        &options_vault::instruction::OpenRfq { slice_amount }.data(),
        options_vault::accounts::OpenRfq {
            cranker: accounts.cranker,
            vault: accounts.vault,
            bucket: accounts.bucket,
            underlying_mint: accounts.underlying_mint,
            settlement_mint: accounts.settlement_mint,
            deployable: pda::vault_deployable(&vp, &accounts.vault),
            proceeds: pda::vault_proceeds(&vp, &accounts.vault),
            underlying_price: accounts.underlying_price,
            settlement_price: accounts.settlement_price,
            auction,
            escrow_vault: pda::escrow_vault(&venue, &auction),
            bid_vault: pda::bid_vault(&venue, &auction),
            venue_event_authority: pda::event_authority(&venue),
            venue_program: venue,
            token_program: anchor_spl::token::ID,
            system_program: anchor_lang::system_program::ID,
            event_authority: pda::event_authority(&vp),
            program: vp,
        }
        .to_account_metas(None),
    )
}

/// `open_swap_rfq` (crank): auction settlement proceeds back to underlying.
pub fn open_swap_rfq(accounts: &OpenRfq, amount_s: u64) -> Instruction {
    let vp = options_vault::ID;
    let venue = auction_venue::ID;
    let auction = pda::auction(&venue, &accounts.vault, accounts.auction_nonce);
    Instruction::new_with_bytes(
        vp,
        &options_vault::instruction::OpenSwapRfq { amount_s }.data(),
        options_vault::accounts::OpenSwapRfq {
            cranker: accounts.cranker,
            vault: accounts.vault,
            underlying_mint: accounts.underlying_mint,
            settlement_mint: accounts.settlement_mint,
            deployable: pda::vault_deployable(&vp, &accounts.vault),
            proceeds: pda::vault_proceeds(&vp, &accounts.vault),
            underlying_price: accounts.underlying_price,
            settlement_price: accounts.settlement_price,
            auction,
            escrow_vault: pda::escrow_vault(&venue, &auction),
            bid_vault: pda::bid_vault(&venue, &auction),
            venue_event_authority: pda::event_authority(&venue),
            venue_program: venue,
            token_program: anchor_spl::token::ID,
            system_program: anchor_lang::system_program::ID,
            event_authority: pda::event_authority(&vp),
            program: vp,
        }
        .to_account_metas(None),
    )
}

/// Accounts for [`settle_rfq`].
pub struct SettleRfq {
    pub cranker: Pubkey,
    pub vault: Pubkey,
    pub auction: Pubkey,
    /// `vault.positions_tail` — seeds the FIFO-tail VaultPosition PDA.
    pub positions_tail: u64,
    pub bucket: Pubkey,
    /// Fresh keypair pubkey for the core Position (co-signer).
    pub position: Pubkey,
    pub bucket_underlying_vault: Pubkey,
    pub call_mint: Pubkey,
    /// Winner's call-coin destination (venue-verified ownership).
    pub call_dest: Pubkey,
    /// Core treasury's ATA for the bid mint (fee destination).
    pub core_treasury_token: Pubkey,
}

/// `settle_rfq` (crank): settle a coupled call auction into the vault.
pub fn settle_rfq(accounts: &SettleRfq) -> Instruction {
    let vp = options_vault::ID;
    let venue = auction_venue::ID;
    let core = options_core::ID;
    Instruction::new_with_bytes(
        vp,
        &options_vault::instruction::SettleRfq {}.data(),
        options_vault::accounts::SettleRfq {
            cranker: accounts.cranker,
            vault: accounts.vault,
            auction: accounts.auction,
            escrow_vault: pda::escrow_vault(&venue, &accounts.auction),
            bid_vault: pda::bid_vault(&venue, &accounts.auction),
            deployable: pda::vault_deployable(&vp, &accounts.vault),
            proceeds: pda::vault_proceeds(&vp, &accounts.vault),
            vault_position: pda::vault_position(&vp, &accounts.vault, accounts.positions_tail),
            bucket: accounts.bucket,
            position: accounts.position,
            bucket_underlying_vault: accounts.bucket_underlying_vault,
            call_mint: accounts.call_mint,
            call_dest: accounts.call_dest,
            core_config: pda::config(&core),
            core_treasury_token: accounts.core_treasury_token,
            core_event_authority: pda::event_authority(&core),
            core_program: core,
            venue_event_authority: pda::event_authority(&venue),
            venue_program: venue,
            token_program: anchor_spl::token::ID,
            system_program: anchor_lang::system_program::ID,
            event_authority: pda::event_authority(&vp),
            program: vp,
        }
        .to_account_metas(None),
    )
}

/// `settle_rfq_expired` (crank): recover a coupled call auction whose
/// bucket expired unsettled. `bidder_refund` when a best bid stands.
pub fn settle_rfq_expired(
    cranker: &Pubkey,
    vault: &Pubkey,
    auction: &Pubkey,
    bucket: &Pubkey,
    bidder_refund: Option<Pubkey>,
) -> Instruction {
    let vp = options_vault::ID;
    let venue = auction_venue::ID;
    Instruction::new_with_bytes(
        vp,
        &options_vault::instruction::SettleRfqExpired {}.data(),
        options_vault::accounts::SettleRfqExpired {
            cranker: *cranker,
            vault: *vault,
            auction: *auction,
            escrow_vault: pda::escrow_vault(&venue, auction),
            bid_vault: pda::bid_vault(&venue, auction),
            bucket: *bucket,
            bidder_refund,
            deployable: pda::vault_deployable(&vp, vault),
            venue_event_authority: pda::event_authority(&venue),
            venue_program: venue,
            token_program: anchor_spl::token::ID,
            event_authority: pda::event_authority(&vp),
            program: vp,
        }
        .to_account_metas(None),
    )
}

/// Accounts for [`settle_swap_rfq`].
pub struct SettleSwapRfq {
    pub cranker: Pubkey,
    pub vault: Pubkey,
    pub auction: Pubkey,
    pub underlying_price: Pubkey,
    pub settlement_price: Pubkey,
    /// Winner's settlement destination (fill path).
    pub winner_dest: Option<Pubkey>,
    /// Standing bidder's refund ATA (refund path).
    pub bidder_refund: Option<Pubkey>,
}

/// `settle_swap_rfq` (crank): absorb a proceeds-swap auction's outcome.
pub fn settle_swap_rfq(accounts: &SettleSwapRfq) -> Instruction {
    let vp = options_vault::ID;
    let venue = auction_venue::ID;
    Instruction::new_with_bytes(
        vp,
        &options_vault::instruction::SettleSwapRfq {}.data(),
        options_vault::accounts::SettleSwapRfq {
            cranker: accounts.cranker,
            vault: accounts.vault,
            auction: accounts.auction,
            escrow_vault: pda::escrow_vault(&venue, &accounts.auction),
            bid_vault: pda::bid_vault(&venue, &accounts.auction),
            deployable: pda::vault_deployable(&vp, &accounts.vault),
            proceeds: pda::vault_proceeds(&vp, &accounts.vault),
            underlying_price: accounts.underlying_price,
            settlement_price: accounts.settlement_price,
            winner_dest: accounts.winner_dest,
            bidder_refund: accounts.bidder_refund,
            venue_event_authority: pda::event_authority(&venue),
            venue_program: venue,
            token_program: anchor_spl::token::ID,
            event_authority: pda::event_authority(&vp),
            program: vp,
        }
        .to_account_metas(None),
    )
}

/// Accounts for [`finalize_round`].
pub struct FinalizeRound {
    pub cranker: Pubkey,
    pub vault: Pubkey,
    /// `vault.round` — seeds this round's RoundState PDA; rounds > 0 also
    /// pass `pps[round − 1]` (derived here when `round > 0`).
    pub round: u64,
    /// Core treasury's underlying token account (fee destination).
    pub core_treasury_token: Pubkey,
    pub underlying_price: Pubkey,
    pub settlement_price: Pubkey,
}

/// `finalize_round` (crank): set pps[round], charge fees, roll the round.
pub fn finalize_round(accounts: &FinalizeRound) -> Instruction {
    let vp = options_vault::ID;
    let vault = &accounts.vault;
    let prev_round_state = accounts
        .round
        .checked_sub(1)
        .map(|prev| pda::round_state(&vp, vault, prev));
    Instruction::new_with_bytes(
        vp,
        &options_vault::instruction::FinalizeRound {}.data(),
        options_vault::accounts::FinalizeRound {
            cranker: accounts.cranker,
            vault: *vault,
            share_mint: pda::share_mint(&vp, vault),
            deployable: pda::vault_deployable(&vp, vault),
            pending: pda::vault_pending(&vp, vault),
            proceeds: pda::vault_proceeds(&vp, vault),
            withdrawal_pool: pda::vault_withdrawal_pool(&vp, vault),
            claimable_shares: pda::vault_claimable_shares(&vp, vault),
            queued_shares: pda::vault_queued_shares(&vp, vault),
            round_state: pda::round_state(&vp, vault, accounts.round),
            prev_round_state,
            core_treasury_token: accounts.core_treasury_token,
            underlying_price: accounts.underlying_price,
            settlement_price: accounts.settlement_price,
            token_program: anchor_spl::token::ID,
            system_program: anchor_lang::system_program::ID,
            event_authority: pda::event_authority(&vp),
            program: vp,
        }
        .to_account_metas(None),
    )
}
