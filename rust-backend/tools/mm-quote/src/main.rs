//! Price-to-premium simulator over the `mm-bot` primitives.
//!
//! Feed in the same inputs the bot would see (USD price of underlying and
//! settlement, a strike, an expiry, etc.) and this prints what the bot
//! would quote — including the intermediate quantities (spot scaled,
//! strike scaled, time-to-expiry, per-unit Black-Scholes price) so the
//! pricing path is easy to inspect by hand.
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
//!   --sigma 0.6 --rate 0.05
//! ```

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use clap::Parser;

use mm_bot::pricing::{
    compute_spot_from_prices, price_rfq, PriceDecision, PricingConfig, RfqPricingInputs, SpotError,
};
use protocol_types::sides::Side;

#[derive(Parser, Debug)]
#[command(name = "mm-quote", about = "Simulate the mm-bot's premium for an RFQ given asset prices.")]
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

    /// Quote TTL in ms — sets `valid_until_ms = now + ttl`.
    #[arg(long, default_value_t = 30_000)]
    quote_ttl_ms: u64,

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

    // The simulator exercises the pricing primitives directly — the pair gate
    // and api-service lookup live in the bot, so here we just feed the inputs
    // the user passed on the CLI straight into the pricer.
    let inputs = RfqPricingInputs {
        write_amount: args.write_amount,
        // Simulator always pretends retail is trading (buying calls) — only
        // the pricing primitives are exercised; `side` is informational.
        side: Side::Trader,
        strike: args.strike,
        strike_scale: args.strike_scale,
        expiry_ms,
    };

    let cfg = PricingConfig {
        rate: args.rate,
        quote_ttl_ms: args.quote_ttl_ms,
        // Simulator prints the Black-Scholes mid — no spread.
        ask_markup_bps: 0,
        bid_markdown_bps: 0,
    };
    let decision = price_rfq(&cfg, &inputs, spot_scaled, args.sigma, now);

    if args.json {
        print_json(&decision, spot_scaled, &inputs, now);
    } else {
        print_human(&decision, spot_scaled, &inputs, now, &args);
    }
    Ok(())
}

fn print_human(
    decision: &PriceDecision,
    spot_scaled: u64,
    inputs: &RfqPricingInputs,
    now: u64,
    args: &Args,
) {
    println!("inputs:");
    println!("  underlying_usd      = {}", args.underlying_usd);
    println!("  settlement_usd      = {}", args.settlement_usd);
    println!("  underlying_decimals = {}", args.underlying_decimals);
    println!("  settlement_decimals = {}", args.settlement_decimals);
    println!("  strike (raw)        = {}", inputs.strike);
    println!("  strike_scale        = {}", inputs.strike_scale);
    println!("  expiry_ms           = {}", inputs.expiry_ms);
    println!("  write_amount        = {}", inputs.write_amount);
    println!("  sigma               = {}", args.sigma);
    println!("  rate                = {}", args.rate);
    println!();
    println!("derived:");
    println!("  spot_scaled         = {spot_scaled}");
    println!("  now_ms              = {now}");
    println!();
    match decision {
        PriceDecision::Quote {
            premium,
            valid_until_ms,
            strike_scaled,
            t_years,
            per_unit,
            ..
        } => {
            println!("decision: QUOTE");
            println!("  strike_scaled      = {strike_scaled}");
            println!("  t_years            = {t_years}");
            println!("  per_unit (BS price)= {per_unit}");
            println!("  premium (raw)      = {premium}");
            println!("  valid_until_ms     = {valid_until_ms}");
        }
        PriceDecision::Decline { reason } => {
            println!("decision: DECLINE");
            println!("  reason             = {reason}");
        }
    }
}

fn print_json(decision: &PriceDecision, spot_scaled: u64, inputs: &RfqPricingInputs, now: u64) {
    // Hand-roll the JSON so we don't take a serde_json dep just for this.
    print!("{{");
    print!("\"now_ms\":{now},");
    print!("\"spot_scaled\":{spot_scaled},");
    print!("\"strike\":\"{}\",", inputs.strike);
    print!("\"strike_scale\":{},", inputs.strike_scale);
    print!("\"expiry_ms\":{},", inputs.expiry_ms);
    print!("\"write_amount\":{},", inputs.write_amount);
    match decision {
        PriceDecision::Quote {
            premium,
            valid_until_ms,
            strike_scaled,
            t_years,
            sigma,
            per_unit,
            ..
        } => {
            print!("\"decision\":\"quote\",");
            print!("\"strike_scaled\":{strike_scaled},");
            print!("\"t_years\":{t_years},");
            print!("\"sigma\":{sigma},");
            print!("\"per_unit\":{per_unit},");
            print!("\"premium\":{premium},");
            print!("\"valid_until_ms\":{valid_until_ms}");
        }
        PriceDecision::Decline { reason } => {
            print!("\"decision\":\"decline\",");
            print!("\"reason\":\"{}\"", reason.replace('"', "\\\""));
        }
    }
    println!("}}");
}
