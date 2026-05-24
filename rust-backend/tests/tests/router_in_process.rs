//! Layer 1 — in-process router tests.
//!
//! Exercises the WS quoting service + a fake indexer fanout backed by
//! `indexer::Store::ingest`. No chain, no PTBs. See
//! [`integration_tests::router`] for the harness.

use std::time::Duration;

use futures_util::SinkExt;
use integration_tests::*;
use tokio_tungstenite::tungstenite::Message;

use shared::protocol_types::asset::AssetType;
use shared::protocol_types::messages::{MmToService, ServiceToMm, ServiceToRetail};
use shared::protocol_types::sides::{MmRole, RetailRole, Side};

// ---------------------------------------------------------------------------
// Original happy-path + tamper tests (Phase 0 preserved these verbatim).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rfq_round_trip() {
    let h = Harness::start().await.unwrap();

    let mut retail = h.connect_retail(RetailRole::Writer).await.unwrap();
    let mut mm = h
        .connect_mm(h.mm_account, &h.mm_sk, vec![MmRole::TraderMm])
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(50)).await;

    send_rfq(&mut retail, "req-1", h.bucket, 10_000, Side::Writer)
        .await
        .unwrap();

    let broadcast_text = next_text(&mut mm).await.unwrap();
    let broadcast: ServiceToMm = serde_json::from_str(&broadcast_text).unwrap();
    let (req_id, bucket_id, write_amount, side, deadline_ms) = match broadcast {
        ServiceToMm::RFQBroadcast { request_id, payload } => (
            request_id,
            payload.bucket_id,
            payload.write_amount,
            payload.side,
            payload.deadline_ms,
        ),
        other => panic!("expected RFQBroadcast, got {:?}", other),
    };
    assert_eq!(req_id, "req-1");
    assert_eq!(bucket_id, h.bucket);
    assert_eq!(write_amount, 10_000);
    assert_eq!(side, Side::Writer);
    assert!(deadline_ms > 0);

    let signed = build_signed_quote(
        &h.mm_sk,
        h.protocol_id.clone(),
        h.mm_account,
        h.bucket,
        10_000,
        500_000,
        1,
        deadline_ms + 5_000,
    );
    let reply = MmToService::Quote {
        request_id: req_id.clone(),
        payload: signed,
    };
    mm.send(Message::Text(serde_json::to_string(&reply).unwrap()))
        .await
        .unwrap();

    let resp_text = next_text(&mut retail).await.unwrap();
    let parsed: ServiceToRetail = serde_json::from_str(&resp_text).unwrap();
    match parsed {
        ServiceToRetail::RFQResponse { request_id, payload } => {
            assert_eq!(request_id, "req-1");
            assert_eq!(payload.bucket_id, h.bucket);
            assert_eq!(payload.write_amount, 10_000);
            assert_eq!(payload.quotes.len(), 1);
            assert_eq!(payload.quotes[0].quote.premium, 500_000);
            assert_eq!(payload.quotes[0].mm_id, h.mm_account);
        }
        other => panic!("expected RFQResponse, got {:?}", other),
    }
}

#[tokio::test]
async fn tampered_quote_is_filtered_out() {
    let h = Harness::start().await.unwrap();

    let mut retail = h.connect_retail(RetailRole::Writer).await.unwrap();
    let mut mm = h
        .connect_mm(h.mm_account, &h.mm_sk, vec![MmRole::TraderMm])
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(50)).await;

    send_rfq(&mut retail, "req-tamper", h.bucket, 10_000, Side::Writer)
        .await
        .unwrap();

    let broadcast_text = next_text(&mut mm).await.unwrap();
    let broadcast: ServiceToMm = serde_json::from_str(&broadcast_text).unwrap();
    let (req_id, deadline_ms) = match broadcast {
        ServiceToMm::RFQBroadcast { request_id, payload } => (request_id, payload.deadline_ms),
        other => panic!("expected RFQBroadcast, got {:?}", other),
    };

    let mut signed = build_signed_quote(
        &h.mm_sk,
        h.protocol_id.clone(),
        h.mm_account,
        h.bucket,
        10_000,
        500_000,
        2,
        deadline_ms + 5_000,
    );
    signed.quote.premium = 9_999_999;
    let reply = MmToService::Quote {
        request_id: req_id.clone(),
        payload: signed,
    };
    mm.send(Message::Text(serde_json::to_string(&reply).unwrap()))
        .await
        .unwrap();

    let resp_text = next_text(&mut retail).await.unwrap();
    let parsed: ServiceToRetail = serde_json::from_str(&resp_text).unwrap();
    match parsed {
        ServiceToRetail::Error { request_id, payload } => {
            assert_eq!(request_id.as_deref(), Some("req-tamper"));
            assert_eq!(payload.code, "no_quotes");
        }
        other => panic!("expected Error, got {:?}", other),
    }

    let avail = h
        .indexer_store
        .account(&h.mm_account)
        .unwrap()
        .balances
        .get(&AssetType::new("USDC"))
        .copied()
        .unwrap_or(0);
    assert_eq!(avail, 1_000_000_000);
}

// ---------------------------------------------------------------------------
// Phase 1 — new scenarios.
// ---------------------------------------------------------------------------

/// Accepting a quote must reserve the MM's settlement balance, so
/// `app.available(...)` reflects `balance - premium` and the reservation
/// table holds exactly one entry for `(account, nonce)`.
#[tokio::test]
async fn mm_balance_reserved_on_accept() {
    let h = Harness::start().await.unwrap();
    let usdc = AssetType::new("USDC");
    let starting = h.app.available(h.mm_account, &usdc);
    assert_eq!(starting, 1_000_000_000);

    let mut retail = h.connect_retail(RetailRole::Writer).await.unwrap();
    let mut mm = h
        .connect_mm(h.mm_account, &h.mm_sk, vec![MmRole::TraderMm])
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    send_rfq(&mut retail, "req-res", h.bucket, 10_000, Side::Writer)
        .await
        .unwrap();
    let broadcast: ServiceToMm = serde_json::from_str(&next_text(&mut mm).await.unwrap()).unwrap();
    let (req_id, deadline_ms) = match broadcast {
        ServiceToMm::RFQBroadcast { request_id, payload } => (request_id, payload.deadline_ms),
        other => panic!("expected RFQBroadcast, got {:?}", other),
    };

    let premium = 7_500_000_u64;
    let nonce = 42_u64;
    let signed = build_signed_quote(
        &h.mm_sk,
        h.protocol_id.clone(),
        h.mm_account,
        h.bucket,
        10_000,
        premium,
        nonce,
        deadline_ms + 5_000,
    );
    mm.send(Message::Text(
        serde_json::to_string(&MmToService::Quote { request_id: req_id, payload: signed })
            .unwrap(),
    ))
    .await
    .unwrap();

    // Wait for the RFQResponse — by the time it lands, validate_and_reserve
    // has already placed the reservation.
    let resp_text = next_text(&mut retail).await.unwrap();
    let parsed: ServiceToRetail = serde_json::from_str(&resp_text).unwrap();
    assert!(matches!(parsed, ServiceToRetail::RFQResponse { .. }));

    assert_eq!(h.app.reservations.len(), 1);
    assert_eq!(
        h.app.available(h.mm_account, &usdc),
        starting - premium,
        "available balance should be debited by the reserved premium"
    );
}

/// Once the `WriteExecuted` event for a quote's `(account, nonce)` lands
/// via the indexer stream, the reservation must be released — even though
/// the on-chain balance has already been debited at the chain level.
#[tokio::test]
async fn reservation_released_on_write_executed_event() {
    let h = Harness::start().await.unwrap();
    let mut retail = h.connect_retail(RetailRole::Writer).await.unwrap();
    let mut mm = h
        .connect_mm(h.mm_account, &h.mm_sk, vec![MmRole::TraderMm])
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    send_rfq(&mut retail, "req-rel", h.bucket, 5_000, Side::Writer)
        .await
        .unwrap();
    let broadcast: ServiceToMm = serde_json::from_str(&next_text(&mut mm).await.unwrap()).unwrap();
    let (req_id, deadline_ms) = match broadcast {
        ServiceToMm::RFQBroadcast { request_id, payload } => (request_id, payload.deadline_ms),
        other => panic!("expected RFQBroadcast, got {:?}", other),
    };
    let signed = build_signed_quote(
        &h.mm_sk,
        h.protocol_id.clone(),
        h.mm_account,
        h.bucket,
        5_000,
        100_000,
        77,
        deadline_ms + 5_000,
    );
    mm.send(Message::Text(
        serde_json::to_string(&MmToService::Quote { request_id: req_id, payload: signed })
            .unwrap(),
    ))
    .await
    .unwrap();
    let _ = next_text(&mut retail).await.unwrap();
    assert_eq!(h.app.reservations.len(), 1);

    // Now publish the chain event the indexer would normally emit when
    // the writer's PTB lands.
    h.ingest_write_executed(h.bucket, h.mm_account, 5_000, 100_000, 77, 0)
        .await;

    // Indexer client → AppState propagation is async; give it a couple
    // ticks (`ingest` already waits 40ms internally).
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    while std::time::Instant::now() < deadline {
        if h.app.reservations.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        h.app.reservations.len(),
        0,
        "WriteExecuted should release the matching reservation"
    );
}

/// Quote with `valid_until_ms` already in the past at validation time
/// must be filtered. Service returns `Error { code: "no_quotes" }`.
#[tokio::test]
async fn stale_quote_filtered() {
    let h = Harness::start().await.unwrap();
    let mut retail = h.connect_retail(RetailRole::Writer).await.unwrap();
    let mut mm = h
        .connect_mm(h.mm_account, &h.mm_sk, vec![MmRole::TraderMm])
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    send_rfq(&mut retail, "req-stale", h.bucket, 1_000, Side::Writer)
        .await
        .unwrap();
    let broadcast: ServiceToMm = serde_json::from_str(&next_text(&mut mm).await.unwrap()).unwrap();
    let req_id = match broadcast {
        ServiceToMm::RFQBroadcast { request_id, .. } => request_id,
        other => panic!("expected RFQBroadcast, got {:?}", other),
    };

    // `valid_until_ms = 1` is unambiguously in the past — service should
    // reject before signature verification matters.
    let signed = build_signed_quote(
        &h.mm_sk,
        h.protocol_id.clone(),
        h.mm_account,
        h.bucket,
        1_000,
        50_000,
        100,
        1,
    );
    mm.send(Message::Text(
        serde_json::to_string(&MmToService::Quote { request_id: req_id, payload: signed })
            .unwrap(),
    ))
    .await
    .unwrap();

    let resp: ServiceToRetail =
        serde_json::from_str(&next_text(&mut retail).await.unwrap()).unwrap();
    match resp {
        ServiceToRetail::Error { payload, .. } => assert_eq!(payload.code, "no_quotes"),
        other => panic!("expected Error, got {:?}", other),
    }
    assert_eq!(h.app.reservations.len(), 0);
}

/// MM signs a quote whose premium exceeds the MM's available USDC
/// balance. Service drops the quote and replies `no_quotes`.
#[tokio::test]
async fn insufficient_balance_filtered() {
    let h = Harness::start().await.unwrap();
    let (poor_mm, poor_sk) = h.add_mm(&AssetType::new("USDC"), 100).await;

    let mut retail = h.connect_retail(RetailRole::Writer).await.unwrap();
    let mut mm = h
        .connect_mm(poor_mm, &poor_sk, vec![MmRole::TraderMm])
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    send_rfq(&mut retail, "req-poor", h.bucket, 1_000, Side::Writer)
        .await
        .unwrap();
    let broadcast: ServiceToMm = serde_json::from_str(&next_text(&mut mm).await.unwrap()).unwrap();
    let (req_id, deadline_ms) = match broadcast {
        ServiceToMm::RFQBroadcast { request_id, payload } => (request_id, payload.deadline_ms),
        other => panic!("expected RFQBroadcast, got {:?}", other),
    };

    // Premium 500 > available balance 100.
    let signed = build_signed_quote(
        &poor_sk,
        h.protocol_id.clone(),
        poor_mm,
        h.bucket,
        1_000,
        500,
        1,
        deadline_ms + 5_000,
    );
    mm.send(Message::Text(
        serde_json::to_string(&MmToService::Quote { request_id: req_id, payload: signed })
            .unwrap(),
    ))
    .await
    .unwrap();

    let resp: ServiceToRetail =
        serde_json::from_str(&next_text(&mut retail).await.unwrap()).unwrap();
    match resp {
        ServiceToRetail::Error { payload, .. } => assert_eq!(payload.code, "no_quotes"),
        other => panic!("expected Error, got {:?}", other),
    }
    assert_eq!(h.app.reservations.len(), 0);
}

/// Two MMs both respond. Writer-side: highest premium first. Sort applies
/// across MMs, not just within one.
#[tokio::test]
async fn multi_mm_sorting() {
    let h = Harness::start().await.unwrap();
    let (mm2_acct, mm2_sk) = h.add_mm(&AssetType::new("USDC"), 1_000_000_000).await;

    let mut retail = h.connect_retail(RetailRole::Writer).await.unwrap();
    let mut mm1 = h
        .connect_mm(h.mm_account, &h.mm_sk, vec![MmRole::TraderMm])
        .await
        .unwrap();
    let mut mm2 = h
        .connect_mm(mm2_acct, &mm2_sk, vec![MmRole::TraderMm])
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;

    send_rfq(&mut retail, "req-multi", h.bucket, 5_000, Side::Writer)
        .await
        .unwrap();

    // Both MMs see the broadcast (same request_id).
    let bcast1: ServiceToMm =
        serde_json::from_str(&next_text(&mut mm1).await.unwrap()).unwrap();
    let bcast2: ServiceToMm =
        serde_json::from_str(&next_text(&mut mm2).await.unwrap()).unwrap();
    let (req1, deadline1) = match bcast1 {
        ServiceToMm::RFQBroadcast { request_id, payload } => (request_id, payload.deadline_ms),
        _ => panic!("expected RFQBroadcast"),
    };
    let (req2, deadline2) = match bcast2 {
        ServiceToMm::RFQBroadcast { request_id, payload } => (request_id, payload.deadline_ms),
        _ => panic!("expected RFQBroadcast"),
    };
    assert_eq!(req1, "req-multi");
    assert_eq!(req2, "req-multi");

    // mm1 = 400k, mm2 = 600k. Writer-side sort puts mm2 first.
    let q1 = build_signed_quote(
        &h.mm_sk, h.protocol_id.clone(), h.mm_account, h.bucket,
        5_000, 400_000, 1, deadline1 + 5_000,
    );
    let q2 = build_signed_quote(
        &mm2_sk, h.protocol_id.clone(), mm2_acct, h.bucket,
        5_000, 600_000, 1, deadline2 + 5_000,
    );
    mm1.send(Message::Text(
        serde_json::to_string(&MmToService::Quote { request_id: "req-multi".into(), payload: q1 })
            .unwrap(),
    ))
    .await
    .unwrap();
    mm2.send(Message::Text(
        serde_json::to_string(&MmToService::Quote { request_id: "req-multi".into(), payload: q2 })
            .unwrap(),
    ))
    .await
    .unwrap();

    let resp: ServiceToRetail =
        serde_json::from_str(&next_text(&mut retail).await.unwrap()).unwrap();
    match resp {
        ServiceToRetail::RFQResponse { payload, .. } => {
            assert_eq!(payload.quotes.len(), 2, "both quotes should be present");
            assert_eq!(
                payload.quotes[0].quote.premium, 600_000,
                "writer-side sorts highest first"
            );
            assert_eq!(payload.quotes[0].mm_id, mm2_acct);
            assert_eq!(payload.quotes[1].quote.premium, 400_000);
            assert_eq!(payload.quotes[1].mm_id, h.mm_account);
        }
        other => panic!("expected RFQResponse, got {:?}", other),
    }
}

/// RFQ with zero MMs connected. After the window, service returns
/// `no_quotes`. Confirms the deadline path doesn't hang.
#[tokio::test]
async fn no_mms_returns_no_quotes() {
    let h = Harness::start().await.unwrap();
    let mut retail = h.connect_retail(RetailRole::Writer).await.unwrap();
    // No MMs connected.

    send_rfq(&mut retail, "req-empty", h.bucket, 1_000, Side::Writer)
        .await
        .unwrap();

    let resp: ServiceToRetail =
        serde_json::from_str(&next_text(&mut retail).await.unwrap()).unwrap();
    match resp {
        ServiceToRetail::Error { payload, request_id } => {
            assert_eq!(payload.code, "no_quotes");
            assert_eq!(request_id.as_deref(), Some("req-empty"));
        }
        other => panic!("expected Error, got {:?}", other),
    }
}

/// MM connects, then closes its WS in the middle of the RFQ window
/// without replying. Service still returns `no_quotes` cleanly — no panic,
/// no hang past the deadline.
#[tokio::test]
async fn mm_disconnect_mid_window() {
    let h = Harness::start().await.unwrap();
    let mut retail = h.connect_retail(RetailRole::Writer).await.unwrap();
    let mut mm = h
        .connect_mm(h.mm_account, &h.mm_sk, vec![MmRole::TraderMm])
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    send_rfq(&mut retail, "req-discon", h.bucket, 1_000, Side::Writer)
        .await
        .unwrap();
    // Wait for the broadcast to land in the MM's read buffer (proves the
    // service really did dispatch to this MM).
    let _ = next_text(&mut mm).await.unwrap();
    // Now slam the door without responding.
    mm.close(None).await.ok();
    drop(mm);

    let resp: ServiceToRetail =
        serde_json::from_str(&next_text(&mut retail).await.unwrap()).unwrap();
    match resp {
        ServiceToRetail::Error { payload, .. } => assert_eq!(payload.code, "no_quotes"),
        other => panic!("expected Error, got {:?}", other),
    }
    assert_eq!(h.app.reservations.len(), 0);
}

/// RFQ against a bucket whose `expiry_ms` is in the past. Service should
/// short-circuit before broadcasting — no MM ever sees the RFQ — and reply
/// `no_quotes`. Spec §5.8 step 1 ("validates bucket_id exists and is not
/// expired"); enforced by the bucket-expiry guard added in
/// `rfq::orchestrate`.
#[tokio::test]
async fn bucket_expired_rejected() {
    let h = Harness::start().await.unwrap();
    let expired = h
        .add_bucket(
            AssetType::new("BTC"),
            AssetType::new("USDC"),
            50_000_000,
            1, // expired before unix epoch
        )
        .await;

    let mut retail = h.connect_retail(RetailRole::Writer).await.unwrap();
    let mut mm = h
        .connect_mm(h.mm_account, &h.mm_sk, vec![MmRole::TraderMm])
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    send_rfq(&mut retail, "req-expired", expired, 1_000, Side::Writer)
        .await
        .unwrap();

    // The MM should *not* see a broadcast — wait the full window plus
    // slack; if anything shows up the assertion below fires.
    let mm_frame = next_text_timeout(&mut mm, Duration::from_millis(600))
        .await
        .unwrap();
    assert!(
        mm_frame.is_none(),
        "no RFQBroadcast should have been sent for an expired bucket, got {:?}",
        mm_frame
    );

    let resp: ServiceToRetail =
        serde_json::from_str(&next_text(&mut retail).await.unwrap()).unwrap();
    match resp {
        ServiceToRetail::Error { payload, .. } => assert_eq!(payload.code, "no_quotes"),
        other => panic!("expected Error, got {:?}", other),
    }
}

/// Trader flow — symmetric mirror of `rfq_round_trip`. Retail is the
/// `Trader`; MM is a `WriterMm` providing the underlying out of an
/// Account-held BTC balance. Reservation in this direction debits BTC,
/// not USDC.
#[tokio::test]
async fn trader_flow_round_trip() {
    let h = Harness::start().await.unwrap();
    // Writer MM needs a balance in the bucket's *underlying* (BTC) since
    // trader-flow reservations debit underlying, not settlement.
    let (writer_mm, writer_mm_sk) = h.add_mm(&AssetType::new("BTC"), 1_000_000).await;

    let mut retail = h.connect_retail(RetailRole::Trader).await.unwrap();
    let mut mm = h
        .connect_mm(writer_mm, &writer_mm_sk, vec![MmRole::WriterMm])
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    send_rfq(&mut retail, "req-trader", h.bucket, 10_000, Side::Trader)
        .await
        .unwrap();

    let broadcast: ServiceToMm =
        serde_json::from_str(&next_text(&mut mm).await.unwrap()).unwrap();
    let (req_id, deadline_ms, side) = match broadcast {
        ServiceToMm::RFQBroadcast { request_id, payload } => {
            (request_id, payload.deadline_ms, payload.side)
        }
        other => panic!("expected RFQBroadcast, got {:?}", other),
    };
    assert_eq!(side, Side::Trader, "broadcast must carry the retail-side discriminator");

    let signed = build_signed_quote(
        &writer_mm_sk,
        h.protocol_id.clone(),
        writer_mm,
        h.bucket,
        10_000,
        250_000,
        1,
        deadline_ms + 5_000,
    );
    mm.send(Message::Text(
        serde_json::to_string(&MmToService::Quote { request_id: req_id, payload: signed })
            .unwrap(),
    ))
    .await
    .unwrap();

    let resp: ServiceToRetail =
        serde_json::from_str(&next_text(&mut retail).await.unwrap()).unwrap();
    match resp {
        ServiceToRetail::RFQResponse { payload, .. } => {
            assert_eq!(payload.quotes.len(), 1);
            assert_eq!(payload.quotes[0].quote.premium, 250_000);
            assert_eq!(payload.quotes[0].mm_id, writer_mm);
        }
        other => panic!("expected RFQResponse, got {:?}", other),
    }

    // Reservation goes against BTC (underlying), not USDC.
    let btc = AssetType::new("BTC");
    let usdc = AssetType::new("USDC");
    assert_eq!(
        h.app.available(writer_mm, &btc),
        1_000_000 - 10_000,
        "trader-flow reservation debits the underlying"
    );
    assert_eq!(
        h.app.available(writer_mm, &usdc),
        0,
        "no USDC was reserved"
    );
}
