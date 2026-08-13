//! `orderbook` service binary.
//!
//! Wires together (spec §3): REST/WS gateway + intake, per-market matching
//! books, the settlement submitter (matched mode, gRPC via sui-tx), and
//! chain sync (GraphQL event ingestion → fill truth, balance mirroring,
//! event-driven pruning). Markets and the exchange package come from
//! `deployments.json`; the relayer key from the service's secrets file.

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use dashmap::DashMap;
use exchange_book::{Book, MatchIntent};
use exchange_types::SuiAddress;
use orderbook::config::{Cli, Config};
use orderbook::db::{establish_pool, run_migrations, Db, OrderStatus};
use orderbook::settlement::{
    vault_maker_of, DeadReason, DirectEscrow, MatchJob, SettleOutcome, Submitter,
};
use sui_types::base_types::ObjectID;
use orderbook::state::{now_ms, AppState, IntakeConfig, WsMsg};
use orderbook::sync::{EventIngestor, ExchangeEvent};
use parking_lot::Mutex;
use sui_tx::chain::ChainClient;
use sui_tx::events::EventClient;
use sui_tx::sui_client::Signer;
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("orderbook");

    let cli = Cli::parse();
    let cfg = Config::load(&cli.config)
        .with_context(|| format!("loading config {}", cli.config.display()))?;
    let network = cfg.network()?;

    // Optional by design: a missing secrets file degrades to public
    // endpoints + open-orderbook mode, never a crash loop.
    let secrets = runtime_config::Secrets::load(&cli.secrets).unwrap_or_else(|e| {
        warn!(path = %cli.secrets.display(), error = %e, "secrets unavailable; using defaults");
        runtime_config::Secrets::default()
    });
    let grpc_url = cfg
        .grpc_url
        .clone()
        .unwrap_or_else(|| secrets.resolve_grpc_url(network.grpc_url()));
    let graphql_url = cfg
        .graphql_url
        .clone()
        .unwrap_or_else(|| secrets.resolve_graphql_url(network.graphql_url()));

    let pool = Arc::new(establish_pool(&cfg.database_url, cfg.db_pool_size)?);
    run_migrations(&pool)?;
    let db = Db::new(pool);

    let (exchange_info, deployed_markets) = cfg.load_markets()?;
    info!(
        package = %exchange_info.package_id,
        markets = deployed_markets.len(),
        "loaded exchange deployment"
    );
    // SO-384: the shared ingress Whitelist now lives in the record's
    // top-level whitelist block (one list for the whole protocol).
    let whitelist_info = cfg.load_whitelist()?;
    // SO-372: direct vault escrow needs the exchange_adapter package + the
    // trading-vault IntegrationRegistry from the same record. Optional —
    // without them every manager is treated as a plain wallet BM.
    let direct_escrow = cfg.load_direct_escrow()?;
    match &direct_escrow {
        Some(d) => info!(adapter = %d.adapter_package, "direct vault escrow enabled"),
        None => warn!("no exchange_adapter in deployments — direct vault escrow disabled"),
    }
    if deployed_markets.is_empty() {
        warn!("no markets in deployments.json exchange block — nothing to serve");
    }

    // Whitelist sync: mirror the deployments set into exchange_markets
    // (new rows land enabled), disable rows whose registry left the record,
    // then serve only what the DB says is enabled — an operator delists a
    // market by flipping its `enabled` off, and that survives restarts.
    for m in &deployed_markets {
        db.upsert_market(m).await?;
    }
    let current_ids: Vec<String> =
        deployed_markets.iter().map(|m| m.registry_id.to_hex()).collect();
    let stale = db.disable_markets_absent_from(current_ids).await?;
    if stale > 0 {
        info!(disabled = stale, "disabled market rows absent from the deployments record");
    }
    let enabled: std::collections::HashSet<String> =
        db.enabled_market_ids().await?.into_iter().collect();
    let markets: Vec<_> = deployed_markets
        .into_iter()
        .filter(|m| {
            let listed = enabled.contains(&m.registry_id.to_hex());
            if !listed {
                warn!(market = %m.symbol, registry = %m.registry_id, "market delisted in DB; not serving");
            }
            listed
        })
        .collect();

    // Books, rebuilt from OPEN orders (write-ahead guarantee, §5.4).
    let books: DashMap<SuiAddress, Arc<Mutex<Book>>> = DashMap::new();
    for m in &markets {
        let mut book = Book::new(m.clone());
        for stored in db.open_orders(&m.registry_id).await? {
            if stored.signed.order.expiry_ms <= now_ms() {
                db.set_order_status(&stored.digest, OrderStatus::Expired).await?;
                continue;
            }
            // Rebuild resting state; crossing orders re-emit intents on the
            // first real event — on-chain digest-keyed fill accounting makes
            // double settlement impossible.
            if let Err(e) = book.place(stored.digest, &stored.signed.order) {
                warn!(digest = %stored.digest, error = %e, "rebuild: order skipped");
            }
        }
        books.insert(m.registry_id, Arc::new(Mutex::new(book)));
    }

    let (match_tx, match_rx) = mpsc::channel::<MatchJob>(1024);
    let (ws_tx, _) = broadcast::channel::<WsMsg>(8192);

    let state = Arc::new(AppState {
        exchange_package: exchange_info.package_id.clone(),
        whitelist_id: whitelist_info.as_ref().map(|w| w.whitelist_id.clone()),
        markets: markets.clone(),
        books,
        db: db.clone(),
        match_tx,
        ws_tx,
        intake: IntakeConfig::default(),
        direct_escrow: direct_escrow.clone(),
    });

    // Chain sync: ingest package events, mirror to store, prune books.
    let ingestor = Arc::new(EventIngestor::new(
        EventClient::new(&graphql_url),
        db.clone(),
        exchange_info.package_id.clone(),
        direct_escrow.as_ref().map(|d| d.adapter_package.clone()),
    ));
    let mut sync_rx = ingestor.subscribe();
    {
        let ingestor = ingestor.clone();
        tokio::spawn(async move { ingestor.run().await });
    }
    {
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                match sync_rx.recv().await {
                    Ok(ev) => handle_sync_event(&state, ev).await,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "sync consumer lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    // Settlement worker (matched mode) — needs a relayer key in secrets.
    match Signer::from_secrets(&secrets, network) {
        Ok(signer) => {
            info!(relayer = %signer.address, "matched-mode settlement enabled");
            let chain = ChainClient::new(&grpc_url)
                .with_context(|| format!("building chain client for {grpc_url}"))?;
            let direct = direct_escrow
                .as_ref()
                .map(|d| -> anyhow::Result<DirectEscrow> {
                    Ok(DirectEscrow {
                        adapter_package: ObjectID::from_hex_literal(&d.adapter_package)
                            .context("parsing exchange_adapter package id")?,
                        integration_registry: ObjectID::from_hex_literal(
                            &d.integration_registry_id,
                        )
                        .context("parsing integrationRegistryId")?,
                    })
                })
                .transpose()?;
            // The whitelist is a hard settlement dependency (SO-384):
            // every settlement entry takes it, so a record without one
            // cannot settle at all.
            let whitelist = whitelist_info
                .as_ref()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "deployments record has no whitelist block — redeploy the protocol with \
                         the whitelist-enabled deployment-manager before enabling settlement"
                    )
                })?
                .whitelist_object()?;
            let submitter = Submitter::new(
                chain,
                signer,
                exchange_info.package()?,
                whitelist,
                direct,
                cfg.gas_budget,
            );
            let state = state.clone();
            tokio::spawn(settlement_worker(state, submitter, match_rx));
        }
        Err(e) => {
            warn!(error = %e, "no relayer key: open-orderbook mode only, match intents dropped");
            let mut match_rx = match_rx;
            tokio::spawn(async move { while match_rx.recv().await.is_some() {} });
        }
    }

    let app = orderbook::router(state);
    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    info!(bind = %cfg.bind, "orderbook service listening");
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
        // Custody mapping is mirrored by the ingestor; nothing to prune —
        // a fresh custody has no resting orders yet.
        ExchangeEvent::Deposit { .. }
        | ExchangeEvent::SignerAdded { .. }
        | ExchangeEvent::VaultCustody { .. } => {}
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
        let Ok(Some(stored)) = state.db.get_order(&d).await else { continue };
        if voided(stored.signed.order.salt) {
            book.lock().remove(&d);
            let _ = state.db.set_order_status(&d, OrderStatus::Pruned).await;
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
    let Ok(balance) = state.db.balance(manager, token).await else { return };
    let Ok(mut committed) = state.db.open_commitment(manager, token).await else { return };
    if committed <= balance {
        return;
    }
    let Ok(open) = state.db.open_orders_by_manager(manager).await else { return };
    let mut candidates: Vec<_> = open
        .into_iter()
        .filter(|s| s.signed.order.maker_token == token)
        .collect();
    candidates.sort_by(|a, b| b.signed.order.salt.cmp(&a.signed.order.salt)); // newest first
    for stored in candidates {
        if committed <= balance {
            break;
        }
        let o = &stored.signed.order;
        if let Some(book) = state.book(&stored.signed.registry_id) {
            book.lock().remove(&stored.digest);
        }
        let _ = state.db.set_order_status(&stored.digest, OrderStatus::Pruned).await;
        notify_prune(state, &o.maker, &stored.digest, "escrow no longer covers order");
        let paid_out =
            exchange_types::math::muldiv_floor(stored.filled_taker, o.maker_amount, o.taker_amount.max(1));
        committed = committed.saturating_sub(o.maker_amount.saturating_sub(paid_out));
    }
}

/// Prune a manager's open orders — all of them, or only those selling
/// `token` (SO-372: vault free balances aren't mirrored, so a starved or
/// disabled direct maker is pruned wholesale rather than downsized).
async fn prune_manager_orders(
    state: &Arc<AppState>,
    manager: &SuiAddress,
    token: Option<&str>,
    reason: &str,
) {
    let Ok(open) = state.db.open_orders_by_manager(manager).await else { return };
    for stored in open {
        if token.is_some_and(|t| stored.signed.order.maker_token != t) {
            continue;
        }
        if let Some(book) = state.book(&stored.signed.registry_id) {
            book.lock().remove(&stored.digest);
        }
        let _ = state.db.set_order_status(&stored.digest, OrderStatus::Pruned).await;
        notify_prune(state, &stored.signed.order.maker, &stored.digest, reason);
    }
}

async fn prune_orders_signed_by(
    state: &Arc<AppState>,
    manager: &SuiAddress,
    signer: &SuiAddress,
) {
    let Ok(open) = state.db.open_orders_by_manager(manager).await else { return };
    for stored in open {
        let derived = exchange_signing::derive_address(
            stored.signed.scheme,
            &stored.signed.public_key,
        );
        if derived == *signer {
            if let Some(book) = state.book(&stored.signed.registry_id) {
                book.lock().remove(&stored.digest);
            }
            let _ = state.db.set_order_status(&stored.digest, OrderStatus::Pruned).await;
            notify_prune(state, &stored.signed.order.maker, &stored.digest, "delegated signer removed");
        }
    }
}

fn notify_prune(
    state: &Arc<AppState>,
    maker: &SuiAddress,
    digest: &exchange_types::Digest,
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
            // provisional fill notifications; chain sync's FillEvent is the
            // final (single source of truth) state
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
            let _ = state.db.set_order_status(&digest, status).await;
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
        SettleOutcome::VaultEscrowInsufficient { ask_side } => {
            // SO-372: the abort names the starved side. The vault's free
            // balance isn't mirrored, so prune ALL of that maker's orders
            // in the starved token; the survivor re-matches.
            book.lock().settle_failed(intent, &[]);
            let side = if ask_side { &job.ask } else { &job.bid };
            let o = &side.signed.order;
            prune_manager_orders(
                state,
                &o.maker_manager_id,
                Some(&o.maker_token),
                "vault escrow no longer covers order",
            )
            .await;
            let survivor = if ask_side { intent.bid_digest } else { intent.ask_digest };
            rematch_and_enqueue(state, &intent.market, survivor).await;
        }
        SettleOutcome::VaultQuotingDisabled => {
            // The maker's direct quoting is off (vault closed, adapter
            // delisted, or curator opt-out): every order of the involved
            // direct manager(s) is unfillable — prune them all.
            book.lock().settle_failed(intent, &[]);
            for (side, vault) in [(&job.ask, &job.ask_vault), (&job.bid, &job.bid_vault)] {
                if vault.is_some() {
                    prune_manager_orders(
                        state,
                        &side.signed.order.maker_manager_id,
                        None,
                        "direct quoting disabled",
                    )
                    .await;
                }
            }
            for d in [intent.ask_digest, intent.bid_digest] {
                rematch_and_enqueue(state, &intent.market, d).await;
            }
        }
        SettleOutcome::Congested => {
            // Shared-object congestion cancelled execution (nothing was
            // recorded): retryable, never prune-worthy. Restore both and
            // re-match — the intents re-enqueue with backoff-by-queue.
            book.lock().settle_failed(intent, &[]);
            for d in [intent.ask_digest, intent.bid_digest] {
                rematch_and_enqueue(state, &intent.market, d).await;
            }
        }
        SettleOutcome::Stale => {
            // someone raced us (external fill); restore, let chain sync's
            // fill events shrink remaining, then re-match
            book.lock().settle_failed(intent, &[]);
            for d in [intent.ask_digest, intent.bid_digest] {
                rematch_and_enqueue(state, &intent.market, d).await;
            }
        }
        SettleOutcome::Failed { error: err } => {
            // tx-alerting rule: every service tx-submission failure alerts
            // at the handler; benign race-losses were decoded above.
            error!(
                alert_id = "tx-failed-exchange-match-settlement",
                ask = %intent.ask_digest,
                bid = %intent.bid_digest,
                error = %err,
                "match settlement failed; restoring book"
            );
            book.lock().settle_failed(intent, &[]);
        }
    }
}

/// Re-run matching for a restored order and enqueue any new intents.
async fn rematch_and_enqueue(
    state: &Arc<AppState>,
    market_id: &SuiAddress,
    digest: exchange_types::Digest,
) {
    let Some(book) = state.book(market_id) else { return };
    let Some(market) = state.market(market_id).cloned() else { return };
    let intents: Vec<MatchIntent> = book.lock().rematch(&digest);
    for intent in intents {
        let ask = state.db.get_order(&intent.ask_digest).await.ok().flatten();
        let bid = state.db.get_order(&intent.bid_digest).await.ok().flatten();
        if let (Some(ask), Some(bid)) = (ask, bid) {
            let ask_vault = vault_maker_of(&state.db, &ask.signed.order.maker_manager_id).await;
            let bid_vault = vault_maker_of(&state.db, &bid.signed.order.maker_manager_id).await;
            let job = MatchJob {
                intent,
                ask,
                bid,
                base_type: market.base.clone(),
                quote_type: market.quote.clone(),
                ask_vault,
                bid_vault,
            };
            // try_send: this can run inside the settlement worker itself, and
            // awaiting on the queue it drains would self-deadlock when full
            if let Err(e) = state.match_tx.try_send(job) {
                error!(alert_id = "tx-failed-exchange-match-queue", error = %e, "match intent dropped");
            }
        }
    }
}
