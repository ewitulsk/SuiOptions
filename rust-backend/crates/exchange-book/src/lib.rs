//! Per-market matching engine (spec §5.5).
//!
//! Price-time priority over two `BTreeMap<price_ticks, VecDeque<RestingOrder>>`
//! sides. The engine is a pure deterministic state machine: given the same
//! input sequence it produces the same outputs, so the caller can log the
//! input sequence (append-only) and replay the book for audit/recovery.
//!
//! Matched quantities are marked pending (not removed) so they can be
//! restored if settlement fails — the restore-and-rematch path is core, not
//! edge-case, logic (spec §7.5). Quantities here are in base units; on-chain
//! truth is taker-token units and chain_sync translates authoritative
//! FillEvents back into `apply_external_fill` calls.

use orderbook_core::{Digest, Market, Order, Side, SuiAddress};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BookError {
    #[error("order tokens do not match this market")]
    WrongMarket,
    #[error("price is not on the market tick grid")]
    OffTick,
    #[error("order size below market minimum")]
    BelowMinSize,
    #[error("duplicate digest")]
    Duplicate,
    #[error("amounts overflow the price grid")]
    Overflow,
}

/// Self-trade prevention policy (cancel-newest default per spec).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SelfTradePolicy {
    /// Stop matching when the incoming order would cross the maker's own
    /// resting order; the incoming remainder is dropped, earlier matches at
    /// better levels stand.
    CancelNewest,
    /// Allow self-matches (still settles on-chain at signed terms).
    Allow,
}

/// A resting order as the engine sees it. Quantities are in base units.
#[derive(Clone, Debug)]
pub struct RestingOrder {
    pub digest: Digest,
    pub maker: SuiAddress,
    pub side: Side,
    pub price_ticks: u64,
    /// Base units still open (settled fills already subtracted).
    pub remaining_base: u64,
    /// Base units locked in in-flight settlements (PENDING_SETTLEMENT).
    pub pending_base: u64,
    /// Arrival sequence — the "time" in price-time priority.
    pub seq: u64,
}

impl RestingOrder {
    pub fn available_base(&self) -> u64 {
        self.remaining_base - self.pending_base
    }
}

/// A match produced by the engine, settled on-chain via
/// `settlement::match_orders`. `ask_digest` is always the order selling Base
/// (the `order_a` argument on-chain).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchIntent {
    pub market: SuiAddress,
    pub ask_digest: Digest,
    pub bid_digest: Digest,
    pub fill_base_amount: u64,
    /// The counterparty (book-resting) level's price in ticks — the engine's
    /// execution-price claim. The chain enforces both signed limits and
    /// prices at the earlier-salt order regardless.
    pub exec_price_ticks: u64,
}

/// What happened to an incoming order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlaceOutcome {
    /// Fully consumed by matches (pending settlement), nothing available rests.
    Matched,
    /// Some quantity now rests available in the book.
    Rested { remaining_base: u64 },
    /// Cut short by self-trade prevention; any remainder was dropped.
    SelfTradeCancelled,
}

/// One aggregated price level for snapshots.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookLevel {
    pub price_ticks: u64,
    pub base_quantity: u64,
    pub order_count: u64,
}

/// Convert an order's implied price to ticks on this market's grid and its
/// size to base units. Errors if the price is off-grid.
pub fn price_and_size(market: &Market, order: &Order) -> Result<(Side, u64, u64), BookError> {
    let side = market.side_of(order).ok_or(BookError::WrongMarket)?;
    let (base_amount, quote_amount) = match side {
        Side::Ask => (order.maker_amount, order.taker_amount),
        Side::Bid => (order.taker_amount, order.maker_amount),
    };
    if base_amount == 0 || quote_amount == 0 {
        return Err(BookError::BelowMinSize);
    }
    // price_ticks = quote_amount * lot / (base_amount * tick), must be exact
    let num = quote_amount as u128 * market.lot_size as u128;
    let den = base_amount as u128 * market.tick_size as u128;
    if den == 0 || num % den != 0 {
        return Err(BookError::OffTick);
    }
    let ticks = u64::try_from(num / den).map_err(|_| BookError::Overflow)?;
    if ticks == 0 {
        return Err(BookError::OffTick);
    }
    if base_amount < market.min_size {
        return Err(BookError::BelowMinSize);
    }
    Ok((side, ticks, base_amount))
}

pub struct Book {
    market: Market,
    bids: BTreeMap<u64, VecDeque<RestingOrder>>,
    asks: BTreeMap<u64, VecDeque<RestingOrder>>,
    index: HashMap<Digest, (Side, u64)>,
    next_seq: u64,
    pub self_trade_policy: SelfTradePolicy,
}

impl Book {
    pub fn new(market: Market) -> Self {
        Book {
            market,
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            index: HashMap::new(),
            next_seq: 0,
            self_trade_policy: SelfTradePolicy::CancelNewest,
        }
    }

    pub fn market(&self) -> &Market {
        &self.market
    }

    /// Place a validated order: match against the opposite side, rest any
    /// remainder (unless cut by self-trade prevention).
    pub fn place(
        &mut self,
        digest: Digest,
        order: &Order,
    ) -> Result<(PlaceOutcome, Vec<MatchIntent>), BookError> {
        if self.index.contains_key(&digest) {
            return Err(BookError::Duplicate);
        }
        let (side, price_ticks, base_amount) = price_and_size(&self.market, order)?;
        let seq = self.next_seq;
        self.next_seq += 1;

        let mut incoming = RestingOrder {
            digest,
            maker: order.maker,
            side,
            price_ticks,
            remaining_base: base_amount,
            pending_base: 0,
            seq,
        };

        let (intents, self_trade) = self.match_incoming(&mut incoming);

        let outcome = if self_trade {
            // Cancel-newest: drop the unmatched remainder. If matches are in
            // flight the order must stay tracked until they settle.
            incoming.remaining_base = incoming.pending_base;
            if incoming.pending_base > 0 {
                self.rest(incoming);
            }
            PlaceOutcome::SelfTradeCancelled
        } else if incoming.available_base() == 0 {
            if incoming.pending_base > 0 {
                self.rest(incoming);
            }
            PlaceOutcome::Matched
        } else {
            let rem = incoming.available_base();
            self.rest(incoming);
            PlaceOutcome::Rested { remaining_base: rem }
        };
        Ok((outcome, intents))
    }

    /// Core matching loop: walk the opposite side best-first, FIFO within a
    /// level, marking matched quantity pending on both sides. Returns the
    /// intents and whether self-trade prevention stopped the walk.
    fn match_incoming(&mut self, incoming: &mut RestingOrder) -> (Vec<MatchIntent>, bool) {
        let mut intents = Vec::new();
        let market_id = self.market.registry_id;
        let policy = self.self_trade_policy;
        let opposite = match incoming.side {
            Side::Ask => &mut self.bids,
            Side::Bid => &mut self.asks,
        };
        let mut self_trade = false;
        'outer: while incoming.available_base() > 0 {
            // Best crossing level with any available (non-pending) quantity.
            // Fully-pending orders are skipped: their size is already spoken
            // for; if their settlement fails they regain size and `rematch`
            // restores them to the flow.
            let crosses = |p: &u64| match incoming.side {
                Side::Ask => *p >= incoming.price_ticks,
                Side::Bid => *p <= incoming.price_ticks,
            };
            let has_avail =
                |q: &VecDeque<RestingOrder>| q.iter().any(|o| o.available_base() > 0);
            let best = match incoming.side {
                Side::Ask => opposite
                    .iter_mut()
                    .rev()
                    .filter(|(p, _)| crosses(p))
                    .find(|(_, q)| has_avail(q)),
                Side::Bid => opposite
                    .iter_mut()
                    .filter(|(p, _)| crosses(p))
                    .find(|(_, q)| has_avail(q)),
            };
            let Some((&level_price, queue)) = best else { break };
            for resting in queue.iter_mut() {
                if incoming.available_base() == 0 {
                    break 'outer;
                }
                if resting.available_base() == 0 {
                    continue;
                }
                if resting.maker == incoming.maker && policy == SelfTradePolicy::CancelNewest {
                    self_trade = true;
                    break 'outer;
                }
                let qty = incoming.available_base().min(resting.available_base());
                let (ask_digest, bid_digest) = match incoming.side {
                    Side::Ask => (incoming.digest, resting.digest),
                    Side::Bid => (resting.digest, incoming.digest),
                };
                intents.push(MatchIntent {
                    market: market_id,
                    ask_digest,
                    bid_digest,
                    fill_base_amount: qty,
                    exec_price_ticks: level_price,
                });
                resting.pending_base += qty;
                incoming.pending_base += qty;
            }
        }
        (intents, self_trade)
    }

    fn rest(&mut self, o: RestingOrder) {
        self.index.insert(o.digest, (o.side, o.price_ticks));
        let side_map = match o.side {
            Side::Ask => &mut self.asks,
            Side::Bid => &mut self.bids,
        };
        side_map.entry(o.price_ticks).or_default().push_back(o);
    }

    fn get_mut(&mut self, digest: &Digest) -> Option<&mut RestingOrder> {
        let (side, price) = *self.index.get(digest)?;
        let side_map = match side {
            Side::Ask => &mut self.asks,
            Side::Bid => &mut self.bids,
        };
        side_map
            .get_mut(&price)?
            .iter_mut()
            .find(|o| o.digest == *digest)
    }

    pub fn get(&self, digest: &Digest) -> Option<&RestingOrder> {
        let (side, price) = *self.index.get(digest)?;
        let side_map = match side {
            Side::Ask => &self.asks,
            Side::Bid => &self.bids,
        };
        side_map.get(&price)?.iter().find(|o| o.digest == *digest)
    }

    /// Settlement succeeded: consume the pending quantity on both orders.
    pub fn settle_success(&mut self, intent: &MatchIntent) {
        for d in [intent.ask_digest, intent.bid_digest] {
            if let Some(o) = self.get_mut(&d) {
                let qty = intent.fill_base_amount.min(o.pending_base);
                o.pending_base -= qty;
                o.remaining_base -= qty;
            }
            self.prune_if_done(&d);
        }
    }

    /// Settlement failed: restore the pending quantity. `drop` lists digests
    /// that must leave the book (expired / cancelled / escrow-pruned maker);
    /// call `rematch` afterwards for survivors that regained size.
    pub fn settle_failed(&mut self, intent: &MatchIntent, drop: &[Digest]) {
        for d in [intent.ask_digest, intent.bid_digest] {
            if let Some(o) = self.get_mut(&d) {
                let qty = intent.fill_base_amount.min(o.pending_base);
                o.pending_base -= qty;
            }
        }
        for d in drop {
            self.remove(d);
        }
    }

    /// Authoritative on-chain fill observed (external taker in
    /// open-orderbook mode, §5.7): reduce remaining directly.
    pub fn apply_external_fill(&mut self, digest: &Digest, base_amount: u64) {
        if let Some(o) = self.get_mut(digest) {
            o.remaining_base = o.remaining_base.saturating_sub(base_amount);
            if o.pending_base > o.remaining_base {
                o.pending_base = o.remaining_base;
            }
        }
        self.prune_if_done(digest);
    }

    /// Remove an order outright (soft/hard cancel, prune).
    pub fn remove(&mut self, digest: &Digest) -> bool {
        let Some((side, price)) = self.index.remove(digest) else {
            return false;
        };
        let side_map = match side {
            Side::Ask => &mut self.asks,
            Side::Bid => &mut self.bids,
        };
        if let Some(q) = side_map.get_mut(&price) {
            q.retain(|o| o.digest != *digest);
            if q.is_empty() {
                side_map.remove(&price);
            }
        }
        true
    }

    fn prune_if_done(&mut self, digest: &Digest) {
        if let Some(o) = self.get(digest) {
            if o.remaining_base == 0 && o.pending_base == 0 {
                self.remove(digest);
            }
        }
    }

    /// Re-run matching for a resting order that regained available size
    /// after a failed settlement. Keeps its original time priority.
    pub fn rematch(&mut self, digest: &Digest) -> Vec<MatchIntent> {
        let Some(o) = self.get(digest) else { return Vec::new() };
        if o.available_base() == 0 {
            return Vec::new();
        }
        let mut incoming = o.clone();
        self.remove(digest);
        let (intents, _self_trade) = self.match_incoming(&mut incoming);
        self.rest(incoming);
        intents
    }

    /// Aggregated depth snapshot, best levels first.
    pub fn snapshot(&self, depth: usize) -> (Vec<BookLevel>, Vec<BookLevel>) {
        let agg = |q: &VecDeque<RestingOrder>, price: u64| BookLevel {
            price_ticks: price,
            base_quantity: q.iter().map(|o| o.available_base()).sum(),
            order_count: q.iter().filter(|o| o.available_base() > 0).count() as u64,
        };
        let bids = self
            .bids
            .iter()
            .rev()
            .map(|(p, q)| agg(q, *p))
            .filter(|l| l.base_quantity > 0)
            .take(depth)
            .collect();
        let asks = self
            .asks
            .iter()
            .map(|(p, q)| agg(q, *p))
            .filter(|l| l.base_quantity > 0)
            .take(depth)
            .collect();
        (bids, asks)
    }

    pub fn best_bid(&self) -> Option<u64> {
        self.bids
            .iter()
            .rev()
            .find(|(_, q)| q.iter().any(|o| o.available_base() > 0))
            .map(|(p, _)| *p)
    }

    pub fn best_ask(&self) -> Option<u64> {
        self.asks
            .iter()
            .find(|(_, q)| q.iter().any(|o| o.available_base() > 0))
            .map(|(p, _)| *p)
    }

    /// All resting digests for a maker (event-driven pruning, §5.7).
    pub fn orders_of(&self, maker: &SuiAddress) -> Vec<Digest> {
        self.bids
            .values()
            .chain(self.asks.values())
            .flatten()
            .filter(|o| o.maker == *maker)
            .map(|o| o.digest)
            .collect()
    }

    /// Every resting order (for reconciliation / rebuild checks).
    pub fn iter_orders(&self) -> impl Iterator<Item = &RestingOrder> {
        self.bids.values().chain(self.asks.values()).flatten()
    }
}

#[cfg(test)]
mod tests;
