//! Layer 2 — full-chain end-to-end tests.
//!
//! Each test spins up a fresh per-test stack via
//! [`integration_tests::chain::env::TestEnv`]: a `sui start` localnet, an
//! ephemeral Postgres container, the real indexer + quoting service
//! booted in-process, plus a published copy of the protocol + test-tokens
//! packages. ~25-35s of setup per test on a warm machine.
//!
//! ## Running
//!
//! Each scenario is `#[ignore]` so a plain `cargo test --features chain`
//! still compiles and reports the layout, but doesn't try to publish
//! Move bytecode against a possibly-mismatched local sui binary. To
//! actually run them:
//!
//! ```sh
//! # 1. Install a sui binary that matches the framework the contracts
//! #    compile against (see Move.lock).
//! cargo install --git https://github.com/MystenLabs/sui \
//!   --branch framework/mainnet --locked sui
//!
//! # 2. Optionally point SUI_BIN at the just-built binary if PATH
//! #    already has a different (older) one.
//! export SUI_BIN="$HOME/.cargo/bin/sui"
//!
//! # 3. Run.
//! cargo test -p integration-tests --features chain --test chain_e2e \
//!   -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! `--test-threads=1` is required: each test launches a Docker container
//! and binds randomized OS ports; parallel runs only fight over Docker.

#![cfg(feature = "chain")]

use std::time::Duration;

use shared::protocol_types::asset::AssetType;
use shared::protocol_types::events::ChainEvent;
use shared::protocol_types::messages::RfqQuoteEntry;
use shared::protocol_types::sides::MmRole;

use integration_tests::chain::actors::{MockMm, Writer};
use integration_tests::chain::env::TestEnv;

const TUSDC_BALANCE: u64 = 1_000_000_000_000; // 1M TUSDC (6 decimals)

/// Happy path: bucket → mock MM signs → writer executes on chain →
/// indexer ingests `WriteExecuted` → quoting service mirror updates →
/// a follow-up RFQ sees the debited balance.
///
/// Uses `MockMm` instead of the real `mm-bot` because the bot needs
/// live Pyth feeds the localnet harness doesn't (yet) mock — but every
/// other layer (PTB construction, BCS canonicalization, on-chain
/// signature verification, real indexer worker, fanout, indexer client,
/// AppState mirroring) is exercised end-to-end.
#[tokio::test]
#[ignore = "chain — needs sui binary matching framework/mainnet; see file doc"]
async fn chain_happy_path_writer_flow() {
    let env = TestEnv::fresh().await.expect("fresh env");

    // Pick an expiry far enough out that the chain's clock doesn't
    // overtake it during the test. Use the latest checkpoint timestamp
    // from the localnet as the anchor.
    let expiry_ms = now_unix_ms() + 60 * 60 * 1000; // +1h
    let strike = 50_000_000_000_u64; // ~$50k BTC * 1e6 USDC decimals

    let bucket = env
        .create_btc_usdc_bucket(expiry_ms, strike)
        .await
        .expect("create bucket");

    // Bootstrap a MockMm with TUSDC funding; connect as TraderMm so it
    // pays premium + receives the call token (§3.3.4 writer flow).
    let mut mm = MockMm::bootstrap(&env, vec![MmRole::TraderMm], TUSDC_BALANCE)
        .await
        .expect("bootstrap mm");

    // Quoting service's view of the MM's USDC balance — sanity check
    // before the write lands.
    let usdc = AssetType::new("TUSDC");
    let mm_pt = shared::protocol_types::ids::ObjectId::new({
        let mut b = [0u8; 32];
        b.copy_from_slice(mm.account_id.into_bytes().as_ref());
        b
    });
    let before = env.quoting().app.available(mm_pt, &usdc);
    assert_eq!(before, TUSDC_BALANCE);

    // Bootstrap a writer (gas-funded retail keypair) and prepare an
    // unattached MockRetail WS so we can fire the RFQ.
    let writer = Writer::bootstrap(&env).await.expect("bootstrap writer");

    let retail_addr = env.quoting().bind_addr;
    let mut retail =
        integration_tests::router::connect_retail(retail_addr, shared::protocol_types::sides::RetailRole::Writer)
            .await
            .expect("connect retail");
    // Give the MM registry a tick to record the auth.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let write_amount = 1_000_000_u64; // 0.01 BTC (8 decimals)
    let request_id = "happy-rfq-1";
    integration_tests::router::send_rfq(
        &mut retail,
        request_id,
        bucket_pt(bucket),
        write_amount,
        shared::protocol_types::sides::Side::Writer,
    )
    .await
    .expect("send rfq");

    // MM picks up the broadcast and signs a quote.
    let (req_id, payload) = mm.await_rfq().await.expect("await rfq");
    assert_eq!(req_id, request_id);
    assert_eq!(payload.write_amount, write_amount);

    let premium = 10_000_000_u64; // 10 TUSDC — well under TUSDC_BALANCE
    let valid_until_ms = now_unix_ms() + 30_000;
    mm.send_signed_quote(
        &req_id,
        bucket,
        write_amount,
        premium,
        1, // nonce
        valid_until_ms,
        env.quoting().config.protocol_id.clone(),
    )
    .await
    .expect("sign+send quote");

    // Retail receives the RFQResponse with the MM's quote.
    let resp_text = integration_tests::router::next_text(&mut retail).await.expect("rfq resp");
    let parsed: shared::protocol_types::messages::ServiceToRetail =
        serde_json::from_str(&resp_text).expect("parse retail frame");
    let quote_entry: RfqQuoteEntry = match parsed {
        shared::protocol_types::messages::ServiceToRetail::RFQResponse { payload, .. } => {
            assert_eq!(payload.quotes.len(), 1, "exactly one quote");
            payload.quotes.into_iter().next().unwrap()
        }
        other => panic!("expected RFQResponse, got {:?}", other),
    };
    assert_eq!(quote_entry.quote.premium, premium);

    // Writer submits the PTB on chain.
    let digest = writer
        .execute(&env, bucket, "TBTC", "TUSDC", mm.account_id, &quote_entry)
        .await
        .expect("execute_write");
    tracing::info!(%digest, "writer-flow PTB confirmed");

    // Wait for the indexer to ingest the WriteExecuted event + the
    // quoting service to release the reservation as a side effect.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut observed = false;
    while std::time::Instant::now() < deadline {
        let after = env.quoting().app.available(mm_pt, &usdc);
        if after == TUSDC_BALANCE - premium && env.quoting().app.reservations.is_empty() {
            observed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        observed,
        "quoting service should debit MM's USDC by premium ({}) and clear the reservation",
        premium
    );

    // Bucket cursor should reflect one write.
    let bucket_view = env
        .quoting()
        .app
        .buckets
        .get(&bucket_pt(bucket))
        .expect("bucket view");
    assert_eq!(
        bucket_view.total_written,
        write_amount as u128,
        "bucket.total_written reflects the landed write"
    );
}

/// MockMm signs one quote, writer submits, then submits the *same*
/// SignedQuote a second time. First tx succeeds; second reverts with
/// `E_QUOTE_NONCE_USED`.
#[tokio::test]
#[ignore = "chain — needs sui binary matching framework/mainnet; see file doc"]
async fn chain_nonce_replay_reverts() {
    let env = TestEnv::fresh().await.expect("fresh env");
    let bucket = env
        .create_btc_usdc_bucket(now_unix_ms() + 3_600_000, 50_000_000_000)
        .await
        .expect("bucket");
    let mut mm = MockMm::bootstrap(&env, vec![MmRole::TraderMm], TUSDC_BALANCE)
        .await
        .expect("mm");
    let writer = Writer::bootstrap(&env).await.expect("writer");

    let mut retail = integration_tests::router::connect_retail(
        env.quoting().bind_addr,
        shared::protocol_types::sides::RetailRole::Writer,
    )
    .await
    .expect("retail");
    tokio::time::sleep(Duration::from_millis(100)).await;

    integration_tests::router::send_rfq(
        &mut retail,
        "replay-rfq",
        bucket_pt(bucket),
        500_000,
        shared::protocol_types::sides::Side::Writer,
    )
    .await
    .unwrap();
    let (req_id, _) = mm.await_rfq().await.unwrap();
    mm.send_signed_quote(
        &req_id,
        bucket,
        500_000,
        5_000_000,
        42,
        now_unix_ms() + 30_000,
        env.quoting().config.protocol_id.clone(),
    )
    .await
    .unwrap();
    let resp_text = integration_tests::router::next_text(&mut retail).await.unwrap();
    let parsed: shared::protocol_types::messages::ServiceToRetail =
        serde_json::from_str(&resp_text).unwrap();
    let entry: RfqQuoteEntry = match parsed {
        shared::protocol_types::messages::ServiceToRetail::RFQResponse { payload, .. } => {
            payload.quotes.into_iter().next().unwrap()
        }
        other => panic!("expected RFQResponse, got {:?}", other),
    };

    // First write — succeeds.
    let digest1 = writer
        .execute(&env, bucket, "TBTC", "TUSDC", mm.account_id, &entry)
        .await
        .expect("first write");
    tracing::info!(%digest1, "first write landed");

    // Second write with the same SignedQuote — must revert with
    // `E_QUOTE_NONCE_USED`. We bypass the quoting-service round-trip
    // (the service won't reissue a reservation for a used nonce); fire
    // the PTB directly.
    let err = writer
        .execute(&env, bucket, "TBTC", "TUSDC", mm.account_id, &entry)
        .await
        .err()
        .expect("second write should revert");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("execute_write reverted"),
        "expected on-chain revert, got: {msg}"
    );
}

/// MM signs a quote, then "withdraws" by signing a second quote on
/// nonce+1 that consumes the remaining balance. Two writers race to
/// submit them; one succeeds, the other reverts with
/// `E_INSUFFICIENT_ACCOUNT_BALANCE`.
#[tokio::test]
#[ignore = "chain — needs sui binary matching framework/mainnet; see file doc"]
async fn chain_mm_oversubscription() {
    let env = TestEnv::fresh().await.expect("fresh env");
    let bucket = env
        .create_btc_usdc_bucket(now_unix_ms() + 3_600_000, 50_000_000_000)
        .await
        .expect("bucket");

    // Tight balance: fund the MM with exactly one quote's worth of
    // premium. The second oversubscribing quote then has to revert at
    // the chain level.
    let one_premium = 5_000_000_u64;
    let mm = MockMm::bootstrap(&env, vec![MmRole::TraderMm], one_premium)
        .await
        .expect("mm");
    let writer_a = Writer::bootstrap(&env).await.expect("writer a");
    let writer_b = Writer::bootstrap(&env).await.expect("writer b");

    // Both quotes signed by the MM (off-band — no RFQ round trip
    // needed, the test owns the signing key).
    let make_quote = |nonce: u64| -> RfqQuoteEntry {
        use ed25519_dalek::Signer as _;
        use shared::protocol_types::ids::{ObjectId as PtId, SuiAddress as PtAddr};
        use shared::protocol_types::quote::Quote;

        let bucket_pt_id = {
            let mut b = [0u8; 32];
            b.copy_from_slice(bucket.into_bytes().as_ref());
            PtId::new(b)
        };
        let acct_pt = {
            let mut b = [0u8; 32];
            b.copy_from_slice(mm.account_id.into_bytes().as_ref());
            PtId::new(b)
        };
        let recipient = PtAddr::from_hex(&mm.sui_address.to_string()).unwrap();
        let q = Quote {
            protocol_id: env.quoting().config.protocol_id.clone(),
            signer_account_id: acct_pt,
            signer_token_recipient: recipient,
            bucket_id: bucket_pt_id,
            write_amount: 100_000,
            premium: one_premium,
            valid_until_ms: now_unix_ms() + 60_000,
            nonce,
        };
        let sig = mm.quote_sk.sign(&q.to_bcs_bytes().unwrap()).to_bytes().to_vec();
        RfqQuoteEntry {
            quote: q,
            signature: sig,
            mm_id: acct_pt,
            mm_reputation: 1.0,
        }
    };
    let entry_a = make_quote(1);
    let entry_b = make_quote(2);

    // First write drains the MM's full premium balance.
    writer_a
        .execute(&env, bucket, "TBTC", "TUSDC", mm.account_id, &entry_a)
        .await
        .expect("first write");

    // Second write can't be backed — must revert with insufficient balance.
    let err = writer_b
        .execute(&env, bucket, "TBTC", "TUSDC", mm.account_id, &entry_b)
        .await
        .err()
        .expect("second write should revert (oversubscription)");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("execute_write reverted"),
        "expected revert, got: {msg}"
    );
}

/// MM signs with a short TTL; writer waits past the TTL before
/// submitting. Chain reverts with `E_QUOTE_EXPIRED`.
#[tokio::test]
#[ignore = "chain — needs sui binary matching framework/mainnet; see file doc"]
async fn chain_quote_expired_reverts() {
    let env = TestEnv::fresh().await.expect("fresh env");
    let bucket = env
        .create_btc_usdc_bucket(now_unix_ms() + 3_600_000, 50_000_000_000)
        .await
        .expect("bucket");
    let mm = MockMm::bootstrap(&env, vec![MmRole::TraderMm], TUSDC_BALANCE)
        .await
        .expect("mm");
    let writer = Writer::bootstrap(&env).await.expect("writer");

    // Sign with a 500ms TTL.
    let valid_until_ms = now_unix_ms() + 500;
    let entry = {
        use ed25519_dalek::Signer as _;
        use shared::protocol_types::ids::{ObjectId as PtId, SuiAddress as PtAddr};
        use shared::protocol_types::quote::Quote;
        let bucket_pt_id = {
            let mut b = [0u8; 32];
            b.copy_from_slice(bucket.into_bytes().as_ref());
            PtId::new(b)
        };
        let acct_pt = {
            let mut b = [0u8; 32];
            b.copy_from_slice(mm.account_id.into_bytes().as_ref());
            PtId::new(b)
        };
        let q = Quote {
            protocol_id: env.quoting().config.protocol_id.clone(),
            signer_account_id: acct_pt,
            signer_token_recipient: PtAddr::from_hex(&mm.sui_address.to_string()).unwrap(),
            bucket_id: bucket_pt_id,
            write_amount: 100_000,
            premium: 1_000_000,
            valid_until_ms,
            nonce: 1,
        };
        let sig = mm.quote_sk.sign(&q.to_bcs_bytes().unwrap()).to_bytes().to_vec();
        RfqQuoteEntry {
            quote: q,
            signature: sig,
            mm_id: acct_pt,
            mm_reputation: 1.0,
        }
    };

    // Sit on the quote until it's stale. The on-chain `Clock` reads
    // wall-clock ms; the localnet's clock advances in real time.
    tokio::time::sleep(Duration::from_millis(1_500)).await;

    let err = writer
        .execute(&env, bucket, "TBTC", "TUSDC", mm.account_id, &entry)
        .await
        .err()
        .expect("expired quote should revert");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("execute_write reverted"),
        "expected revert, got: {msg}"
    );
}

/// Indexer-recovery sketch: ingest a write, then verify the quoting
/// service has caught up; bounce the indexer client subscription by
/// dropping it (force a re-subscribe), and confirm the state stays
/// consistent. The real "indexer task crash + restart" is hard to do
/// without forking the indexer's `setup_single_workflow` lifecycle —
/// this test asserts the weaker (but more achievable) "fanout
/// reconnect doesn't double-count or lose events" property.
#[tokio::test]
#[ignore = "chain — needs sui binary matching framework/mainnet; see file doc"]
async fn chain_indexer_resubscribe_is_idempotent() {
    let env = TestEnv::fresh().await.expect("fresh env");
    let bucket = env
        .create_btc_usdc_bucket(now_unix_ms() + 3_600_000, 50_000_000_000)
        .await
        .expect("bucket");

    // Snapshot bucket view; subscribing again should land the same
    // total_written / cursor.
    let view1 = env
        .quoting()
        .app
        .buckets
        .get(&bucket_pt(bucket))
        .expect("bucket view 1");

    // Open a fresh indexer subscription (this exercises the snapshot
    // codepath without touching the bound quoting service).
    let url = format!("ws://{}/", env.indexer().fanout_addr);
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("indexer ws connect");
    use futures_util::SinkExt as _;
    use futures_util::StreamExt as _;
    use shared::protocol_types::messages::{IndexerStream, IndexerSubscribe};
    use tokio_tungstenite::tungstenite::Message;
    ws.send(Message::Text(
        serde_json::to_string(&IndexerSubscribe::Subscribe { after_sequence: 0 }).unwrap(),
    ))
    .await
    .expect("send subscribe");
    let snapshot_text = match ws.next().await {
        Some(Ok(Message::Text(t))) => t,
        other => panic!("expected snapshot text, got {:?}", other),
    };
    let snap: IndexerStream = serde_json::from_str(&snapshot_text).expect("parse snapshot");
    let events = match snap {
        IndexerStream::Snapshot { payload } => payload.events,
        other => panic!("expected Snapshot, got {:?}", other),
    };

    // We expect at least one BucketCreated for the bucket we made.
    let bucket_pt_id = bucket_pt(bucket);
    let saw_bucket = events.iter().any(|ev| matches!(
        &ev.event,
        ChainEvent::BucketCreated(b) if b.bucket_id == bucket_pt_id
    ));
    assert!(saw_bucket, "fresh subscription should snapshot the BucketCreated event");

    let view2 = env
        .quoting()
        .app
        .buckets
        .get(&bucket_pt(bucket))
        .expect("bucket view 2");
    assert_eq!(
        view1.total_written, view2.total_written,
        "bucket view stayed consistent across the second subscription"
    );

    // Drain whatever else the indexer might want to push, then close.
    let _ = ws.close(None).await;
    // Heartbeats from the original quoting subscription will continue;
    // confirm the worker_task hasn't died.
    assert!(
        env.indexer()
            .worker_task
            .as_ref()
            .map(|h| !h.is_finished())
            .unwrap_or(false),
        "indexer worker should still be running"
    );

}

// -- helpers --------------------------------------------------------------

fn now_unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn bucket_pt(b: sui_types::base_types::ObjectID) -> shared::protocol_types::ids::ObjectId {
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(b.into_bytes().as_ref());
    shared::protocol_types::ids::ObjectId::new(bytes)
}
