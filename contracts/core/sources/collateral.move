/// The dependency-inverted collateral protocol
/// (docs/audit-restructure/04-collateral-abstraction-plan.md).
///
/// Core never depends on a collateral implementation. Instead, the bucket
/// modules verify a signed quote (consuming its nonce) and mint a
/// `CollateralRequest<T>` — a hot potato demanding `amount` of `T` from the
/// object the quote's SIGNED `collateral_source` names. Any external package
/// that depends on core implements the standardized release interface:
///
///     public fun release<T>(
///         account: &mut <ImplementationType>,
///         request: &CollateralRequest<T>,
///         ctx: &mut TxContext,
///     ): Balance<T>
///
/// and MUST only release against a request whose `source(request)` equals
/// its own object id. The request reference is the proof: only core mints
/// one, only after signature + expiry + nonce verification, and the potato
/// (no abilities) forces same-transaction consumption by an
/// `execute_*_flow` — so either the write completes with the collateral
/// delivered, or everything (including the nonce burn) reverts.
///
/// A malicious implementation can only refuse (abort): the counterparty is
/// protected by core's amount/type checks, not by trusting the source.
module options_core::collateral;

use options_core::quote::{Self, Quote};

/// Which side of the trade the demanded collateral funds. Carried on the
/// potato so a writer-flow request (sized by the premium) can never be
/// routed into a trader-flow execute (sized by the write collateral) —
/// relevant for puts, where both legs are the settlement asset.
public enum Flow has copy, drop {
    Writer,
    Trader,
}

/// A verified, single-use demand for `amount` of `T` from the object the
/// signed quote names. No abilities: must be consumed this transaction.
public struct CollateralRequest<phantom T> {
    quote: Quote,
    amount: u64,
    flow: Flow,
    /// The demanding bucket's object id, resolved from the bucket the
    /// request was minted against — NOT from the quote, which binds the
    /// bucket's spec rather than its address (see `quote::Quote`). Purely
    /// informational for release implementations; nothing authorizes on it.
    bucket_id: ID,
}

public(package) fun new_writer_request<T>(
    quote: Quote,
    amount: u64,
    bucket_id: ID,
): CollateralRequest<T> {
    CollateralRequest { quote, amount, flow: Flow::Writer, bucket_id }
}

public(package) fun new_trader_request<T>(
    quote: Quote,
    amount: u64,
    bucket_id: ID,
): CollateralRequest<T> {
    CollateralRequest { quote, amount, flow: Flow::Trader, bucket_id }
}

public(package) fun destroy<T>(request: CollateralRequest<T>): (Quote, u64, bool) {
    let CollateralRequest { quote, amount, flow, bucket_id: _ } = request;
    let is_writer = match (flow) {
        Flow::Writer => true,
        Flow::Trader => false,
    };
    (quote, amount, is_writer)
}

// ---- getters (the release-implementation surface) ----

/// The exact amount of `T` the implementation must return.
public fun amount<T>(request: &CollateralRequest<T>): u64 { request.amount }

/// The object the signed quote authorizes debiting. Implementations MUST
/// assert this equals their own object id before releasing.
public fun source<T>(request: &CollateralRequest<T>): ID {
    quote::collateral_source(&request.quote)
}

/// The demanding bucket, for implementation-side bookkeeping.
public fun bucket_id<T>(request: &CollateralRequest<T>): ID {
    request.bucket_id
}

/// The consumed quote nonce, for implementation-side bookkeeping.
public fun quote_nonce<T>(request: &CollateralRequest<T>): u64 {
    quote::nonce(&request.quote)
}

/// True for writer-flow requests (the demanded funds are the premium).
public fun is_writer_flow<T>(request: &CollateralRequest<T>): bool {
    match (request.flow) {
        Flow::Writer => true,
        Flow::Trader => false,
    }
}

/// Where the signer's side of the trade is routed (their `Coin<Call>` in
/// writer flow; their `Position` + net premium in trader flow). Exposed
/// so custody implementations whose funds belong to THIRD PARTIES (e.g.
/// a curated vault backing an MM bot) can refuse to release unless the
/// outputs come back to the custody object itself — without this, the
/// quote signer could route the trade's proceeds to any address.
public fun signer_token_recipient<T>(request: &CollateralRequest<T>): address {
    quote::signer_token_recipient(&request.quote)
}

/// Test-only constructor so external implementations (mm_collateral, …)
/// can unit-test `release` without running a full quote flow.
#[test_only]
public fun new_request_for_testing<T>(
    quote: Quote,
    amount: u64,
    writer_flow: bool,
    bucket_id: ID,
): CollateralRequest<T> {
    if (writer_flow) {
        new_writer_request(quote, amount, bucket_id)
    } else {
        new_trader_request(quote, amount, bucket_id)
    }
}

#[test_only]
public fun destroy_for_testing<T>(request: CollateralRequest<T>): (Quote, u64, bool) {
    destroy(request)
}
