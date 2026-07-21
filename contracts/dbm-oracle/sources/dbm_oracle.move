/// Trustlessly computed equity oracle for a DeepBook Margin external
/// account (docs/mm-bot-v2/04-deepbook-margin-integration-plan.md §3a):
/// unlike the keeper-attested `equity_oracle`, the value here is DERIVED
/// on-chain inside the appraisal PTB — the `MarginManager` is a readable
/// shared object, so no operator input and no delta/interval guardrails
/// are needed. Plugs into the same `vault::record_external_equity`
/// surface with its own witness; pin `DbmOracle` on the vault via
/// `set_external_account`.
///
///   equity = value(assets) − value(debts), floored at zero,
///
/// where assets = manager balance-manager holdings + balances locked in
/// the manager's own DeepBook pool orders (`calculate_assets`), debts
/// come from `calculate_debts` at current interest accrual, and both
/// legs are valued into the vault's deposit asset through ordinary
/// allowlisted `PriceAttestation`s (a leg equal to the deposit asset is
/// 1:1). Asset values round down and debt values round up — equity is
/// conservatively understated, consistent with the vault's
/// conservative-marks policy. DEEP fee balances held by the manager are
/// ignored (a further understatement).
///
/// Binding checks: the manager's `owner` must BE the vault's registered
/// external account (a curator cannot point the appraisal at someone
/// else's richer manager), and the pool passed must be the manager's own
/// `deepbook_pool` (locked balances are read from it).
module dbm_oracle::dbm_oracle;

use std::type_name::{Self, TypeName};
use sui::clock::Clock;

use deepbook::pool::Pool;
use deepbook_margin::margin_manager::MarginManager;
use deepbook_margin::margin_pool::MarginPool;

use trading_vault::price::{Self, PriceAttestation};
use trading_vault::registry::{OracleRegistry, VaultProtocolConfig};
use trading_vault::vault::{Self, Appraisal, TradingVault};

const E_NOT_VAULT_ACCOUNT: u64 = 1;
const E_WRONG_POOL: u64 = 2;
const E_MISSING_ATTESTATION: u64 = 3;
const E_ATT_ASSET_MISMATCH: u64 = 4;
const E_HAS_DEBT: u64 = 5;
const E_OVERFLOW: u64 = 6;

/// Witness minted only here; allowlist in `OracleRegistry` and pin on the
/// vault as its `equity_oracle`.
public struct DbmOracle has drop {}

/// Record the equity leg for a debt-free manager (no `MarginPool` needed
/// — `calculate_debts` requires the pool the manager borrowed from and
/// aborts when it never borrowed).
public fun record_no_debt<Base, Quote>(
    vault: &TradingVault,
    reg: &OracleRegistry,
    cfg: &VaultProtocolConfig,
    a: &mut Appraisal,
    manager: &MarginManager<Base, Quote>,
    pool: &Pool<Base, Quote>,
    base_att: Option<PriceAttestation>,
    quote_att: Option<PriceAttestation>,
    clock: &Clock,
) {
    let (base_shares, quote_shares) = manager.borrowed_shares();
    assert!(base_shares == 0 && quote_shares == 0, E_HAS_DEBT);
    record_internal(vault, reg, cfg, a, manager, pool, 0, 0, base_att, quote_att, clock);
}

/// Record the equity leg for a manager with debt in `DebtAsset` (its one
/// borrowed side; `calculate_debts` verifies pool membership and asset).
public fun record<Base, Quote, DebtAsset>(
    vault: &TradingVault,
    reg: &OracleRegistry,
    cfg: &VaultProtocolConfig,
    a: &mut Appraisal,
    manager: &MarginManager<Base, Quote>,
    pool: &Pool<Base, Quote>,
    margin_pool: &MarginPool<DebtAsset>,
    base_att: Option<PriceAttestation>,
    quote_att: Option<PriceAttestation>,
    clock: &Clock,
) {
    let (base_shares, quote_shares) = manager.borrowed_shares();
    let (base_debt, quote_debt) = if (base_shares == 0 && quote_shares == 0) {
        (0, 0)
    } else {
        manager.calculate_debts(margin_pool, clock)
    };
    record_internal(
        vault, reg, cfg, a, manager, pool, base_debt, quote_debt, base_att, quote_att, clock,
    );
}

fun record_internal<Base, Quote>(
    vault: &TradingVault,
    reg: &OracleRegistry,
    cfg: &VaultProtocolConfig,
    a: &mut Appraisal,
    manager: &MarginManager<Base, Quote>,
    pool: &Pool<Base, Quote>,
    base_debt: u64,
    quote_debt: u64,
    base_att: Option<PriceAttestation>,
    quote_att: Option<PriceAttestation>,
    clock: &Clock,
) {
    assert!(manager.owner() == vault::external_account(vault), E_NOT_VAULT_ACCOUNT);
    assert!(manager.deepbook_pool() == object::id(pool), E_WRONG_POOL);

    let deposit_asset = vault::deposit_asset(vault);
    let price_base = leg_price<Base>(vault, cfg, deposit_asset, base_att, clock);
    let price_quote = leg_price<Quote>(vault, cfg, deposit_asset, quote_att, clock);

    let (base, quote) = manager.calculate_assets(pool);
    let scale = price::price_scale() as u256;
    // Assets round down, debts round up: equity is understated.
    let assets = (base as u256) * (price_base as u256) / scale
        + (quote as u256) * (price_quote as u256) / scale;
    let debts = ((base_debt as u256) * (price_base as u256) + scale - 1) / scale
        + ((quote_debt as u256) * (price_quote as u256) + scale - 1) / scale;

    let equity = if (assets > debts) { assets - debts } else { 0 };
    assert!(equity <= (std::u64::max_value!() as u256), E_OVERFLOW);
    vault::record_external_equity(vault, reg, a, DbmOracle {}, equity as u64);
}

/// One valuation leg into the vault's deposit asset: 1:1 when `T` IS the
/// deposit asset (any attestation passed is ignored); otherwise the
/// attestation is required, must price exactly `T` into the deposit
/// asset, and must be fresh under the protocol backstop. Mirrors
/// `options_oracle::leg_price`.
fun leg_price<T>(
    vault: &TradingVault,
    cfg: &VaultProtocolConfig,
    deposit_asset: TypeName,
    mut att: Option<PriceAttestation>,
    clock: &Clock,
): u128 {
    let t = type_name::with_defining_ids<T>();
    if (t == deposit_asset) {
        return price::price_scale()
    };
    assert!(att.is_some(), E_MISSING_ATTESTATION);
    let a = att.extract();
    assert!(price::asset(&a) == t, E_ATT_ASSET_MISMATCH);
    vault::check_attestation(vault, cfg, &a, clock);
    price::price(&a)
}
