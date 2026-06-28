//! RFQ orchestration (§5.8).
//!
//! One retail `RFQRequest` →
//!   1. broadcast `RFQBroadcast` to every connected MM on the opposite side;
//!   2. collect `Quote`s until the deadline elapses (or every MM declines);
//!   3. validate each quote (signature, expiry, bucket match, write_amount,
//!      reservation feasibility against the signer's available balance);
//!   4. for valid quotes, atomically reserve the signer's balance;
//!   5. sort best-price-first for the retail side;
//!   6. return the eligible quotes — caller forwards as `RFQResponse`.
//!
//! The validation/sort/reservation step is pure and testable without any WS.
//! See [`validate_and_reserve`] / [`sort_best_first`].

pub mod bulk_view;
mod matcher;

pub use matcher::{collect_with_deadline, MatcherInput, MatcherOutput};

use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info, trace, warn};

use protocol_types::asset::AssetType;
use protocol_types::errors::ProtocolError;
use protocol_types::ids::ObjectId;
use protocol_types::messages::{MmQuotePayload, RfqQuoteEntry};
use protocol_types::quote::Quote;
use protocol_types::sides::Side;

use indexer_graphql::{Account, Bucket};

use crate::state::{AppState, InsertOutcome, Reservation, ReservationTable};

/// What's wrong with a quote — surfaced for logging / reputation but never
/// propagated to retail (the retail-facing list just shows what was good).
#[derive(Debug, PartialEq, Eq)]
pub enum QuoteRejection {
    ProtocolMismatch,
    BucketMismatch,
    WriteAmountMismatch,
    Expired,
    SignatureInvalid,
    InsufficientAvailableBalance,
    DuplicateNonce,
    UnknownSigner,
    UnknownBucket,
    InvalidPubkey,
    /// Admin has invalidated the bucket; a quote against it would revert
    /// on chain. Defense-in-depth against a race between the MM signing
    /// and the BucketInvalidated event landing.
    BucketInvalidated,
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

/// The reservation amount and asset for one quote, given the bucket and the
/// retail-side direction. See §3.3.4:
///
/// - Writer flow (retail writes): signer is Trader MM, signer provides the
///   premium in the bucket's Settlement asset. Same for calls and puts.
/// - Trader flow (retail trades): signer is Writer MM (the put writer for a
///   put bucket). For a **call** bucket they escrow `write_amount` of the
///   Underlying asset; for a **cash-secured put** they instead post CASH
///   collateral in the Settlement asset, `ceil(write_amount × strike /
///   10^strike_scale)`.
pub fn reservation_for(side: Side, bucket: &Bucket, quote: &Quote) -> (AssetType, u64) {
    match side {
        Side::Writer => (bucket.settlement_type.clone(), quote.premium),
        Side::Trader if bucket.option_kind == "put" => {
            (bucket.settlement_type.clone(), put_collateral(bucket, quote.write_amount))
        }
        Side::Trader => (bucket.asset_type.clone(), quote.write_amount),
    }
}

/// Cash collateral a put writer must post for `write_amount` of underlying:
/// `ceil(write_amount × strike / 10^strike_scale)`, computed in u128 and
/// downcast to u64 (saturating — an overflow here means the bucket params are
/// unrealistic and the reservation check would reject anyway).
fn put_collateral(bucket: &Bucket, write_amount: u64) -> u64 {
    let scale = 10u128.pow(bucket.strike_scale as u32);
    let numerator = (write_amount as u128).saturating_mul(bucket.strike);
    let collateral = numerator.div_ceil(scale);
    u64::try_from(collateral).unwrap_or(u64::MAX)
}

/// Run a single quote through every check. On success, places a reservation
/// and returns the entry that would go into `RFQResponse`. On failure,
/// returns the rejection reason and reserves nothing.
///
/// `bucket` and `account` are fetched just-in-time by the caller
/// ([`orchestrate`]) so this stays a pure, synchronous function: no indexer
/// round-trips happen here. The caller is also responsible for calling
/// [`AppState::reconcile_executed`] before this so `available` doesn't
/// double-count a reservation whose write already landed.
pub fn validate_and_reserve(
    state: &AppState,
    side: Side,
    bucket: &Bucket,
    account: &Account,
    write_amount: u64,
    payload: &MmQuotePayload,
    mm_account_id: ObjectId,
    protocol_id: &[u8],
    now_ms: u64,
) -> Result<RfqQuoteEntry, QuoteRejection> {
    let quote = &payload.quote;
    trace!(mm = %mm_account_id, bucket = %bucket.bucket_id, nonce = quote.nonce, premium = quote.premium, "validating quote");
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
    if quote.signer_account_id != mm_account_id {
        return Err(QuoteRejection::SignatureInvalid);
    }

    // Signature against the MM's registered pubkey + scheme.
    let scheme = account.signing_scheme.ok_or(QuoteRejection::InvalidPubkey)?;
    let signed = protocol_types::quote::SignedQuote {
        quote: quote.clone(),
        signature: payload.signature.clone(),
    };
    signed
        .verify(scheme, &account.signing_pubkey, protocol_id, mm_account_id, now_ms)
        .map_err(QuoteRejection::from)?;

    // Reservation feasibility.
    if bucket.invalidated {
        return Err(QuoteRejection::BucketInvalidated);
    }
    let (asset, amount) = reservation_for(side, bucket, quote);
    if state.available(account, &asset) < amount {
        return Err(QuoteRejection::InsufficientAvailableBalance);
    }

    // Reserve. Nonce uniqueness is enforced here too — duplicate nonce
    // means the MM signed two quotes with the same nonce, which would
    // revert on chain; reject.
    let outcome = state.reservations.insert(Reservation {
        account_id: mm_account_id,
        nonce: quote.nonce,
        asset_type: asset.clone(),
        amount,
        valid_until_ms: quote.valid_until_ms,
        created_at_ms: now_ms,
    });
    if outcome == InsertOutcome::DuplicateKey {
        return Err(QuoteRejection::DuplicateNonce);
    }

    // Reputation: count the signature.
    state.reputation.record_signed(mm_account_id);

    let rep = state.reputation.snapshot(&mm_account_id).composite_score();
    Ok(RfqQuoteEntry {
        quote: quote.clone(),
        signature: payload.signature.clone(),
        mm_id: mm_account_id,
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

/// Release every reservation we just made for `request_id`. Used when a
/// retail user lets the response time-out, or on shutdown. Reservations
/// keyed by (account, nonce); we walk the supplied entries and drop each.
pub fn release_reservations(reservations: &ReservationTable, entries: &[RfqQuoteEntry]) {
    for e in entries {
        reservations.release(e.mm_id, e.quote.nonce);
    }
}

/// Drive an RFQ from the retail request through to the validated, sorted
/// list of `RfqQuoteEntry`s. Spawns the broadcast and the deadline-bounded
/// collection — pure plumbing; the validation lives in
/// [`validate_and_reserve`].
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

    // Validate + reserve. Each MM's account (signing key + balances) is
    // fetched JIT, after reconciling any of its reservations whose write has
    // already landed so `available` isn't understated.
    let mut accepted = Vec::with_capacity(raw_responses.len());
    for (mm_id, payload) in raw_responses {
        if let Err(e) = state.reconcile_executed(mm_id).await {
            warn!(mm = %mm_id, error = %e, "reservation reconcile failed; proceeding with stale reservations");
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
                warn!(mm = %mm_id, error = %e, "indexer account lookup failed; rejecting quote");
                metrics::counter!("quoting_quotes_received_total", "outcome" => "invalid")
                    .increment(1);
                continue;
            }
        };
        let validate_span = tracing::info_span!("validate_quote", mm = %mm_id);
        match validate_span.in_scope(|| {
            validate_and_reserve(
                &state,
                side,
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

    use std::collections::BTreeMap;

    use protocol_types::asset::AssetType;
    use protocol_types::ids::SuiAddress;
    use protocol_types::messages::MmQuotePayload;

    /// A quoting state with no live indexer — the validation path is pure, so
    /// tests pass the bucket + account in directly. The URL is unreachable on
    /// purpose: any test that actually hit it would be a bug.
    fn test_state() -> AppState {
        AppState::with_global_rfq_cap(256, "http://127.0.0.1:1/graphql".into())
    }

    fn mk_account(mm: ObjectId, signing_pubkey: Vec<u8>, balance: u64) -> Account {
        Account {
            account_id: mm,
            owner: Some(SuiAddress::ZERO),
            signing_scheme: Some(protocol_types::SigningScheme::Ed25519),
            signing_pubkey,
            balances: BTreeMap::from([(AssetType::new("USDC"), balance)]),
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
            option_kind: "call".into(),
        }
    }

    fn signed_quote(
        sk: &SigningKey,
        protocol_id: Vec<u8>,
        mm_account: ObjectId,
        bucket: ObjectId,
        write_amount: u64,
        premium: u64,
        nonce: u64,
    ) -> MmQuotePayload {
        let q = Quote {
            protocol_id,
            signer_account_id: mm_account,
            signer_token_recipient: SuiAddress::ZERO,
            bucket_id: bucket,
            write_amount,
            premium,
            valid_until_ms: 999_999,
            nonce,
        };
        let sig = sk.sign(&q.to_bcs_bytes().unwrap()).to_bytes().to_vec();
        MmQuotePayload { quote: q, signature: sig }
    }

    #[test]
    fn happy_path_writes_a_reservation() {
        let sk = SigningKey::generate(&mut OsRng);
        let mm = ObjectId::new([0x01; 32]);
        let state = test_state();
        let account = mk_account(mm, sk.verifying_key().to_bytes().to_vec(), 10_000);
        let bucket = mk_bucket();
        let p = signed_quote(&sk, b"P".to_vec(), mm, bucket.bucket_id, 100, 500, 1);

        let entry =
            validate_and_reserve(&state, Side::Writer, &bucket, &account, 100, &p, mm, b"P", 0)
                .unwrap();
        assert_eq!(entry.mm_id, mm);
        assert_eq!(entry.quote.premium, 500);
        // 10000 USDC balance, 500 reserved → 9500 available.
        assert_eq!(state.available(&account, &AssetType::new("USDC")), 9500);
    }

    #[test]
    fn rejects_insufficient_balance() {
        let sk = SigningKey::generate(&mut OsRng);
        let mm = ObjectId::new([0x01; 32]);
        let state = test_state();
        let account = mk_account(mm, sk.verifying_key().to_bytes().to_vec(), 100);
        let bucket = mk_bucket();
        let p = signed_quote(&sk, b"P".to_vec(), mm, bucket.bucket_id, 100, 500, 1);
        assert_eq!(
            validate_and_reserve(&state, Side::Writer, &bucket, &account, 100, &p, mm, b"P", 0)
                .unwrap_err(),
            QuoteRejection::InsufficientAvailableBalance,
        );
        assert_eq!(state.reservations.len(), 0);
    }

    #[test]
    fn rejects_duplicate_nonce() {
        let sk = SigningKey::generate(&mut OsRng);
        let mm = ObjectId::new([0x01; 32]);
        let state = test_state();
        let account = mk_account(mm, sk.verifying_key().to_bytes().to_vec(), 10_000);
        let bucket = mk_bucket();
        let p1 = signed_quote(&sk, b"P".to_vec(), mm, bucket.bucket_id, 100, 500, 7);
        let p2 = signed_quote(&sk, b"P".to_vec(), mm, bucket.bucket_id, 100, 600, 7);
        validate_and_reserve(&state, Side::Writer, &bucket, &account, 100, &p1, mm, b"P", 0)
            .unwrap();
        assert_eq!(
            validate_and_reserve(&state, Side::Writer, &bucket, &account, 100, &p2, mm, b"P", 0)
                .unwrap_err(),
            QuoteRejection::DuplicateNonce,
        );
    }

    #[test]
    fn rejects_expired_quote() {
        let sk = SigningKey::generate(&mut OsRng);
        let mm = ObjectId::new([0x01; 32]);
        let state = test_state();
        let account = mk_account(mm, sk.verifying_key().to_bytes().to_vec(), 10_000);
        let bucket = mk_bucket();
        let p = signed_quote(&sk, b"P".to_vec(), mm, bucket.bucket_id, 100, 500, 1);
        let rej =
            validate_and_reserve(&state, Side::Writer, &bucket, &account, 100, &p, mm, b"P", 1_000_000)
                .unwrap_err();
        assert_eq!(rej, QuoteRejection::Expired);
    }

    #[test]
    fn rejects_tampered_signature() {
        let sk = SigningKey::generate(&mut OsRng);
        let mm = ObjectId::new([0x01; 32]);
        let state = test_state();
        let account = mk_account(mm, sk.verifying_key().to_bytes().to_vec(), 10_000);
        let bucket = mk_bucket();
        let mut p = signed_quote(&sk, b"P".to_vec(), mm, bucket.bucket_id, 100, 500, 1);
        p.quote.premium = 9_999; // tamper after signing
        assert_eq!(
            validate_and_reserve(&state, Side::Writer, &bucket, &account, 100, &p, mm, b"P", 0)
                .unwrap_err(),
            QuoteRejection::SignatureInvalid,
        );
    }

    #[test]
    fn rejects_bucket_mismatch() {
        let sk = SigningKey::generate(&mut OsRng);
        let mm = ObjectId::new([0x01; 32]);
        let state = test_state();
        let account = mk_account(mm, sk.verifying_key().to_bytes().to_vec(), 10_000);
        let bucket = mk_bucket();
        let p = signed_quote(&sk, b"P".to_vec(), mm, ObjectId::new([0xaa; 32]), 100, 500, 1);
        assert_eq!(
            validate_and_reserve(&state, Side::Writer, &bucket, &account, 100, &p, mm, b"P", 0)
                .unwrap_err(),
            QuoteRejection::BucketMismatch,
        );
    }

    #[test]
    fn rejects_quote_against_invalidated_bucket() {
        // Defense-in-depth: even if retail.rs lets an RFQ through (race
        // with the BucketInvalidated event), the per-quote check must
        // refuse to reserve. See SO-69.
        let sk = SigningKey::generate(&mut OsRng);
        let mm = ObjectId::new([0x01; 32]);
        let state = test_state();
        let account = mk_account(mm, sk.verifying_key().to_bytes().to_vec(), 10_000);
        let mut bucket = mk_bucket();
        bucket.invalidated = true;
        let p = signed_quote(&sk, b"P".to_vec(), mm, bucket.bucket_id, 100, 500, 1);
        assert_eq!(
            validate_and_reserve(&state, Side::Writer, &bucket, &account, 100, &p, mm, b"P", 0)
                .unwrap_err(),
            QuoteRejection::BucketInvalidated,
        );
        assert_eq!(state.reservations.len(), 0);
    }

    #[test]
    fn put_trader_reserves_ceil_cash_collateral() {
        // write_amount=100, strike=50, scale=0 → 100*50 = 5000 settlement.
        let mut bucket = mk_bucket();
        bucket.option_kind = "put".into();
        let q = Quote {
            protocol_id: vec![],
            signer_account_id: ObjectId::ZERO,
            signer_token_recipient: SuiAddress::ZERO,
            bucket_id: bucket.bucket_id,
            write_amount: 100,
            premium: 7,
            valid_until_ms: 0,
            nonce: 0,
        };
        let (asset, amount) = reservation_for(Side::Trader, &bucket, &q);
        assert_eq!(asset, bucket.settlement_type);
        assert_eq!(amount, 5000);

        // ceil: strike=3, scale=1 (0.3), write_amount=10 → 30/10 = 3 exactly;
        // write_amount=11 → 33/10 → ceil 4.
        bucket.strike = 3;
        bucket.strike_scale = 1;
        let mut q2 = q.clone();
        q2.write_amount = 11;
        let (_, amt) = reservation_for(Side::Trader, &bucket, &q2);
        assert_eq!(amt, 4);

        // The Writer branch (premium in settlement) is unchanged for puts.
        let (wasset, wamt) = reservation_for(Side::Writer, &bucket, &q);
        assert_eq!(wasset, bucket.settlement_type);
        assert_eq!(wamt, 7);
    }

    #[test]
    fn call_trader_still_reserves_underlying() {
        let bucket = mk_bucket(); // option_kind == "call"
        let q = Quote {
            protocol_id: vec![],
            signer_account_id: ObjectId::ZERO,
            signer_token_recipient: SuiAddress::ZERO,
            bucket_id: bucket.bucket_id,
            write_amount: 100,
            premium: 7,
            valid_until_ms: 0,
            nonce: 0,
        };
        let (asset, amount) = reservation_for(Side::Trader, &bucket, &q);
        assert_eq!(asset, bucket.asset_type);
        assert_eq!(amount, 100);
    }

    #[test]
    fn writer_side_sorts_highest_premium_first() {
        let mk = |premium: u64, rep: f64| RfqQuoteEntry {
            quote: Quote {
                protocol_id: vec![],
                signer_account_id: ObjectId::ZERO,
                signer_token_recipient: SuiAddress::ZERO,
                bucket_id: ObjectId::ZERO,
                write_amount: 100,
                premium,
                valid_until_ms: 0,
                nonce: 0,
            },
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
            quote: Quote {
                protocol_id: vec![],
                signer_account_id: ObjectId::ZERO,
                signer_token_recipient: SuiAddress::ZERO,
                bucket_id: ObjectId::ZERO,
                write_amount: 100,
                premium,
                valid_until_ms: 0,
                nonce: 0,
            },
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
