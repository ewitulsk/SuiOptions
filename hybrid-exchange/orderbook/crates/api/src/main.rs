//! `orderbook-service`: the hybrid exchange off-chain service.
//!
//! Wires together (spec §3): REST/WS gateway + intake, per-market matching
//! books, the settlement submitter (matched mode), and chain sync
//! (event ingestion → fill truth, balance mirroring, event-driven pruning).

use dashmap::DashMap;
use orderbook_api::state::{now_ms, AppState, IntakeConfig, WsMsg};
use orderbook_book::{Book, MatchIntent};
use orderbook_chain_sync::{EventIngestor, ExchangeEvent};
use orderbook_core::SuiAddress;
use orderbook_settlement::{
    DeadReason, MatchJob, SettleOutcome, Submitter, SubmitterConfig,
};
use orderbook_signing::keys::Ed25519Keypair;
use orderbook_store::{OrderStatus, Store};
use orderbook_suirpc::SuiRpcClient;
use parking_lot::Mutex;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = orderbook_ops::Config::from_env()?;
    orderbook_ops::init_telemetry(config.metrics_bind);

    let store = Store::connect(&config.database_url).await?;
    let markets = config.load_markets()?;
    tracing::info!(count = markets.len(), "loaded markets");

    // Books, rebuilt from OPEN orders (write-ahead guarantee, §5.4).
    let books: DashMap<SuiAddress, Arc<Mutex<Book>>> = DashMap::new();
    for m in &markets {
        store.upsert_market(m).await?;
        let mut book = Book::new(m.clone());
        for stored in store.open_orders(&m.registry_id).await? {
            if stored.signed.order.expiry_ms <= now_ms() {
                store.set_order_status(&stored.digest, OrderStatus::Expired).await?;
                continue;
            }
            // Rebuild resting state; crossing orders will re-emit intents on
            // the first real event — on-chain fill state makes double
            // settlement impossible (digest-keyed accounting).
            if let Err(e) = book.place(stored.digest, &stored.signed.order) {
                tracing::warn!(digest = %stored.digest, error = %e, "rebuild: order skipped");
            }
        }
        books.insert(m.registry_id, Arc::new(Mutex::new(book)));
    }

    let (match_tx, match_rx) = mpsc::channel::<MatchJob>(1024);
    let (ws_tx, _) = broadcast::channel::<WsMsg>(8192);

    let state = Arc::new(AppState {
        markets: markets.clone(),
        books,
        store: store.clone(),
        match_tx,
        ws_tx,
        intake: IntakeConfig::default(),
    });

    // Chain sync: ingest package events, mirror to store, prune books.
    let ingestor = Arc::new(EventIngestor::new(
        SuiRpcClient::new(&config.rpc_url),
        store.clone(),
        config.package_id.clone(),
    ));
    let mut sync_rx = ingestor.subscribe();
    {
        let ingestor = ingestor.clone();
        tokio::spawn(async move {
            if let Err(e) = ingestor.run().await {
                tracing::error!(error = %e, "chain_sync ingestor exited");
            }
        });
    }
    {
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                match sync_rx.recv().await {
                    Ok(ev) => handle_sync_event(&state, ev).await,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "sync consumer lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    // Settlement worker (matched mode) — requires a relayer key.
    if let Some(seed_hex) = &config.relayer_seed_hex {
        let seed: [u8; 32] = hex::decode(seed_hex.trim_start_matches("0x"))?
            .try_into()
            .map_err(|_| anyhow::anyhow!("RELAYER_SEED_HEX must be 32 bytes"))?;
        let key = Ed25519Keypair::from_seed(seed);
        tracing::info!(relayer = %key.address(), "matched-mode settlement enabled");
        let submitter = Submitter::new(
            SuiRpcClient::new(&config.rpc_url),
            key,
            SubmitterConfig { package: config.package_id.clone(), ..Default::default() },
        );
        let state = state.clone();
        tokio::spawn(settlement_worker(state, submitter, match_rx));
    } else {
        tracing::warn!("no RELAYER_SEED_HEX: open-orderbook mode only, match intents dropped");
        let mut match_rx = match_rx;
        tokio::spawn(async move { while match_rx.recv().await.is_some() {} });
    }

    let app = orderbook_api::router(state);
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    tracing::info!(bind = %config.bind, "orderbook-service listening");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Apply a chain event to books/WS (store mirroring already done by the
/// ingestor). FillEvents are authoritative — including external fills.
async fn handle_sync_event(state: &Arc<AppState>, ev: ExchangeEvent) {
    match ev {
        ExchangeEvent::Fill {
            registry,
            digest,
            maker,
            taker,
            base_amount,
            quote_amount,
            maker_sold_base,
            timestamp_ms,
            ..
        } => {
            if let Some(book) = state.book(&registry) {
                book.lock().apply_external_fill(&digest, base_amount);
            }
            state.publish(
                format!("trades.{}", registry.to_hex()),
                serde_json::json!({
                    "type": "trade",
                    "digest": digest.to_hex(),
                    "baseAmount": base_amount.to_string(),
                    "quoteAmount": quote_amount.to_string(),
                    "makerSoldBase": maker_sold_base,
                    "timestampMs": timestamp_ms,
                }),
            );
            for addr in [maker, taker] {
                state.publish(
                    format!("orders.{}", addr.to_hex()),
                    serde_json::json!({
                        "type": "fill",
                        "digest": digest.to_hex(),
                        "baseAmount": base_amount.to_string(),
                        "quoteAmount": quote_amount.to_string(),
                        "final": true,
                    }),
                );
            }
            if let Some(m) = state.market(&registry) {
                let m = m.clone();
                state.publish_book_snapshot(&m);
            }
        }
        ExchangeEvent::Cancel { registry, digest, maker } => {
            if let Some(book) = state.book(&registry) {
                book.lock().remove(&digest);
            }
            state.publish(
                format!("orders.{}", maker.to_hex()),
                serde_json::json!({ "type": "cancelled", "digest": digest.to_hex(), "soft": false }),
            );
            if let Some(m) = state.market(&registry) {
                let m = m.clone();
                state.publish_book_snapshot(&m);
            }
        }
        ExchangeEvent::SaltWatermark { registry, maker, min_valid_salt } => {
            // prune all resting orders of this maker at or below the watermark
            prune_maker_orders(state, &registry, &maker, |salt| salt <= min_valid_salt).await;
        }
        ExchangeEvent::Withdraw { manager, token, .. } => {
            // §5.7: on any escrow decrease, re-validate that maker's resting
            // orders and prune anything no longer covered
            prune_uncovered(state, &manager, &token).await;
        }
        ExchangeEvent::SignerRemoved { manager, signer } => {
            // mirror the on-chain void: prune all resting orders signed by
            // that key against this manager
            prune_orders_signed_by(state, &manager, &signer).await;
        }
        ExchangeEvent::Deposit { .. } | ExchangeEvent::SignerAdded { .. } => {}
    }
}

async fn prune_maker_orders(
    state: &Arc<AppState>,
    registry: &SuiAddress,
    maker: &SuiAddress,
    voided: impl Fn(u64) -> bool,
) {
    let Some(book) = state.book(registry) else { return };
    let digests = book.lock().orders_of(maker);
    for d in digests {
        let Ok(Some(stored)) = state.store.get_order(&d).await else { continue };
        if voided(stored.signed.order.salt) {
            book.lock().remove(&d);
            let _ = state.store.set_order_status(&d, OrderStatus::Pruned).await;
            notify_prune(state, maker, &d, "salt watermark");
        }
    }
    if let Some(m) = state.market(registry) {
        let m = m.clone();
        state.publish_book_snapshot(&m);
    }
}

/// Prune newest-first until the manager's escrow covers the remaining open
/// commitment for `token` (spec §5.7).
async fn prune_uncovered(state: &Arc<AppState>, manager: &SuiAddress, token: &str) {
    let Ok(balance) = state.store.balance(manager, token).await else { return };
    let Ok(mut committed) = state.store.open_commitment(manager, token).await else { return };
    if committed <= balance {
        return;
    }
    // collect this manager's open orders selling `token`, newest salt first
    let mut candidates: Vec<(orderbook_core::Digest, u64, u64, SuiAddress, SuiAddress)> =
        Vec::new();
    for m in &state.markets {
        let Some(book) = state.book(&m.registry_id) else { continue };
        let digests: Vec<_> = book.lock().iter_orders().map(|o| o.digest).collect();
        for d in digests {
            let Ok(Some(stored)) = state.store.get_order(&d).await else { continue };
            let o = &stored.signed.order;
            if o.maker_manager_id == *manager && o.maker_token == token {
                let remaining_maker = o
                    .maker_amount
                    .saturating_sub(orderbook_core::math::muldiv_floor(
                        stored.filled_taker,
                        o.maker_amount,
                        o.taker_amount,
                    ));
                candidates.push((d, o.salt, remaining_maker, m.registry_id, o.maker));
            }
        }
    }
    candidates.sort_by(|a, b| b.1.cmp(&a.1)); // newest salt first
    for (digest, _salt, remaining_maker, registry, maker) in candidates {
        if committed <= balance {
            break;
        }
        if let Some(book) = state.book(&registry) {
            book.lock().remove(&digest);
        }
        let _ = state.store.set_order_status(&digest, OrderStatus::Pruned).await;
        notify_prune(state, &maker, &digest, "escrow no longer covers order");
        committed = committed.saturating_sub(remaining_maker);
    }
}

async fn prune_orders_signed_by(
    state: &Arc<AppState>,
    manager: &SuiAddress,
    signer: &SuiAddress,
) {
    for m in &state.markets {
        let Some(book) = state.book(&m.registry_id) else { continue };
        let digests: Vec<_> = book.lock().iter_orders().map(|o| o.digest).collect();
        for d in digests {
            let Ok(Some(stored)) = state.store.get_order(&d).await else { continue };
            let o = &stored.signed.order;
            if o.maker_manager_id != *manager {
                continue;
            }
            let derived = orderbook_signing::derive_address(
                stored.signed.scheme,
                &stored.signed.public_key,
            );
            if derived == *signer {
                book.lock().remove(&d);
                let _ = state.store.set_order_status(&d, OrderStatus::Pruned).await;
                notify_prune(state, &o.maker, &d, "delegated signer removed");
            }
        }
    }
}

fn notify_prune(
    state: &Arc<AppState>,
    maker: &SuiAddress,
    digest: &orderbook_core::Digest,
    reason: &str,
) {
    state.publish(
        format!("orders.{}", maker.to_hex()),
        serde_json::json!({ "type": "pruned", "digest": digest.to_hex(), "reason": reason }),
    );
}

/// Matched-mode settlement loop (spec §5.6): one in-flight pipeline; abort
/// decoding drives restore-and-rematch.
async fn settlement_worker(
    state: Arc<AppState>,
    submitter: Submitter,
    mut rx: mpsc::Receiver<MatchJob>,
) {
    while let Some(job) = rx.recv().await {
        let outcome = submitter.submit_match(&job).await;
        handle_settle_outcome(&state, &job, outcome).await;
    }
}

async fn handle_settle_outcome(state: &Arc<AppState>, job: &MatchJob, outcome: SettleOutcome) {
    let intent = &job.intent;
    let Some(book) = state.book(&intent.market) else { return };
    match outcome {
        SettleOutcome::Confirmed { tx_digest } => {
            book.lock().settle_success(intent);
            // provisional fill notifications; chain_sync's FillEvent marks
            // SETTLED state (single source of truth)
            for (digest, addr) in [
                (intent.ask_digest, job.ask.signed.order.maker),
                (intent.bid_digest, job.bid.signed.order.maker),
            ] {
                state.publish(
                    format!("orders.{}", addr.to_hex()),
                    serde_json::json!({
                        "type": "fill",
                        "digest": digest.to_hex(),
                        "baseAmount": intent.fill_base_amount.to_string(),
                        "txDigest": tx_digest,
                        "final": false,
                    }),
                );
            }
        }
        SettleOutcome::OrderDead { digest, reason } => {
            book.lock().settle_failed(intent, &[digest]);
            let status = match reason {
                DeadReason::Expired => OrderStatus::Expired,
                DeadReason::Cancelled => OrderStatus::Cancelled,
                _ => OrderStatus::Pruned,
            };
            let _ = state.store.set_order_status(&digest, status).await;
            let survivor = if digest == intent.ask_digest {
                intent.bid_digest
            } else {
                intent.ask_digest
            };
            rematch_and_enqueue(state, &intent.market, survivor).await;
        }
        SettleOutcome::InsufficientEscrow => {
            // restore both, then re-check each maker's escrow coverage;
            // prune whoever is uncovered (spec: prune/downsize that maker)
            book.lock().settle_failed(intent, &[]);
            for side in [&job.ask, &job.bid] {
                let o = &side.signed.order;
                prune_uncovered(state, &o.maker_manager_id, &o.maker_token).await;
            }
            for d in [intent.ask_digest, intent.bid_digest] {
                rematch_and_enqueue(state, &intent.market, d).await;
            }
        }
        SettleOutcome::Stale => {
            // someone raced us (external fill); restore, let chain_sync's
            // fill events shrink remaining, then re-match
            book.lock().settle_failed(intent, &[]);
            for d in [intent.ask_digest, intent.bid_digest] {
                rematch_and_enqueue(state, &intent.market, d).await;
            }
        }
        SettleOutcome::Failed { error } => {
            tracing::error!(
                alert_id = "tx-failed-match-settlement",
                ask = %intent.ask_digest,
                bid = %intent.bid_digest,
                error = %error,
                "match settlement failed after retries; restoring book"
            );
            book.lock().settle_failed(intent, &[]);
        }
    }
}

/// Re-run matching for a restored order and enqueue any new intents.
async fn rematch_and_enqueue(state: &Arc<AppState>, market_id: &SuiAddress, digest: orderbook_core::Digest) {
    let Some(book) = state.book(market_id) else { return };
    let Some(market) = state.market(market_id).cloned() else { return };
    let intents: Vec<MatchIntent> = book.lock().rematch(&digest);
    for intent in intents {
        let ask = state.store.get_order(&intent.ask_digest).await.ok().flatten();
        let bid = state.store.get_order(&intent.bid_digest).await.ok().flatten();
        if let (Some(ask), Some(bid)) = (ask, bid) {
            let job = MatchJob {
                intent,
                ask,
                bid,
                base_type: market.base.clone(),
                quote_type: market.quote.clone(),
            };
            // try_send: this can run inside the settlement worker itself, and
            // awaiting on the queue it drains would self-deadlock when full
            if let Err(e) = state.match_tx.try_send(job) {
                tracing::error!(alert_id = "tx-failed-match-queue", error = %e, "match intent dropped");
            }
        }
    }
}
