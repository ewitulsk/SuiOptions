//! RFQ orchestration (§5.8).
//!
//! One retail `RFQRequest` →
//!   1. broadcast `RFQBroadcast` to every connected MM on the opposite side;
//!   2. collect `Quote`s until the deadline elapses (or every MM declines);
//!   3. validate each quote (signature over the collateral-abstraction BCS
//!      layout, expiry, nonce-unseen, bucket sanity, routing fields present);
//!   4. sort best-price-first for the retail side;
//!   5. return the eligible quotes — caller forwards as `RFQResponse`.
//!
//! There is NO balance/reservation feasibility check (plan §7): a collateral
//! implementation need not have a readable balance, so enforcement is the
//! on-chain revert and the revert-rate reputation filter.
//!
//! The validation/sort step is pure and testable without any WS. See
//! [`validate_quote`] / [`sort_best_first`].

pub mod bulk_view;
mod matcher;

pub use matcher::{collect_with_deadline, MatcherInput, MatcherOutput};

use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info, trace, warn};

use protocol_types::errors::ProtocolError;
use protocol_types::ids::{ObjectId, SuiAddress};
use protocol_types::messages::{MmQuotePayload, RfqQuoteEntry};
use protocol_types::sides::Side;

use indexer_graphql::{Account, Bucket};

use crate::state::{AppState, InsertOutcome};

/// What's wrong with a quote — surfaced for logging / reputation but never
/// propagated to retail (the retail-facing list just shows what was good).
#[derive(Debug, PartialEq, Eq)]
pub enum QuoteRejection {
    ProtocolMismatch,
    BucketMismatch,
    WriteAmountMismatch,
    Expired,
    SignatureInvalid,
    DuplicateNonce,
    UnknownSigner,
    UnknownBucket,
    InvalidPubkey,
    /// Admin has invalidated the bucket; a quote against it would revert
    /// on chain. Defense-in-depth against a race between the MM signing
    /// and the BucketInvalidated event landing.
    BucketInvalidated,
    /// The collateral routing (`collateral_source` / `release_package` /
    /// `release_module`) is absent — the quote could never execute.
    MissingRouting,
}

impl From<ProtocolError> for QuoteRejection {
    fn from(e: ProtocolError) -> Self {
        match e {
            ProtocolError::QuoteExpired => Self::Expired,
            ProtocolError::QuoteSignatureInvalid => Self::SignatureInvalid,
            ProtocolError::QuoteProtocolMismatch => Self::ProtocolMismatch,
            ProtocolError::QuoteBucketMismatch => Self::BucketMismatch,
            ProtocolError::QuoteAccountMismatch => Self::SignatureInvalid,
            _ => Self::SignatureInvalid,
        }
    }
}

/// Run a single quote through every check. On success, records the nonce as
/// seen and returns the entry that would go into `RFQResponse`. On failure,
/// returns the rejection reason.
///
/// `bucket` and `account` (the MM's QuoteSigner registration) are fetched
/// just-in-time by the caller ([`orchestrate`]) so this stays a pure,
/// synchronous function: no indexer round-trips happen here.
pub fn validate_quote(
    state: &AppState,
    bucket: &Bucket,
    account: &Account,
    write_amount: u64,
    payload: &MmQuotePayload,
    mm_signer_id: ObjectId,
    protocol_id: &[u8],
    now_ms: u64,
) -> Result<RfqQuoteEntry, QuoteRejection> {
    let quote = &payload.quote;
    trace!(mm = %mm_signer_id, bucket = %bucket.bucket_id, nonce = quote.nonce, premium = quote.premium, "validating quote");
    // Cheap structural checks first.
    if quote.protocol_id != protocol_id {
        return Err(QuoteRejection::ProtocolMismatch);
    }
    if quote.bucket_id != bucket.bucket_id {
        return Err(QuoteRejection::BucketMismatch);
    }
    if quote.write_amount != write_amount {
        return Err(QuoteRejection::WriteAmountMismatch);
    }
    if now_ms >= quote.valid_until_ms {
        return Err(QuoteRejection::Expired);
    }
    if quote.signer_id != mm_signer_id {
        return Err(QuoteRejection::SignatureInvalid);
    }
    // Routing fields present: without them the release call can't be built,
    // so the quote could never execute.
    if quote.collateral_source == ObjectId::ZERO
        || quote.release_package == SuiAddress::ZERO
        || quote.release_module.is_empty()
    {
        return Err(QuoteRejection::MissingRouting);
    }

    // Signature against the MM's registered pubkey + scheme.
    let scheme = account.signing_scheme.ok_or(QuoteRejection::InvalidPubkey)?;
    let signed = protocol_types::quote::SignedQuote {
        quote: quote.clone(),
        signature: payload.signature.clone(),
    };
    signed
        .verify(scheme, &account.signing_pubkey, protocol_id, mm_signer_id, now_ms)
        .map_err(QuoteRejection::from)?;

    // Bucket sanity.
    if bucket.invalidated {
        return Err(QuoteRejection::BucketInvalidated);
    }

    // Nonce-unseen. A duplicate nonce means the MM signed two quotes with
    // the same nonce, which would revert on chain; reject.
    let outcome = state
        .nonces
        .insert(mm_signer_id, quote.nonce, quote.valid_until_ms, now_ms);
    if outcome == InsertOutcome::DuplicateKey {
        return Err(QuoteRejection::DuplicateNonce);
    }

    // Reputation: count the signature.
    state.reputation.record_signed(mm_signer_id);

    let rep = state.reputation.snapshot(&mm_signer_id).composite_score();
    Ok(RfqQuoteEntry {
        quote: quote.clone(),
        signature: payload.signature.clone(),
        mm_id: mm_signer_id,
        mm_reputation: rep,
    })
}

/// Sort `quotes` in place so the best-for-retail entry is first.
///
/// - Writer-side retail wants the **highest** premium (they're selling).
/// - Trader-side retail wants the **lowest** premium (they're buying).
///
/// Tiebreak: higher reputation first.
pub fn sort_best_first(side: Side, quotes: &mut [RfqQuoteEntry]) {
    quotes.sort_by(|a, b| match side {
        Side::Writer => b
            .quote
            .premium
            .cmp(&a.quote.premium)
            .then(b.mm_reputation.partial_cmp(&a.mm_reputation).unwrap_or(std::cmp::Ordering::Equal)),
        Side::Trader => a
            .quote
            .premium
            .cmp(&b.quote.premium)
            .then(b.mm_reputation.partial_cmp(&a.mm_reputation).unwrap_or(std::cmp::Ordering::Equal)),
    });
}

/// Drive an RFQ from the retail request through to the validated, sorted
/// list of `RfqQuoteEntry`s. Spawns the broadcast and the deadline-bounded
/// collection — pure plumbing; the validation lives in [`validate_quote`].
pub async fn orchestrate(
    state: Arc<AppState>,
    side: Side,
    bucket: Bucket,
    write_amount: u64,
    request_id: String,
    rfq_window: Duration,
    protocol_id: Vec<u8>,
    now_ms: u64,
) -> Vec<RfqQuoteEntry> {
    let bucket_id = bucket.bucket_id;
    let deadline_ms = now_ms.saturating_add(rfq_window.as_millis() as u64);

    // The bucket is fetched JIT by the caller (retail.rs) so the broadcast can
    // include its strike + expiry — MMs price against these instead of
    // guessing. Defense-in-depth against direct callers: refuse invalidated.
    if bucket.invalidated {
        debug!(%bucket_id, "rfq for invalidated bucket — returning empty");
        return Vec::new();
    }

    let mm_role = side.counterparty_mm();
    let mms = state.mms.all_for_role(mm_role);
    debug!(?side, mms = mms.len(), request_id = %request_id, "rfq broadcast");
    metrics::counter!("quoting_rfq_broadcasts_total").increment(1);

    // Floor capacity at 8 so a single-MM deployment still has headroom for
    // the response burst; without this the channel is `channel(1)` and
    // `tx.send().await` from the MM read task serializes on every quote.
    let (mut input, output) = matcher::channel(mms.len().max(8));
    // Publish the matcher's input side under the request_id so the MM read
    // tasks can route their responses to it.
    state
        .pending_rfqs
        .insert(request_id.clone(), input.tx.clone());

    // Hand the receiver off to the matcher.
    let collector = tokio::spawn(collect_with_deadline(output, rfq_window));

    // Broadcast to MMs.
    for mm in mms {
        let frame = protocol_types::messages::ServiceToMm::RFQBroadcast {
            request_id: request_id.clone(),
            payload: protocol_types::messages::RfqBroadcastPayload {
                // Only the bucket address travels — the MM resolves the
                // strike/expiry/coin-types itself from api-service so it never
                // trusts pricing inputs delivered over the wire.
                bucket_id,
                write_amount,
                side,
                deadline_ms,
            },
        };
        // Tell the matcher who we expect a response from so it can decide
        // to short-circuit when every MM has answered.
        input.expect(mm.account_id);
        // try_send: a slow MM whose outbound channel is full should miss
        // this window, not back-pressure the orchestrator. Awaiting an
        // unbounded send here lets one stuck MM pile up orchestrators
        // indefinitely under load.
        if let Err(e) = mm.tx.try_send(frame) {
            debug!(mm = %mm.account_id, request_id = %request_id, error = %e, "dropping rfq broadcast: mm channel full or closed");
            input.unexpect(mm.account_id);
        }
    }
    // Drop the sender side(s) we hold so the matcher's close-detection works
    // once every MM that's going to answer has done so.
    drop(input);

    let collected = collector.await.unwrap_or(MatcherOutput::default());
    metrics::counter!("quoting_quotes_received_total", "outcome" => "declined")
        .increment(collected.declines.len() as u64);
    let raw_responses = collected.responses;
    debug!(request_id = %request_id, responses = raw_responses.len(), "rfq collection complete");
    // Once the deadline closes the receiver, remove the routing entry.
    state.pending_rfqs.remove(&request_id);

    // Validate. Each MM's QuoteSigner registration (signing key) is fetched
    // JIT; freshly-landed fills are recorded for the reputation fill-rate.
    let mut accepted = Vec::with_capacity(raw_responses.len());
    for (mm_id, payload) in raw_responses {
        if let Err(e) = state.record_fills(mm_id).await {
            warn!(mm = %mm_id, error = %e, "fill reconcile failed; reputation may lag");
        }
        let account = match state.indexer.account(mm_id).await {
            Ok(Some(a)) => a,
            Ok(None) => {
                debug!(mm = %mm_id, "rfq quote from signer the indexer doesn't know — rejecting");
                metrics::counter!("quoting_quotes_received_total", "outcome" => "invalid")
                    .increment(1);
                continue;
            }
            Err(e) => {
                warn!(mm = %mm_id, error = %e, "indexer signer lookup failed; rejecting quote");
                metrics::counter!("quoting_quotes_received_total", "outcome" => "invalid")
                    .increment(1);
                continue;
            }
        };
        let validate_span = tracing::info_span!("validate_quote", mm = %mm_id);
        match validate_span.in_scope(|| {
            validate_quote(
                &state,
                &bucket,
                &account,
                write_amount,
                &payload,
                mm_id,
                &protocol_id,
                now_ms,
            )
        }) {
            Ok(entry) => {
                metrics::counter!("quoting_quotes_received_total", "outcome" => "valid")
                    .increment(1);
                accepted.push(entry);
            }
            Err(rej) => {
                debug!(mm = %mm_id, rejection = ?rej, "rfq quote rejected");
                metrics::counter!("quoting_quotes_received_total", "outcome" => "invalid")
                    .increment(1);
            }
        }
    }
    sort_best_first(side, &mut accepted);
    info!(
        request_id = %request_id,
        accepted = accepted.len(),
        ?side,
        %bucket_id,
        "rfq orchestration complete"
    );
    accepted
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;

    use protocol_types::asset::AssetType;
    use protocol_types::ids::SuiAddress;
    use protocol_types::messages::MmQuotePayload;
    use protocol_types::quote::Quote;

    /// A quoting state with no live indexer — the validation path is pure, so
    /// tests pass the bucket + account in directly. The URL is unreachable on
    /// purpose: any test that actually hit it would be a bug.
    fn test_state() -> AppState {
        AppState::with_global_rfq_cap(256, "http://127.0.0.1:1/graphql".into())
    }

    fn mk_account(mm: ObjectId, signing_pubkey: Vec<u8>) -> Account {
        Account {
            account_id: mm,
            owner: Some(SuiAddress::ZERO),
            signing_scheme: Some(protocol_types::SigningScheme::Ed25519),
            signing_pubkey,
        }
    }

    fn mk_bucket() -> Bucket {
        Bucket {
            bucket_id: ObjectId::new([0x99; 32]),
            asset_type: AssetType::new("BTC"),
            settlement_type: AssetType::new("USDC"),
            call_type: AssetType::new("0x9::call_0::CALL_0"),
            strike: 50,
            strike_scale: 0,
            expiry_ms: 1_000_000,
            total_written: 0,
            exercise_cursor: 0,
            cleaned: false,
            invalidated: false,
            deepbook_pool_id: None,
            option_kind: "call".into(),
        }
    }

    fn quote_with_routing(
        mm_signer: ObjectId,
        protocol_id: Vec<u8>,
        bucket: ObjectId,
        write_amount: u64,
        premium: u64,
        nonce: u64,
    ) -> Quote {
        Quote {
            protocol_id,
            signer_id: mm_signer,
            collateral_source: ObjectId::new([0xc0; 32]),
            release_package: SuiAddress::new([0xd0; 32]),
            release_module: "mm_collateral".into(),
            signer_token_recipient: SuiAddress::ZERO,
            bucket_id: bucket,
            write_amount,
            premium,
            valid_until_ms: 999_999,
            nonce,
        }
    }

    fn signed_quote(
        sk: &SigningKey,
        protocol_id: Vec<u8>,
        mm_signer: ObjectId,
        bucket: ObjectId,
        write_amount: u64,
        premium: u64,
        nonce: u64,
    ) -> MmQuotePayload {
        let q = quote_with_routing(mm_signer, protocol_id, bucket, write_amount, premium, nonce);
        let sig = sk.sign(&q.to_bcs_bytes().unwrap()).to_bytes().to_vec();
        MmQuotePayload { quote: q, signature: sig }
    }

    #[test]
    fn happy_path_records_the_nonce() {
        let sk = SigningKey::generate(&mut OsRng);
        let mm = ObjectId::new([0x01; 32]);
        let state = test_state();
        let account = mk_account(mm, sk.verifying_key().to_bytes().to_vec());
        let bucket = mk_bucket();
        let p = signed_quote(&sk, b"P".to_vec(), mm, bucket.bucket_id, 100, 500, 1);

        let entry = validate_quote(&state, &bucket, &account, 100, &p, mm, b"P", 0).unwrap();
        assert_eq!(entry.mm_id, mm);
        assert_eq!(entry.quote.premium, 500);
        assert_eq!(state.nonces.len(), 1);
    }

    #[test]
    fn rejects_duplicate_nonce() {
        let sk = SigningKey::generate(&mut OsRng);
        let mm = ObjectId::new([0x01; 32]);
        let state = test_state();
        let account = mk_account(mm, sk.verifying_key().to_bytes().to_vec());
        let bucket = mk_bucket();
        let p1 = signed_quote(&sk, b"P".to_vec(), mm, bucket.bucket_id, 100, 500, 7);
        let p2 = signed_quote(&sk, b"P".to_vec(), mm, bucket.bucket_id, 100, 600, 7);
        validate_quote(&state, &bucket, &account, 100, &p1, mm, b"P", 0).unwrap();
        assert_eq!(
            validate_quote(&state, &bucket, &account, 100, &p2, mm, b"P", 0).unwrap_err(),
            QuoteRejection::DuplicateNonce,
        );
    }

    #[test]
    fn rejects_expired_quote() {
        let sk = SigningKey::generate(&mut OsRng);
        let mm = ObjectId::new([0x01; 32]);
        let state = test_state();
        let account = mk_account(mm, sk.verifying_key().to_bytes().to_vec());
        let bucket = mk_bucket();
        let p = signed_quote(&sk, b"P".to_vec(), mm, bucket.bucket_id, 100, 500, 1);
        let rej =
            validate_quote(&state, &bucket, &account, 100, &p, mm, b"P", 1_000_000).unwrap_err();
        assert_eq!(rej, QuoteRejection::Expired);
    }

    #[test]
    fn rejects_tampered_signature() {
        let sk = SigningKey::generate(&mut OsRng);
        let mm = ObjectId::new([0x01; 32]);
        let state = test_state();
        let account = mk_account(mm, sk.verifying_key().to_bytes().to_vec());
        let bucket = mk_bucket();
        let mut p = signed_quote(&sk, b"P".to_vec(), mm, bucket.bucket_id, 100, 500, 1);
        p.quote.premium = 9_999; // tamper after signing
        assert_eq!(
            validate_quote(&state, &bucket, &account, 100, &p, mm, b"P", 0).unwrap_err(),
            QuoteRejection::SignatureInvalid,
        );
    }

    #[test]
    fn rejects_tampered_routing() {
        // Swapping the release routing after signing must fail signature
        // verification — the routing is inside the signed payload.
        let sk = SigningKey::generate(&mut OsRng);
        let mm = ObjectId::new([0x01; 32]);
        let state = test_state();
        let account = mk_account(mm, sk.verifying_key().to_bytes().to_vec());
        let bucket = mk_bucket();
        let mut p = signed_quote(&sk, b"P".to_vec(), mm, bucket.bucket_id, 100, 500, 1);
        p.quote.collateral_source = ObjectId::new([0xee; 32]);
        assert_eq!(
            validate_quote(&state, &bucket, &account, 100, &p, mm, b"P", 0).unwrap_err(),
            QuoteRejection::SignatureInvalid,
        );
    }

    #[test]
    fn rejects_missing_routing() {
        let sk = SigningKey::generate(&mut OsRng);
        let mm = ObjectId::new([0x01; 32]);
        let state = test_state();
        let account = mk_account(mm, sk.verifying_key().to_bytes().to_vec());
        let bucket = mk_bucket();
        // Sign a quote whose routing fields are zero/empty — structurally
        // valid JSON, but it could never execute.
        let mut q =
            quote_with_routing(mm, b"P".to_vec(), bucket.bucket_id, 100, 500, 1);
        q.collateral_source = ObjectId::ZERO;
        let sig = sk.sign(&q.to_bcs_bytes().unwrap()).to_bytes().to_vec();
        let p = MmQuotePayload { quote: q, signature: sig };
        assert_eq!(
            validate_quote(&state, &bucket, &account, 100, &p, mm, b"P", 0).unwrap_err(),
            QuoteRejection::MissingRouting,
        );
    }

    #[test]
    fn rejects_bucket_mismatch() {
        let sk = SigningKey::generate(&mut OsRng);
        let mm = ObjectId::new([0x01; 32]);
        let state = test_state();
        let account = mk_account(mm, sk.verifying_key().to_bytes().to_vec());
        let bucket = mk_bucket();
        let p = signed_quote(&sk, b"P".to_vec(), mm, ObjectId::new([0xaa; 32]), 100, 500, 1);
        assert_eq!(
            validate_quote(&state, &bucket, &account, 100, &p, mm, b"P", 0).unwrap_err(),
            QuoteRejection::BucketMismatch,
        );
    }

    #[test]
    fn rejects_quote_against_invalidated_bucket() {
        // Defense-in-depth: even if retail.rs lets an RFQ through (race
        // with the BucketInvalidated event), the per-quote check must
        // refuse. See SO-69.
        let sk = SigningKey::generate(&mut OsRng);
        let mm = ObjectId::new([0x01; 32]);
        let state = test_state();
        let account = mk_account(mm, sk.verifying_key().to_bytes().to_vec());
        let mut bucket = mk_bucket();
        bucket.invalidated = true;
        let p = signed_quote(&sk, b"P".to_vec(), mm, bucket.bucket_id, 100, 500, 1);
        assert_eq!(
            validate_quote(&state, &bucket, &account, 100, &p, mm, b"P", 0).unwrap_err(),
            QuoteRejection::BucketInvalidated,
        );
        assert_eq!(state.nonces.len(), 0);
    }

    #[test]
    fn writer_side_sorts_highest_premium_first() {
        let mk = |premium: u64, rep: f64| RfqQuoteEntry {
            quote: quote_with_routing(ObjectId::ZERO, vec![], ObjectId::ZERO, 100, premium, 0),
            signature: vec![],
            mm_id: ObjectId::ZERO,
            mm_reputation: rep,
        };
        let mut v = vec![mk(500, 0.5), mk(600, 0.1), mk(600, 0.9), mk(450, 1.0)];
        sort_best_first(Side::Writer, &mut v);
        assert_eq!(v[0].quote.premium, 600);
        assert!((v[0].mm_reputation - 0.9).abs() < 1e-9);
        assert_eq!(v[1].quote.premium, 600);
        assert_eq!(v[2].quote.premium, 500);
        assert_eq!(v[3].quote.premium, 450);
    }

    #[test]
    fn trader_side_sorts_lowest_premium_first() {
        let mk = |premium: u64| RfqQuoteEntry {
            quote: quote_with_routing(ObjectId::ZERO, vec![], ObjectId::ZERO, 100, premium, 0),
            signature: vec![],
            mm_id: ObjectId::ZERO,
            mm_reputation: 0.0,
        };
        let mut v = vec![mk(500), mk(300), mk(700), mk(100)];
        sort_best_first(Side::Trader, &mut v);
        assert_eq!(v.iter().map(|e| e.quote.premium).collect::<Vec<_>>(), vec![100, 300, 500, 700]);
    }

    /// Regression: a slow/stalled MM must not back-pressure the orchestrator.
    /// Before the fix, `orchestrate` awaited `mm.tx.send(frame).await` with
    /// no timeout — a stuck MM (channel full) blocked the orchestrator for
    /// the full RFQ window per request, and a concurrent burst piled up
    /// indefinitely. With `try_send`, the orchestrator drops the broadcast
    /// to that MM and returns within the RFQ window.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn orchestrate_does_not_block_on_full_mm_channel() {
        use crate::state::MmConnection;
        use protocol_types::sides::MmRole;
        use std::sync::Arc;
        use tokio::sync::mpsc;

        let mm = ObjectId::new([0x01; 32]);
        let bucket = mk_bucket();
        let state = Arc::new(test_state());

        // Register an MM whose outbound channel is full and never drained.
        let (tx, rx) = mpsc::channel(1);
        // Fill the channel so subsequent `try_send` calls fail with Full.
        tx.try_send(protocol_types::messages::ServiceToMm::Ping).unwrap();
        // Keep rx alive but don't drain it — emulates a stuck write loop.
        let _rx_keepalive = rx;
        state.mms.insert(MmConnection {
            account_id: mm,
            roles: Arc::new(parking_lot::RwLock::new(vec![MmRole::TraderMm])),
            bulk_view: false,
            tx,
        });

        // Fire 64 concurrent orchestrations. Each should complete inside
        // the 50ms window — total wall time must stay near the window,
        // not multiply by the number of concurrent calls.
        let start = std::time::Instant::now();
        let mut handles = Vec::new();
        for i in 0..64 {
            let st = Arc::clone(&state);
            let bkt = bucket.clone();
            handles.push(tokio::spawn(async move {
                orchestrate(
                    st,
                    Side::Writer,
                    bkt,
                    100,
                    format!("req-{i}"),
                    std::time::Duration::from_millis(50),
                    b"P".to_vec(),
                    0,
                )
                .await
            }));
        }
        for h in handles {
            let quotes = h.await.unwrap();
            assert!(quotes.is_empty(), "no quotes expected from a stuck MM");
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "orchestrate took {elapsed:?} — likely blocking on full MM channel"
        );
    }
}
