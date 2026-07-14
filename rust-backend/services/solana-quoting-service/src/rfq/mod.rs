//! RFQ orchestration — the Solana port of the Sui twin's.
//!
//! One retail `RFQRequest` →
//!   1. broadcast `RFQBroadcast` to every connected MM on the opposite side;
//!   2. collect `Quote`s until the deadline elapses (or every MM declines);
//!   3. validate each quote (ed25519 signature over the canonical Borsh
//!      bytes, expiry, bucket match, write_amount, reservation feasibility
//!      against the signer's available per-mint balance);
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

use base64::Engine as _;
use tracing::{debug, info, trace, warn};

use protocol_types::sides::Side;
use solana_indexer_graphql::{Account, Bucket};

use crate::messages::{MmQuotePayload, RfqQuoteEntry};
use crate::quote::{QuoteWire, SolanaQuote};
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
    /// The indexer reports a signing scheme other than Ed25519 (0) for this
    /// account. Program v1 only supports ed25519; anything else is
    /// unverifiable here and would revert on chain.
    SchemeUnknown,
    /// A pubkey field in the quote isn't valid base58 / 32 bytes, so the
    /// canonical Borsh bytes can't even be built.
    MalformedQuote,
    /// Admin has invalidated the bucket; a quote against it would revert
    /// on chain. Defense-in-depth against a race between the MM signing
    /// and the BucketInvalidated event landing.
    BucketInvalidated,
}

/// The reservation amount and mint for one quote, given the bucket and the
/// retail-side direction (same rules as the Sui twin, mints instead of coin
/// types):
///
/// - Writer flow (retail writes): signer is Trader MM, signer provides the
///   premium in the bucket's settlement mint. Same for calls and puts.
/// - Trader flow (retail trades): signer is Writer MM (the put writer for a
///   put bucket). For a **call** bucket they escrow `write_amount` of the
///   underlying mint; for a **cash-secured put** they instead post CASH
///   collateral in the settlement mint, `ceil(write_amount × strike /
///   10^strike_scale)`.
pub fn reservation_for(side: Side, bucket: &Bucket, quote: &QuoteWire) -> (String, u64) {
    match side {
        Side::Writer => (bucket.settlement_mint.clone(), quote.premium),
        Side::Trader if bucket.option_kind == "put" => (
            bucket.settlement_mint.clone(),
            put_collateral(bucket, quote.write_amount),
        ),
        Side::Trader => (bucket.underlying_mint.clone(), quote.write_amount),
    }
}

/// Cash collateral a put writer must post for `write_amount` of underlying:
/// `ceil(write_amount × strike / 10^strike_scale)`, computed in u128 and
/// downcast to u64 (saturating — an overflow here means the bucket params are
/// unrealistic and the reservation check would reject anyway).
fn put_collateral(bucket: &Bucket, write_amount: u64) -> u64 {
    // `checked_pow` guards a pathological strike_scale (>38) from panicking;
    // such a scale can't represent a real price, so fall back to a divisor
    // of 1 — the reservation check downstream rejects the absurd amount.
    let scale = 10u128.checked_pow(bucket.strike_scale as u32).unwrap_or(1);
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
#[allow(clippy::too_many_arguments)]
pub fn validate_and_reserve(
    state: &AppState,
    side: Side,
    bucket: &Bucket,
    account: &Account,
    write_amount: u64,
    payload: &MmQuotePayload,
    mm_account_id: &str,
    protocol_id: &str,
    now_ms: u64,
) -> Result<RfqQuoteEntry, QuoteRejection> {
    let quote = &payload.quote;
    trace!(mm = %mm_account_id, bucket = %bucket.bucket_id, nonce = quote.nonce, premium = quote.premium, "validating quote");
    // Cheap structural checks first (byte-exact base58 comparisons).
    if quote.protocol_id != protocol_id {
        return Err(QuoteRejection::ProtocolMismatch);
    }
    if quote.bucket != bucket.bucket_id {
        return Err(QuoteRejection::BucketMismatch);
    }
    if quote.write_amount != write_amount {
        return Err(QuoteRejection::WriteAmountMismatch);
    }
    if now_ms >= quote.valid_until_ms {
        return Err(QuoteRejection::Expired);
    }
    if quote.signer_account != mm_account_id {
        return Err(QuoteRejection::SignatureInvalid);
    }

    // ed25519 only (program v1): a non-zero registered scheme is
    // unverifiable here and would revert on chain.
    if account.signing_scheme != 0 {
        return Err(QuoteRejection::SchemeUnknown);
    }
    if account.signing_pubkey.len() != 32 {
        return Err(QuoteRejection::InvalidPubkey);
    }
    // Canonical Borsh bytes — exactly what the Ed25519SigVerify precompile
    // will carry and the program will compare against.
    let canonical = SolanaQuote::try_from(quote).map_err(|_| QuoteRejection::MalformedQuote)?;
    let quote_bytes = canonical.to_bytes();
    if !crate::quote::verify_ed25519(&account.signing_pubkey, &quote_bytes, &payload.signature) {
        return Err(QuoteRejection::SignatureInvalid);
    }

    // Reservation feasibility.
    if bucket.invalidated {
        return Err(QuoteRejection::BucketInvalidated);
    }
    let (mint, amount) = reservation_for(side, bucket, quote);
    if state.available(account, &mint) < amount {
        return Err(QuoteRejection::InsufficientAvailableBalance);
    }

    // Reserve. Nonce uniqueness is enforced here too — duplicate nonce
    // means the MM signed two quotes with the same nonce, which would
    // revert on chain (nonce_record PDA init fails); reject.
    let outcome = state.reservations.insert(Reservation {
        account_id: mm_account_id.to_string(),
        nonce: quote.nonce,
        mint,
        amount,
        valid_until_ms: quote.valid_until_ms,
        created_at_ms: now_ms,
    });
    if outcome == InsertOutcome::DuplicateKey {
        return Err(QuoteRejection::DuplicateNonce);
    }

    // Reputation: count the signature.
    state.reputation.record_signed(mm_account_id);

    let rep = state.reputation.snapshot(mm_account_id).composite_score();
    Ok(RfqQuoteEntry {
        quote: quote.clone(),
        signature: payload.signature.clone(),
        quote_bytes_b64: base64::engine::general_purpose::STANDARD.encode(&quote_bytes),
        mm_id: mm_account_id.to_string(),
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
        reservations.release(&e.mm_id, e.quote.nonce);
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
    protocol_id: String,
    now_ms: u64,
) -> Vec<RfqQuoteEntry> {
    let bucket_id = bucket.bucket_id.clone();
    let deadline_ms = now_ms.saturating_add(rfq_window.as_millis() as u64);

    // The bucket is fetched JIT by the caller (retail.rs). Defense-in-depth
    // against direct callers: refuse invalidated.
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
        let frame = crate::messages::ServiceToMm::RFQBroadcast {
            request_id: request_id.clone(),
            payload: crate::messages::RfqBroadcastPayload {
                // Only the bucket address travels — the MM resolves the
                // strike/expiry/mints itself from solana-api-service so it
                // never trusts pricing inputs delivered over the wire.
                bucket_id: bucket_id.clone(),
                write_amount,
                side,
                deadline_ms,
            },
        };
        // Tell the matcher who we expect a response from so it can decide
        // to short-circuit when every MM has answered.
        input.expect(&mm.account_id);
        // try_send: a slow MM whose outbound channel is full should miss
        // this window, not back-pressure the orchestrator. Awaiting an
        // unbounded send here lets one stuck MM pile up orchestrators
        // indefinitely under load.
        if let Err(e) = mm.tx.try_send(frame) {
            debug!(mm = %mm.account_id, request_id = %request_id, error = %e, "dropping rfq broadcast: mm channel full or closed");
            input.unexpect(&mm.account_id);
        }
    }
    // Drop the sender side(s) we hold so the matcher's close-detection works
    // once every MM that's going to answer has done so.
    drop(input);

    let collected = collector.await.unwrap_or_default();
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
        if let Err(e) = state.reconcile_executed(&mm_id).await {
            warn!(mm = %mm_id, error = %e, "reservation reconcile failed; proceeding with stale reservations");
        }
        let account = match state.indexer.account(&mm_id).await {
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
                &mm_id,
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

    const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const TBTC: &str = "So11111111111111111111111111111111111111112";

    fn b58(tag: u8) -> String {
        bs58::encode([tag; 32]).into_string()
    }

    /// A quoting state with no live indexer — the validation path is pure, so
    /// tests pass the bucket + account in directly. The URL is unreachable on
    /// purpose: any test that actually hit it would be a bug.
    fn test_state() -> AppState {
        AppState::with_global_rfq_cap(256, "http://127.0.0.1:1/graphql".into())
    }

    fn mk_account(mm: &str, signing_pubkey: Vec<u8>, balance: u64) -> Account {
        Account {
            account_id: mm.into(),
            owner: b58(0xee),
            signing_scheme: 0,
            signing_pubkey,
            balances: BTreeMap::from([(USDC.to_string(), balance)]),
        }
    }

    fn mk_bucket() -> Bucket {
        Bucket {
            bucket_id: b58(0x99),
            underlying_mint: TBTC.into(),
            settlement_mint: USDC.into(),
            option_mint: b58(0x77),
            option_kind: "call".into(),
            strike: 50,
            strike_scale: 0,
            expiry_ms: 1_000_000,
            total_written: 0,
            exercise_cursor: 0,
            cleaned: false,
            invalidated: false,
        }
    }

    fn signed_quote(
        sk: &SigningKey,
        protocol_id: &str,
        mm_account: &str,
        bucket: &str,
        write_amount: u64,
        premium: u64,
        nonce: u64,
    ) -> MmQuotePayload {
        let q = QuoteWire {
            protocol_id: protocol_id.into(),
            signer_account: mm_account.into(),
            signer_token_recipient: b58(0x55),
            bucket: bucket.into(),
            write_amount,
            premium,
            valid_until_ms: 999_999,
            nonce,
        };
        let bytes = SolanaQuote::try_from(&q).unwrap().to_bytes();
        let sig = sk.sign(&bytes).to_bytes().to_vec();
        MmQuotePayload { quote: q, signature: sig }
    }

    #[test]
    fn happy_path_writes_a_reservation_and_ships_quote_bytes() {
        let sk = SigningKey::generate(&mut OsRng);
        let mm = b58(0x01);
        let pid = b58(0xaa);
        let state = test_state();
        let account = mk_account(&mm, sk.verifying_key().to_bytes().to_vec(), 10_000);
        let bucket = mk_bucket();
        let p = signed_quote(&sk, &pid, &mm, &bucket.bucket_id, 100, 500, 1);

        let entry =
            validate_and_reserve(&state, Side::Writer, &bucket, &account, 100, &p, &mm, &pid, 0)
                .unwrap();
        assert_eq!(entry.mm_id, mm);
        assert_eq!(entry.quote.premium, 500);
        // quote_bytes_b64 decodes to the canonical Borsh bytes.
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&entry.quote_bytes_b64)
            .unwrap();
        assert_eq!(
            decoded,
            SolanaQuote::try_from(&p.quote).unwrap().to_bytes()
        );
        assert_eq!(decoded.len(), 4 * 32 + 4 * 8);
        // 10000 USDC balance, 500 reserved → 9500 available.
        assert_eq!(state.available(&account, USDC), 9500);
    }

    #[test]
    fn rejects_insufficient_balance() {
        let sk = SigningKey::generate(&mut OsRng);
        let mm = b58(0x01);
        let pid = b58(0xaa);
        let state = test_state();
        let account = mk_account(&mm, sk.verifying_key().to_bytes().to_vec(), 100);
        let bucket = mk_bucket();
        let p = signed_quote(&sk, &pid, &mm, &bucket.bucket_id, 100, 500, 1);
        assert_eq!(
            validate_and_reserve(&state, Side::Writer, &bucket, &account, 100, &p, &mm, &pid, 0)
                .unwrap_err(),
            QuoteRejection::InsufficientAvailableBalance,
        );
        assert_eq!(state.reservations.len(), 0);
    }

    #[test]
    fn rejects_duplicate_nonce() {
        let sk = SigningKey::generate(&mut OsRng);
        let mm = b58(0x01);
        let pid = b58(0xaa);
        let state = test_state();
        let account = mk_account(&mm, sk.verifying_key().to_bytes().to_vec(), 10_000);
        let bucket = mk_bucket();
        let p1 = signed_quote(&sk, &pid, &mm, &bucket.bucket_id, 100, 500, 7);
        let p2 = signed_quote(&sk, &pid, &mm, &bucket.bucket_id, 100, 600, 7);
        validate_and_reserve(&state, Side::Writer, &bucket, &account, 100, &p1, &mm, &pid, 0)
            .unwrap();
        assert_eq!(
            validate_and_reserve(&state, Side::Writer, &bucket, &account, 100, &p2, &mm, &pid, 0)
                .unwrap_err(),
            QuoteRejection::DuplicateNonce,
        );
    }

    #[test]
    fn rejects_expired_quote() {
        let sk = SigningKey::generate(&mut OsRng);
        let mm = b58(0x01);
        let pid = b58(0xaa);
        let state = test_state();
        let account = mk_account(&mm, sk.verifying_key().to_bytes().to_vec(), 10_000);
        let bucket = mk_bucket();
        let p = signed_quote(&sk, &pid, &mm, &bucket.bucket_id, 100, 500, 1);
        let rej = validate_and_reserve(
            &state, Side::Writer, &bucket, &account, 100, &p, &mm, &pid, 1_000_000,
        )
        .unwrap_err();
        assert_eq!(rej, QuoteRejection::Expired);
    }

    #[test]
    fn rejects_tampered_signature() {
        let sk = SigningKey::generate(&mut OsRng);
        let mm = b58(0x01);
        let pid = b58(0xaa);
        let state = test_state();
        let account = mk_account(&mm, sk.verifying_key().to_bytes().to_vec(), 10_000);
        let bucket = mk_bucket();
        let mut p = signed_quote(&sk, &pid, &mm, &bucket.bucket_id, 100, 500, 1);
        p.quote.premium = 9_999; // tamper after signing
        assert_eq!(
            validate_and_reserve(&state, Side::Writer, &bucket, &account, 100, &p, &mm, &pid, 0)
                .unwrap_err(),
            QuoteRejection::SignatureInvalid,
        );
    }

    #[test]
    fn rejects_protocol_mismatch() {
        let sk = SigningKey::generate(&mut OsRng);
        let mm = b58(0x01);
        let state = test_state();
        let account = mk_account(&mm, sk.verifying_key().to_bytes().to_vec(), 10_000);
        let bucket = mk_bucket();
        // Signed for a different Config PDA than the deployment's.
        let p = signed_quote(&sk, &b58(0xbb), &mm, &bucket.bucket_id, 100, 500, 1);
        assert_eq!(
            validate_and_reserve(
                &state, Side::Writer, &bucket, &account, 100, &p, &mm, &b58(0xaa), 0,
            )
            .unwrap_err(),
            QuoteRejection::ProtocolMismatch,
        );
    }

    #[test]
    fn rejects_bucket_mismatch() {
        let sk = SigningKey::generate(&mut OsRng);
        let mm = b58(0x01);
        let pid = b58(0xaa);
        let state = test_state();
        let account = mk_account(&mm, sk.verifying_key().to_bytes().to_vec(), 10_000);
        let bucket = mk_bucket();
        let p = signed_quote(&sk, &pid, &mm, &b58(0xcc), 100, 500, 1);
        assert_eq!(
            validate_and_reserve(&state, Side::Writer, &bucket, &account, 100, &p, &mm, &pid, 0)
                .unwrap_err(),
            QuoteRejection::BucketMismatch,
        );
    }

    #[test]
    fn rejects_non_ed25519_scheme() {
        // ed25519 only (program v1): a registered scheme != 0 is
        // unverifiable — reject before touching the signature.
        let sk = SigningKey::generate(&mut OsRng);
        let mm = b58(0x01);
        let pid = b58(0xaa);
        let state = test_state();
        let mut account = mk_account(&mm, sk.verifying_key().to_bytes().to_vec(), 10_000);
        account.signing_scheme = 1;
        let bucket = mk_bucket();
        let p = signed_quote(&sk, &pid, &mm, &bucket.bucket_id, 100, 500, 1);
        assert_eq!(
            validate_and_reserve(&state, Side::Writer, &bucket, &account, 100, &p, &mm, &pid, 0)
                .unwrap_err(),
            QuoteRejection::SchemeUnknown,
        );
    }

    #[test]
    fn rejects_quote_against_invalidated_bucket() {
        // Defense-in-depth: even if retail.rs lets an RFQ through (race
        // with the BucketInvalidated event), the per-quote check must
        // refuse to reserve.
        let sk = SigningKey::generate(&mut OsRng);
        let mm = b58(0x01);
        let pid = b58(0xaa);
        let state = test_state();
        let account = mk_account(&mm, sk.verifying_key().to_bytes().to_vec(), 10_000);
        let mut bucket = mk_bucket();
        bucket.invalidated = true;
        let p = signed_quote(&sk, &pid, &mm, &bucket.bucket_id, 100, 500, 1);
        assert_eq!(
            validate_and_reserve(&state, Side::Writer, &bucket, &account, 100, &p, &mm, &pid, 0)
                .unwrap_err(),
            QuoteRejection::BucketInvalidated,
        );
        assert_eq!(state.reservations.len(), 0);
    }

    #[test]
    fn rejects_malformed_pubkey_in_quote() {
        let sk = SigningKey::generate(&mut OsRng);
        let mm = b58(0x01);
        let pid = b58(0xaa);
        let state = test_state();
        let account = mk_account(&mm, sk.verifying_key().to_bytes().to_vec(), 10_000);
        let bucket = mk_bucket();
        let mut p = signed_quote(&sk, &pid, &mm, &bucket.bucket_id, 100, 500, 1);
        p.quote.signer_token_recipient = "not-base58-0OIl".into();
        assert_eq!(
            validate_and_reserve(&state, Side::Writer, &bucket, &account, 100, &p, &mm, &pid, 0)
                .unwrap_err(),
            QuoteRejection::MalformedQuote,
        );
    }

    #[test]
    fn put_trader_reserves_ceil_cash_collateral() {
        // write_amount=100, strike=50, scale=0 → 100*50 = 5000 settlement.
        let mut bucket = mk_bucket();
        bucket.option_kind = "put".into();
        let q = QuoteWire {
            protocol_id: b58(0xaa),
            signer_account: b58(0x01),
            signer_token_recipient: b58(0x55),
            bucket: bucket.bucket_id.clone(),
            write_amount: 100,
            premium: 7,
            valid_until_ms: 0,
            nonce: 0,
        };
        let (mint, amount) = reservation_for(Side::Trader, &bucket, &q);
        assert_eq!(mint, bucket.settlement_mint);
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
        let (wmint, wamt) = reservation_for(Side::Writer, &bucket, &q);
        assert_eq!(wmint, bucket.settlement_mint);
        assert_eq!(wamt, 7);
    }

    #[test]
    fn call_trader_still_reserves_underlying() {
        let bucket = mk_bucket(); // option_kind == "call"
        let q = QuoteWire {
            protocol_id: b58(0xaa),
            signer_account: b58(0x01),
            signer_token_recipient: b58(0x55),
            bucket: bucket.bucket_id.clone(),
            write_amount: 100,
            premium: 7,
            valid_until_ms: 0,
            nonce: 0,
        };
        let (mint, amount) = reservation_for(Side::Trader, &bucket, &q);
        assert_eq!(mint, bucket.underlying_mint);
        assert_eq!(amount, 100);
    }

    fn mk_entry(premium: u64, rep: f64) -> RfqQuoteEntry {
        RfqQuoteEntry {
            quote: QuoteWire {
                protocol_id: "p".into(),
                signer_account: "s".into(),
                signer_token_recipient: "r".into(),
                bucket: "b".into(),
                write_amount: 100,
                premium,
                valid_until_ms: 0,
                nonce: 0,
            },
            signature: vec![],
            quote_bytes_b64: String::new(),
            mm_id: "mm".into(),
            mm_reputation: rep,
        }
    }

    #[test]
    fn writer_side_sorts_highest_premium_first() {
        let mut v = vec![
            mk_entry(500, 0.5),
            mk_entry(600, 0.1),
            mk_entry(600, 0.9),
            mk_entry(450, 1.0),
        ];
        sort_best_first(Side::Writer, &mut v);
        assert_eq!(v[0].quote.premium, 600);
        assert!((v[0].mm_reputation - 0.9).abs() < 1e-9);
        assert_eq!(v[1].quote.premium, 600);
        assert_eq!(v[2].quote.premium, 500);
        assert_eq!(v[3].quote.premium, 450);
    }

    #[test]
    fn trader_side_sorts_lowest_premium_first() {
        let mut v = vec![
            mk_entry(500, 0.0),
            mk_entry(300, 0.0),
            mk_entry(700, 0.0),
            mk_entry(100, 0.0),
        ];
        sort_best_first(Side::Trader, &mut v);
        assert_eq!(
            v.iter().map(|e| e.quote.premium).collect::<Vec<_>>(),
            vec![100, 300, 500, 700]
        );
    }

    /// Regression (ported): a slow/stalled MM must not back-pressure the
    /// orchestrator. With `try_send`, the orchestrator drops the broadcast
    /// to that MM and returns within the RFQ window.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn orchestrate_does_not_block_on_full_mm_channel() {
        use crate::state::MmConnection;
        use protocol_types::sides::MmRole;
        use std::sync::Arc;
        use tokio::sync::mpsc;

        let mm = b58(0x01);
        let bucket = mk_bucket();
        let state = Arc::new(test_state());

        // Register an MM whose outbound channel is full and never drained.
        let (tx, rx) = mpsc::channel(1);
        // Fill the channel so subsequent `try_send` calls fail with Full.
        tx.try_send(crate::messages::ServiceToMm::Ping).unwrap();
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
                    b58(0xaa),
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
