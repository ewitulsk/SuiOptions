//! Price simulator over the desk's pricing primitives (SO-299).
//!
//! Feed in the same inputs the bot would see (USD price of underlying and
//! settlement, a strike, an expiry, etc.) and this prints the model fair
//! value — including the intermediate quantities (spot scaled, strike
//! scaled, time-to-expiry, per-unit BAW price) so the pricing path is
//! easy to inspect by hand. The old spread/smile quote model died in the
//! SO-299 strategy reset; this tool now prints the AMERICAN (BAW) fair
//! the desk marks against, at an explicit sigma.
//!
//! Examples:
//!
//! ```sh
//! # 30-day, ATM BTC call against USDC, $60k spot, σ=60%, write 1e8 raw.
//! cargo run -p mm-quote -- \
//!   --underlying-usd 60000 --settlement-usd 1.0 \
//!   --underlying-decimals 8 --settlement-decimals 6 \
//!   --strike 60000000 --strike-scale 3 \
//!   --days-to-expiry 30 --write-amount 100000000 \
//!   --sigma 0.6
//! ```

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use clap::{Parser, ValueEnum};

use mm_bot::pricing::{
    compute_spot_from_prices, rebase_strike_to_scale_zero, time_to_expiry_years, SpotError,
};
use pricing::american::{call_price_baw, put_price_baw, AmericanInputs};

/// Option product to price. `call` is the default so existing invocations are
/// unchanged; `put` switches to the American put pricer.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Product {
    Call,
    Put,
}

#[derive(Parser, Debug)]
#[command(name = "mm-quote", about = "Simulate the desk's model fair value for an option given asset prices.")]
struct Args {
    /// USD price of the underlying asset (e.g. BTC).
    #[arg(long)]
    underlying_usd: f64,

    /// USD price of the settlement asset (e.g. 1.0 for USDC).
    #[arg(long, default_value_t = 1.0)]
    settlement_usd: f64,

    /// Smallest-units exponent of the underlying coin type.
    #[arg(long, default_value_t = 8)]
    underlying_decimals: u8,

    /// Smallest-units exponent of the settlement coin type.
    #[arg(long, default_value_t = 6)]
    settlement_decimals: u8,

    /// Bucket strike at `strike_scale` (i.e. raw on-chain integer).
    #[arg(long)]
    strike: u128,

    /// Bucket strike scale (0..=9). Effective strike = strike / 10^scale.
    #[arg(long, default_value_t = 0)]
    strike_scale: u8,

    /// Time to expiry, in days. Mutually exclusive with `--expiry-ms`.
    #[arg(long, conflicts_with = "expiry_ms")]
    days_to_expiry: Option<f64>,

    /// Absolute expiry as Unix ms (a Sui clock-style timestamp). Mutually
    /// exclusive with `--days-to-expiry`.
    #[arg(long)]
    expiry_ms: Option<u64>,

    /// Write amount in *underlying* smallest-units.
    #[arg(long)]
    write_amount: u64,

    /// Annualized volatility, decimal (0.6 = 60%).
    #[arg(long)]
    sigma: f64,

    /// Annualized risk-free rate, continuous compounding (0.05 = 5%).
    #[arg(long, default_value_t = 0.0)]
    rate: f64,

    /// Annualized staking/carry yield of the underlying (BAW dividend
    /// rate; drives early-exercise optimality).
    #[arg(long, default_value_t = 0.0)]
    carry_yield: f64,

    /// `call` or `put`. Selects the American pricer. Defaults to `call`.
    #[arg(long, value_enum, default_value_t = Product::Call)]
    product: Product,

    /// Emit machine-readable JSON instead of a human-formatted block.
    #[arg(long)]
    json: bool,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Build the same scaled spot the live bot would compute.
    let spot_scaled = match compute_spot_from_prices(
        args.underlying_usd,
        args.settlement_usd,
        args.underlying_decimals,
        args.settlement_decimals,
    ) {
        Ok(s) => s,
        Err(SpotError::NonPositivePrice) => bail!("non-positive or non-finite price"),
        Err(SpotError::OutOfRange) => bail!("scaled spot out of range"),
        Err(e) => bail!("unexpected spot error: {:?}", e),
    };

    let now = now_ms();
    let expiry_ms = match (args.days_to_expiry, args.expiry_ms) {
        (Some(_), Some(_)) => bail!("--days-to-expiry and --expiry-ms are mutually exclusive"),
        (Some(d), None) => {
            if !(d.is_finite() && d >= 0.0) {
                bail!("--days-to-expiry must be a non-negative finite number");
            }
            now.saturating_add((d * 86_400_000.0) as u64)
        }
        (None, Some(ms)) => ms,
        (None, None) => bail!("one of --days-to-expiry or --expiry-ms is required"),
    };

    let strike_scaled = rebase_strike_to_scale_zero(args.strike, args.strike_scale);
    let t_years = time_to_expiry_years(expiry_ms, now);
    let inputs = AmericanInputs {
        spot: spot_scaled,
        strike: strike_scaled,
        t_years,
        sigma: args.sigma,
        rate: args.rate,
        carry_yield: args.carry_yield,
    };
    let per_unit = match args.product {
        Product::Call => call_price_baw(&inputs),
        Product::Put => put_price_baw(&inputs),
    };
    let premium = (per_unit * args.write_amount as f64).floor().max(0.0) as u64;

    if args.json {
        print!("{{");
        print!("\"now_ms\":{now},");
        print!("\"spot_scaled\":{spot_scaled},");
        print!("\"strike\":\"{}\",", args.strike);
        print!("\"strike_scale\":{},", args.strike_scale);
        print!("\"strike_scaled\":{strike_scaled},");
        print!("\"expiry_ms\":{expiry_ms},");
        print!("\"write_amount\":{},", args.write_amount);
        print!("\"t_years\":{t_years},");
        print!("\"sigma\":{},", args.sigma);
        print!("\"per_unit\":{per_unit},");
        print!("\"premium\":{premium}");
        println!("}}");
    } else {
        println!("inputs:");
        println!("  underlying_usd      = {}", args.underlying_usd);
        println!("  settlement_usd      = {}", args.settlement_usd);
        println!("  underlying_decimals = {}", args.underlying_decimals);
        println!("  settlement_decimals = {}", args.settlement_decimals);
        println!("  strike (raw)        = {}", args.strike);
        println!("  strike_scale        = {}", args.strike_scale);
        println!("  expiry_ms           = {expiry_ms}");
        println!("  write_amount        = {}", args.write_amount);
        println!("  sigma               = {}", args.sigma);
        println!("  rate                = {}", args.rate);
        println!("  carry_yield         = {}", args.carry_yield);
        println!();
        println!("derived:");
        println!("  spot_scaled         = {spot_scaled}");
        println!("  strike_scaled       = {strike_scaled}");
        println!("  t_years             = {t_years}");
        println!("  now_ms              = {now}");
        println!();
        println!("model fair (BAW American):");
        println!("  per_unit            = {per_unit}");
        println!("  premium (raw)       = {premium}");
    }
    Ok(())
}
