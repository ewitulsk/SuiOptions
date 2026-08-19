/// Versioned v2 event schema (overhaul plan §2.5, §5). Tranches and risk
/// states are emitted as u8 wire codes (`capital::tranche_code` /
/// `capital::risk_state_code`) so off-chain decoders never parse Move
/// enums: tranche 0=Untranched 1=Senior 2=Junior; state 0=Healthy
/// 1=CoverageBreach 2=Impaired 3=ResetPending; lane 0=senior 1=junior.
module vault_v2::events;

use std::type_name::TypeName;
use sui::event;
use sui::vec_map::VecMap;

// ─────────────────────────── vault lifecycle ───────────────────────────

public struct VaultCreated has copy, drop {
    vault_id: ID,
    creator: address,
    curator_cap_id: ID,
    accounting_asset: TypeName,
    lockup_ms: u64,
    curator_fee_bps: u64,
    unwind_grace_ms: u64,
    /// 0 = Untranched, 1 = SeniorJunior. Immutable (§3.2).
    structure_code: u8,
    senior_hurdle_bps_annual: u64,
    target_junior_bps: u64,
    maintenance_junior_bps: u64,
    upside_code: u8,
    residual_participation_bps: u64,
    total_return_cap_bps: u64,
    /// §9.2: the exact terms version + spec hash governing issuance.
    terms_version: u64,
    spec_hash: vector<u8>,
}

public struct DepositAssetAdded has copy, drop { vault_id: ID, asset: TypeName }

public struct DepositAssetRemoved has copy, drop { vault_id: ID, asset: TypeName }

public struct HaircutsSet has copy, drop {
    vault_id: ID,
    entry_haircut_bps: u64,
    exit_haircut_bps: u64,
}

public struct QuoteAdapterAdded has copy, drop { vault_id: ID, adapter: TypeName }

public struct QuoteAdapterRemoved has copy, drop { vault_id: ID, adapter: TypeName }

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

// ─────────────────────────── position lifecycle ───────────────────────────

public struct PositionMinted has copy, drop {
    vault_id: ID,
    position_id: ID,
    tranche: u8,
    shares: u128,
    cost_basis: u64,
    locked_until_ms: u64,
    capital_generation: u64,
}

public struct PositionSplit has copy, drop {
    vault_id: ID,
    parent_id: ID,
    child_id: ID,
    parent_shares: u128,
    parent_basis: u64,
    child_shares: u128,
    child_basis: u64,
}

public struct PositionMerged has copy, drop {
    vault_id: ID,
    kept_id: ID,
    merged_id: ID,
    shares: u128,
    cost_basis: u64,
    locked_until_ms: u64,
}

/// A wiped-generation junior position was destroyed at zero value
/// (§8.5 cleanup).
public struct WipedPositionBurned has copy, drop {
    vault_id: ID,
    position_id: ID,
    capital_generation: u64,
    shares: u128,
}

// ───────────────────────── deposits and the queue ─────────────────────────

public struct Deposited has copy, drop {
    vault_id: ID,
    depositor: address,
    /// The escrowed curator commitment position id for commitment
    /// deposits; none for ordinary deposits.
    commitment_position: Option<ID>,
    position_id: ID,
    tranche: u8,
    capital_generation: u64,
    asset: TypeName,
    amount: u64,
    value: u64,
    shares: u128,
    tranche_shares: u128,
    locked_until_ms: u64,
}

public struct WithdrawRequested has copy, drop {
    vault_id: ID,
    /// The vault-wide global sequence (§3.6) — the request's key.
    global_seq: u64,
    lane: u8,
    position_id: ID,
    recipient: address,
    tranche: u8,
    capital_generation: u64,
    shares: u128,
    basis: u64,
    payout_asset: TypeName,
    requested_at_ms: u64,
}

public struct PayoutAssetAmended has copy, drop {
    vault_id: ID,
    global_seq: u64,
    payout_asset: TypeName,
}

public struct WithdrawFulfilled has copy, drop {
    vault_id: ID,
    global_seq: u64,
    lane: u8,
    recipient: address,
    tranche: u8,
    capital_generation: u64,
    shares: u128,
    value: u64,
    basis: u64,
    profit: u64,
    gross_fee: u64,
    protocol_cut: u64,
    curator_net: u64,
    curator_shares_minted: u128,
    payout: u64,
    payout_asset: TypeName,
    payout_units: u64,
    price: u128,
    tranche_shares: u128,
}

// ─────────────────────────── capital state ───────────────────────────

/// Emitted by every consumed-appraisal capital sync: the §3.4a waterfall
/// decomposition at that NAV plus the resulting risk state. This is the
/// event stream behind per-tranche PPS history and the /waterfall API.
public struct CapitalSynced has copy, drop {
    vault_id: ID,
    total_nav: u128,
    senior_nav: u128,
    junior_nav: u128,
    senior_claim: u128,
    senior_shares: u128,
    junior_shares: u128,
    risk_state: u8,
    active_junior_generation: u64,
    curator_commitment_breached: bool,
}

public struct RiskStateChanged has copy, drop {
    vault_id: ID,
    old_state: u8,
    new_state: u8,
    timestamp_ms: u64,
}

public struct JuniorResetProposed has copy, drop {
    vault_id: ID,
    old_generation: u64,
    proposed_at_ms: u64,
    executable_at_ms: u64,
    total_nav: u128,
    senior_claim: u128,
    senior_deficit: u128,
    required_deposit: u64,
}

public struct JuniorResetCancelled has copy, drop {
    vault_id: ID,
    old_generation: u64,
}

public struct JuniorResetExecuted has copy, drop {
    vault_id: ID,
    old_generation: u64,
    new_generation: u64,
    recapitalizer: address,
    deposit_value: u64,
    post_junior_nav: u128,
    position_id: ID,
}

/// The curator released shares from the escrowed commitment position
/// into a wallet-held (freely transferable) position NFT.
public struct CommitmentReleased has copy, drop {
    vault_id: ID,
    curator_cap_id: ID,
    position_id: ID,
    shares: u128,
    basis: u64,
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

public struct PositionAppraised has copy, drop {
    vault_id: ID,
    adapter: TypeName,
    position_id: ID,
    value: u64,
}

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

public struct MmCoinExercised has copy, drop {
    vault_id: ID,
    bucket_id: ID,
    coin_position_id: ID,
    is_put: bool,
    amount: u64,
    settlement_amount: u64,
}

public struct MmOffsetClosed has copy, drop {
    vault_id: ID,
    bucket_id: ID,
    position_id: ID,
    is_put: bool,
    amount: u64,
    collateral_returned: u64,
    position_closed: bool,
}

public struct MmCoinReleased has copy, drop {
    vault_id: ID,
    coin_position_id: ID,
    asset_type: TypeName,
    amount: u64,
}

// ─────────────────────────── terminal settlement ───────────────────────────

/// The one-time settlement snapshot (§8.7): the waterfall run once on
/// the final NAV, freezing each tranche's total entitlement against its
/// outstanding supply (queued shares included).
public struct SettlementSnapshot has copy, drop {
    vault_id: ID,
    final_nav: u128,
    senior_pool: u64,
    senior_supply: u128,
    junior_pool: u64,
    junior_supply: u128,
    active_junior_generation: u64,
}

/// A claim redeemed against the settlement pool — either a wallet-held
/// position (`from_queue == false`, `global_seq == 0`) or an
/// already-queued request (`from_queue == true`).
public struct SettlementRedeemed has copy, drop {
    vault_id: ID,
    position_id: ID,
    from_queue: bool,
    global_seq: u64,
    recipient: address,
    tranche: u8,
    capital_generation: u64,
    shares: u128,
    entitlement: u64,
    basis: u64,
    gross_fee: u64,
    protocol_cut: u64,
    curator_net: u64,
    payout: u64,
}

public struct SettlementCuratorFeesClaimed has copy, drop {
    vault_id: ID,
    curator_cap_id: ID,
    amount: u64,
}

// ─────────────────────────── protocol admin ───────────────────────────

public struct AdapterAllowed has copy, drop { adapter: TypeName }

public struct AdapterDisallowed has copy, drop { adapter: TypeName }

public struct OracleAllowed has copy, drop { oracle: TypeName }

public struct OracleDisallowed has copy, drop { oracle: TypeName }

public struct OraclePinned has copy, drop { asset: TypeName, oracle: TypeName }

public struct OracleUnpinned has copy, drop { asset: TypeName }

public struct ProtocolConfigUpdated has copy, drop {
    min_curator_share_bps: u64,
    enforce_curator_share: bool,
    max_curator_fee_bps: u64,
    protocol_fee_bps: u64,
    max_price_age_ms: u64,
    max_deposit_assets: u64,
    paused: bool,
    max_senior_hurdle_bps: u64,
    min_target_junior_bps: u64,
    min_maintenance_junior_bps: u64,
    min_curator_commitment_bps: u64,
}

public struct RegistrarPubkeySet has copy, drop { pubkey: vector<u8> }

// ─────────────────────────────── emitters ───────────────────────────────

public(package) fun emit_vault_created(
    vault_id: ID,
    creator: address,
    curator_cap_id: ID,
    accounting_asset: TypeName,
    lockup_ms: u64,
    curator_fee_bps: u64,
    unwind_grace_ms: u64,
    structure_code: u8,
    senior_hurdle_bps_annual: u64,
    target_junior_bps: u64,
    maintenance_junior_bps: u64,
    upside_code: u8,
    residual_participation_bps: u64,
    total_return_cap_bps: u64,
    terms_version: u64,
    spec_hash: vector<u8>,
) {
    event::emit(VaultCreated {
        vault_id,
        creator,
        curator_cap_id,
        accounting_asset,
        lockup_ms,
        curator_fee_bps,
        unwind_grace_ms,
        structure_code,
        senior_hurdle_bps_annual,
        target_junior_bps,
        maintenance_junior_bps,
        upside_code,
        residual_participation_bps,
        total_return_cap_bps,
        terms_version,
        spec_hash,
    });
}

public(package) fun emit_deposit_asset_added(vault_id: ID, asset: TypeName) {
    event::emit(DepositAssetAdded { vault_id, asset });
}

public(package) fun emit_deposit_asset_removed(vault_id: ID, asset: TypeName) {
    event::emit(DepositAssetRemoved { vault_id, asset });
}

public(package) fun emit_haircuts_set(vault_id: ID, entry_haircut_bps: u64, exit_haircut_bps: u64) {
    event::emit(HaircutsSet { vault_id, entry_haircut_bps, exit_haircut_bps });
}

public(package) fun emit_quote_adapter_added(vault_id: ID, adapter: TypeName) {
    event::emit(QuoteAdapterAdded { vault_id, adapter });
}

public(package) fun emit_quote_adapter_removed(vault_id: ID, adapter: TypeName) {
    event::emit(QuoteAdapterRemoved { vault_id, adapter });
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

public(package) fun emit_position_minted(
    vault_id: ID,
    position_id: ID,
    tranche: u8,
    shares: u128,
    cost_basis: u64,
    locked_until_ms: u64,
    capital_generation: u64,
) {
    event::emit(PositionMinted {
        vault_id,
        position_id,
        tranche,
        shares,
        cost_basis,
        locked_until_ms,
        capital_generation,
    });
}

public(package) fun emit_position_split(
    vault_id: ID,
    parent_id: ID,
    child_id: ID,
    parent_shares: u128,
    parent_basis: u64,
    child_shares: u128,
    child_basis: u64,
) {
    event::emit(PositionSplit {
        vault_id,
        parent_id,
        child_id,
        parent_shares,
        parent_basis,
        child_shares,
        child_basis,
    });
}

public(package) fun emit_position_merged(
    vault_id: ID,
    kept_id: ID,
    merged_id: ID,
    shares: u128,
    cost_basis: u64,
    locked_until_ms: u64,
) {
    event::emit(PositionMerged { vault_id, kept_id, merged_id, shares, cost_basis, locked_until_ms });
}

public(package) fun emit_wiped_position_burned(
    vault_id: ID,
    position_id: ID,
    capital_generation: u64,
    shares: u128,
) {
    event::emit(WipedPositionBurned { vault_id, position_id, capital_generation, shares });
}

public(package) fun emit_deposited(
    vault_id: ID,
    depositor: address,
    commitment_position: Option<ID>,
    position_id: ID,
    tranche: u8,
    capital_generation: u64,
    asset: TypeName,
    amount: u64,
    value: u64,
    shares: u128,
    tranche_shares: u128,
    locked_until_ms: u64,
) {
    event::emit(Deposited {
        vault_id,
        depositor,
        commitment_position,
        position_id,
        tranche,
        capital_generation,
        asset,
        amount,
        value,
        shares,
        tranche_shares,
        locked_until_ms,
    });
}

public(package) fun emit_withdraw_requested(
    vault_id: ID,
    global_seq: u64,
    lane: u8,
    position_id: ID,
    recipient: address,
    tranche: u8,
    capital_generation: u64,
    shares: u128,
    basis: u64,
    payout_asset: TypeName,
    requested_at_ms: u64,
) {
    event::emit(WithdrawRequested {
        vault_id,
        global_seq,
        lane,
        position_id,
        recipient,
        tranche,
        capital_generation,
        shares,
        basis,
        payout_asset,
        requested_at_ms,
    });
}

public(package) fun emit_payout_asset_amended(vault_id: ID, global_seq: u64, payout_asset: TypeName) {
    event::emit(PayoutAssetAmended { vault_id, global_seq, payout_asset });
}

public(package) fun emit_withdraw_fulfilled(
    vault_id: ID,
    global_seq: u64,
    lane: u8,
    recipient: address,
    tranche: u8,
    capital_generation: u64,
    shares: u128,
    value: u64,
    basis: u64,
    profit: u64,
    gross_fee: u64,
    protocol_cut: u64,
    curator_net: u64,
    curator_shares_minted: u128,
    payout: u64,
    payout_asset: TypeName,
    payout_units: u64,
    price: u128,
    tranche_shares: u128,
) {
    event::emit(WithdrawFulfilled {
        vault_id,
        global_seq,
        lane,
        recipient,
        tranche,
        capital_generation,
        shares,
        value,
        basis,
        profit,
        gross_fee,
        protocol_cut,
        curator_net,
        curator_shares_minted,
        payout,
        payout_asset,
        payout_units,
        price,
        tranche_shares,
    });
}

public(package) fun emit_capital_synced(
    vault_id: ID,
    total_nav: u128,
    senior_nav: u128,
    junior_nav: u128,
    senior_claim: u128,
    senior_shares: u128,
    junior_shares: u128,
    risk_state: u8,
    active_junior_generation: u64,
    curator_commitment_breached: bool,
) {
    event::emit(CapitalSynced {
        vault_id,
        total_nav,
        senior_nav,
        junior_nav,
        senior_claim,
        senior_shares,
        junior_shares,
        risk_state,
        active_junior_generation,
        curator_commitment_breached,
    });
}

public(package) fun emit_risk_state_changed(
    vault_id: ID,
    old_state: u8,
    new_state: u8,
    timestamp_ms: u64,
) {
    event::emit(RiskStateChanged { vault_id, old_state, new_state, timestamp_ms });
}

public(package) fun emit_junior_reset_proposed(
    vault_id: ID,
    old_generation: u64,
    proposed_at_ms: u64,
    executable_at_ms: u64,
    total_nav: u128,
    senior_claim: u128,
    senior_deficit: u128,
    required_deposit: u64,
) {
    event::emit(JuniorResetProposed {
        vault_id,
        old_generation,
        proposed_at_ms,
        executable_at_ms,
        total_nav,
        senior_claim,
        senior_deficit,
        required_deposit,
    });
}

public(package) fun emit_junior_reset_cancelled(vault_id: ID, old_generation: u64) {
    event::emit(JuniorResetCancelled { vault_id, old_generation });
}

public(package) fun emit_junior_reset_executed(
    vault_id: ID,
    old_generation: u64,
    new_generation: u64,
    recapitalizer: address,
    deposit_value: u64,
    post_junior_nav: u128,
    position_id: ID,
) {
    event::emit(JuniorResetExecuted {
        vault_id,
        old_generation,
        new_generation,
        recapitalizer,
        deposit_value,
        post_junior_nav,
        position_id,
    });
}

public(package) fun emit_commitment_released(
    vault_id: ID,
    curator_cap_id: ID,
    position_id: ID,
    shares: u128,
    basis: u64,
) {
    event::emit(CommitmentReleased { vault_id, curator_cap_id, position_id, shares, basis });
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

#[test_only]
public fun capital_synced_fields(e: &CapitalSynced): (u128, u128, u128, u128, u8, bool) {
    (
        e.total_nav,
        e.senior_nav,
        e.junior_nav,
        e.senior_claim,
        e.risk_state,
        e.curator_commitment_breached,
    )
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

public(package) fun emit_settlement_snapshot(
    vault_id: ID,
    final_nav: u128,
    senior_pool: u64,
    senior_supply: u128,
    junior_pool: u64,
    junior_supply: u128,
    active_junior_generation: u64,
) {
    event::emit(SettlementSnapshot {
        vault_id,
        final_nav,
        senior_pool,
        senior_supply,
        junior_pool,
        junior_supply,
        active_junior_generation,
    });
}

public(package) fun emit_settlement_redeemed(
    vault_id: ID,
    position_id: ID,
    from_queue: bool,
    global_seq: u64,
    recipient: address,
    tranche: u8,
    capital_generation: u64,
    shares: u128,
    entitlement: u64,
    basis: u64,
    gross_fee: u64,
    protocol_cut: u64,
    curator_net: u64,
    payout: u64,
) {
    event::emit(SettlementRedeemed {
        vault_id,
        position_id,
        from_queue,
        global_seq,
        recipient,
        tranche,
        capital_generation,
        shares,
        entitlement,
        basis,
        gross_fee,
        protocol_cut,
        curator_net,
        payout,
    });
}

public(package) fun emit_settlement_curator_fees_claimed(
    vault_id: ID,
    curator_cap_id: ID,
    amount: u64,
) {
    event::emit(SettlementCuratorFeesClaimed { vault_id, curator_cap_id, amount });
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
    max_deposit_assets: u64,
    paused: bool,
    max_senior_hurdle_bps: u64,
    min_target_junior_bps: u64,
    min_maintenance_junior_bps: u64,
    min_curator_commitment_bps: u64,
) {
    event::emit(ProtocolConfigUpdated {
        min_curator_share_bps,
        enforce_curator_share,
        max_curator_fee_bps,
        protocol_fee_bps,
        max_price_age_ms,
        max_deposit_assets,
        paused,
        max_senior_hurdle_bps,
        min_target_junior_bps,
        min_maintenance_junior_bps,
        min_curator_commitment_bps,
    });
}

public(package) fun emit_registrar_pubkey_set(pubkey: vector<u8>) {
    event::emit(RegistrarPubkeySet { pubkey });
}
