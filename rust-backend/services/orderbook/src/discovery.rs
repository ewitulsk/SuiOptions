//! Runtime market discovery (SO-416): turn a chain `MarketCreatedEvent`
//! into a servable [`Market`].
//!
//! The chain is authoritative for everything it carries — registry id and
//! the base/quote strings exactly as makers must sign them (the event's
//! strings are `order::canonical_type<T>()` output, re-canonicalized here
//! as a belt), plus tick/min/fee. Two fields are off-chain conventions the
//! factory derives:
//!
//!  * `lot_size` — base units per price-tick denomination, `10^decimals`
//!    by the deployment-manager's convention. For a spot base the decimals
//!    come from the deployments token catalog; for an any-strike option
//!    coin they are the UNDERLYING's decimals (`option_coin::register_*`
//!    mints with the underlying's decimals), resolved by decoding the coin
//!    type back to its bucket spec.
//!  * `symbol` — a deterministic human handle; option series render as
//!    `TBTC-20261225-65000-C/TUSDC`.
//!
//! A base that is neither a catalog token nor a decodable option coin of
//! the core package is skipped with a warning — without decimals there is
//! no sane price grid, and serving a market with a garbage lot size is
//! worse than not serving it.

use exchange_types::{Market, SuiAddress};
use protocol_types::bucket_spec::{decode_option_coin_type, BucketSpec};

/// One resolvable token: canonical (exchange-form) coin type, symbol,
/// decimals — from the deployments record's token catalog.
#[derive(Clone, Debug)]
pub struct CatalogToken {
    pub canonical: String,
    pub symbol: String,
    pub decimals: u8,
}

pub struct MarketFactory {
    /// options_core package id — decodes option-coin bases. `None` disables
    /// option decoding (spot-only discovery).
    core_package: Option<String>,
    tokens: Vec<CatalogToken>,
}

impl MarketFactory {
    pub fn new(core_package: Option<String>, tokens: Vec<CatalogToken>) -> Self {
        MarketFactory { core_package, tokens }
    }

    fn token(&self, canonical: &str) -> Option<&CatalogToken> {
        self.tokens.iter().find(|t| t.canonical == canonical)
    }

    /// Build the servable market, or `None` (with a warning) when the base
    /// cannot be resolved to decimals.
    pub fn build(
        &self,
        registry_id: SuiAddress,
        base: &str,
        quote: &str,
        tick_size: u64,
        min_size: u64,
        fee_bps: u64,
    ) -> Option<Market> {
        let base = match exchange_types::canonicalize_move_type(base) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(registry = %registry_id, error = %e, "discovered market: bad base type");
                return None;
            }
        };
        let quote = match exchange_types::canonicalize_move_type(quote) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(registry = %registry_id, error = %e, "discovered market: bad quote type");
                return None;
            }
        };
        let quote_symbol = self
            .token(&quote)
            .map(|t| t.symbol.clone())
            .unwrap_or_else(|| short_name(&quote));

        let (base_symbol, decimals) = if let Some(t) = self.token(&base) {
            (t.symbol.clone(), t.decimals)
        } else if let Some((spec, underlying)) = self.decode_option(&base) {
            (option_symbol(&spec, &underlying.symbol), underlying.decimals)
        } else {
            tracing::warn!(
                registry = %registry_id,
                base,
                "discovered market: base is neither a catalog token nor a decodable \
                 option coin; skipping (no decimals => no price grid)"
            );
            return None;
        };

        Some(Market {
            symbol: format!("{base_symbol}/{quote_symbol}"),
            registry_id,
            base,
            quote,
            tick_size,
            min_size,
            lot_size: 10u64.pow(u32::from(decimals)),
            current_fee_bps: fee_bps,
        })
    }

    /// Decode an option-coin base and resolve its underlying in the catalog.
    fn decode_option(&self, base: &str) -> Option<(BucketSpec, &CatalogToken)> {
        let core = self.core_package.as_deref()?;
        let spec = decode_option_coin_type(core, base)?;
        // BucketSpec carries chain form (no 0x); the catalog is exchange
        // canonical — normalize before comparing (move-type-normalization).
        let underlying = exchange_types::canonicalize_move_type(&spec.asset)
            .ok()
            .and_then(|c| self.token(&c));
        match underlying {
            Some(t) => Some((spec, t)),
            None => {
                tracing::warn!(
                    underlying = spec.asset,
                    "option market's underlying not in token catalog; cannot derive decimals"
                );
                None
            }
        }
    }
}

/// `TBTC-20261225-65000-C` — underlying, UTC expiry date, strike, side.
fn option_symbol(spec: &BucketSpec, underlying_symbol: &str) -> String {
    let side = if spec.is_put { 'P' } else { 'C' };
    let date = utc_yyyymmdd(spec.expiry_ms);
    format!("{underlying_symbol}-{date}-{}-{side}", strike_str(spec.sig, spec.exp))
}

/// Render `sig / 10^exp` without floating point: `65000`, `0.5`, `1250.25`.
fn strike_str(sig: u64, exp: u8) -> String {
    if exp == 0 {
        return sig.to_string();
    }
    let digits = sig.to_string();
    let exp = exp as usize;
    if digits.len() > exp {
        let (int, frac) = digits.split_at(digits.len() - exp);
        format!("{int}.{frac}")
    } else {
        format!("0.{}{digits}", "0".repeat(exp - digits.len()))
    }
}

/// Civil date from a unix-ms timestamp (days-from-epoch algorithm), enough
/// for a display symbol.
fn utc_yyyymmdd(ms: u64) -> String {
    let days = (ms / 86_400_000) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}")
}

/// Fallback symbol for a token outside the catalog: the struct name.
fn short_name(canonical: &str) -> String {
    canonical.rsplit("::").next().unwrap_or(canonical).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol_types::bucket_spec::option_coin_type;

    const CORE: &str = "0xabcd";
    const TBTC: &str = "0x9b72409a9f38a8784420d17577aa6dbe5aa2ab4224cd04c44d8b515f6c97ba86::tbtc::TBTC";
    const TUSDC: &str = "0x11::tusdc::TUSDC";

    fn factory() -> MarketFactory {
        let canon = |s: &str| exchange_types::canonicalize_move_type(s).unwrap();
        MarketFactory::new(
            Some(CORE.to_owned()),
            vec![
                CatalogToken { canonical: canon(TBTC), symbol: "TBTC".into(), decimals: 8 },
                CatalogToken { canonical: canon(TUSDC), symbol: "TUSDC".into(), decimals: 6 },
            ],
        )
    }

    fn spec() -> BucketSpec {
        BucketSpec {
            asset: protocol_types::asset::chain_form_move_type(TBTC),
            settlement: protocol_types::asset::chain_form_move_type(TUSDC),
            // 2026-12-25 00:00 UTC, minute-aligned.
            expiry_ms: 1_798_156_800_000,
            sig: 65_000,
            exp: 0,
            is_put: false,
        }
    }

    #[test]
    fn builds_option_market_from_event_strings() {
        let f = factory();
        // The exact string the chain event carries: canonical_type<OptionCall<..>>.
        let base = exchange_types::canonicalize_move_type(
            &option_coin_type(CORE, &spec()).unwrap(),
        )
        .unwrap();
        let m = f
            .build(SuiAddress::parse("0x5c").unwrap(), &base, TUSDC, 1000, 10, 10)
            .expect("market");
        assert_eq!(m.symbol, "TBTC-20261225-65000-C/TUSDC");
        assert_eq!(m.lot_size, 100_000_000); // 10^8: the UNDERLYING's decimals
        assert_eq!(m.base, base);
        assert_eq!(m.tick_size, 1000);
    }

    #[test]
    fn builds_spot_market_from_catalog() {
        let f = factory();
        let m = f
            .build(SuiAddress::parse("0x5d").unwrap(), TBTC, TUSDC, 1000, 10, 10)
            .expect("market");
        assert_eq!(m.symbol, "TBTC/TUSDC");
        assert_eq!(m.lot_size, 100_000_000);
    }

    #[test]
    fn unresolvable_base_is_skipped() {
        let f = factory();
        assert!(f
            .build(
                SuiAddress::parse("0x5e").unwrap(),
                "0x99::mystery::COIN",
                TUSDC,
                1000,
                10,
                10
            )
            .is_none());
    }

    #[test]
    fn strike_rendering() {
        assert_eq!(strike_str(65_000, 0), "65000");
        assert_eq!(strike_str(5, 1), "0.5");
        assert_eq!(strike_str(125_025, 2), "1250.25");
        assert_eq!(strike_str(5, 3), "0.005");
    }
}
