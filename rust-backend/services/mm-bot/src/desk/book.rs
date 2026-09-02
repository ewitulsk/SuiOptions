//! The desk's book, runtime half: reconstruction from vault custody,
//! the chain/indexer readers, fill detection and the P&L sinks. The
//! book itself (holdings, reservations, P&L counters) is
//! `desk_core::book`, re-exported here (SO-450).
//!
//! On boot the inventory is reconstructed from VAULT custody:
//!   - The budget base from the indexer's `trading_vaults` view via
//!     [`budget_base`] (SO-418): the latest appraised NAV on untranched
//!     vaults, the junior/risk-bearing measure on tranched ones — falling
//!     back to the vault's settlement free balance
//!     (`vault::free_balance_of<Settlement>` dev-inspect) when nothing
//!     has priced the vault yet. **Documented choice**: the appraised
//!     figure includes positions; the free-balance fallback under-counts
//!     by design and only covers a freshly-created vault.
//!   - Held option coins per live bucket via
//!     `vault::free_balance_of<OptionCoin>` dev-inspect (the
//!     `custody_balance` pattern from the old vault_deepbook quoter),
//!     plus the bot wallet's own float of the same coin types.
//!   - Written positions: vault-custodied `Position` objects. Ids come
//!     from the indexer's `trading_vault_positions` view (the same
//!     indexer source the NAV path uses), amount + bucket from on-chain
//!     object reads, strike/expiry/kind from the api-service
//!     bucket-metadata path the holdings reconstruction already uses.
//!     This mirrors `sui_tx::tx::appraisal::discover_holdings`'
//!     classification (`::position::Position` type suffix; RfqTickets,
//!     DeepBook custody and coin objects are not written inventory)
//!     without pulling in the appraisal composer's full
//!     dynamic-field walk. Same-bucket held coins mark written lines
//!     `covered` ([`Book::recompute_covered`]); the uncovered remainder
//!     is the V2 naked-short budget.
//!
//! Fill detection (P&L attribution): [`classify_fill`] + [`apply_fills`]
//! turn indexer events into spread-line records, resumed from a
//! persisted sequence cursor ([`FillCursor`], write-after-apply).

use std::collections::HashMap;
use std::io::Write as _;
use std::path::Path;
use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use protocol_types::events::{ChainEvent, IndexedEvent};
use protocol_types::ids::{ObjectId, SuiAddress};
use serde::{Deserialize, Serialize};
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::TransactionKind;

pub use desk_core::book::*;

/// Sink every P&L record the book queued since the last flush: the
/// Prometheus counter/gauge per line and the append-only JSONL ledger.
pub fn flush_pnl(book: &mut Book, pnl_path: Option<&Path>) {
    for rec in book.drain_pnl_records() {
        let label = match rec.line {
            PnlLine::Spread => "spread",
            PnlLine::Scalp => "scalp",
            PnlLine::Theta => "theta",
            PnlLine::Funding => "funding",
        };
        metrics::counter!("mm_desk_pnl_total", "line" => label)
            .increment(rec.amount.abs().round() as u64);
        metrics::gauge!("mm_desk_pnl", "line" => label).set(book.pnl_line(rec.line));
        if let Some(path) = pnl_path {
            if let Err(e) = append_jsonl(path, &rec) {
                tracing::warn!(error = %format!("{e:#}"), "pnl jsonl append failed");
            }
        }
    }
}

fn append_jsonl<T: Serialize>(path: &Path, rec: &T) -> Result<()> {
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

// ── v2 budget base (SO-418) ────────────────────────────────────────────

/// Mirror of the Move `SHARE_OFFSET` (shares are offset-scaled vs value;
/// observed pps_e12 = value × 1e12 × OFFSET / shares).
const SHARE_OFFSET: u128 = 1_000_000;

/// The desk's premium-budget base for one vault view (SO-418).
///
/// v1 used `latest_pps_e12 × total_shares` — total NAV. v2 bounds
/// reservations by the RISK-BEARING measure instead:
///
/// - untranched (`structure_code == 0`): the whole book is risk capital —
///   `latest_nav` from the last consumed appraisal/capital sync, falling
///   back to the observed pps over the junior book (which carries the
///   untranched supply).
/// - tranched: the junior side only — `junior_nav` from the last capital
///   sync, falling back to the junior observed pps × junior shares. The
///   senior claim is not the desk's to deploy against.
///
/// `None` = no usable measure yet (fresh vault); callers fall back to the
/// free settlement balance, which under-counts by design.
pub fn budget_base(v: &indexer_graphql::TradingVault) -> Option<u64> {
    let from_pps = |pps: Option<u128>, shares: u128| {
        pps.map(|pps| {
            u64::try_from(
                pps.saturating_mul(shares) / 1_000_000_000_000u128 / SHARE_OFFSET,
            )
            .unwrap_or(u64::MAX)
        })
    };
    let nav = if v.structure_code == 0 {
        v.latest_nav
            .or_else(|| from_pps(v.latest_pps_e12, v.junior_shares).map(u128::from))
    } else {
        v.junior_nav
            .or_else(|| from_pps(v.latest_junior_pps_e12, v.junior_shares).map(u128::from))
    };
    nav.map(|n| u64::try_from(n).unwrap_or(u64::MAX))
}

/// Is this vault "risk-off" for the desk (SO-418)? Mirrors the §8.4b gate
/// set the quote sessions and `vault_mm` releases abort on (code 124),
/// plus the terminal states: capital risk state not Healthy, curator
/// commitment breached, lifecycle not open, or settled.
pub fn vault_risk_off(v: &indexer_graphql::TradingVault) -> bool {
    v.risk_state != 0 || v.curator_commitment_breached || v.state != "open" || v.settled
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
    /// Options package id — lets a holding be reconstructed from its
    /// option-coin type when the bucket catalog does not list it.
    pub options_package: Option<String>,
}

/// Reconstruct the book from vault custody (module docs describe the
/// sources; SO-418 switched the budget base from total pps × shares to
/// the risk-state-aware measure — see [`budget_base`]).
pub async fn reconstruct(p: ReconstructParams<'_>) -> Result<Book> {
    // Budget base: latest NAV (untranched) / junior NAV (tranched) from
    // the indexer view, else settlement free balance.
    let vault_hex = p.vault_id.to_hex_literal();
    let vaults = p.indexer.trading_vaults().await.context("indexer trading_vaults")?;
    let ours = vaults
        .iter()
        .find(|v| v.vault_id.to_hex() == vault_hex || format!("0x{}", v.vault_id.to_hex()) == vault_hex);
    let nav = match ours.and_then(budget_base) {
        Some(nav) => nav,
        None => {
            if ours.is_none() {
                tracing::warn!(vault = %vault_hex, "vault not in indexer view yet; NAV from free balance");
            }
            free_balance_of(p.wrap, p.trading_vault_package, p.vault_id, &p.settlement_coin_type)
                .await
                .unwrap_or(0)
        }
    };

    let mut book = Book::new(nav);
    book.holdings =
        fetch_holdings(
            p.wrap,
            p.indexer,
            p.api,
            p.trading_vault_package,
            p.vault_id,
            p.options_package.as_deref(),
        )
        .await?;
    book.written = fetch_written(p.wrap, p.indexer, p.api, p.vault_id).await?;
    book.recompute_covered();
    tracing::info!(
        nav = book.nav,
        holdings = book.holdings.len(),
        written = book.written.len(),
        naked = book.naked_written_units(),
        "book reconstructed from vault custody"
    );
    Ok(book)
}

/// Held option coins: every live bucket's option-coin balance in the
/// vault's free balances + VaultMm coin-custody positions + the bot
/// wallet float. Used at boot AND by the refresher's periodic custody
/// re-sync (auction wins / sweeps change balances out-of-band).
pub async fn fetch_holdings(
    wrap: &sui_tx::sui_client::SuiClientWrapper,
    indexer: &indexer_graphql::IndexerClient,
    api: &api_service_client::ApiServiceClient,
    trading_vault_package: ObjectID,
    vault_id: ObjectID,
    // Options package id, for decoding option-coin types the catalog omits.
    options_package: Option<&str>,
) -> Result<Vec<Holding>> {
    let mut holdings = Vec::new();
    // Pool-less buckets count. `tradeable_buckets` requires a DeepBook pool
    // and the default board drops off-ladder series, so scanning it left the
    // desk blind to any-strike inventory — understating NAV, net vega and the
    // cover available to a V2 write, silently.
    let buckets = api.writable_buckets().await.context("writable buckets")?;
    // VaultMm coin-custody positions (writer-flow sweeps store option
    // coins AS positions), keyed by the canonical option-coin type.
    let mut coin_positions = fetch_coin_positions(wrap, indexer, vault_id).await?;
    for b in &buckets {
        if b.call_coin_type.is_empty() {
            continue;
        }
        let vault_held = free_balance_of(wrap, trading_vault_package, vault_id, &b.call_coin_type)
            .await
            .unwrap_or(0);
        let wallet_held = match sui_types::parse_sui_struct_tag(&b.call_coin_type) {
            Ok(tag) => wrap
                .client
                .balance(wrap.signer.address, &tag)
                .await
                .map(|bal| u64::try_from(bal).unwrap_or(u64::MAX))
                .unwrap_or(0),
            Err(_) => 0,
        };
        let positions = coin_positions
            .remove(&protocol_types::asset::canonicalize_move_type(&b.call_coin_type))
            .unwrap_or_default();
        if vault_held == 0 && wallet_held == 0 && positions.is_empty() {
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
            coin_positions: positions,
        });
    }

    // Anything still in `coin_positions` is custody the catalog does not know
    // about — a bucket at an expiry the board has since dropped, say. The
    // option-coin type encodes its own spec, so the line is reconstructable
    // without any catalog at all; losing it would understate the book.
    if !coin_positions.is_empty() {
        for (coin_type, positions) in std::mem::take(&mut coin_positions) {
            let Some(spec) = options_package
                .and_then(|pkg| protocol_types::bucket_spec::decode_option_coin_type(pkg, &coin_type))
            else {
                tracing::warn!(
                    %coin_type,
                    "vault holds a coin that is not a decodable option coin; excluded from the book"
                );
                continue;
            };
            let amount: u64 = positions.iter().map(|p| p.amount).sum();
            tracing::info!(
                %coin_type,
                amount,
                "recovered a holding the bucket catalog omitted (decoded from the coin type)"
            );
            holdings.push(Holding {
                bucket_id: ObjectId::ZERO,
                option_coin_type: coin_type.clone(),
                asset_coin_type: protocol_types::asset::canonicalize_move_type(&spec.asset),
                settlement_coin_type: protocol_types::asset::canonicalize_move_type(
                    &spec.settlement,
                ),
                is_put: spec.is_put,
                strike: spec.sig as u128,
                strike_scale: spec.exp,
                expiry_ms: spec.expiry_ms,
                amount_vault: 0,
                amount_wallet: 0,
                coin_positions: positions,
            });
        }
    }
    Ok(holdings)
}

/// VaultMm coin-custody positions: active vault positions whose object
/// type is `0x2::coin::Coin<T>`, grouped by the canonical `T`. Ids come
/// from the indexer's `trading_vault_positions` view (like
/// [`fetch_written`]); amounts from on-chain object reads.
async fn fetch_coin_positions(
    wrap: &sui_tx::sui_client::SuiClientWrapper,
    indexer: &indexer_graphql::IndexerClient,
    vault_id: ObjectID,
) -> Result<HashMap<String, Vec<CoinPosition>>> {
    let vault_pt = ObjectId::new(vault_id.into_bytes());
    let positions = indexer
        .trading_vault_positions(vault_pt)
        .await
        .context("indexer trading_vault_positions")?;
    let mut out: HashMap<String, Vec<CoinPosition>> = HashMap::new();
    for pos in positions.iter().filter(|p| p.active) {
        let pos_id = ObjectID::new(*pos.position_id.as_bytes());
        let Some((object, _)) = wrap
            .client
            .try_get_object_json(pos_id)
            .await
            .with_context(|| format!("reading vault position {pos_id}"))?
        else {
            continue; // removed since the indexer view was written
        };
        // `0x2::coin::Coin<T>` custody positions only; everything else
        // (written Positions, custody objects, tickets) is not a coin.
        let Some(coin) = object.as_coin_maybe() else {
            continue;
        };
        let Some(inner) = object
            .struct_tag()
            .and_then(|t| t.type_params.first().cloned())
        else {
            continue;
        };
        let amount = coin.value();
        if amount == 0 {
            continue;
        }
        out.entry(protocol_types::asset::canonicalize_move_type(
            &inner.to_canonical_string(/* with_prefix */ true),
        ))
            .or_default()
            .push(CoinPosition { position_id: pos.position_id, amount });
    }
    Ok(out)
}

/// Written (short) positions: vault-custodied `Position` objects. Ids
/// from the indexer's `trading_vault_positions` view (active only),
/// amount + bucket from on-chain object reads, series metadata from the
/// bucket-pricing lookup. Used at boot AND by the refresher's periodic
/// custody re-sync (new writes/offset closes land out-of-band). Callers
/// run [`Book::recompute_covered`] after installing the result.
pub async fn fetch_written(
    wrap: &sui_tx::sui_client::SuiClientWrapper,
    indexer: &indexer_graphql::IndexerClient,
    api: &api_service_client::ApiServiceClient,
    vault_id: ObjectID,
) -> Result<Vec<Written>> {
    let vault_pt = ObjectId::new(vault_id.into_bytes());
    let positions = indexer
        .trading_vault_positions(vault_pt)
        .await
        .context("indexer trading_vault_positions")?;
    let mut written = Vec::new();
    for pos in positions.iter().filter(|p| p.active) {
        let pos_id = ObjectID::new(*pos.position_id.as_bytes());
        let Some((object, json)) = wrap
            .client
            .try_get_object_json(pos_id)
            .await
            .with_context(|| format!("reading vault position {pos_id}"))?
        else {
            continue; // removed since the indexer view was written
        };
        let ty = object
            .struct_tag()
            .map(|t| t.to_canonical_string(/* with_prefix */ true))
            .unwrap_or_default();
        // discover_holdings' classification: only `::position::Position`
        // objects are written option inventory (RfqTickets, DeepBook
        // custody and held coins are not).
        if !ty.ends_with("::position::Position") {
            continue;
        }
        let fields =
            json.ok_or_else(|| anyhow!("position {pos_id} has no readable Move content"))?;
        let bucket_id = fields
            .get("bucket_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| ObjectId::from_hex(s).ok())
            .ok_or_else(|| anyhow!("position {pos_id} missing bucket_id"))?;
        let range_start = json_u128(&fields, "range_start")?;
        let range_end = json_u128(&fields, "range_end")?;
        let amount = u64::try_from(range_end.saturating_sub(range_start)).unwrap_or(u64::MAX);
        if amount == 0 {
            continue; // fully offset-closed, awaiting destroy_empty
        }
        let Some(bucket) = api.bucket_pricing(bucket_id).await? else {
            tracing::warn!(
                position = %pos_id,
                bucket = %bucket_id,
                "written position's bucket unknown to api-service; skipping line"
            );
            continue;
        };
        written.push(Written {
            bucket_id,
            position_id: pos.position_id,
            asset_coin_type: bucket.asset_coin_type.clone(),
            is_put: bucket.is_put,
            strike: bucket.strike,
            strike_scale: bucket.strike_scale,
            expiry_ms: bucket.expiry_ms,
            amount,
            covered: 0,
        });
    }
    Ok(written)
}

/// A `u128` position field that may arrive as a JSON number or string.
fn json_u128(fields: &serde_json::Value, name: &str) -> Result<u128> {
    let v = fields
        .get(name)
        .ok_or_else(|| anyhow!("position missing field {name}"))?;
    match v {
        serde_json::Value::Number(n) => n
            .as_u64()
            .map(u128::from)
            .ok_or_else(|| anyhow!("non-u64 {name}: {n}")),
        serde_json::Value::String(s) => s.parse().with_context(|| format!("parsing {name} {s:?}")),
        other => Err(anyhow!("unexpected {name}: {other}")),
    }
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
        .dev_inspect_ptb(wrap.signer.address, pt)
        .await
        .context("dev-inspecting free_balance_of")?;
    sui_tx::chain::decode_return_value::<u64>(&res, 0).context("decoding free balance")
}

/// The vault's on-chain external-account limits via dev-inspect of
/// `vault::external_limits`: `(budget_bps, daily_release_bps,
/// released_in_window, window_start_ms)`. Aborts (→ `Err`) when no
/// external account is registered — callers check the indexer view's
/// `external_account` first.
pub async fn external_limits(
    wrap: &sui_tx::sui_client::SuiClientWrapper,
    trading_vault_package: ObjectID,
    vault_id: ObjectID,
) -> Result<(u64, u64, u64, u64)> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let vault = pt.obj(sui_tx::tx::shared_object_arg(&wrap.client, vault_id, false).await?)?;
    pt.programmable_move_call(
        trading_vault_package,
        Identifier::new("vault").unwrap(),
        Identifier::new("external_limits").unwrap(),
        vec![],
        vec![vault],
    );
    let res = wrap
        .client
        .dev_inspect_ptb(wrap.signer.address, pt)
        .await
        .context("dev-inspecting external_limits")?;
    let decode = |n: usize| sui_tx::chain::decode_return_value::<u64>(&res, n);
    Ok((
        decode(0).context("decoding budget_bps")?,
        decode(1).context("decoding daily_release_bps")?,
        decode(2).context("decoding released_in_window")?,
        decode(3).context("decoding window_start_ms")?,
    ))
}

// ── fill detection → spread-line attribution ───────────────────────────
//
// A poller (spawned in `mod.rs`) scans the indexer events feed for fills
// that touch OUR vault and books the spread line:
//
//   spread += (model fair at the current surface − premium paid)   [buys]
//   spread += (premium received − model fair at the current surface) [writes]
//
// **V1 attribution approximation (documented)**: fair is evaluated at
// DETECTION time, not at the on-chain fill time — the surface may have
// moved between the fill and the poll that observes it, so the
// spread-vs-scalp/theta split is approximate while the P&L total stays
// exact. The scalp line comes from `HedgeVenue::realized_pnl` deltas in
// the rebalancer; theta/funding accrue in the refresher.
//
// Identity: the desk is a vault-only maker, so "our" fills are exactly
// the events whose collateral released from the vault
// (`WriteExecuted`/`PutWriteExecuted` with `collateral_source == vault`
// — for our quotes the vault IS the QuoteSigner's collateral source and
// `signer_token_recipient` is the vault address, see `VaultRouting`) and
// the auction-channel WINS. Vault-funded bids route every auction output
// to the BidTicket's address (never the vault), so `RfqSettled`
// recipients can't identify us; instead a win is detected when the
// keeper redeems the ticket into the vault (`TvBidRedeemed` with our
// vault_id), joined to its `TvBidPlaced` for the ticket cost + bucket
// ([`classify_ticket_win`]).

/// Persisted events-feed cursor (sequence high-water mark). Written
/// AFTER fills are applied, so a crash between apply and persist
/// re-applies at most one batch; a clean restart never double-counts.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct FillCursor {
    #[serde(default)]
    pub last_sequence: u64,
}

impl FillCursor {
    /// `None` when no cursor file exists yet (first boot — the poller
    /// seeds from the indexer head so history isn't replayed as fills).
    pub fn load(path: &Path) -> Option<Self> {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
    }

    pub fn persist(&self, path: &Path) {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        if let Err(e) = serde_json::to_string(self).map_err(anyhow::Error::from).and_then(|s| {
            std::fs::write(path, s).map_err(anyhow::Error::from)
        }) {
            tracing::warn!(error = %format!("{e:#}"), path = %path.display(), "fill cursor persist failed");
        }
    }
}

/// Classify one indexed event as a desk fill, or `None` when it isn't
/// ours / isn't a fill. `vault` is the trading vault's object id (its
/// address is the same 32 bytes).
pub fn classify_fill(ev: &IndexedEvent, vault: ObjectId) -> Option<DetectedFill> {
    let vault_addr = SuiAddress::new(*vault.as_bytes());
    match &ev.event {
        ChainEvent::WriteExecuted(w) if w.collateral_source == vault => {
            let (side, premium) = if w.call_token_recipient == vault_addr {
                (FillSide::Bought, w.gross_premium)
            } else {
                (FillSide::Wrote, w.net_premium)
            };
            Some(DetectedFill {
                sequence: ev.sequence,
                bucket_id: w.bucket_id,
                side,
                amount: w.write_amount,
                premium,
                link: FillLink::WsQuote { nonce: w.nonce },
            })
        }
        ChainEvent::PutWriteExecuted(w) if w.collateral_source == vault => {
            let (side, premium) = if w.put_token_recipient == vault_addr {
                (FillSide::Bought, w.gross_premium)
            } else {
                (FillSide::Wrote, w.net_premium)
            };
            Some(DetectedFill {
                sequence: ev.sequence,
                bucket_id: w.bucket_id,
                side,
                amount: w.write_amount,
                premium,
                link: FillLink::WsQuote { nonce: w.nonce },
            })
        }
        _ => None,
    }
}

/// Auction-channel win detection under vault-funded bids (SO-299): the
/// settle routes winnings to the TICKET address, never the vault, so a
/// win becomes observable when the keeper's crank redeems the ticket
/// into the vault (`TvBidRedeemed`). The ticket's `TvBidPlaced` (joined
/// by ticket id) supplies the cost (escrow) and the bucket.
pub fn classify_ticket_win(
    ev: &IndexedEvent,
    vault: ObjectId,
    placed_by_ticket: &HashMap<ObjectId, protocol_types::events::TvBidPlaced>,
) -> Option<DetectedFill> {
    let ChainEvent::TvBidRedeemed(r) = &ev.event else {
        return None;
    };
    if r.vault_id != vault {
        return None;
    }
    let Some(placed) = placed_by_ticket.get(&r.ticket_id) else {
        tracing::warn!(
            ticket = %r.ticket_id.to_hex(),
            "won ticket redeemed but its BidPlaced left the event window; fill not attributed"
        );
        return None;
    };
    Some(DetectedFill {
        sequence: ev.sequence,
        bucket_id: placed.bucket_id,
        side: FillSide::Bought,
        amount: placed.win_amount,
        premium: placed.escrow_amount,
        link: FillLink::AuctionTicket { ticket: r.ticket_id },
    })
}

/// Apply detected fills (paired with their model fair TOTAL premium at
/// detection) to the spread line, advance the cursor, then persist it
/// (write-after-apply). Fills at or below the cursor are skipped, so a
/// replay of an already-applied batch is a no-op. Returns how many fills
/// were applied.
pub fn apply_fills(
    book: &mut Book,
    cursor: &mut FillCursor,
    cursor_path: &Path,
    pnl_path: Option<&Path>,
    fills: &[(DetectedFill, f64)],
    now_ms: u64,
) -> usize {
    let mut applied = 0;
    for (f, fair_total) in fills {
        if f.sequence <= cursor.last_sequence {
            continue;
        }
        let (spread, label) = match f.side {
            FillSide::Bought => (fair_total - f.premium as f64, "bought"),
            FillSide::Wrote => (f.premium as f64 - fair_total, "wrote"),
        };
        let note = format!(
            "fill seq={} bucket={} {} amount={} premium={}",
            f.sequence,
            f.bucket_id.to_hex(),
            label,
            f.amount,
            f.premium
        );
        book.record_pnl(PnlLine::Spread, spread, &note, now_ms);
        flush_pnl(book, pnl_path);
        metrics::counter!("mm_desk_fills_total", "side" => label).increment(1);
        cursor.last_sequence = f.sequence;
        applied += 1;
    }
    if applied > 0 {
        cursor.persist(cursor_path);
    }
    applied
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(b: u8) -> ObjectId {
        ObjectId::new([b; 32])
    }


    // ── v2 budget base (SO-418) ────────────────────────────────────────

    #[test]
    fn budget_base_untranched_prefers_latest_nav_then_pps_over_junior_book() {
        let mut v = crate::desk::provision::test_vault_view(1, 1, "open");
        // Fresh vault: nothing to price from.
        assert_eq!(budget_base(&v), None);
        // Observed pps over the junior book (untranched supply lives
        // there): pps_e12 = value×1e12×OFFSET/shares.
        // pps_e12 = value×1e12×OFFSET/shares → a par vault reads 1e12.
        v.junior_shares = 500 * 1_000_000; // 500 value at OFFSET scale
        v.latest_pps_e12 = Some(1_000_000_000_000);
        assert_eq!(budget_base(&v), Some(500));
        // The appraised NAV wins once present.
        v.latest_nav = Some(1_234);
        assert_eq!(budget_base(&v), Some(1_234));
    }

    #[test]
    fn budget_base_tranched_bounds_by_the_junior_measure() {
        let mut v = crate::desk::provision::test_vault_view(1, 1, "open");
        v.structure_code = 1;
        v.latest_nav = Some(10_000); // total NAV must NOT be the budget
        assert_eq!(budget_base(&v), None, "senior claim is not deployable");
        v.junior_nav = Some(3_000);
        assert_eq!(budget_base(&v), Some(3_000));
        // Without a sync, the junior observed pps stands in.
        v.junior_nav = None;
        v.junior_shares = 2_000 * 1_000_000;
        v.latest_junior_pps_e12 = Some(1_000_000_000_000); // par

        assert_eq!(budget_base(&v), Some(2_000));
    }

    #[test]
    fn risk_off_covers_state_breach_lifecycle_and_settlement() {
        let healthy = crate::desk::provision::test_vault_view(1, 1, "open");
        assert!(!vault_risk_off(&healthy));
        let mut v = healthy.clone();
        v.risk_state = 1; // CoverageBreach
        assert!(vault_risk_off(&v));
        let mut v = healthy.clone();
        v.curator_commitment_breached = true;
        assert!(vault_risk_off(&v));
        let mut v = healthy.clone();
        v.state = "closing".into();
        assert!(vault_risk_off(&v));
        let mut v = healthy.clone();
        v.settled = true;
        assert!(vault_risk_off(&v));
    }

    // ── fill detection ─────────────────────────────────────────────────

    fn hexid(b: u8) -> String {
        oid(b).to_hex()
    }

    /// Decode a canned IndexedEvent from the exact wire JSON the indexer
    /// GraphQL client produces (tagged ChainEvent envelope).
    fn canned_event(seq: u64, event: serde_json::Value) -> IndexedEvent {
        serde_json::from_value(serde_json::json!({
            "sequence": seq.to_string(),
            "timestamp_ms": "1000",
            "event": event,
        }))
        .unwrap()
    }

    fn canned_write_executed(collateral_source: u8, call_recipient: u8) -> serde_json::Value {
        serde_json::json!({
            "type": "WriteExecuted",
            "payload": {
                "bucket_id": hexid(1),
                "signer_id": hexid(7),
                "collateral_source": hexid(collateral_source),
                "signer_token_recipient": hexid(9),
                "executor": hexid(8),
                "position_id": hexid(6),
                "position_recipient": hexid(8),
                "call_token_recipient": hexid(call_recipient),
                "write_amount": "1000",
                "gross_premium": "500",
                "fee": "50",
                "net_premium": "450",
                "range_start": "0",
                "range_end": "1000",
                "nonce": "1",
            }
        })
    }

    #[test]
    fn classify_fill_scopes_to_our_vault_and_sides() {
        let vault = oid(9);
        // Our V1 buy: collateral from the vault, tokens to the vault.
        let ev = canned_event(10, canned_write_executed(9, 9));
        let f = classify_fill(&ev, vault).unwrap();
        assert_eq!(f.side, FillSide::Bought);
        assert_eq!((f.amount, f.premium), (1000, 500));
        // Our V2 write: collateral from the vault, tokens to retail →
        // premium is the NET the vault receives.
        let ev = canned_event(11, canned_write_executed(9, 3));
        let f = classify_fill(&ev, vault).unwrap();
        assert_eq!(f.side, FillSide::Wrote);
        assert_eq!(f.premium, 450);
        // Someone else's fill: not ours.
        let ev = canned_event(12, canned_write_executed(4, 4));
        assert_eq!(classify_fill(&ev, vault), None);
    }

    #[test]
    fn fill_replay_attributes_spread_and_cursor_survives_rerun() {
        let vault = oid(9);
        let dir = std::env::temp_dir();
        let cursor_path = dir.join(format!("mm-desk-fill-cursor-test-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&cursor_path);

        // Two canned fills: a WS-RFQ buy and an auction win observed as
        // a ticket redemption (TvBidRedeemed ⋈ TvBidPlaced).
        let ev1 = canned_event(100, canned_write_executed(9, 9));
        let placed = canned_event(90, serde_json::json!({
            "type": "TvBidPlaced",
            "payload": {
                "vault_id": hexid(9),
                "ticket_id": hexid(2),
                "auction_id": hexid(3),
                "bucket_id": hexid(1),
                "escrow_amount": "900",
                "win_type": "0x1::c::C",
                "win_amount": "2000",
                "is_put": false,
            }
        }));
        let ev2 = canned_event(101, serde_json::json!({
            "type": "TvBidRedeemed",
            "payload": {
                "vault_id": hexid(9),
                "ticket_id": hexid(2),
                "auction_id": hexid(3),
                "win_type": "0x1::c::C",
                "win_amount": "2000",
            }
        }));
        let placed_by_ticket: HashMap<_, _> = match &placed.event {
            ChainEvent::TvBidPlaced(b) => HashMap::from([(b.ticket_id, b.clone())]),
            other => panic!("unexpected {other:?}"),
        };
        let win = classify_ticket_win(&ev2, vault, &placed_by_ticket).unwrap();
        assert_eq!((win.amount, win.premium, win.side), (2000, 900, FillSide::Bought));
        assert_eq!(win.bucket_id, oid(1));
        // A redemption for someone else's vault is not ours; a missing
        // BidPlaced join can't be attributed.
        assert_eq!(classify_ticket_win(&ev2, oid(4), &placed_by_ticket), None);
        assert_eq!(classify_ticket_win(&ev2, vault, &HashMap::new()), None);
        let fills: Vec<(DetectedFill, f64)> = vec![
            // Model fair 600 vs 500 paid → spread +100.
            (classify_fill(&ev1, vault).unwrap(), 600.0),
            // Model fair 850 vs 900 paid → spread −50.
            (win, 850.0),
        ];

        let mut book = Book::new(0);
        let mut cursor = FillCursor::default();
        let applied = apply_fills(&mut book, &mut cursor, &cursor_path, None, &fills, 1);
        assert_eq!(applied, 2);
        assert!((book.pnl.spread - 50.0).abs() < 1e-9);
        assert_eq!(cursor.last_sequence, 101);

        // Restart: reload the persisted cursor, replay the same batch —
        // nothing double-counts.
        let mut cursor2 = FillCursor::load(&cursor_path).expect("cursor persisted");
        assert_eq!(cursor2.last_sequence, 101);
        let applied = apply_fills(&mut book, &mut cursor2, &cursor_path, None, &fills, 2);
        assert_eq!(applied, 0);
        assert!((book.pnl.spread - 50.0).abs() < 1e-9);
        let _ = std::fs::remove_file(&cursor_path);
    }

    #[test]
    fn pnl_lines_accumulate_and_append_jsonl() {
        let path = std::env::temp_dir().join(format!("mm-desk-pnl-test-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut b = Book::new(0);
        b.record_pnl(PnlLine::Spread, 10.0, "fill", 1);
        b.record_pnl(PnlLine::Theta, -3.0, "decay", 2);
        flush_pnl(&mut b, Some(&path));
        assert_eq!(b.pnl.spread, 10.0);
        assert_eq!(b.pnl.theta, -3.0);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("\"line\":\"spread\""));
        let _ = std::fs::remove_file(&path);
    }
