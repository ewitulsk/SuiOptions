/// Oracle adapter for option coins (§10 "held-option-coin appraisal"):
/// mints a `PriceAttestation` for a bucket's fungible option coin so the
/// generic appraisal paths (custody `value_asset`, free-balance legs) can
/// price it like any other asset — no changes to vault core or the
/// DeepBook adapter.
///
/// Pricing is conservative INTRINSIC only, derived from the bucket's own
/// strike math plus ordinary (already-allowlisted) attestations for the
/// underlying and settlement legs — never from an order book, which a
/// depositor could manipulate:
///
///   call:  price(C→Q) = max(price(U→Q) − strike_leg, dust)
///   put:   price(P→Q) = max(strike_leg − price(U→Q), dust)
///   strike_leg = strike × price(S→Q) / 10^strike_scale
///
/// all at `price::price_scale()` (1e12) per RAW coin unit (option coins
/// mint 1:1 with underlying raw units). Post-expiry both coins are
/// worthless — exercise is pre-expiry only — so they mark at dust with no
/// input attestations required.
///
/// `price::attest` rejects a zero price as a broken-oracle guard, so a
/// worthless (OTM or expired) coin marks at 1 — a dust floor that
/// overstates value by at most amount/1e12 raw quote units.
///
/// The attestation's timestamp is the OLDEST input leg (the freshness
/// backstop sees the weakest link); legs equal to the quote asset are
/// 1:1 and contribute the current chain time.
///
/// Premium mark-to-market (SO-299 follow-up): on top of intrinsic, a
/// BOUNDED time-value term prices the optionality when the `VolBook`
/// carries a fresh keeper-posted vol for the underlying —
/// Brenner–Subrahmanyam at the money (0.4·S·σ·√T), decayed
/// hyperbolically away from the money, capped at the no-arbitrage
/// bound (call ≤ spot, put ≤ strike). No vol posted (or stale) means
/// extrinsic 0 — exactly the historical intrinsic-only mark; appraisals
/// never wedge on the vol path.
module options_adapter::options_oracle;

use std::type_name::{Self, TypeName};
use sui::clock::Clock;

use options_core::bucket::{Self, Bucket};
use options_core::put_bucket::{Self, PutBucket};

use options_adapter::vol_book::{Self, VolBook};

use trading_vault::price::{Self, PriceAttestation};
use trading_vault::registry::OracleRegistry;

const E_MISSING_ATTESTATION: u64 = 1;
const E_ATT_ASSET_MISMATCH: u64 = 2;
const E_ATT_QUOTE_MISMATCH: u64 = 3;
const E_PRICE_OVERFLOW: u64 = 4;

/// The dust floor for worthless coins (`price::attest` requires > 0).
const DUST_PRICE: u128 = 1;

const YEAR_MS: u128 = 31_536_000_000; // 365d

/// Witness minted only by this module; allowlist in `OracleRegistry`.
public struct OptionsOracle has drop {}

/// Price a call coin `C` into `Q` from its bucket's terms. `underlying_att`
/// / `settlement_att` are `U→Q` / `S→Q` attestations, each omittable when
/// that leg IS `Q` (1:1) — and both ignored once the bucket has expired.
public fun attest_call<U, S, C, Q>(
    reg: &OracleRegistry,
    bucket: &Bucket<U, S, C>,
    vol: &VolBook,
    underlying_att: Option<PriceAttestation>,
    settlement_att: Option<PriceAttestation>,
    clock: &Clock,
): PriceAttestation {
    let q = type_name::with_defining_ids<Q>();
    let now = clock.timestamp_ms();
    let c = type_name::with_defining_ids<C>();
    if (now >= bucket::expiry_ms(bucket)) {
        return price::attest(OptionsOracle {}, reg, c, q, DUST_PRICE, now)
    };
    let (price_u, ts_u) = leg_price<U>(q, underlying_att, now);
    let (price_s, ts_s) = leg_price<S>(q, settlement_att, now);
    let strike_leg = strike_in_quote(bucket::strike(bucket), bucket::strike_scale(bucket), price_s);
    let intrinsic = if (price_u > strike_leg) { price_u - strike_leg } else { 0 };
    let vol_bps = vol_book::current_vol_bps(vol, type_name::with_defining_ids<U>(), clock);
    let extrinsic =
        extrinsic_in_quote(vol_bps, price_u, strike_leg, bucket::expiry_ms(bucket) - now);
    // No-arbitrage bound: a call is never worth more than the underlying.
    let value = (intrinsic + extrinsic).min(price_u);
    price::attest(OptionsOracle {}, reg, c, q, value.max(DUST_PRICE), ts_u.min(ts_s))
}

/// Put twin: intrinsic = max(strike_leg − spot, dust). The holder must
/// deliver underlying to exercise, so the payout nets the spot cost.
public fun attest_put<U, S, P, Q>(
    reg: &OracleRegistry,
    bucket: &PutBucket<U, S, P>,
    vol: &VolBook,
    underlying_att: Option<PriceAttestation>,
    settlement_att: Option<PriceAttestation>,
    clock: &Clock,
): PriceAttestation {
    let q = type_name::with_defining_ids<Q>();
    let now = clock.timestamp_ms();
    let p = type_name::with_defining_ids<P>();
    if (now >= put_bucket::expiry_ms(bucket)) {
        return price::attest(OptionsOracle {}, reg, p, q, DUST_PRICE, now)
    };
    let (price_u, ts_u) = leg_price<U>(q, underlying_att, now);
    let (price_s, ts_s) = leg_price<S>(q, settlement_att, now);
    let strike_leg =
        strike_in_quote(put_bucket::strike(bucket), put_bucket::strike_scale(bucket), price_s);
    let intrinsic = if (strike_leg > price_u) { strike_leg - price_u } else { 0 };
    let vol_bps = vol_book::current_vol_bps(vol, type_name::with_defining_ids<U>(), clock);
    let extrinsic =
        extrinsic_in_quote(vol_bps, price_u, strike_leg, put_bucket::expiry_ms(bucket) - now);
    // No-arbitrage bound: a put is never worth more than its strike.
    let value = (intrinsic + extrinsic).min(strike_leg);
    price::attest(OptionsOracle {}, reg, p, q, value.max(DUST_PRICE), ts_u.min(ts_s))
}

// ═══════════════════════════════ internals ═══════════════════════════════

/// One input leg: `T == Q` is 1:1 at chain time (any attestation passed is
/// ignored — it has `drop`); otherwise the attestation is required and must
/// price exactly `T → Q`.
fun leg_price<T>(quote: TypeName, mut att: Option<PriceAttestation>, now: u64): (u128, u64) {
    let t = type_name::with_defining_ids<T>();
    if (t == quote) {
        return (price::price_scale(), now)
    };
    assert!(att.is_some(), E_MISSING_ATTESTATION);
    let a = att.extract();
    assert!(price::asset(&a) == t, E_ATT_ASSET_MISMATCH);
    assert!(price::quote_asset(&a) == quote, E_ATT_QUOTE_MISMATCH);
    (price::price(&a), price::timestamp_ms(&a))
}

/// Per-raw-underlying-unit strike cost in `Q`, at scale 1e12:
/// strike × price_s / 10^strike_scale, computed in u256. The linear
/// per-unit form matches the bucket's aggregate `apply_strike` to within
/// one raw unit per exercise (its rounding is on the aggregate).
fun strike_in_quote(strike: u128, strike_scale: u8, price_s: u128): u128 {
    // `create_bucket` caps strike_scale at 38, so pow10 cannot overflow.
    let mut divisor: u256 = 1;
    let mut i: u8 = 0;
    while (i < strike_scale) {
        divisor = divisor * 10;
        i = i + 1;
    };
    let v = (strike as u256) * (price_s as u256) / divisor;
    assert!(v <= (std::u128::max_value!() as u256), E_PRICE_OVERFLOW);
    v as u128
}

/// Bounded time value per raw unit in `Q` at 1e12 scale:
/// 0.4·S·σ·√T at the money (Brenner–Subrahmanyam), decayed
/// hyperbolically away from the money —
/// `atm · base / (base + 2·|S − K|)` with `base = min(S, K)` — and 0
/// whenever no fresh vol is posted. σ enters as annualized bps, √T in
/// 1e4 fixed point.
fun extrinsic_in_quote(vol_bps: u64, price_u: u128, strike_leg: u128, tt_ms: u64): u128 {
    if (vol_bps == 0 || price_u == 0 || strike_leg == 0) {
        return 0
    };
    // √(T years) in 1e4 fixed point: sqrt(tt_ms·1e8 / YEAR_MS).
    let sqrt_t = ((tt_ms as u128) * 100_000_000 / YEAR_MS).sqrt();
    // 0.4 = 2/5; vol_bps and sqrt_t each carry 1e4.
    let atm = (price_u as u256) * (vol_bps as u256) * (sqrt_t as u256) * 2 / 5 / 100_000_000;
    let (base, diff) = if (price_u > strike_leg) {
        ((strike_leg as u256), ((price_u - strike_leg) as u256))
    } else {
        ((price_u as u256), ((strike_leg - price_u) as u256))
    };
    let v = atm * base / (base + 2 * diff);
    assert!(v <= (std::u128::max_value!() as u256), E_PRICE_OVERFLOW);
    v as u128
}

#[test_only]
public fun strike_in_quote_for_testing(strike: u128, strike_scale: u8, price_s: u128): u128 {
    strike_in_quote(strike, strike_scale, price_s)
}

#[test_only]
public fun extrinsic_in_quote_for_testing(
    vol_bps: u64,
    price_u: u128,
    strike_leg: u128,
    tt_ms: u64,
): u128 {
    extrinsic_in_quote(vol_bps, price_u, strike_leg, tt_ms)
}
