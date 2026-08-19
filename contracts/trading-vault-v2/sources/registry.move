/// Protocol-level governance for v2 curated trading vaults: the fee/floor
/// knobs, the capital-structure floors and caps (§9.2 of the overhaul
/// plan), and the two allowlists. Trading venues (`IntegrationRegistry`)
/// and price sources (`OracleRegistry`) are governed independently.
/// Removal from either registry is an instant kill switch for new curator
/// sessions / attestations; it never strands funds already in custody —
/// the permissionless take-less exits (`begin_force_session`,
/// `begin_crank_session`) are deliberately ungated.
module vault_v2::registry;

use std::type_name::TypeName;
use sui::table::{Self, Table};
use sui::vec_set::{Self, VecSet};

use options_core::admin::AdminCap;

use vault_v2::errors;
use vault_v2::events;

// Defaults (basis points unless noted).
const DEFAULT_MIN_CURATOR_SHARE_BPS: u64 = 500;
const DEFAULT_MAX_CURATOR_FEE_BPS: u64 = 3_000;
/// Morpho-style: the protocol fee is a share OF the curator's
/// performance fee, not an extra fee on the user.
const DEFAULT_PROTOCOL_FEE_BPS: u64 = 1_000;
const DEFAULT_MAX_PRICE_AGE_MS: u64 = 60_000;
const DEFAULT_MAX_DEPOSIT_ASSETS: u64 = 8;
// Capital-structure floors/caps (§8.2–§8.4): creators may choose
// stricter values; the protocol bounds the range.
const DEFAULT_MAX_SENIOR_HURDLE_BPS: u64 = 2_000; // 20% annual
const DEFAULT_MIN_TARGET_JUNIOR_BPS: u64 = 1_000; // 10%
const DEFAULT_MIN_MAINTENANCE_JUNIOR_BPS: u64 = 500; // 5%
/// Curator first-loss commitment, as bps of total NAV in marked junior
/// value (§8.6).
const DEFAULT_MIN_CURATOR_COMMITMENT_BPS: u64 = 200; // 2%

const BPS_DENOM: u64 = 10_000;

public struct VaultProtocolConfig has key {
    id: UID,
    min_curator_share_bps: u64,
    /// Protocol-level disablement of the curator floor/commitment tests.
    enforce_curator_share: bool,
    max_curator_fee_bps: u64,
    /// Share of the curator's performance fee routed to the treasury.
    protocol_fee_bps: u64,
    max_price_age_ms: u64,
    max_deposit_assets: u64,
    /// Blocks new deposits protocol-wide; never blocks exits.
    paused: bool,
    /// Ed25519 pubkey of the protocol's external-account registrar
    /// (hedge-signer). Empty = the attested path is disabled.
    registrar_pubkey: vector<u8>,
    // Capital-structure bounds (v2).
    max_senior_hurdle_bps: u64,
    min_target_junior_bps: u64,
    min_maintenance_junior_bps: u64,
    min_curator_commitment_bps: u64,
}

/// Allowlist of integration-adapter witness types.
public struct IntegrationRegistry has key {
    id: UID,
    allowed: VecSet<TypeName>,
}

/// Allowlist of oracle-adapter witness types, plus optional per-asset
/// pins. `allowed` answers "may this adapter attest at all"; `pins`
/// narrows an asset to exactly one adapter.
public struct OracleRegistry has key {
    id: UID,
    allowed: VecSet<TypeName>,
    pins: Table<TypeName, TypeName>,
}

fun init(ctx: &mut TxContext) {
    transfer::share_object(VaultProtocolConfig {
        id: object::new(ctx),
        min_curator_share_bps: DEFAULT_MIN_CURATOR_SHARE_BPS,
        enforce_curator_share: true,
        max_curator_fee_bps: DEFAULT_MAX_CURATOR_FEE_BPS,
        protocol_fee_bps: DEFAULT_PROTOCOL_FEE_BPS,
        max_price_age_ms: DEFAULT_MAX_PRICE_AGE_MS,
        max_deposit_assets: DEFAULT_MAX_DEPOSIT_ASSETS,
        paused: false,
        registrar_pubkey: vector[],
        max_senior_hurdle_bps: DEFAULT_MAX_SENIOR_HURDLE_BPS,
        min_target_junior_bps: DEFAULT_MIN_TARGET_JUNIOR_BPS,
        min_maintenance_junior_bps: DEFAULT_MIN_MAINTENANCE_JUNIOR_BPS,
        min_curator_commitment_bps: DEFAULT_MIN_CURATOR_COMMITMENT_BPS,
    });
    transfer::share_object(IntegrationRegistry { id: object::new(ctx), allowed: vec_set::empty() });
    transfer::share_object(OracleRegistry {
        id: object::new(ctx),
        allowed: vec_set::empty(),
        pins: table::new(ctx),
    });
}

// ═══════════════════════════════ admin ═══════════════════════════════

public fun allow_adapter(_: &AdminCap, reg: &mut IntegrationRegistry, adapter: TypeName) {
    reg.allowed.insert(adapter);
    events::emit_adapter_allowed(adapter);
}

public fun disallow_adapter(_: &AdminCap, reg: &mut IntegrationRegistry, adapter: TypeName) {
    reg.allowed.remove(&adapter);
    events::emit_adapter_disallowed(adapter);
}

public fun allow_oracle(_: &AdminCap, reg: &mut OracleRegistry, oracle: TypeName) {
    reg.allowed.insert(oracle);
    events::emit_oracle_allowed(oracle);
}

public fun disallow_oracle(_: &AdminCap, reg: &mut OracleRegistry, oracle: TypeName) {
    reg.allowed.remove(&oracle);
    events::emit_oracle_disallowed(oracle);
}

/// Pin `asset` to exactly one oracle adapter (must be allowlisted).
public fun pin_oracle(
    _: &AdminCap,
    reg: &mut OracleRegistry,
    asset: TypeName,
    oracle: TypeName,
) {
    assert!(reg.allowed.contains(&oracle), errors::oracle_not_allowed());
    if (reg.pins.contains(asset)) {
        *reg.pins.borrow_mut(asset) = oracle;
    } else {
        reg.pins.add(asset, oracle);
    };
    events::emit_oracle_pinned(asset, oracle);
}

public fun unpin_oracle(_: &AdminCap, reg: &mut OracleRegistry, asset: TypeName) {
    reg.pins.remove(asset);
    events::emit_oracle_unpinned(asset);
}

public fun set_min_curator_share_bps(_: &AdminCap, cfg: &mut VaultProtocolConfig, bps: u64) {
    assert!(bps <= BPS_DENOM, errors::config_invalid());
    cfg.min_curator_share_bps = bps;
    emit_config(cfg);
}

public fun set_enforce_curator_share(_: &AdminCap, cfg: &mut VaultProtocolConfig, on: bool) {
    cfg.enforce_curator_share = on;
    emit_config(cfg);
}

public fun set_max_curator_fee_bps(_: &AdminCap, cfg: &mut VaultProtocolConfig, bps: u64) {
    assert!(bps <= BPS_DENOM, errors::config_invalid());
    cfg.max_curator_fee_bps = bps;
    emit_config(cfg);
}

public fun set_protocol_fee_bps(_: &AdminCap, cfg: &mut VaultProtocolConfig, bps: u64) {
    assert!(bps <= BPS_DENOM, errors::config_invalid());
    cfg.protocol_fee_bps = bps;
    emit_config(cfg);
}

public fun set_max_price_age_ms(_: &AdminCap, cfg: &mut VaultProtocolConfig, ms: u64) {
    assert!(ms > 0, errors::config_invalid());
    cfg.max_price_age_ms = ms;
    emit_config(cfg);
}

public fun set_max_deposit_assets(_: &AdminCap, cfg: &mut VaultProtocolConfig, n: u64) {
    assert!(n > 0, errors::config_invalid());
    cfg.max_deposit_assets = n;
    emit_config(cfg);
}

public fun set_paused(_: &AdminCap, cfg: &mut VaultProtocolConfig, paused: bool) {
    cfg.paused = paused;
    emit_config(cfg);
}

public fun set_max_senior_hurdle_bps(_: &AdminCap, cfg: &mut VaultProtocolConfig, bps: u64) {
    assert!(bps <= BPS_DENOM, errors::config_invalid());
    cfg.max_senior_hurdle_bps = bps;
    emit_config(cfg);
}

public fun set_min_target_junior_bps(_: &AdminCap, cfg: &mut VaultProtocolConfig, bps: u64) {
    assert!(bps > 0 && bps < BPS_DENOM, errors::config_invalid());
    cfg.min_target_junior_bps = bps;
    emit_config(cfg);
}

public fun set_min_maintenance_junior_bps(_: &AdminCap, cfg: &mut VaultProtocolConfig, bps: u64) {
    assert!(bps > 0 && bps < BPS_DENOM, errors::config_invalid());
    cfg.min_maintenance_junior_bps = bps;
    emit_config(cfg);
}

public fun set_min_curator_commitment_bps(_: &AdminCap, cfg: &mut VaultProtocolConfig, bps: u64) {
    assert!(bps <= BPS_DENOM, errors::config_invalid());
    cfg.min_curator_commitment_bps = bps;
    emit_config(cfg);
}

/// Seed (or clear) the registrar pubkey that gates curator self-serve
/// external-account registration. Empty disables the attested path.
public fun set_registrar_pubkey(_: &AdminCap, cfg: &mut VaultProtocolConfig, pubkey: vector<u8>) {
    assert!(pubkey.length() == 32 || pubkey.is_empty(), errors::config_invalid());
    cfg.registrar_pubkey = pubkey;
    events::emit_registrar_pubkey_set(cfg.registrar_pubkey);
}

fun emit_config(cfg: &VaultProtocolConfig) {
    events::emit_protocol_config_updated(
        cfg.min_curator_share_bps,
        cfg.enforce_curator_share,
        cfg.max_curator_fee_bps,
        cfg.protocol_fee_bps,
        cfg.max_price_age_ms,
        cfg.max_deposit_assets,
        cfg.paused,
        cfg.max_senior_hurdle_bps,
        cfg.min_target_junior_bps,
        cfg.min_maintenance_junior_bps,
        cfg.min_curator_commitment_bps,
    );
}

// ══════════════════════════════ getters ══════════════════════════════

public fun is_adapter_allowed(reg: &IntegrationRegistry, adapter: &TypeName): bool {
    reg.allowed.contains(adapter)
}

public fun is_oracle_allowed(reg: &OracleRegistry, oracle: &TypeName): bool {
    reg.allowed.contains(oracle)
}

/// Is `oracle` permitted to price `asset`? Allowlisted AND, when the
/// asset carries a pin, equal to it.
public fun is_oracle_allowed_for(
    reg: &OracleRegistry,
    oracle: &TypeName,
    asset: &TypeName,
): bool {
    if (!reg.allowed.contains(oracle)) return false;
    if (!reg.pins.contains(*asset)) return true;
    reg.pins.borrow(*asset) == oracle
}

public fun has_oracle_pin(reg: &OracleRegistry, asset: &TypeName): bool {
    reg.pins.contains(*asset)
}

/// Aborts when `asset` is unpinned — pair with [`has_oracle_pin`].
public fun oracle_pin(reg: &OracleRegistry, asset: &TypeName): TypeName {
    *reg.pins.borrow(*asset)
}

public fun min_curator_share_bps(cfg: &VaultProtocolConfig): u64 { cfg.min_curator_share_bps }

public fun enforce_curator_share(cfg: &VaultProtocolConfig): bool { cfg.enforce_curator_share }

public fun max_curator_fee_bps(cfg: &VaultProtocolConfig): u64 { cfg.max_curator_fee_bps }

public fun protocol_fee_bps(cfg: &VaultProtocolConfig): u64 { cfg.protocol_fee_bps }

public fun max_price_age_ms(cfg: &VaultProtocolConfig): u64 { cfg.max_price_age_ms }

public fun max_deposit_assets(cfg: &VaultProtocolConfig): u64 { cfg.max_deposit_assets }

public fun is_paused(cfg: &VaultProtocolConfig): bool { cfg.paused }

public fun registrar_pubkey(cfg: &VaultProtocolConfig): &vector<u8> { &cfg.registrar_pubkey }

public fun max_senior_hurdle_bps(cfg: &VaultProtocolConfig): u64 { cfg.max_senior_hurdle_bps }

public fun min_target_junior_bps(cfg: &VaultProtocolConfig): u64 { cfg.min_target_junior_bps }

public fun min_maintenance_junior_bps(cfg: &VaultProtocolConfig): u64 {
    cfg.min_maintenance_junior_bps
}

public fun min_curator_commitment_bps(cfg: &VaultProtocolConfig): u64 {
    cfg.min_curator_commitment_bps
}

#[test_only]
public fun init_for_testing(ctx: &mut TxContext) {
    init(ctx)
}
