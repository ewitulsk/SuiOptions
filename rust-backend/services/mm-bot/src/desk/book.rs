//! The desk's book — single source of truth (00-plan Phase 2).
//!
//! Tracks held option inventory (vault custody + wallet float), written
//! positions, NAV, the reservation ledger (`reservations + deployed ≤ NAV`
//! before every quote), and realized P&L attribution counters
//! (spread / scalp / theta / funding) exported as metrics and appended to
//! a JSONL file.
//!
//! On boot the inventory is reconstructed from VAULT custody:
//!   - NAV from the indexer's `trading_vaults` view
//!     (`latest_pps_e12 × total_shares / 1e12`, deposit-asset raw units),
//!     falling back to the vault's settlement free balance
//!     (`vault::free_balance_of<Settlement>` dev-inspect) when the vault
//!     has no observed pps yet. **Documented choice**: pps×shares is the
//!     appraised NAV (positions included); the free-balance fallback
//!     under-counts by design and only covers a freshly-created vault.
//!   - Held option coins per live bucket via
//!     `vault::free_balance_of<OptionCoin>` dev-inspect (the
//!     `custody_balance` pattern from the old vault_deepbook quoter),
//!     plus the bot wallet's own float of the same coin types.
//!   - Written positions: TODO(SO-299) — no V2 writes exist yet; the
//!     ledger starts empty and is populated at write time. Reconstruction
//!     from vault-held `Position`s lands with the V2 desk.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use protocol_types::ids::ObjectId;
use serde::Serialize;
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::TransactionKind;

use super::model::Greeks;

/// One held option line (long calls/puts bought from retail).
#[derive(Clone, Debug)]
pub struct Holding {
    pub bucket_id: ObjectId,
    /// The bucket's fungible option-coin type.
    pub option_coin_type: String,
    pub asset_coin_type: String,
    pub settlement_coin_type: String,
    pub is_put: bool,
    pub strike: u128,
    pub strike_scale: u8,
    pub expiry_ms: u64,
    /// Units held in the VAULT's free balances (curator custody).
    pub amount_vault: u64,
    /// Units held in the bot wallet (auction winnings pending sweep, or
    /// coins staged for exit execution).
    pub amount_wallet: u64,
    /// The bucket's DeepBook option pool, when one exists (resale venue).
    pub pool_id: Option<String>,
}

impl Holding {
    pub fn amount(&self) -> u64 {
        self.amount_vault.saturating_add(self.amount_wallet)
    }
    pub fn strike_scaled(&self) -> f64 {
        self.strike as f64 / 10f64.powi(self.strike_scale as i32)
    }
}

/// One written (short) option line — V2 trader flow.
#[derive(Clone, Debug)]
pub struct Written {
    pub bucket_id: ObjectId,
    pub is_put: bool,
    pub strike: u128,
    pub strike_scale: u8,
    pub expiry_ms: u64,
    pub amount: u64,
    /// Of `amount`, how many units are covered by a held long in the same
    /// series (netting). `amount - covered` is naked short budget usage.
    pub covered: u64,
}

impl Written {
    pub fn naked(&self) -> u64 {
        self.amount.saturating_sub(self.covered)
    }
}

/// A premium reservation held while a signed quote is outstanding.
#[derive(Clone, Debug)]
pub struct Reservation {
    pub amount: u64,
    pub expires_ms: u64,
}

/// Realized P&L attribution counters, settlement raw units.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Pnl {
    pub spread: f64,
    pub scalp: f64,
    pub theta: f64,
    pub funding: f64,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PnlLine {
    Spread,
    Scalp,
    Theta,
    Funding,
}

#[derive(Serialize)]
struct PnlRecord<'a> {
    ts_ms: u64,
    line: PnlLine,
    amount: f64,
    note: &'a str,
}

/// Aggregated greeks for a set of positions, in book units:
/// delta/gamma in underlying raw units, vega/theta in settlement raw.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GreeksAgg {
    pub delta_units: f64,
    pub gamma_units: f64,
    /// Premium change per 1.0 of vol (divide by 100 for per vol pt).
    pub vega: f64,
    /// Premium change per calendar day (negative = decay cost) —
    /// `pricing::Greeks::theta` convention.
    pub theta_per_day: f64,
}

impl GreeksAgg {
    fn add(&mut self, g: &Greeks, amount: f64, sign: f64) {
        self.delta_units += sign * g.delta * amount;
        self.gamma_units += sign * g.gamma * amount;
        self.vega += sign * g.vega * amount;
        self.theta_per_day += sign * g.theta * amount;
    }
}

/// Why a reservation was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReserveError {
    /// reservations + deployed + amount would exceed NAV.
    ExceedsNav,
}

/// The book. Wrapped in a lock by the desk; all methods are synchronous.
pub struct Book {
    /// NAV in settlement raw units (see module docs for the source).
    pub nav: u64,
    /// Mark-to-model premium currently deployed in held options,
    /// settlement raw. Refreshed by the desk's book-refresh tick.
    pub deployed: u64,
    pub holdings: Vec<Holding>,
    pub written: Vec<Written>,
    reservations: HashMap<u64, Reservation>,
    next_reservation_id: u64,
    pub pnl: Pnl,
    /// JSONL sink for P&L attribution records (append-only).
    pnl_path: Option<PathBuf>,
}

impl Book {
    pub fn new(nav: u64, pnl_path: Option<PathBuf>) -> Self {
        Self {
            nav,
            deployed: 0,
            holdings: Vec::new(),
            written: Vec::new(),
            reservations: HashMap::new(),
            next_reservation_id: 1,
            pnl: Pnl::default(),
            pnl_path,
        }
    }

    // ── reservation ledger ────────────────────────────────────────────

    pub fn reserved_total(&self) -> u64 {
        self.reservations.values().map(|r| r.amount).sum()
    }

    /// Reserve `amount` of premium for an outstanding quote. Enforces
    /// `reservations + deployed ≤ NAV`.
    pub fn reserve(&mut self, amount: u64, ttl_ms: u64, now_ms: u64) -> Result<u64, ReserveError> {
        self.expire_reservations(now_ms);
        let committed = self.reserved_total() as u128 + self.deployed as u128 + amount as u128;
        if committed > self.nav as u128 {
            return Err(ReserveError::ExceedsNav);
        }
        let id = self.next_reservation_id;
        self.next_reservation_id += 1;
        self.reservations.insert(
            id,
            Reservation {
                amount,
                expires_ms: now_ms.saturating_add(ttl_ms),
            },
        );
        Ok(id)
    }

    pub fn release_reservation(&mut self, id: u64) {
        self.reservations.remove(&id);
    }

    pub fn expire_reservations(&mut self, now_ms: u64) {
        self.reservations.retain(|_, r| r.expires_ms > now_ms);
    }

    // ── inventory ─────────────────────────────────────────────────────

    /// Net naked short units across all written lines (V2 budget).
    pub fn naked_written_units(&self) -> u64 {
        self.written.iter().map(Written::naked).sum()
    }

    /// Net greeks per expiry (ms) bucket and in total. Longs count
    /// positive, written shorts negative. `marks` maps bucket_id →
    /// per-unit greeks (computed by the caller via [`MarketModel`], so
    /// this stays pure and unit-testable).
    pub fn net_greeks(
        &self,
        per_unit: &HashMap<ObjectId, Greeks>,
    ) -> (HashMap<u64, GreeksAgg>, GreeksAgg) {
        let mut by_expiry: HashMap<u64, GreeksAgg> = HashMap::new();
        let mut total = GreeksAgg::default();
        for h in &self.holdings {
            if let Some(g) = per_unit.get(&h.bucket_id) {
                let amt = h.amount() as f64;
                by_expiry.entry(h.expiry_ms).or_default().add(g, amt, 1.0);
                total.add(g, amt, 1.0);
            }
        }
        for w in &self.written {
            if let Some(g) = per_unit.get(&w.bucket_id) {
                let amt = w.amount as f64;
                by_expiry.entry(w.expiry_ms).or_default().add(g, amt, -1.0);
                total.add(g, amt, -1.0);
            }
        }
        (by_expiry, total)
    }

    // ── P&L attribution ───────────────────────────────────────────────

    /// Record a realized P&L line: bumps the counter, exports the metric,
    /// appends a JSONL record.
    pub fn record_pnl(&mut self, line: PnlLine, amount: f64, note: &str, now_ms: u64) {
        match line {
            PnlLine::Spread => self.pnl.spread += amount,
            PnlLine::Scalp => self.pnl.scalp += amount,
            PnlLine::Theta => self.pnl.theta += amount,
            PnlLine::Funding => self.pnl.funding += amount,
        }
        let label = match line {
            PnlLine::Spread => "spread",
            PnlLine::Scalp => "scalp",
            PnlLine::Theta => "theta",
            PnlLine::Funding => "funding",
        };
        metrics::counter!("mm_desk_pnl_total", "line" => label)
            .increment(amount.abs().round() as u64);
        metrics::gauge!("mm_desk_pnl", "line" => label).set(match line {
            PnlLine::Spread => self.pnl.spread,
            PnlLine::Scalp => self.pnl.scalp,
            PnlLine::Theta => self.pnl.theta,
            PnlLine::Funding => self.pnl.funding,
        });
        if let Some(path) = &self.pnl_path {
            let rec = PnlRecord { ts_ms: now_ms, line, amount, note };
            if let Err(e) = append_jsonl(path, &rec) {
                tracing::warn!(error = %format!("{e:#}"), "pnl jsonl append failed");
            }
        }
    }
}

fn append_jsonl<T: Serialize>(path: &PathBuf, rec: &T) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    serde_json::to_writer(&mut f, rec)?;
    f.write_all(b"\n")?;
    Ok(())
}

// ── boot reconstruction ────────────────────────────────────────────────

/// Everything reconstruction needs (kept together so `spawn_desk` stays
/// readable).
pub struct ReconstructParams<'a> {
    pub wrap: &'a sui_tx::sui_client::SuiClientWrapper,
    pub indexer: &'a indexer_graphql::IndexerClient,
    pub api: &'a api_service_client::ApiServiceClient,
    pub trading_vault_package: ObjectID,
    pub vault_id: ObjectID,
    pub settlement_coin_type: String,
    pub pnl_path: Option<PathBuf>,
}

/// Reconstruct the book from vault custody (module docs describe the
/// sources and the NAV choice).
pub async fn reconstruct(p: ReconstructParams<'_>) -> Result<Book> {
    // NAV: appraised pps × shares from the indexer view, else settlement
    // free balance.
    let vault_hex = p.vault_id.to_hex_literal();
    let vaults = p.indexer.trading_vaults().await.context("indexer trading_vaults")?;
    let ours = vaults
        .iter()
        .find(|v| v.vault_id.to_hex() == vault_hex || format!("0x{}", v.vault_id.to_hex()) == vault_hex);
    let nav = match ours {
        Some(v) => match v.latest_pps_e12 {
            Some(pps) => u64::try_from(pps.saturating_mul(v.total_shares) / 1_000_000_000_000u128)
                .unwrap_or(u64::MAX),
            None => {
                free_balance_of(p.wrap, p.trading_vault_package, p.vault_id, &p.settlement_coin_type)
                    .await
                    .unwrap_or(0)
            }
        },
        None => {
            tracing::warn!(vault = %vault_hex, "vault not in indexer view yet; NAV from free balance");
            free_balance_of(p.wrap, p.trading_vault_package, p.vault_id, &p.settlement_coin_type)
                .await
                .unwrap_or(0)
        }
    };

    let mut book = Book::new(nav, p.pnl_path);
    book.holdings =
        fetch_holdings(p.wrap, p.api, p.trading_vault_package, p.vault_id).await?;
    // TODO(SO-299): reconstruct `written` from vault-held Positions once
    // the V2 desk writes; empty until then.
    tracing::info!(
        nav = book.nav,
        holdings = book.holdings.len(),
        "book reconstructed from vault custody"
    );
    Ok(book)
}

/// Held option coins: every live bucket's option-coin balance in the
/// vault's free balances + the bot wallet float. Used at boot AND by the
/// refresher's periodic custody re-sync (auction wins / sweeps change
/// balances out-of-band).
pub async fn fetch_holdings(
    wrap: &sui_tx::sui_client::SuiClientWrapper,
    api: &api_service_client::ApiServiceClient,
    trading_vault_package: ObjectID,
    vault_id: ObjectID,
) -> Result<Vec<Holding>> {
    let mut holdings = Vec::new();
    let buckets = api.tradeable_buckets().await.context("tradeable buckets")?;
    for b in &buckets {
        if b.call_coin_type.is_empty() {
            continue;
        }
        let vault_held = free_balance_of(wrap, trading_vault_package, vault_id, &b.call_coin_type)
            .await
            .unwrap_or(0);
        let wallet_held = wrap
            .client
            .coin_read_api()
            .get_balance(wrap.signer.address, Some(b.call_coin_type.clone()))
            .await
            .map(|bal| u64::try_from(bal.total_balance).unwrap_or(u64::MAX))
            .unwrap_or(0);
        if vault_held == 0 && wallet_held == 0 {
            continue;
        }
        // is_put isn't on TradeableBucket; resolve it from the cached
        // bucket-pricing lookup.
        let is_put = api
            .bucket_pricing(b.bucket_id.clone())
            .await
            .ok()
            .flatten()
            .map(|bp| bp.is_put)
            .unwrap_or(false);
        holdings.push(Holding {
            bucket_id: b.bucket_id.clone(),
            option_coin_type: b.call_coin_type.clone(),
            asset_coin_type: b.asset_coin_type.clone(),
            settlement_coin_type: b.settlement_coin_type.clone(),
            is_put,
            strike: b.strike_raw,
            strike_scale: b.strike_scale,
            expiry_ms: b.expiry_ms,
            amount_vault: vault_held,
            amount_wallet: wallet_held,
            pool_id: (!b.pool_id.is_empty()).then(|| b.pool_id.clone()),
        });
    }
    Ok(holdings)
}

/// `vault::free_balance_of<T>(vault)` via dev-inspect (the old
/// vault_deepbook `custody_balance` pattern).
pub async fn free_balance_of(
    wrap: &sui_tx::sui_client::SuiClientWrapper,
    trading_vault_package: ObjectID,
    vault_id: ObjectID,
    coin_type: &str,
) -> Result<u64> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let vault = pt.obj(sui_tx::tx::shared_object_arg(&wrap.client, vault_id, false).await?)?;
    let tag = TypeTag::from_str(coin_type).with_context(|| format!("parsing {coin_type}"))?;
    pt.programmable_move_call(
        trading_vault_package,
        Identifier::new("vault").unwrap(),
        Identifier::new("free_balance_of").unwrap(),
        vec![tag],
        vec![vault],
    );
    let res = wrap
        .client
        .read_api()
        .dev_inspect_transaction_block(
            wrap.signer.address,
            TransactionKind::ProgrammableTransaction(pt.finish()),
            None,
            None,
            None,
        )
        .await
        .context("dev-inspecting free_balance_of")?;
    if let Some(err) = res.error {
        return Err(anyhow!("free_balance_of dev-inspect failed: {err}"));
    }
    let results = res.results.unwrap_or_default();
    let (bytes, _) = results
        .last()
        .and_then(|r| r.return_values.first())
        .ok_or_else(|| anyhow!("free_balance_of returned no values"))?;
    bcs::from_bytes::<u64>(bytes).context("decoding free balance")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(b: u8) -> ObjectId {
        ObjectId::new([b; 32])
    }

    fn holding(bucket: u8, expiry: u64, amount: u64) -> Holding {
        Holding {
            bucket_id: oid(bucket),
            option_coin_type: "0x1::c::C".into(),
            asset_coin_type: "0x1::a::A".into(),
            settlement_coin_type: "0x1::s::S".into(),
            is_put: false,
            strike: 100,
            strike_scale: 0,
            expiry_ms: expiry,
            amount_vault: amount,
            amount_wallet: 0,
            pool_id: None,
        }
    }

    #[test]
    fn reservations_enforce_nav_bound() {
        let mut b = Book::new(1_000, None);
        b.deployed = 300;
        let r1 = b.reserve(400, 30_000, 0).unwrap();
        // 300 deployed + 400 reserved + 400 more > 1000 → refused.
        assert_eq!(b.reserve(400, 30_000, 0), Err(ReserveError::ExceedsNav));
        // Exactly filling the gap is fine.
        assert!(b.reserve(300, 30_000, 0).is_ok());
        b.release_reservation(r1);
        assert_eq!(b.reserved_total(), 300);
    }

    #[test]
    fn reservations_ttl_expire() {
        let mut b = Book::new(1_000, None);
        b.reserve(900, 10_000, 0).unwrap();
        assert_eq!(b.reserve(900, 10_000, 5_000), Err(ReserveError::ExceedsNav));
        // Past the TTL the stale reservation frees its budget.
        assert!(b.reserve(900, 10_000, 20_000).is_ok());
    }

    #[test]
    fn net_greeks_aggregates_by_expiry_with_signs() {
        let mut b = Book::new(0, None);
        b.holdings.push(holding(1, 100, 10));
        b.holdings.push(holding(2, 200, 5));
        b.written.push(Written {
            bucket_id: oid(3),
            is_put: false,
            strike: 100,
            strike_scale: 0,
            expiry_ms: 100,
            amount: 4,
            covered: 0,
        });
        let g = Greeks { delta: 0.5, gamma: 0.01, vega: 20.0, theta: -5.0, rho: 0.0 };
        let mut per_unit = HashMap::new();
        per_unit.insert(oid(1), g);
        per_unit.insert(oid(2), g);
        per_unit.insert(oid(3), g);
        let (by_expiry, total) = b.net_greeks(&per_unit);
        // Expiry 100: +10 long, −4 written → net 6 units of each greek.
        let e100 = by_expiry.get(&100).unwrap();
        assert!((e100.delta_units - 3.0).abs() < 1e-9); // 0.5 × 6
        assert!((e100.vega - 120.0).abs() < 1e-9); // 20 × 6
        let e200 = by_expiry.get(&200).unwrap();
        assert!((e200.delta_units - 2.5).abs() < 1e-9);
        assert!((total.delta_units - 5.5).abs() < 1e-9);
        assert!((total.theta_per_day - (-55.0)).abs() < 1e-9);
    }

    #[test]
    fn naked_written_units_sums_uncovered() {
        let mut b = Book::new(0, None);
        b.written.push(Written {
            bucket_id: oid(1),
            is_put: false,
            strike: 100,
            strike_scale: 0,
            expiry_ms: 1,
            amount: 10,
            covered: 7,
        });
        b.written.push(Written {
            bucket_id: oid(2),
            is_put: true,
            strike: 100,
            strike_scale: 0,
            expiry_ms: 1,
            amount: 5,
            covered: 5,
        });
        assert_eq!(b.naked_written_units(), 3);
    }

    #[test]
    fn pnl_lines_accumulate_and_append_jsonl() {
        let path = std::env::temp_dir().join(format!("mm-desk-pnl-test-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut b = Book::new(0, Some(path.clone()));
        b.record_pnl(PnlLine::Spread, 10.0, "fill", 1);
        b.record_pnl(PnlLine::Theta, -3.0, "decay", 2);
        assert_eq!(b.pnl.spread, 10.0);
        assert_eq!(b.pnl.theta, -3.0);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("\"line\":\"spread\""));
        let _ = std::fs::remove_file(&path);
    }
}
