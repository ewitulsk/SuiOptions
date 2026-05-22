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

mod matcher;

pub use matcher::{collect_with_deadline, MatcherInput, MatcherOutput};

use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info, trace, warn};

use shared::protocol_types::asset::AssetType;
use shared::protocol_types::errors::ProtocolError;
use shared::protocol_types::ids::ObjectId;
use shared::protocol_types::messages::{MmQuotePayload, RfqQuoteEntry};
use shared::protocol_types::quote::Quote;
use shared::protocol_types::sides::Side;

use crate::state::{
    AppState, BucketView, InsertOutcome, Reservation, ReservationTable,
};

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
///   premium in the bucket's Settlement asset.
/// - Trader flow (retail trades): signer is Writer MM, signer provides
///   `write_amount` of the bucket's Underlying asset.
pub fn reservation_for(side: Side, bucket: &BucketView, quote: &Quote) -> (AssetType, u64) {
    match side {
        Side::Writer => (bucket.settlement_type.clone(), quote.premium),
        Side::Trader => (bucket.asset_type.clone(), quote.write_amount),
    }
}

/// Run a single quote through every check. On success, places a reservation
/// and returns the entry that would go into `RFQResponse`. On failure,
/// returns the rejection reason and reserves nothing.
pub fn validate_and_reserve(
    state: &AppState,
    side: Side,
    bucket_id: ObjectId,
    write_amount: u64,
    payload: &MmQuotePayload,
    mm_account_id: ObjectId,
    protocol_id: &[u8],
    now_ms: u64,
) -> Result<RfqQuoteEntry, QuoteRejection> {
    let quote = &payload.quote;
    trace!(mm = %mm_account_id, %bucket_id, nonce = quote.nonce, premium = quote.premium, "validating quote");
    // Cheap structural checks first.
    if quote.protocol_id != protocol_id {
        return Err(QuoteRejection::ProtocolMismatch);
    }
    if quote.bucket_id != bucket_id {
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
    let acct = state
        .accounts
        .snapshot(&mm_account_id)
        .ok_or(QuoteRejection::UnknownSigner)?;
    let scheme = acct.signing_scheme.ok_or(QuoteRejection::InvalidPubkey)?;
    let signed = shared::protocol_types::quote::SignedQuote {
        quote: quote.clone(),
        signature: payload.signature.clone(),
    };
    signed
        .verify(scheme, &acct.signing_pubkey, protocol_id, mm_account_id, now_ms)
        .map_err(QuoteRejection::from)?;

    // Reservation feasibility.
    let bucket = state
        .buckets
        .get(&bucket_id)
        .ok_or(QuoteRejection::UnknownBucket)?;
    let (asset, amount) = reservation_for(side, &bucket, quote);
    if state.available(mm_account_id, &asset) < amount {
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
    bucket_id: ObjectId,
    write_amount: u64,
    request_id: String,
    rfq_window: Duration,
    protocol_id: Vec<u8>,
    now_ms: u64,
) -> Vec<RfqQuoteEntry> {
    let deadline_ms = now_ms.saturating_add(rfq_window.as_millis() as u64);

    // Resolve the bucket up-front so the broadcast can include the
    // bucket's strike + expiry. MMs price against these instead of
    // guessing — every quote that comes back already knows what option
    // it's quoting.
    let bucket = match state.buckets.get(&bucket_id) {
        Some(b) => b,
        None => {
            debug!(%bucket_id, "rfq for unknown bucket — returning empty");
            return Vec::new();
        }
    };

    let mm_role = side.counterparty_mm();
    let mms = state.mms.all_for_role(mm_role);
    debug!(?side, mms = mms.len(), request_id = %request_id, "rfq broadcast");

    let (mut input, output) = matcher::channel(mms.len().max(1));
    // Publish the matcher's input side under the request_id so the MM read
    // tasks can route their responses to it.
    state
        .pending_rfqs
        .insert(request_id.clone(), input.tx.clone());

    // Hand the receiver off to the matcher.
    let collector = tokio::spawn(collect_with_deadline(output, rfq_window));

    // Broadcast to MMs.
    for mm in mms {
        let frame = shared::protocol_types::messages::ServiceToMm::RFQBroadcast {
            request_id: request_id.clone(),
            payload: shared::protocol_types::messages::RfqBroadcastPayload {
                bucket_id,
                write_amount,
                side,
                deadline_ms,
                strike: bucket.strike,
                expiry_ms: bucket.expiry_ms,
            },
        };
        // Tell the matcher who we expect a response from so it can decide
        // to short-circuit when every MM has answered.
        input.expect(mm.account_id);
        let _ = mm.tx.send(frame).await;
    }
    // Drop the sender side(s) we hold so the matcher's close-detection works
    // once every MM that's going to answer has done so.
    drop(input);

    let raw_responses = collector.await.unwrap_or(MatcherOutput::default()).responses;
    debug!(request_id = %request_id, responses = raw_responses.len(), "rfq collection complete");
    // Once the deadline closes the receiver, remove the routing entry.
    state.pending_rfqs.remove(&request_id);

    // Validate + reserve.
    let mut accepted = Vec::with_capacity(raw_responses.len());
    for (mm_id, payload) in raw_responses {
        match validate_and_reserve(
            &state,
            side,
            bucket_id,
            write_amount,
            &payload,
            mm_id,
            &protocol_id,
            now_ms,
        ) {
            Ok(entry) => accepted.push(entry),
            Err(rej) => {
                debug!(mm = %mm_id, rejection = ?rej, "rfq quote rejected");
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

    use shared::protocol_types::asset::AssetType;
    use shared::protocol_types::events::{
        AccountCreated, AccountDeposit, BucketCreated, ChainEvent, IndexedEvent,
    };
    use shared::protocol_types::ids::SuiAddress;
    use shared::protocol_types::messages::MmQuotePayload;

    fn make_state(mm_account: ObjectId, signing_pubkey: Vec<u8>, balance: u64) -> AppState {
        let s = AppState::new();
        // Seed account + bucket via events.
        let bucket = ObjectId::new([0x99; 32]);
        s.ingest_event(&IndexedEvent {
            sequence: 0,
            timestamp_ms: 0,
            event: ChainEvent::AccountCreated(AccountCreated {
                account_id: mm_account,
                owner: SuiAddress::ZERO,
                signing_scheme: shared::protocol_types::SigningScheme::Ed25519,
                signing_pubkey,
            }),
        });
        s.ingest_event(&IndexedEvent {
            sequence: 1,
            timestamp_ms: 0,
            event: ChainEvent::AccountDeposit(AccountDeposit {
                account_id: mm_account,
                asset_type: AssetType::new("USDC"),
                amount: balance,
            }),
        });
        s.ingest_event(&IndexedEvent {
            sequence: 2,
            timestamp_ms: 0,
            event: ChainEvent::BucketCreated(BucketCreated {
                bucket_id: bucket,
                asset_type: AssetType::new("BTC"),
                settlement_type: AssetType::new("USDC"),
                expiry_ms: 1_000_000,
                strike: 50,
            }),
        });
        s
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
        let state = make_state(mm, sk.verifying_key().to_bytes().to_vec(), 10_000);
        let bucket = ObjectId::new([0x99; 32]);
        let p = signed_quote(&sk, b"P".to_vec(), mm, bucket, 100, 500, 1);

        let entry = validate_and_reserve(&state, Side::Writer, bucket, 100, &p, mm, b"P", 0).unwrap();
        assert_eq!(entry.mm_id, mm);
        assert_eq!(entry.quote.premium, 500);
        // 10000 USDC balance, 500 reserved → 9500 available.
        assert_eq!(state.available(mm, &AssetType::new("USDC")), 9500);
    }

    #[test]
    fn rejects_insufficient_balance() {
        let sk = SigningKey::generate(&mut OsRng);
        let mm = ObjectId::new([0x01; 32]);
        let state = make_state(mm, sk.verifying_key().to_bytes().to_vec(), 100);
        let bucket = ObjectId::new([0x99; 32]);
        let p = signed_quote(&sk, b"P".to_vec(), mm, bucket, 100, 500, 1);
        assert_eq!(
            validate_and_reserve(&state, Side::Writer, bucket, 100, &p, mm, b"P", 0).unwrap_err(),
            QuoteRejection::InsufficientAvailableBalance,
        );
        assert_eq!(state.reservations.len(), 0);
    }

    #[test]
    fn rejects_duplicate_nonce() {
        let sk = SigningKey::generate(&mut OsRng);
        let mm = ObjectId::new([0x01; 32]);
        let state = make_state(mm, sk.verifying_key().to_bytes().to_vec(), 10_000);
        let bucket = ObjectId::new([0x99; 32]);
        let p1 = signed_quote(&sk, b"P".to_vec(), mm, bucket, 100, 500, 7);
        let p2 = signed_quote(&sk, b"P".to_vec(), mm, bucket, 100, 600, 7);
        validate_and_reserve(&state, Side::Writer, bucket, 100, &p1, mm, b"P", 0).unwrap();
        assert_eq!(
            validate_and_reserve(&state, Side::Writer, bucket, 100, &p2, mm, b"P", 0).unwrap_err(),
            QuoteRejection::DuplicateNonce,
        );
    }

    #[test]
    fn rejects_expired_quote() {
        let sk = SigningKey::generate(&mut OsRng);
        let mm = ObjectId::new([0x01; 32]);
        let state = make_state(mm, sk.verifying_key().to_bytes().to_vec(), 10_000);
        let bucket = ObjectId::new([0x99; 32]);
        let p = signed_quote(&sk, b"P".to_vec(), mm, bucket, 100, 500, 1);
        let rej = validate_and_reserve(&state, Side::Writer, bucket, 100, &p, mm, b"P", 1_000_000)
            .unwrap_err();
        assert_eq!(rej, QuoteRejection::Expired);
    }

    #[test]
    fn rejects_tampered_signature() {
        let sk = SigningKey::generate(&mut OsRng);
        let mm = ObjectId::new([0x01; 32]);
        let state = make_state(mm, sk.verifying_key().to_bytes().to_vec(), 10_000);
        let bucket = ObjectId::new([0x99; 32]);
        let mut p = signed_quote(&sk, b"P".to_vec(), mm, bucket, 100, 500, 1);
        p.quote.premium = 9_999; // tamper after signing
        assert_eq!(
            validate_and_reserve(&state, Side::Writer, bucket, 100, &p, mm, b"P", 0).unwrap_err(),
            QuoteRejection::SignatureInvalid,
        );
    }

    #[test]
    fn rejects_bucket_mismatch() {
        let sk = SigningKey::generate(&mut OsRng);
        let mm = ObjectId::new([0x01; 32]);
        let state = make_state(mm, sk.verifying_key().to_bytes().to_vec(), 10_000);
        let bucket = ObjectId::new([0x99; 32]);
        let p = signed_quote(&sk, b"P".to_vec(), mm, ObjectId::new([0xaa; 32]), 100, 500, 1);
        assert_eq!(
            validate_and_reserve(&state, Side::Writer, bucket, 100, &p, mm, b"P", 0).unwrap_err(),
            QuoteRejection::BucketMismatch,
        );
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
}
