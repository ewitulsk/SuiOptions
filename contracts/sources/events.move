module options_protocol::events;

use std::type_name::TypeName;
use sui::event;

public struct BucketCreated has copy, drop {
    bucket_id: ID,
    asset_type: TypeName,
    settlement_type: TypeName,
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
    expiry_ms: u64,
    strike: u128,
    strike_scale: u8,
) {
    event::emit(BucketCreated {
        bucket_id,
        asset_type,
        settlement_type,
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
