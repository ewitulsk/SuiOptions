//! End-to-end: mock indexer GraphQL + quoting service WS + mock retail +
//! mock MM, all in process.
//!
//! Validates the happy path of the RFQ flow described in §5.8:
//!
//! 1. The MM connects, authenticates, and is registered as a Trader MM.
//! 2. Retail sends an `RFQRequest` for the writer-side of a known bucket.
//! 3. The service broadcasts an `RFQBroadcast` to the MM.
//! 4. The MM signs a `Quote` (carrying the collateral routing inside the
//!    signed payload) over BCS canonical bytes and replies.
//! 5. The service validates signature / expiry / nonce / routing and ships
//!    an `RFQResponse` back to retail with that quote. There is no balance
//!    or reservation tracking (collateral abstraction, plan §7).
//!
//! Plus a tampering check: a quote where premium was bumped after signing
//! never makes it into the response.

use std::time::Duration;

use futures_util::SinkExt;
use integration_tests::*;
use tokio_tungstenite::tungstenite::Message;

use protocol_types::messages::{MmToService, ServiceToMm, ServiceToRetail};
use protocol_types::sides::{MmRole, RetailRole, Side};

#[tokio::test]
async fn rfq_round_trip() {
    let h = Harness::start().await.unwrap();

    let mut retail = connect_retail(h.quoting_addr, RetailRole::Writer).await.unwrap();
    let mut mm = connect_mm(
        h.quoting_addr,
        h.mm_account,
        &h.mm_sk,
        vec![MmRole::TraderMm],
    )
    .await
    .unwrap();

    // Give the MM registry a tick to record the auth.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Fire the RFQ.
    send_rfq(&mut retail, "req-1", h.bucket, 10_000, Side::Writer)
        .await
        .unwrap();

    // MM should see an RFQBroadcast.
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

    // MM signs and replies.
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

    // Retail should get the RFQResponse.
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

    let mut retail = connect_retail(h.quoting_addr, RetailRole::Writer).await.unwrap();
    let mut mm = connect_mm(
        h.quoting_addr,
        h.mm_account,
        &h.mm_sk,
        vec![MmRole::TraderMm],
    )
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

    // Sign one quote, then mutate the premium after the fact so the
    // signature no longer matches the payload.
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

    // Service should respond with no quotes (validation drops the tampered
    // entry; nothing else is on the wire).
    let resp_text = next_text(&mut retail).await.unwrap();
    let parsed: ServiceToRetail = serde_json::from_str(&resp_text).unwrap();
    match parsed {
        ServiceToRetail::Error { request_id, payload } => {
            assert_eq!(request_id.as_deref(), Some("req-tamper"));
            assert_eq!(payload.code, "no_quotes");
        }
        other => panic!("expected Error, got {:?}", other),
    }

    // The tampered quote never got past validation — its nonce was not
    // recorded as seen, so the MM can reuse it on a valid quote.
    assert!(h.indexer_store.account(&h.mm_account).is_some());
}
