use super::*;
use exchange_types::{Market, Order, SuiAddress};

fn market() -> Market {
    Market {
        symbol: "SUI/USDC".into(),
        registry_id: SuiAddress::parse("0x5c").unwrap(),
        base: "BASE".into(),
        quote: "QUOTE".into(),
        tick_size: 1_000, // 0.001 quote per lot
        min_size: 100,
        lot_size: 1_000_000,
        current_fee_bps: 10,
    }
}

fn addr(n: u8) -> SuiAddress {
    let mut a = [0u8; 32];
    a[31] = n;
    SuiAddress(a)
}

fn digest(n: u8) -> Digest {
    let mut d = [0u8; 32];
    d[0] = n;
    Digest(d)
}

/// price_ticks * tick / lot = quote per base unit. With tick 1000 and lot 1e6,
/// price 2000 ticks == 2.0 quote/base: quote_amount = base * 2.
fn ask_order(maker: SuiAddress, base_amount: u64, price_ticks: u64) -> Order {
    let quote = (base_amount as u128 * price_ticks as u128 * 1_000 / 1_000_000) as u64;
    Order {
        maker_token: "BASE".into(),
        taker_token: "QUOTE".into(),
        maker_amount: base_amount,
        taker_amount: quote,
        max_fee_bps: 10,
        maker,
        maker_manager_id: addr(0xEE),
        taker: SuiAddress::ZERO,
        sender: SuiAddress::ZERO,
        expiry_ms: u64::MAX,
        salt: 1,
    }
}

fn bid_order(maker: SuiAddress, base_amount: u64, price_ticks: u64) -> Order {
    let quote = (base_amount as u128 * price_ticks as u128 * 1_000 / 1_000_000) as u64;
    Order {
        maker_token: "QUOTE".into(),
        taker_token: "BASE".into(),
        maker_amount: quote,
        taker_amount: base_amount,
        max_fee_bps: 10,
        maker,
        maker_manager_id: addr(0xEE),
        taker: SuiAddress::ZERO,
        sender: SuiAddress::ZERO,
        expiry_ms: u64::MAX,
        salt: 2,
    }
}

#[test]
fn price_grid() {
    let m = market();
    let (side, ticks, base) = price_and_size(&m, &ask_order(addr(1), 10_000, 2_000)).unwrap();
    assert_eq!(side, Side::Ask);
    assert_eq!(ticks, 2_000);
    assert_eq!(base, 10_000);
    let (side, ticks, _) = price_and_size(&m, &bid_order(addr(1), 10_000, 1_999)).unwrap();
    assert_eq!(side, Side::Bid);
    assert_eq!(ticks, 1_999);

    // off-tick: quote amount not divisible on the grid
    let mut o = ask_order(addr(1), 10_000, 2_000);
    o.taker_amount += 1;
    assert_eq!(price_and_size(&m, &o), Err(BookError::OffTick));
    // below min size
    let o = ask_order(addr(1), 99, 2_000);
    assert_eq!(price_and_size(&m, &o), Err(BookError::BelowMinSize));
}

#[test]
fn rest_and_match_price_time_priority() {
    let mut b = Book::new(market());
    // two asks at 2000, one at 1990 (better)
    b.place(digest(1), &ask_order(addr(1), 1_000, 2_000)).unwrap();
    b.place(digest(2), &ask_order(addr(2), 1_000, 2_000)).unwrap();
    b.place(digest(3), &ask_order(addr(3), 1_000, 1_990)).unwrap();
    assert_eq!(b.best_ask(), Some(1_990));

    // crossing bid for 2500 base at 2000: eats 1990 fully, then FIFO at 2000
    let (outcome, intents) = b.place(digest(4), &bid_order(addr(4), 2_500, 2_000)).unwrap();
    assert_eq!(outcome, PlaceOutcome::Matched);
    assert_eq!(intents.len(), 3);
    assert_eq!(intents[0].ask_digest, digest(3));
    assert_eq!(intents[0].fill_base_amount, 1_000);
    assert_eq!(intents[0].exec_price_ticks, 1_990); // price improvement
    assert_eq!(intents[1].ask_digest, digest(1)); // FIFO within level
    assert_eq!(intents[1].fill_base_amount, 1_000);
    assert_eq!(intents[2].ask_digest, digest(2));
    assert_eq!(intents[2].fill_base_amount, 500);

    // all matched quantity is pending, not gone
    assert_eq!(b.get(&digest(2)).unwrap().pending_base, 500);
    assert_eq!(b.get(&digest(2)).unwrap().available_base(), 500);
    // nothing at 1990 available anymore
    assert_eq!(b.best_ask(), Some(2_000));
}

#[test]
fn settle_success_consumes_and_prunes() {
    let mut b = Book::new(market());
    b.place(digest(1), &ask_order(addr(1), 1_000, 2_000)).unwrap();
    let (_, intents) = b.place(digest(2), &bid_order(addr(2), 1_000, 2_000)).unwrap();
    assert_eq!(intents.len(), 1);
    b.settle_success(&intents[0]);
    // both fully settled orders leave the book
    assert!(b.get(&digest(1)).is_none());
    assert!(b.get(&digest(2)).is_none());
    assert_eq!(b.best_ask(), None);
    assert_eq!(b.best_bid(), None);
}

#[test]
fn settle_failure_restores_and_rematches() {
    let mut b = Book::new(market());
    b.place(digest(1), &ask_order(addr(1), 1_000, 2_000)).unwrap();
    let (_, intents) = b.place(digest(2), &bid_order(addr(2), 1_000, 2_000)).unwrap();
    // second ask arrives while settlement is in flight; doesn't match (bid pending)
    b.place(digest(3), &ask_order(addr(3), 1_000, 2_000)).unwrap();

    // maker 1's escrow failed: drop the ask, restore the bid, re-match
    b.settle_failed(&intents[0], &[digest(1)]);
    assert!(b.get(&digest(1)).is_none());
    assert_eq!(b.get(&digest(2)).unwrap().available_base(), 1_000);

    let intents2 = b.rematch(&digest(2));
    assert_eq!(intents2.len(), 1);
    assert_eq!(intents2[0].ask_digest, digest(3));
    assert_eq!(intents2[0].bid_digest, digest(2));
    assert_eq!(intents2[0].fill_base_amount, 1_000);
}

#[test]
fn self_trade_cancel_newest() {
    let mut b = Book::new(market());
    b.place(digest(1), &ask_order(addr(9), 1_000, 2_000)).unwrap();
    // same maker's bid would cross its own ask
    let (outcome, intents) = b.place(digest(2), &bid_order(addr(9), 500, 2_000)).unwrap();
    assert_eq!(outcome, PlaceOutcome::SelfTradeCancelled);
    assert!(intents.is_empty());
    // resting ask untouched, incoming dropped
    assert_eq!(b.get(&digest(1)).unwrap().available_base(), 1_000);
    assert!(b.get(&digest(2)).is_none());
}

#[test]
fn self_trade_keeps_better_level_matches() {
    let mut b = Book::new(market());
    b.place(digest(1), &ask_order(addr(1), 300, 1_990)).unwrap(); // other maker, better price
    b.place(digest(2), &ask_order(addr(9), 1_000, 2_000)).unwrap(); // own order
    let (outcome, intents) = b.place(digest(3), &bid_order(addr(9), 1_000, 2_000)).unwrap();
    assert_eq!(outcome, PlaceOutcome::SelfTradeCancelled);
    // the match at 1990 against the other maker stands...
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0].ask_digest, digest(1));
    assert_eq!(intents[0].fill_base_amount, 300);
    // ...and the remainder was dropped, own resting ask untouched
    assert_eq!(b.get(&digest(2)).unwrap().available_base(), 1_000);
    assert_eq!(b.get(&digest(3)).unwrap().available_base(), 0);
}

#[test]
fn external_fill_is_authoritative() {
    let mut b = Book::new(market());
    b.place(digest(1), &ask_order(addr(1), 1_000, 2_000)).unwrap();
    b.apply_external_fill(&digest(1), 400);
    assert_eq!(b.get(&digest(1)).unwrap().remaining_base, 600);
    b.apply_external_fill(&digest(1), 600);
    assert!(b.get(&digest(1)).is_none());
}

#[test]
fn cancel_removes() {
    let mut b = Book::new(market());
    b.place(digest(1), &ask_order(addr(1), 1_000, 2_000)).unwrap();
    assert!(b.remove(&digest(1)));
    assert!(!b.remove(&digest(1)));
    assert_eq!(b.best_ask(), None);
}

#[test]
fn snapshot_aggregates() {
    let mut b = Book::new(market());
    b.place(digest(1), &ask_order(addr(1), 1_000, 2_000)).unwrap();
    b.place(digest(2), &ask_order(addr(2), 500, 2_000)).unwrap();
    b.place(digest(3), &ask_order(addr(3), 700, 2_010)).unwrap();
    b.place(digest(4), &bid_order(addr(4), 900, 1_980)).unwrap();
    let (bids, asks) = b.snapshot(10);
    assert_eq!(
        asks,
        vec![
            BookLevel { price_ticks: 2_000, base_quantity: 1_500, order_count: 2 },
            BookLevel { price_ticks: 2_010, base_quantity: 700, order_count: 1 },
        ]
    );
    assert_eq!(
        bids,
        vec![BookLevel { price_ticks: 1_980, base_quantity: 900, order_count: 1 }]
    );
}

#[test]
fn deterministic_replay() {
    // same input sequence => same intents and same final snapshot
    let run = || {
        let mut b = Book::new(market());
        let mut all = Vec::new();
        let (_, i1) = b.place(digest(1), &ask_order(addr(1), 1_000, 2_000)).unwrap();
        let (_, i2) = b.place(digest(2), &bid_order(addr(2), 400, 2_000)).unwrap();
        all.extend(i1);
        all.extend(i2);
        b.settle_success(&all[0]);
        let (_, i3) = b.place(digest(3), &bid_order(addr(3), 800, 2_005)).unwrap();
        all.extend(i3);
        (all, b.snapshot(10))
    };
    assert_eq!(run().0, run().0);
    assert_eq!(run().1, run().1);
}

#[test]
fn orders_of_maker() {
    let mut b = Book::new(market());
    b.place(digest(1), &ask_order(addr(1), 1_000, 2_000)).unwrap();
    b.place(digest(2), &ask_order(addr(1), 1_000, 2_010)).unwrap();
    b.place(digest(3), &ask_order(addr(2), 1_000, 2_020)).unwrap();
    let mut mine = b.orders_of(&addr(1));
    mine.sort();
    assert_eq!(mine, vec![digest(1), digest(2)]);
}
