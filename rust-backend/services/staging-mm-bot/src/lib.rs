//! Library surface for the `staging-mm-bot` binary.
//!
//! A staging-only maker bot for the hybrid exchange: mints test tokens from
//! the permissionless faucet and quotes tick-snapped bid/ask ladders around
//! the oracle-service mid on every `{SYM}/TUSDC` market the orderbook
//! serves. Funded-BM mode escrows the mints in its own `BalanceManager`;
//! vault-direct mode (SO-372/SO-375) quotes a trading vault's free balances
//! and tops them up with real attested `vault::deposit`s instead. Pure
//! decisions live in [`ladder`]; the async loops live in `main.rs`.

use std::path::PathBuf;

use clap::Parser;

pub mod client;
pub mod ladder;
pub mod server;
pub mod signing;
pub mod positions;
pub mod vault;

#[derive(Parser, Debug)]
#[command(name = "staging-mm-bot", about = "Faucet-funded maker bot for the hybrid exchange (staging)")]
pub struct Cli {
    #[arg(short, long, default_value = "services/staging-mm-bot/config/config.toml")]
    pub config: PathBuf,

    /// Per-binary secrets TOML. Holds the Sui signing key under the network
    /// selected by `network` in the bot config. The same key owns the
    /// BalanceManager, signs orders, and pays gas.
    #[arg(short = 's', long, default_value = "services/staging-mm-bot/config/secrets.toml")]
    pub secrets: PathBuf,

    /// Base URL of the orderbook service (markets, order intake, balances).
    #[arg(long, env = "ORDERBOOK_URL", default_value = "http://127.0.0.1:9014")]
    pub orderbook_url: String,

    /// Base URL of the token-info service. Source of the token catalog,
    /// decimals, Pyth feed ids, and the test-token faucet ids.
    #[arg(long, env = "TOKEN_INFO_URL", default_value = "http://127.0.0.1:9005")]
    pub token_info_url: String,

    /// Base URL of the oracle-service: live prices over its WS fanout.
    #[arg(long, env = "ORACLE_URL", default_value = "http://127.0.0.1:9013")]
    pub oracle_url: String,

    /// Indexer GraphQL endpoint. Vault-direct mode resolves its vault
    /// (self-created discovery, custody wiring state) from here; unused in
    /// funded-BM mode.
    #[arg(
        long,
        env = "INDEXER_GRAPHQL_URL",
        default_value = "http://127.0.0.1:9002/graphql"
    )]
    pub indexer_graphql_url: String,

    #[arg(long, default_value_t = 200_000_000)]
    pub gas_budget: u64,
}

cli_spec::define_program! {
    id          = "staging-mm-bot",
    cargo_pkg   = "staging-mm-bot",
    working_dir = ".",
    description = "Staging maker bot for the hybrid exchange. Mints test tokens from the \
                   faucet into a BalanceManager escrow and quotes oracle-tracking bid/ask \
                   ladders on every orderbook market.",
    cli         = crate::Cli,
}

/// Per-key exponential backoff for the funding pass (SO-461): a deposit
/// that keeps aborting on chain (SO-432's unattested oracle) must not be
/// resubmitted every tick — each failed attempt still pays gas.
pub mod funding_backoff {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    pub const FIRST: Duration = Duration::from_secs(5 * 60);
    pub const MAX: Duration = Duration::from_secs(6 * 3600);

    /// Delay after the `failures`-th consecutive failure: 5 min doubling
    /// to a 6 h ceiling.
    pub fn delay(failures: u32) -> Duration {
        if failures == 0 {
            return Duration::ZERO;
        }
        let mult = 1u32 << (failures - 1).min(20);
        FIRST.saturating_mul(mult).min(MAX)
    }

    #[derive(Debug, Clone, Copy)]
    pub struct Streak {
        pub failures: u32,
        pub until: Instant,
    }

    #[derive(Debug, Default)]
    pub struct Backoff {
        streaks: HashMap<String, Streak>,
    }

    impl Backoff {
        /// `Some(remaining)` while `key` is still backing off.
        pub fn blocked(&self, key: &str, now: Instant) -> Option<Duration> {
            self.streaks.get(key).and_then(|s| s.until.checked_duration_since(now)).filter(|d| !d.is_zero())
        }

        /// Record a failure; returns the streak length (1 = first of a run,
        /// the one worth an alert).
        pub fn fail(&mut self, key: &str, now: Instant) -> u32 {
            let failures = self.streaks.get(key).map(|s| s.failures).unwrap_or(0) + 1;
            self.streaks.insert(key.to_string(), Streak { failures, until: now + delay(failures) });
            failures
        }

        pub fn succeed(&mut self, key: &str) {
            self.streaks.remove(key);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn delay_doubles_from_five_minutes_and_caps_at_six_hours() {
            assert_eq!(delay(0), Duration::ZERO);
            assert_eq!(delay(1), Duration::from_secs(300));
            assert_eq!(delay(2), Duration::from_secs(600));
            assert_eq!(delay(5), Duration::from_secs(4800));
            assert_eq!(delay(7), Duration::from_secs(19_200));
            assert_eq!(delay(8), MAX);
            assert_eq!(delay(40), MAX);
        }

        #[test]
        fn streaks_block_then_clear_on_success() {
            let mut b = Backoff::default();
            let t0 = Instant::now();
            assert!(b.blocked("TSUI", t0).is_none());
            assert_eq!(b.fail("TSUI", t0), 1);
            assert!(b.blocked("TSUI", t0 + Duration::from_secs(299)).is_some());
            assert!(b.blocked("TSUI", t0 + Duration::from_secs(301)).is_none());
            assert_eq!(b.fail("TSUI", t0 + Duration::from_secs(301)), 2);
            assert!(b.blocked("TSUI", t0 + Duration::from_secs(301 + 599)).is_some());
            assert!(b.blocked("TWAL", t0).is_none(), "keys are independent");
            b.succeed("TSUI");
            assert!(b.blocked("TSUI", t0 + Duration::from_secs(302)).is_none());
            assert_eq!(b.fail("TSUI", t0), 1, "success resets the streak");
        }
    }
}
