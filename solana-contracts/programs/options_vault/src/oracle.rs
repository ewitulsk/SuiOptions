//! Pyth wrapper — the port of `oracle.move` onto the Solana Pyth
//! receiver's `PriceUpdateV2` accounts. Turns two price updates into the
//! U/S cross in "settlement smallest-units per underlying smallest-unit"
//! as `(price_scaled: u128, scale: u8)` — the same shape as the bucket's
//! strike encoding, so strike/spot comparisons are exact integer math.
//!
//! The account is parsed manually rather than via pyth-solana-receiver-sdk
//! to avoid pinning a second anchor-lang version into the build; the
//! layout is defense-checked by owner + discriminator + feed id.
//!
//! Guardrails (identical to oracle.move): feed-ID pinning, publish-time
//! staleness (future skew tolerated), positive price, confidence-ratio
//! cap. Move's u256 rescale becomes checked u128 — overflow degrades to a
//! clean error (unreachable at the fixed scale 12 for real markets).

use anchor_lang::prelude::*;

use crate::error::VaultError;

/// Fixed output scale: the returned price is `price_scaled / 10^12`
/// (mirrors `oracle::ORACLE_PRICE_SCALE`).
pub const ORACLE_PRICE_SCALE: u8 = 12;

/// Largest exponent magnitude honored from a feed (mirrors oracle.move).
pub const MAX_EXPO_MAGNITUDE: u32 = 30;
pub const MAX_NET_EXPO: u32 = 38;

/// The Pyth pull-oracle receiver program that owns `PriceUpdateV2`
/// accounts (mainnet + devnet share this address).
pub const PYTH_RECEIVER_ID: Pubkey =
    anchor_lang::pubkey!("rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ");

/// sha256("account:PriceUpdateV2")[..8].
pub const PRICE_UPDATE_V2_DISCRIMINATOR: [u8; 8] = [34, 241, 35, 99, 157, 126, 244, 205];

pub struct ParsedPrice {
    pub feed_id: [u8; 32],
    pub price: i64,
    pub conf: u64,
    pub exponent: i32,
    pub publish_time: i64,
}

/// Manual parse of a `PriceUpdateV2`:
/// discriminator(8) ‖ write_authority(32) ‖ verification_level(1..2) ‖
/// PriceFeedMessage{feed_id 32, price i64, conf u64, exponent i32,
/// publish_time i64, prev_publish_time i64, ema_price i64, ema_conf u64}
/// ‖ posted_slot u64. Only fully-verified updates are accepted.
pub fn parse_price_update(info: &AccountInfo) -> Result<ParsedPrice> {
    require!(*info.owner == PYTH_RECEIVER_ID, VaultError::OracleFeedMismatch);
    let data = info.try_borrow_data()?;
    require!(data.len() >= 8 + 32 + 1, VaultError::OraclePriceInvalid);
    require!(
        data[..8] == PRICE_UPDATE_V2_DISCRIMINATOR,
        VaultError::OraclePriceInvalid
    );
    // verification_level: enum { Partial { num_signatures: u8 } = 0, Full = 1 }
    let level_tag = data[40];
    let msg_start = match level_tag {
        1 => 41,             // Full
        0 => 42,             // Partial { num_signatures } — rejected below
        _ => return err!(VaultError::OraclePriceInvalid),
    };
    require!(level_tag == 1, VaultError::OraclePriceInvalid);
    require!(data.len() >= msg_start + 32 + 8 + 8 + 4 + 8, VaultError::OraclePriceInvalid);

    let mut feed_id = [0u8; 32];
    feed_id.copy_from_slice(&data[msg_start..msg_start + 32]);
    let p = msg_start + 32;
    let price = i64::from_le_bytes(data[p..p + 8].try_into().unwrap());
    let conf = u64::from_le_bytes(data[p + 8..p + 16].try_into().unwrap());
    let exponent = i32::from_le_bytes(data[p + 16..p + 20].try_into().unwrap());
    let publish_time = i64::from_le_bytes(data[p + 20..p + 28].try_into().unwrap());
    Ok(ParsedPrice {
        feed_id,
        price,
        conf,
        exponent,
        publish_time,
    })
}

/// Extract and validate one leg: feed identity, staleness, positivity,
/// confidence ratio (mirrors `oracle::validated_price`).
pub fn validated_price(
    info: &AccountInfo,
    expected_feed: &[u8; 32],
    max_age_secs: u64,
    max_conf_bps: u64,
    now_secs: u64,
) -> Result<ParsedPrice> {
    let p = parse_price_update(info)?;
    require!(&p.feed_id == expected_feed, VaultError::OracleFeedMismatch);

    // Staleness; a publish time slightly in the future (skew) is fine.
    let publish = p.publish_time.max(0) as u64;
    if publish < now_secs {
        require!(now_secs - publish <= max_age_secs, VaultError::OraclePriceStale);
    }

    require!(p.price > 0, VaultError::OraclePriceInvalid);

    // conf / price ≤ max_conf_bps / 10⁴.
    require!(
        (p.conf as u128) * 10_000 <= (p.price as u128) * (max_conf_bps as u128),
        VaultError::OracleConfidence
    );
    Ok(p)
}

/// cross = (u_mag × 10^u_expo) / (s_mag × 10^s_expo) ×
///         10^(settlement_decimals − underlying_decimals),
/// emitted at `ORACLE_PRICE_SCALE` with floor division (mirrors
/// `oracle::cross_from_prices`).
pub fn cross_from_prices(
    u: &ParsedPrice,
    s: &ParsedPrice,
    underlying_decimals: u8,
    settlement_decimals: u8,
) -> Result<(u128, u8)> {
    let u_mag = u.price as u128;
    let s_mag = s.price as u128;

    let (u_expo_mag, u_expo_neg) = expo_parts(u.exponent)?;
    let (s_expo_mag, s_expo_neg) = expo_parts(s.exponent)?;
    let mut net_pos = ORACLE_PRICE_SCALE as u32 + settlement_decimals as u32;
    let mut net_neg = underlying_decimals as u32;
    if u_expo_neg { net_neg += u_expo_mag } else { net_pos += u_expo_mag };
    if s_expo_neg { net_pos += s_expo_mag } else { net_neg += s_expo_mag };
    let (num_exp, den_exp) = if net_pos >= net_neg {
        (net_pos - net_neg, 0u32)
    } else {
        (0u32, net_neg - net_pos)
    };
    require!(
        num_exp <= MAX_NET_EXPO && den_exp <= MAX_NET_EXPO,
        VaultError::OraclePriceInvalid
    );

    let numerator = u_mag
        .checked_mul(10u128.checked_pow(num_exp).ok_or(VaultError::MathOverflow)?)
        .ok_or(VaultError::MathOverflow)?;
    let denominator = s_mag
        .checked_mul(10u128.checked_pow(den_exp).ok_or(VaultError::MathOverflow)?)
        .ok_or(VaultError::MathOverflow)?;
    let cross = numerator / denominator;
    require!(cross > 0, VaultError::OraclePriceInvalid);
    Ok((cross, ORACLE_PRICE_SCALE))
}

fn expo_parts(expo: i32) -> Result<(u32, bool)> {
    let mag = expo.unsigned_abs();
    require!(mag <= MAX_EXPO_MAGNITUDE, VaultError::OraclePriceInvalid);
    Ok((mag, expo < 0))
}

/// The U/S cross with all guardrails applied (mirrors
/// `oracle::spot_cross`, driven by the vault's pinned config).
pub fn spot_cross(
    underlying_info: &AccountInfo,
    settlement_info: &AccountInfo,
    config: &crate::state::VaultConfig,
    now_secs: u64,
) -> Result<(u128, u8)> {
    let u = validated_price(
        underlying_info,
        &config.underlying_feed_id,
        config.max_price_age_secs,
        config.max_conf_bps,
        now_secs,
    )?;
    let s = validated_price(
        settlement_info,
        &config.settlement_feed_id,
        config.max_price_age_secs,
        config.max_conf_bps,
        now_secs,
    )?;
    cross_from_prices(&u, &s, config.underlying_decimals, config.settlement_decimals)
}
