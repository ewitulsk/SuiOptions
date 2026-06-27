module options_protocol::events;

use std::type_name::TypeName;
use sui::event;

public struct BucketCreated has copy, drop {
    bucket_id: ID,
    asset_type: TypeName,
    settlement_type: TypeName,
    /// Fully-qualified type of the per-bucket option coin (`Coin<call_type>`).
    call_type: TypeName,
    expiry_ms: u64,
    /// See `bucket::Bucket.strike` — actual ratio is
    /// `strike / 10^strike_scale`.
    strike: u128,
    strike_scale: u8,
}

public struct WriteExecuted has copy, drop {
    bucket_id: ID,
    signer_account_id: ID,
    signer_token_recipient: address,
    executor: address,
    position_id: ID,
    position_recipient: address,
    call_token_recipient: address,
    write_amount: u64,
    gross_premium: u64,
    fee: u64,
    net_premium: u64,
    range_start: u128,
    range_end: u128,
    nonce: u64,
}

/// Emitted by `bucket::write_collateralized` (self-writes / venue escrow
/// writes). Deliberately distinct from `WriteExecuted`: it has no premium
/// and no signer — the indexer treats it as a new event type, existing
/// consumers of `WriteExecuted` are unaffected.
public struct CollateralizedWrite has copy, drop {
    bucket_id: ID,
    /// Tx sender (the venue or self-writer).
    writer: address,
    amount: u64,
    range_start: u128,
    range_end: u128,
}

public struct Exercised has copy, drop {
    bucket_id: ID,
    exerciser: address,
    amount: u64,
    settlement_paid: u64,
    cursor_after: u128,
}

public struct Redeemed has copy, drop {
    bucket_id: ID,
    position_id: ID,
    redeemer: address,
    range_start: u128,
    range_end: u128,
    underlying_returned: u64,
    settlement_returned: u64,
}

public struct ExpiredOptionBurned has copy, drop {
    bucket_id: ID,
    burner: address,
    amount: u64,
}

public struct BucketCleaned has copy, drop {
    bucket_id: ID,
}

public struct BucketInvalidated has copy, drop {
    bucket_id: ID,
    at_ms: u64,
    admin: address,
    reason: vector<u8>,
}

public struct BucketRevalidated has copy, drop {
    bucket_id: ID,
    at_ms: u64,
    admin: address,
    reason: vector<u8>,
}

public struct RfqCreated has copy, drop {
    rfq_id: ID,
    bucket_id: ID,
    /// Originating object (vault ID, or seller address-as-ID for
    /// standalone use). Indexing/attribution only.
    origin: ID,
    amount: u64,
    reserve_premium: u64,
    deadline_ms: u64,
    max_deadline_ms: u64,
    min_increment_bps: u64,
}

public struct RfqBid has copy, drop {
    rfq_id: ID,
    bidder: address,
    call_recipient: address,
    premium: u64,
    /// 0 if this is the first bid.
    previous_premium: u64,
    /// Post-anti-snipe deadline.
    new_deadline_ms: u64,
}

/// Mirrors `WriteExecuted`'s economic fields so the indexer's positions
/// materializer can treat both as "a position was minted with premium X".
public struct RfqSettled has copy, drop {
    rfq_id: ID,
    bucket_id: ID,
    origin: ID,
    winner: address,
    call_recipient: address,
    position_id: ID,
    position_recipient: address,
    amount: u64,
    gross_premium: u64,
    fee: u64,
    net_premium: u64,
    range_start: u128,
    range_end: u128,
}

/// Emitted when an auction resolves without a write: no bids, or the
/// bucket expired/was invalidated mid-auction (both escrows refunded).
public struct RfqExpiredUnsold has copy, drop {
    rfq_id: ID,
    bucket_id: ID,
    origin: ID,
    amount: u64,
    reserve_premium: u64,
}

// ---- proceeds-swap auction (swap_auction.move) ----

/// A vault opened a swap auction: `amount_s` settlement escrowed, bids
/// (in underlying) must clear `reserve_underlying`.
public struct SwapRfqCreated has copy, drop {
    swap_id: ID,
    /// Originating vault ID.
    origin: ID,
    amount_s: u64,
    reserve_underlying: u64,
    deadline_ms: u64,
    max_deadline_ms: u64,
    min_increment_bps: u64,
}

public struct SwapRfqBid has copy, drop {
    swap_id: ID,
    bidder: address,
    /// Underlying offered by this bid.
    underlying: u64,
    /// 0 if this is the first bid.
    previous_underlying: u64,
    /// Post-anti-snipe deadline.
    new_deadline_ms: u64,
}

/// A swap auction filled: the winner took `settlement_filled` settlement
/// for `underlying_in` underlying, which cleared the fresh-Pyth band.
/// `vault_id == origin`; carries `round` for the round-economics
/// materializer (the realized swap rate feeds the perf-fee conversion).
public struct SwapRfqSettled has copy, drop {
    swap_id: ID,
    vault_id: ID,
    round: u64,
    winner: address,
    settlement_filled: u64,
    underlying_in: u64,
}

/// A swap auction closed without converting: no bids, or the best bid
/// fell out of the Pyth band before settle (price moved). The settlement
/// returns to the vault's proceeds for re-auction.
public struct SwapRfqUnfilled has copy, drop {
    swap_id: ID,
    vault_id: ID,
    round: u64,
    amount_s: u64,
}

public struct VaultCreated has copy, drop {
    vault_id: ID,
    underlying_type: TypeName,
    settlement_type: TypeName,
    share_type: TypeName,
    // Active config at genesis (the consumer-facing subset; see
    // `VaultConfigApplied` for updates). Off-chain services read these so
    // they never hard-code fees/strike-band/cadence.
    mgmt_fee_bps_annual: u64,
    perf_fee_bps: u64,
    round_ms: u64,
    selling_window_ms: u64,
    min_strike_bps_over_spot: u64,
    max_strike_bps_over_spot: u64,
}

/// The active config snapshot at a finalize boundary. Emitted every finalize
/// (config is pending-then-applied, so this records what the round actually
/// ran with — robust even if `VaultCreated` was missed). Indexer upserts the
/// current snapshot from these.
public struct VaultConfigApplied has copy, drop {
    vault_id: ID,
    round: u64,
    mgmt_fee_bps_annual: u64,
    perf_fee_bps: u64,
    round_ms: u64,
    selling_window_ms: u64,
    min_strike_bps_over_spot: u64,
    max_strike_bps_over_spot: u64,
}

public struct VaultDeposit has copy, drop {
    vault_id: ID,
    depositor: address,
    /// The round the deposit participates from (receipt round).
    round: u64,
    amount: u64,
}

public struct SharesClaimed has copy, drop {
    vault_id: ID,
    owner: address,
    round: u64,
    amount: u64,
    shares: u64,
}

public struct WithdrawInitiated has copy, drop {
    vault_id: ID,
    owner: address,
    /// The round the withdrawal settles with (receipt round).
    round: u64,
    shares: u64,
}

public struct WithdrawCompleted has copy, drop {
    vault_id: ID,
    owner: address,
    round: u64,
    shares: u64,
    amount: u64,
}

public struct InstantWithdraw has copy, drop {
    vault_id: ID,
    owner: address,
    round: u64,
    amount: u64,
}

public struct VaultBucketSelected has copy, drop {
    vault_id: ID,
    round: u64,
    bucket_id: ID,
    strike: u128,
    strike_scale: u8,
    expiry_ms: u64,
    selling_ends_ms: u64,
    /// Pyth cross at selection (oracle scale).
    spot: u128,
    spot_scale: u8,
}

public struct VaultPositionRedeemed has copy, drop {
    vault_id: ID,
    round: u64,
    position_id: ID,
    underlying_returned: u64,
    settlement_returned: u64,
}

public struct VaultFeesCharged has copy, drop {
    vault_id: ID,
    round: u64,
    mgmt_fee: u64,
    perf_fee: u64,
}

public struct VaultRoundFinalized has copy, drop {
    vault_id: ID,
    /// The round that was finalized (the pps index).
    round: u64,
    pps: u128,
    aum: u64,
    /// Live shares the round's P&L accrued to (supply + queued).
    shares: u64,
    premium_collected: u64,
    premium_underlying: u64,
    withdrawals_owed: u64,
    shares_burned: u64,
    deposits_processed: u64,
    shares_minted: u64,
}

public struct VaultConfigUpdated has copy, drop {
    vault_id: ID,
    /// Configs apply at the next finalize; this is the current round.
    round: u64,
}

public struct VaultDepositsPaused has copy, drop {
    vault_id: ID,
    paused: bool,
}

// ---- cash-secured puts (put_bucket.move / rfq_put.move) ----
//
// Deliberately distinct from the covered-call events above: a put is a
// separate product (collateral is settlement, exercise delivers
// underlying), so the indexer keys it on its own event types and existing
// call consumers are untouched — mirroring how `CollateralizedWrite` was
// kept distinct from `WriteExecuted`.

public struct PutBucketCreated has copy, drop {
    bucket_id: ID,
    asset_type: TypeName,
    settlement_type: TypeName,
    /// Fully-qualified type of the per-bucket put coin (`Coin<put_type>`).
    put_type: TypeName,
    expiry_ms: u64,
    strike: u128,
    strike_scale: u8,
}

public struct PutWriteExecuted has copy, drop {
    bucket_id: ID,
    signer_account_id: ID,
    signer_token_recipient: address,
    executor: address,
    position_id: ID,
    position_recipient: address,
    put_token_recipient: address,
    write_amount: u64,
    /// Cash collateral escrowed = ceil(write_amount × strike).
    collateral: u64,
    gross_premium: u64,
    fee: u64,
    net_premium: u64,
    range_start: u128,
    range_end: u128,
    nonce: u64,
}

public struct PutCollateralizedWrite has copy, drop {
    bucket_id: ID,
    writer: address,
    write_amount: u64,
    collateral: u64,
    range_start: u128,
    range_end: u128,
}

public struct PutExercised has copy, drop {
    bucket_id: ID,
    exerciser: address,
    /// Underlying delivered into the bucket (== put coins burned).
    amount: u64,
    /// Settlement (cash) paid out to the exerciser = floor(amount × strike).
    settlement_paid: u64,
    cursor_after: u128,
}

public struct PutRedeemed has copy, drop {
    bucket_id: ID,
    position_id: ID,
    redeemer: address,
    range_start: u128,
    range_end: u128,
    /// Assigned (exercised) underlying handed to the writer.
    underlying_returned: u64,
    /// Unassigned cash collateral returned = floor(unexercised × strike).
    settlement_returned: u64,
}

public struct PutExpiredOptionBurned has copy, drop {
    bucket_id: ID,
    burner: address,
    amount: u64,
}

public struct PutBucketCleaned has copy, drop {
    bucket_id: ID,
    /// Rounding-remainder cash swept to the admin at cleanup.
    dust_swept: u64,
}

public struct PutBucketInvalidated has copy, drop {
    bucket_id: ID,
    at_ms: u64,
    admin: address,
    reason: vector<u8>,
}

public struct PutBucketRevalidated has copy, drop {
    bucket_id: ID,
    at_ms: u64,
    admin: address,
    reason: vector<u8>,
}

public struct PutRfqCreated has copy, drop {
    rfq_id: ID,
    bucket_id: ID,
    origin: ID,
    amount: u64,
    collateral: u64,
    reserve_premium: u64,
    deadline_ms: u64,
    max_deadline_ms: u64,
    min_increment_bps: u64,
}

public struct PutRfqBid has copy, drop {
    rfq_id: ID,
    bidder: address,
    put_recipient: address,
    premium: u64,
    previous_premium: u64,
    new_deadline_ms: u64,
}

public struct PutRfqSettled has copy, drop {
    rfq_id: ID,
    bucket_id: ID,
    origin: ID,
    winner: address,
    put_recipient: address,
    position_id: ID,
    position_recipient: address,
    amount: u64,
    gross_premium: u64,
    fee: u64,
    net_premium: u64,
    range_start: u128,
    range_end: u128,
}

public struct PutRfqExpiredUnsold has copy, drop {
    rfq_id: ID,
    bucket_id: ID,
    origin: ID,
    amount: u64,
    reserve_premium: u64,
}

public struct AccountCreated has copy, drop {
    account_id: ID,
    owner: address,
    signing_scheme: u8,
    signing_pubkey: vector<u8>,
}

public struct AccountDeposit has copy, drop {
    account_id: ID,
    asset_type: TypeName,
    amount: u64,
}

public struct AccountWithdraw has copy, drop {
    account_id: ID,
    asset_type: TypeName,
    amount: u64,
}

public struct SigningKeyRotated has copy, drop {
    account_id: ID,
    new_scheme: u8,
    new_pubkey: vector<u8>,
}

/// A Position object entered the account's custody (session flows: positions
/// are held by the account object, not a wallet address).
public struct AccountPositionDeposit has copy, drop {
    account_id: ID,
    position_id: ID,
    bucket_id: ID,
}

/// A Position object left the account's custody (redeemed or withdrawn).
public struct AccountPositionWithdraw has copy, drop {
    account_id: ID,
    position_id: ID,
}

/// Any other object (vault receipt, …) entered the account's custody.
public struct AccountObjectDeposit has copy, drop {
    account_id: ID,
    object_id: ID,
    object_type: TypeName,
}

/// A custodied object left the account's custody.
public struct AccountObjectWithdraw has copy, drop {
    account_id: ID,
    object_id: ID,
    object_type: TypeName,
}

public struct FeeUpdated has copy, drop {
    old_bps: u64,
    new_bps: u64,
}

public struct TreasuryWithdrawn has copy, drop {
    asset_type: TypeName,
    amount: u64,
    recipient: address,
}

public(package) fun emit_bucket_created(
    bucket_id: ID,
    asset_type: TypeName,
    settlement_type: TypeName,
    call_type: TypeName,
    expiry_ms: u64,
    strike: u128,
    strike_scale: u8,
) {
    event::emit(BucketCreated {
        bucket_id,
        asset_type,
        settlement_type,
        call_type,
        expiry_ms,
        strike,
        strike_scale,
    });
}

public(package) fun emit_write_executed(
    bucket_id: ID,
    signer_account_id: ID,
    signer_token_recipient: address,
    executor: address,
    position_id: ID,
    position_recipient: address,
    call_token_recipient: address,
    write_amount: u64,
    gross_premium: u64,
    fee: u64,
    net_premium: u64,
    range_start: u128,
    range_end: u128,
    nonce: u64,
) {
    event::emit(WriteExecuted {
        bucket_id,
        signer_account_id,
        signer_token_recipient,
        executor,
        position_id,
        position_recipient,
        call_token_recipient,
        write_amount,
        gross_premium,
        fee,
        net_premium,
        range_start,
        range_end,
        nonce,
    });
}

public(package) fun emit_collateralized_write(
    bucket_id: ID,
    writer: address,
    amount: u64,
    range_start: u128,
    range_end: u128,
) {
    event::emit(CollateralizedWrite { bucket_id, writer, amount, range_start, range_end });
}

public(package) fun emit_exercised(
    bucket_id: ID,
    exerciser: address,
    amount: u64,
    settlement_paid: u64,
    cursor_after: u128,
) {
    event::emit(Exercised { bucket_id, exerciser, amount, settlement_paid, cursor_after });
}

public(package) fun emit_redeemed(
    bucket_id: ID,
    position_id: ID,
    redeemer: address,
    range_start: u128,
    range_end: u128,
    underlying_returned: u64,
    settlement_returned: u64,
) {
    event::emit(Redeemed {
        bucket_id,
        position_id,
        redeemer,
        range_start,
        range_end,
        underlying_returned,
        settlement_returned,
    });
}

public(package) fun emit_expired_option_burned(
    bucket_id: ID,
    burner: address,
    amount: u64,
) {
    event::emit(ExpiredOptionBurned { bucket_id, burner, amount });
}

public(package) fun emit_bucket_cleaned(bucket_id: ID) {
    event::emit(BucketCleaned { bucket_id });
}

public(package) fun emit_bucket_invalidated(
    bucket_id: ID,
    at_ms: u64,
    admin: address,
    reason: vector<u8>,
) {
    event::emit(BucketInvalidated { bucket_id, at_ms, admin, reason });
}

public(package) fun emit_bucket_revalidated(
    bucket_id: ID,
    at_ms: u64,
    admin: address,
    reason: vector<u8>,
) {
    event::emit(BucketRevalidated { bucket_id, at_ms, admin, reason });
}

public(package) fun emit_rfq_created(
    rfq_id: ID,
    bucket_id: ID,
    origin: ID,
    amount: u64,
    reserve_premium: u64,
    deadline_ms: u64,
    max_deadline_ms: u64,
    min_increment_bps: u64,
) {
    event::emit(RfqCreated {
        rfq_id,
        bucket_id,
        origin,
        amount,
        reserve_premium,
        deadline_ms,
        max_deadline_ms,
        min_increment_bps,
    });
}

public(package) fun emit_rfq_bid(
    rfq_id: ID,
    bidder: address,
    call_recipient: address,
    premium: u64,
    previous_premium: u64,
    new_deadline_ms: u64,
) {
    event::emit(RfqBid {
        rfq_id,
        bidder,
        call_recipient,
        premium,
        previous_premium,
        new_deadline_ms,
    });
}

public(package) fun emit_rfq_settled(
    rfq_id: ID,
    bucket_id: ID,
    origin: ID,
    winner: address,
    call_recipient: address,
    position_id: ID,
    position_recipient: address,
    amount: u64,
    gross_premium: u64,
    fee: u64,
    net_premium: u64,
    range_start: u128,
    range_end: u128,
) {
    event::emit(RfqSettled {
        rfq_id,
        bucket_id,
        origin,
        winner,
        call_recipient,
        position_id,
        position_recipient,
        amount,
        gross_premium,
        fee,
        net_premium,
        range_start,
        range_end,
    });
}

public(package) fun emit_rfq_expired_unsold(
    rfq_id: ID,
    bucket_id: ID,
    origin: ID,
    amount: u64,
    reserve_premium: u64,
) {
    event::emit(RfqExpiredUnsold { rfq_id, bucket_id, origin, amount, reserve_premium });
}

public(package) fun emit_vault_created(
    vault_id: ID,
    underlying_type: TypeName,
    settlement_type: TypeName,
    share_type: TypeName,
    mgmt_fee_bps_annual: u64,
    perf_fee_bps: u64,
    round_ms: u64,
    selling_window_ms: u64,
    min_strike_bps_over_spot: u64,
    max_strike_bps_over_spot: u64,
) {
    event::emit(VaultCreated {
        vault_id,
        underlying_type,
        settlement_type,
        share_type,
        mgmt_fee_bps_annual,
        perf_fee_bps,
        round_ms,
        selling_window_ms,
        min_strike_bps_over_spot,
        max_strike_bps_over_spot,
    });
}

public(package) fun emit_vault_config_applied(
    vault_id: ID,
    round: u64,
    mgmt_fee_bps_annual: u64,
    perf_fee_bps: u64,
    round_ms: u64,
    selling_window_ms: u64,
    min_strike_bps_over_spot: u64,
    max_strike_bps_over_spot: u64,
) {
    event::emit(VaultConfigApplied {
        vault_id,
        round,
        mgmt_fee_bps_annual,
        perf_fee_bps,
        round_ms,
        selling_window_ms,
        min_strike_bps_over_spot,
        max_strike_bps_over_spot,
    });
}

public(package) fun emit_vault_deposit(vault_id: ID, depositor: address, round: u64, amount: u64) {
    event::emit(VaultDeposit { vault_id, depositor, round, amount });
}

public(package) fun emit_shares_claimed(
    vault_id: ID,
    owner: address,
    round: u64,
    amount: u64,
    shares: u64,
) {
    event::emit(SharesClaimed { vault_id, owner, round, amount, shares });
}

public(package) fun emit_withdraw_initiated(vault_id: ID, owner: address, round: u64, shares: u64) {
    event::emit(WithdrawInitiated { vault_id, owner, round, shares });
}

public(package) fun emit_withdraw_completed(
    vault_id: ID,
    owner: address,
    round: u64,
    shares: u64,
    amount: u64,
) {
    event::emit(WithdrawCompleted { vault_id, owner, round, shares, amount });
}

public(package) fun emit_instant_withdraw(vault_id: ID, owner: address, round: u64, amount: u64) {
    event::emit(InstantWithdraw { vault_id, owner, round, amount });
}

public(package) fun emit_vault_bucket_selected(
    vault_id: ID,
    round: u64,
    bucket_id: ID,
    strike: u128,
    strike_scale: u8,
    expiry_ms: u64,
    selling_ends_ms: u64,
    spot: u128,
    spot_scale: u8,
) {
    event::emit(VaultBucketSelected {
        vault_id,
        round,
        bucket_id,
        strike,
        strike_scale,
        expiry_ms,
        selling_ends_ms,
        spot,
        spot_scale,
    });
}

public(package) fun emit_vault_position_redeemed(
    vault_id: ID,
    round: u64,
    position_id: ID,
    underlying_returned: u64,
    settlement_returned: u64,
) {
    event::emit(VaultPositionRedeemed {
        vault_id,
        round,
        position_id,
        underlying_returned,
        settlement_returned,
    });
}

public(package) fun emit_swap_rfq_created(
    swap_id: ID,
    origin: ID,
    amount_s: u64,
    reserve_underlying: u64,
    deadline_ms: u64,
    max_deadline_ms: u64,
    min_increment_bps: u64,
) {
    event::emit(SwapRfqCreated {
        swap_id,
        origin,
        amount_s,
        reserve_underlying,
        deadline_ms,
        max_deadline_ms,
        min_increment_bps,
    });
}

public(package) fun emit_swap_rfq_bid(
    swap_id: ID,
    bidder: address,
    underlying: u64,
    previous_underlying: u64,
    new_deadline_ms: u64,
) {
    event::emit(SwapRfqBid {
        swap_id,
        bidder,
        underlying,
        previous_underlying,
        new_deadline_ms,
    });
}

public(package) fun emit_swap_rfq_settled(
    swap_id: ID,
    vault_id: ID,
    round: u64,
    winner: address,
    settlement_filled: u64,
    underlying_in: u64,
) {
    event::emit(SwapRfqSettled {
        swap_id,
        vault_id,
        round,
        winner,
        settlement_filled,
        underlying_in,
    });
}

public(package) fun emit_swap_rfq_unfilled(
    swap_id: ID,
    vault_id: ID,
    round: u64,
    amount_s: u64,
) {
    event::emit(SwapRfqUnfilled { swap_id, vault_id, round, amount_s });
}

public(package) fun emit_vault_fees_charged(vault_id: ID, round: u64, mgmt_fee: u64, perf_fee: u64) {
    event::emit(VaultFeesCharged { vault_id, round, mgmt_fee, perf_fee });
}

public(package) fun emit_vault_round_finalized(
    vault_id: ID,
    round: u64,
    pps: u128,
    aum: u64,
    shares: u64,
    premium_collected: u64,
    premium_underlying: u64,
    withdrawals_owed: u64,
    shares_burned: u64,
    deposits_processed: u64,
    shares_minted: u64,
) {
    event::emit(VaultRoundFinalized {
        vault_id,
        round,
        pps,
        aum,
        shares,
        premium_collected,
        premium_underlying,
        withdrawals_owed,
        shares_burned,
        deposits_processed,
        shares_minted,
    });
}

public(package) fun emit_vault_config_updated(vault_id: ID, round: u64) {
    event::emit(VaultConfigUpdated { vault_id, round });
}

public(package) fun emit_vault_deposits_paused(vault_id: ID, paused: bool) {
    event::emit(VaultDepositsPaused { vault_id, paused });
}

public(package) fun emit_account_created(
    account_id: ID,
    owner: address,
    signing_scheme: u8,
    signing_pubkey: vector<u8>,
) {
    event::emit(AccountCreated {
        account_id,
        owner,
        signing_scheme,
        signing_pubkey,
    });
}

public(package) fun emit_account_deposit(
    account_id: ID,
    asset_type: TypeName,
    amount: u64,
) {
    event::emit(AccountDeposit { account_id, asset_type, amount });
}

public(package) fun emit_account_withdraw(
    account_id: ID,
    asset_type: TypeName,
    amount: u64,
) {
    event::emit(AccountWithdraw { account_id, asset_type, amount });
}

public(package) fun emit_signing_key_rotated(
    account_id: ID,
    new_scheme: u8,
    new_pubkey: vector<u8>,
) {
    event::emit(SigningKeyRotated { account_id, new_scheme, new_pubkey });
}

public(package) fun emit_account_position_deposit(
    account_id: ID,
    position_id: ID,
    bucket_id: ID,
) {
    event::emit(AccountPositionDeposit { account_id, position_id, bucket_id });
}

public(package) fun emit_account_position_withdraw(account_id: ID, position_id: ID) {
    event::emit(AccountPositionWithdraw { account_id, position_id });
}

public(package) fun emit_account_object_deposit(
    account_id: ID,
    object_id: ID,
    object_type: TypeName,
) {
    event::emit(AccountObjectDeposit { account_id, object_id, object_type });
}

public(package) fun emit_account_object_withdraw(
    account_id: ID,
    object_id: ID,
    object_type: TypeName,
) {
    event::emit(AccountObjectWithdraw { account_id, object_id, object_type });
}

public(package) fun emit_fee_updated(old_bps: u64, new_bps: u64) {
    event::emit(FeeUpdated { old_bps, new_bps });
}

public(package) fun emit_treasury_withdrawn(
    asset_type: TypeName,
    amount: u64,
    recipient: address,
) {
    event::emit(TreasuryWithdrawn { asset_type, amount, recipient });
}

// ---- cash-secured put emitters ----

public(package) fun emit_put_bucket_created(
    bucket_id: ID,
    asset_type: TypeName,
    settlement_type: TypeName,
    put_type: TypeName,
    expiry_ms: u64,
    strike: u128,
    strike_scale: u8,
) {
    event::emit(PutBucketCreated {
        bucket_id,
        asset_type,
        settlement_type,
        put_type,
        expiry_ms,
        strike,
        strike_scale,
    });
}

public(package) fun emit_put_write_executed(
    bucket_id: ID,
    signer_account_id: ID,
    signer_token_recipient: address,
    executor: address,
    position_id: ID,
    position_recipient: address,
    put_token_recipient: address,
    write_amount: u64,
    collateral: u64,
    gross_premium: u64,
    fee: u64,
    net_premium: u64,
    range_start: u128,
    range_end: u128,
    nonce: u64,
) {
    event::emit(PutWriteExecuted {
        bucket_id,
        signer_account_id,
        signer_token_recipient,
        executor,
        position_id,
        position_recipient,
        put_token_recipient,
        write_amount,
        collateral,
        gross_premium,
        fee,
        net_premium,
        range_start,
        range_end,
        nonce,
    });
}

public(package) fun emit_put_collateralized_write(
    bucket_id: ID,
    writer: address,
    write_amount: u64,
    collateral: u64,
    range_start: u128,
    range_end: u128,
) {
    event::emit(PutCollateralizedWrite {
        bucket_id,
        writer,
        write_amount,
        collateral,
        range_start,
        range_end,
    });
}

public(package) fun emit_put_exercised(
    bucket_id: ID,
    exerciser: address,
    amount: u64,
    settlement_paid: u64,
    cursor_after: u128,
) {
    event::emit(PutExercised { bucket_id, exerciser, amount, settlement_paid, cursor_after });
}

public(package) fun emit_put_redeemed(
    bucket_id: ID,
    position_id: ID,
    redeemer: address,
    range_start: u128,
    range_end: u128,
    underlying_returned: u64,
    settlement_returned: u64,
) {
    event::emit(PutRedeemed {
        bucket_id,
        position_id,
        redeemer,
        range_start,
        range_end,
        underlying_returned,
        settlement_returned,
    });
}

public(package) fun emit_put_expired_option_burned(
    bucket_id: ID,
    burner: address,
    amount: u64,
) {
    event::emit(PutExpiredOptionBurned { bucket_id, burner, amount });
}

public(package) fun emit_put_bucket_cleaned(bucket_id: ID, dust_swept: u64) {
    event::emit(PutBucketCleaned { bucket_id, dust_swept });
}

public(package) fun emit_put_bucket_invalidated(
    bucket_id: ID,
    at_ms: u64,
    admin: address,
    reason: vector<u8>,
) {
    event::emit(PutBucketInvalidated { bucket_id, at_ms, admin, reason });
}

public(package) fun emit_put_bucket_revalidated(
    bucket_id: ID,
    at_ms: u64,
    admin: address,
    reason: vector<u8>,
) {
    event::emit(PutBucketRevalidated { bucket_id, at_ms, admin, reason });
}

public(package) fun emit_put_rfq_created(
    rfq_id: ID,
    bucket_id: ID,
    origin: ID,
    amount: u64,
    collateral: u64,
    reserve_premium: u64,
    deadline_ms: u64,
    max_deadline_ms: u64,
    min_increment_bps: u64,
) {
    event::emit(PutRfqCreated {
        rfq_id,
        bucket_id,
        origin,
        amount,
        collateral,
        reserve_premium,
        deadline_ms,
        max_deadline_ms,
        min_increment_bps,
    });
}

public(package) fun emit_put_rfq_bid(
    rfq_id: ID,
    bidder: address,
    put_recipient: address,
    premium: u64,
    previous_premium: u64,
    new_deadline_ms: u64,
) {
    event::emit(PutRfqBid {
        rfq_id,
        bidder,
        put_recipient,
        premium,
        previous_premium,
        new_deadline_ms,
    });
}

public(package) fun emit_put_rfq_settled(
    rfq_id: ID,
    bucket_id: ID,
    origin: ID,
    winner: address,
    put_recipient: address,
    position_id: ID,
    position_recipient: address,
    amount: u64,
    gross_premium: u64,
    fee: u64,
    net_premium: u64,
    range_start: u128,
    range_end: u128,
) {
    event::emit(PutRfqSettled {
        rfq_id,
        bucket_id,
        origin,
        winner,
        put_recipient,
        position_id,
        position_recipient,
        amount,
        gross_premium,
        fee,
        net_premium,
        range_start,
        range_end,
    });
}

public(package) fun emit_put_rfq_expired_unsold(
    rfq_id: ID,
    bucket_id: ID,
    origin: ID,
    amount: u64,
    reserve_premium: u64,
) {
    event::emit(PutRfqExpiredUnsold { rfq_id, bucket_id, origin, amount, reserve_premium });
}

/// Test-only constructors so tests can assert emitted event *contents*
/// (via `sui::event::events_by_type` + `==`), not just emission counts.
#[test_only]
public fun new_write_executed_for_testing(
    bucket_id: ID,
    signer_account_id: ID,
    signer_token_recipient: address,
    executor: address,
    position_id: ID,
    position_recipient: address,
    call_token_recipient: address,
    write_amount: u64,
    gross_premium: u64,
    fee: u64,
    net_premium: u64,
    range_start: u128,
    range_end: u128,
    nonce: u64,
): WriteExecuted {
    WriteExecuted {
        bucket_id,
        signer_account_id,
        signer_token_recipient,
        executor,
        position_id,
        position_recipient,
        call_token_recipient,
        write_amount,
        gross_premium,
        fee,
        net_premium,
        range_start,
        range_end,
        nonce,
    }
}

/// The one `WriteExecuted` field a test cannot know up front (the Position
/// is minted inside the call): expose it so the expected struct can be
/// completed, then cross-checked against the recipient's inventory.
#[test_only]
public fun write_executed_position_id(e: &WriteExecuted): ID {
    e.position_id
}

#[test_only]
public fun new_collateralized_write_for_testing(
    bucket_id: ID,
    writer: address,
    amount: u64,
    range_start: u128,
    range_end: u128,
): CollateralizedWrite {
    CollateralizedWrite { bucket_id, writer, amount, range_start, range_end }
}
