module options_core::events;

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
    /// The `QuoteSigner` whose quote authorized this write.
    signer_id: ID,
    /// The external collateral object the signer's funds released from.
    collateral_source: ID,
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
    /// The `QuoteSigner` whose quote authorized this write.
    signer_id: ID,
    /// The external collateral object the signer's funds released from.
    collateral_source: ID,
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

public struct SignerCreated has copy, drop {
    signer_id: ID,
    owner: address,
    signing_scheme: u8,
    signing_pubkey: vector<u8>,
}

public struct SigningKeyRotated has copy, drop {
    signer_id: ID,
    new_scheme: u8,
    new_pubkey: vector<u8>,
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
    signer_id: ID,
    collateral_source: ID,
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
        signer_id,
        collateral_source,
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

public(package) fun emit_signer_created(
    signer_id: ID,
    owner: address,
    signing_scheme: u8,
    signing_pubkey: vector<u8>,
) {
    event::emit(SignerCreated {
        signer_id,
        owner,
        signing_scheme,
        signing_pubkey,
    });
}

public(package) fun emit_signing_key_rotated(
    signer_id: ID,
    new_scheme: u8,
    new_pubkey: vector<u8>,
) {
    event::emit(SigningKeyRotated { signer_id, new_scheme, new_pubkey });
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
    signer_id: ID,
    collateral_source: ID,
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
        signer_id,
        collateral_source,
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

/// Test-only constructors so tests can assert emitted event *contents*
/// (via `sui::event::events_by_type` + `==`), not just emission counts.
#[test_only]
public fun new_write_executed_for_testing(
    bucket_id: ID,
    signer_id: ID,
    collateral_source: ID,
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
        signer_id,
        collateral_source,
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
