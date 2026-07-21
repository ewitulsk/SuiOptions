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

// ─────────────────────────── protocol admin ───────────────────────────

public struct AdapterAllowed has copy, drop { adapter: TypeName }

public struct AdapterDisallowed has copy, drop { adapter: TypeName }

public struct OracleAllowed has copy, drop { oracle: TypeName }

public struct OracleDisallowed has copy, drop { oracle: TypeName }

public struct ProtocolConfigUpdated has copy, drop {
    min_curator_share_bps: u64,
    enforce_curator_share: bool,
    max_curator_fee_bps: u64,
    protocol_fee_bps: u64,
    max_price_age_ms: u64,
    paused: bool,
}

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
