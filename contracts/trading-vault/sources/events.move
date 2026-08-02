module trading_vault::events;

use std::type_name::TypeName;
use sui::event;
use sui::vec_map::VecMap;

// ─────────────────────────── vault lifecycle ───────────────────────────

public struct VaultCreated has copy, drop {
    vault_id: ID,
    creator: address,
    curator: address,
    curator_cap_id: ID,
    deposit_asset: TypeName,
    lockup_ms: u64,
    curator_fee_bps: u64,
    rotation_authority: u8,
    max_positions: u64,
    unwind_grace_ms: u64,
}

public struct VaultClosing has copy, drop { vault_id: ID }

public struct VaultClosed has copy, drop { vault_id: ID }

public struct DepositsPaused has copy, drop { vault_id: ID, paused: bool }

public struct MmReleaseToggled has copy, drop { vault_id: ID, enabled: bool }

public struct CuratorRotated has copy, drop {
    vault_id: ID,
    old_cap_id: ID,
    new_cap_id: ID,
    recipient: address,
}

// ───────────────────────── stakes and the queue ─────────────────────────

/// StakeKey mirror for events: `curator_cap` is none for address stakes.
public struct Deposited has copy, drop {
    vault_id: ID,
    depositor: address,
    curator_cap: Option<ID>,
    amount: u64,
    shares: u128,
    total_shares: u128,
    locked_until_ms: u64,
}

public struct WithdrawRequested has copy, drop {
    vault_id: ID,
    seq: u64,
    recipient: address,
    curator_cap: Option<ID>,
    shares: u128,
    basis: u64,
    requested_at_ms: u64,
}

/// The crystallization record: everything the dashboard's fee breakdown
/// needs, per fulfilled request.
public struct WithdrawFulfilled has copy, drop {
    vault_id: ID,
    seq: u64,
    recipient: address,
    shares: u128,
    value: u64,
    basis: u64,
    profit: u64,
    gross_fee: u64,
    protocol_cut: u64,
    curator_net: u64,
    curator_shares_minted: u128,
    payout: u64,
    total_shares: u128,
}

// ──────────────────────── sessions and custody ────────────────────────

public struct SessionSettled has copy, drop {
    vault_id: ID,
    adapter: TypeName,
    forced: bool,
    taken: VecMap<TypeName, u64>,
    returned: VecMap<TypeName, u64>,
    positions_added: u64,
    positions_removed: u64,
}

public struct PositionStored has copy, drop {
    vault_id: ID,
    adapter: TypeName,
    position_id: ID,
}

public struct PositionRemoved has copy, drop {
    vault_id: ID,
    adapter: TypeName,
    position_id: ID,
}

/// One custodied position's mark (deposit-asset units) recorded into an
/// appraisal. Only meaningful once the appraisal is CONSUMED in the same
/// transaction — an aborted appraisal drops its events with the tx.
public struct PositionAppraised has copy, drop {
    vault_id: ID,
    adapter: TypeName,
    position_id: ID,
    value: u64,
}

/// A complete appraisal was consumed: `total_value` is the NAV every
/// consume path (deposit / fulfillment / release / crank) validated.
public struct VaultAppraised has copy, drop {
    vault_id: ID,
    total_value: u128,
    position_total: u64,
}

// ─────────────────────────── external account ───────────────────────────

public struct ExternalAccountSet has copy, drop {
    vault_id: ID,
    account: address,
    equity_oracle: TypeName,
    budget_bps: u64,
    daily_release_bps: u64,
}

public struct ExternalAccountCleared has copy, drop { vault_id: ID }

public struct ExternalReleased has copy, drop {
    vault_id: ID,
    account: address,
    amount: u64,
    exposure: u64,
    nav: u128,
}

public struct ExternalReturned has copy, drop {
    vault_id: ID,
    from: address,
    amount: u64,
    exposure: u64,
}

// ───────────────────────── mm desk (vault_mm) ─────────────────────────

/// `amount` custodied option-coin units exercised under a curator
/// session. Calls: `settlement_amount` left the vault as the strike
/// payment and `amount` underlying came back. Puts: `amount` underlying
/// left as delivery and `settlement_amount` came back as the payout.
public struct MmCoinExercised has copy, drop {
    vault_id: ID,
    bucket_id: ID,
    coin_position_id: ID,
    is_put: bool,
    amount: u64,
    settlement_amount: u64,
}

/// A written Position netted against same-bucket option coins held in
/// VaultMm custody (`close_offset`), freeing `collateral_returned`
/// (underlying for calls, cash for puts) into vault balances.
public struct MmOffsetClosed has copy, drop {
    vault_id: ID,
    bucket_id: ID,
    position_id: ID,
    is_put: bool,
    amount: u64,
    collateral_returned: u64,
    position_closed: bool,
}

/// A VaultMm-custodied coin moved from position custody into the
/// vault's free balances (post-SO-297: option coins are appraisable as
/// free balances).
public struct MmCoinReleased has copy, drop {
    vault_id: ID,
    coin_position_id: ID,
    asset_type: TypeName,
    amount: u64,
}

// ─────────────────────────── protocol admin ───────────────────────────

public struct AdapterAllowed has copy, drop { adapter: TypeName }

public struct AdapterDisallowed has copy, drop { adapter: TypeName }

public struct OracleAllowed has copy, drop { oracle: TypeName }

public struct OracleDisallowed has copy, drop { oracle: TypeName }

/// An asset was pinned to a single oracle adapter, or unpinned back to
/// "any allowlisted" (SO-335).
public struct OraclePinned has copy, drop { asset: TypeName, oracle: TypeName }

public struct OracleUnpinned has copy, drop { asset: TypeName }

public struct ProtocolConfigUpdated has copy, drop {
    min_curator_share_bps: u64,
    enforce_curator_share: bool,
    max_curator_fee_bps: u64,
    protocol_fee_bps: u64,
    max_price_age_ms: u64,
    paused: bool,
}

/// Emitted separately from `ProtocolConfigUpdated` so the existing
/// config event's shape (and its off-chain decoder) stays untouched.
public struct RegistrarPubkeySet has copy, drop { pubkey: vector<u8> }

// ─────────────────────────────── emitters ───────────────────────────────

public(package) fun emit_vault_created(
    vault_id: ID,
    creator: address,
    curator: address,
    curator_cap_id: ID,
    deposit_asset: TypeName,
    lockup_ms: u64,
    curator_fee_bps: u64,
    rotation_authority: u8,
    max_positions: u64,
    unwind_grace_ms: u64,
) {
    event::emit(VaultCreated {
        vault_id,
        creator,
        curator,
        curator_cap_id,
        deposit_asset,
        lockup_ms,
        curator_fee_bps,
        rotation_authority,
        max_positions,
        unwind_grace_ms,
    });
}

public(package) fun emit_vault_closing(vault_id: ID) {
    event::emit(VaultClosing { vault_id });
}

public(package) fun emit_vault_closed(vault_id: ID) {
    event::emit(VaultClosed { vault_id });
}

public(package) fun emit_deposits_paused(vault_id: ID, paused: bool) {
    event::emit(DepositsPaused { vault_id, paused });
}

public(package) fun emit_mm_release_toggled(vault_id: ID, enabled: bool) {
    event::emit(MmReleaseToggled { vault_id, enabled });
}

public(package) fun emit_curator_rotated(
    vault_id: ID,
    old_cap_id: ID,
    new_cap_id: ID,
    recipient: address,
) {
    event::emit(CuratorRotated { vault_id, old_cap_id, new_cap_id, recipient });
}

public(package) fun emit_deposited(
    vault_id: ID,
    depositor: address,
    curator_cap: Option<ID>,
    amount: u64,
    shares: u128,
    total_shares: u128,
    locked_until_ms: u64,
) {
    event::emit(Deposited {
        vault_id,
        depositor,
        curator_cap,
        amount,
        shares,
        total_shares,
        locked_until_ms,
    });
}

public(package) fun emit_withdraw_requested(
    vault_id: ID,
    seq: u64,
    recipient: address,
    curator_cap: Option<ID>,
    shares: u128,
    basis: u64,
    requested_at_ms: u64,
) {
    event::emit(WithdrawRequested {
        vault_id,
        seq,
        recipient,
        curator_cap,
        shares,
        basis,
        requested_at_ms,
    });
}

public(package) fun emit_withdraw_fulfilled(
    vault_id: ID,
    seq: u64,
    recipient: address,
    shares: u128,
    value: u64,
    basis: u64,
    profit: u64,
    gross_fee: u64,
    protocol_cut: u64,
    curator_net: u64,
    curator_shares_minted: u128,
    payout: u64,
    total_shares: u128,
) {
    event::emit(WithdrawFulfilled {
        vault_id,
        seq,
        recipient,
        shares,
        value,
        basis,
        profit,
        gross_fee,
        protocol_cut,
        curator_net,
        curator_shares_minted,
        payout,
        total_shares,
    });
}

public(package) fun emit_session_settled(
    vault_id: ID,
    adapter: TypeName,
    forced: bool,
    taken: VecMap<TypeName, u64>,
    returned: VecMap<TypeName, u64>,
    positions_added: u64,
    positions_removed: u64,
) {
    event::emit(SessionSettled {
        vault_id,
        adapter,
        forced,
        taken,
        returned,
        positions_added,
        positions_removed,
    });
}

public(package) fun emit_position_stored(vault_id: ID, adapter: TypeName, position_id: ID) {
    event::emit(PositionStored { vault_id, adapter, position_id });
}

public(package) fun emit_position_removed(vault_id: ID, adapter: TypeName, position_id: ID) {
    event::emit(PositionRemoved { vault_id, adapter, position_id });
}

public(package) fun emit_position_appraised(
    vault_id: ID,
    adapter: TypeName,
    position_id: ID,
    value: u64,
) {
    event::emit(PositionAppraised { vault_id, adapter, position_id, value });
}

public(package) fun emit_vault_appraised(vault_id: ID, total_value: u128, position_total: u64) {
    event::emit(VaultAppraised { vault_id, total_value, position_total });
}

#[test_only]
public fun position_appraised_fields(e: &PositionAppraised): (ID, TypeName, ID, u64) {
    (e.vault_id, e.adapter, e.position_id, e.value)
}

#[test_only]
public fun vault_appraised_fields(e: &VaultAppraised): (ID, u128, u64) {
    (e.vault_id, e.total_value, e.position_total)
}

public(package) fun emit_external_account_set(
    vault_id: ID,
    account: address,
    equity_oracle: TypeName,
    budget_bps: u64,
    daily_release_bps: u64,
) {
    event::emit(ExternalAccountSet {
        vault_id,
        account,
        equity_oracle,
        budget_bps,
        daily_release_bps,
    });
}

public(package) fun emit_external_account_cleared(vault_id: ID) {
    event::emit(ExternalAccountCleared { vault_id });
}

public(package) fun emit_external_released(
    vault_id: ID,
    account: address,
    amount: u64,
    exposure: u64,
    nav: u128,
) {
    event::emit(ExternalReleased { vault_id, account, amount, exposure, nav });
}

public(package) fun emit_external_returned(
    vault_id: ID,
    from: address,
    amount: u64,
    exposure: u64,
) {
    event::emit(ExternalReturned { vault_id, from, amount, exposure });
}

public(package) fun emit_mm_coin_exercised(
    vault_id: ID,
    bucket_id: ID,
    coin_position_id: ID,
    is_put: bool,
    amount: u64,
    settlement_amount: u64,
) {
    event::emit(MmCoinExercised {
        vault_id,
        bucket_id,
        coin_position_id,
        is_put,
        amount,
        settlement_amount,
    });
}

public(package) fun emit_mm_offset_closed(
    vault_id: ID,
    bucket_id: ID,
    position_id: ID,
    is_put: bool,
    amount: u64,
    collateral_returned: u64,
    position_closed: bool,
) {
    event::emit(MmOffsetClosed {
        vault_id,
        bucket_id,
        position_id,
        is_put,
        amount,
        collateral_returned,
        position_closed,
    });
}

public(package) fun emit_mm_coin_released(
    vault_id: ID,
    coin_position_id: ID,
    asset_type: TypeName,
    amount: u64,
) {
    event::emit(MmCoinReleased { vault_id, coin_position_id, asset_type, amount });
}

public(package) fun emit_adapter_allowed(adapter: TypeName) {
    event::emit(AdapterAllowed { adapter });
}

public(package) fun emit_adapter_disallowed(adapter: TypeName) {
    event::emit(AdapterDisallowed { adapter });
}

public(package) fun emit_oracle_allowed(oracle: TypeName) {
    event::emit(OracleAllowed { oracle });
}

public(package) fun emit_oracle_disallowed(oracle: TypeName) {
    event::emit(OracleDisallowed { oracle });
}

public(package) fun emit_oracle_pinned(asset: TypeName, oracle: TypeName) {
    event::emit(OraclePinned { asset, oracle });
}

public(package) fun emit_oracle_unpinned(asset: TypeName) {
    event::emit(OracleUnpinned { asset });
}

public(package) fun emit_protocol_config_updated(
    min_curator_share_bps: u64,
    enforce_curator_share: bool,
    max_curator_fee_bps: u64,
    protocol_fee_bps: u64,
    max_price_age_ms: u64,
    paused: bool,
) {
    event::emit(ProtocolConfigUpdated {
        min_curator_share_bps,
        enforce_curator_share,
        max_curator_fee_bps,
        protocol_fee_bps,
        max_price_age_ms,
        paused,
    });
}

public(package) fun emit_registrar_pubkey_set(pubkey: vector<u8>) {
    event::emit(RegistrarPubkeySet { pubkey });
}
