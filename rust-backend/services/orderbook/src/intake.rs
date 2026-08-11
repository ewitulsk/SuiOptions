//! Order intake pipeline (spec §5.4): synchronous, ordered checks with a
//! specific error code at the first failure. The service never accepts an
//! order the chain would reject on signature grounds, and v1 rejects
//! over-committed makers outright — the book stays honest-by-construction.

use crate::state::{now_ms, AppState, IntakeConfig};
use exchange_book::price_and_size;
use exchange_types::order::SignedOrder;
use exchange_types::{canonicalize_move_type, Digest, Market, Side};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum IntakeErrorCode {
    UnknownMarket,
    TokenMismatch,
    ZeroAmount,
    AmountRange,
    OffTick,
    BelowMinSize,
    ExpiryTooSoon,
    ExpiryTooFar,
    SaltNotMonotonic,
    SaltVoided,
    BadSignature,
    SignerNotAuthorized,
    InsufficientEscrow,
    Duplicate,
    Internal,
}

#[derive(Debug, thiserror::Error)]
#[error("intake rejected: {code:?} — {detail}")]
pub struct IntakeError {
    pub code: IntakeErrorCode,
    pub detail: String,
}

fn reject(code: IntakeErrorCode, detail: impl Into<String>) -> IntakeError {
    IntakeError { code, detail: detail.into() }
}

/// Stage 1 (pure): schema, market, tick/size, expiry window, signature.
/// Returns the digest, side and price. Delegated-signer authorization and
/// everything requiring the store happens in stage 2.
pub fn validate_stateless(
    market: &Market,
    signed: &SignedOrder,
    cfg: &IntakeConfig,
    now: u64,
) -> Result<(Digest, Side, u64), IntakeError> {
    let o = &signed.order;
    if signed.registry_id != market.registry_id {
        return Err(reject(IntakeErrorCode::UnknownMarket, "registry mismatch"));
    }
    // canonical token strings, exactly as signed
    for t in [&o.maker_token, &o.taker_token] {
        match canonicalize_move_type(t) {
            Ok(c) if c == *t => {}
            _ => {
                return Err(reject(
                    IntakeErrorCode::TokenMismatch,
                    format!("token type not canonical: {t}"),
                ))
            }
        }
    }
    if o.maker_amount == 0 || o.taker_amount == 0 {
        return Err(reject(IntakeErrorCode::ZeroAmount, "zero amount"));
    }
    if o.maker_amount > i64::MAX as u64 || o.taker_amount > i64::MAX as u64 {
        return Err(reject(IntakeErrorCode::AmountRange, "amount exceeds i64 range"));
    }
    let (side, price_ticks, _base) = price_and_size(market, o).map_err(|e| match e {
        exchange_book::BookError::WrongMarket => {
            reject(IntakeErrorCode::TokenMismatch, "tokens do not match market")
        }
        exchange_book::BookError::OffTick => reject(IntakeErrorCode::OffTick, "price off tick"),
        exchange_book::BookError::BelowMinSize => {
            reject(IntakeErrorCode::BelowMinSize, "below min size")
        }
        other => reject(IntakeErrorCode::Internal, other.to_string()),
    })?;
    if o.expiry_ms < now + cfg.min_ttl_ms {
        return Err(reject(IntakeErrorCode::ExpiryTooSoon, "expiry too soon"));
    }
    if o.expiry_ms > now + cfg.max_ttl_ms {
        return Err(reject(IntakeErrorCode::ExpiryTooFar, "expiry too far"));
    }
    // §5.4 step 3: full signature verification. Authorization of a delegated
    // signer (vs the maker itself) is completed in stage 2 with the mirrored
    // approved-signer set.
    let digest = exchange_signing::order_digest(o, &signed.registry_id);
    exchange_signing::verify_signature(
        signed.scheme,
        &digest.0,
        &signed.signature,
        &signed.public_key,
    )
    .map_err(|e| reject(IntakeErrorCode::BadSignature, e.to_string()))?;
    Ok((digest, side, price_ticks))
}

/// Full pipeline: stage 1 + store-backed checks (salt monotonicity vs both
/// the local max and the on-chain watermark; delegated-signer set; escrow
/// coverage), then persist (write-ahead) — the caller inserts into the book
/// after this returns.
pub async fn intake_order(
    state: &AppState,
    signed: &SignedOrder,
) -> Result<(Digest, Side, u64), IntakeError> {
    let market = state
        .market(&signed.registry_id)
        .ok_or_else(|| reject(IntakeErrorCode::UnknownMarket, "unknown registry"))?;
    let now = now_ms();
    let (digest, side, price_ticks) = validate_stateless(market, signed, &state.intake, now)?;
    let o = &signed.order;

    // delegated signer authorization (§4.3): derived address must be the
    // maker or a mirrored approved signer of the pinned manager
    let derived = exchange_signing::derive_address(signed.scheme, &signed.public_key);
    if derived != o.maker {
        let ok = state
            .db
            .is_approved_signer(&o.maker_manager_id, &derived)
            .await
            .map_err(|e| reject(IntakeErrorCode::Internal, e.to_string()))?;
        if !ok {
            return Err(reject(
                IntakeErrorCode::SignerNotAuthorized,
                format!("{derived} is not an approved signer"),
            ));
        }
    }

    // §5.4 step 2: salt monotonic per (maker, market), above the watermark
    let watermark = state
        .db
        .watermark(&signed.registry_id, &o.maker)
        .await
        .map_err(|e| reject(IntakeErrorCode::Internal, e.to_string()))?;
    if o.salt <= watermark {
        return Err(reject(IntakeErrorCode::SaltVoided, "salt at or below watermark"));
    }
    let max_salt = state
        .db
        .max_salt(&signed.registry_id, &o.maker)
        .await
        .map_err(|e| reject(IntakeErrorCode::Internal, e.to_string()))?;
    if let Some(max) = max_salt {
        if o.salt <= max {
            return Err(reject(
                IntakeErrorCode::SaltNotMonotonic,
                format!("salt {} <= previous {}", o.salt, max),
            ));
        }
    }

    // §5.4 step 4: uncommitted escrow must cover the new order. DIRECT
    // vault managers (SO-372) are exempt: their manager is identity-only
    // and the escrow is the vault's free balance, enforced per fill
    // on-chain with side-tagged aborts the settlement worker prunes on.
    let direct = state
        .db
        .vault_manager(&o.maker_manager_id)
        .await
        .map_err(|e| reject(IntakeErrorCode::Internal, e.to_string()))?
        .is_some_and(|v| v.direct);
    if !direct {
        let balance = state
            .db
            .balance(&o.maker_manager_id, &o.maker_token)
            .await
            .map_err(|e| reject(IntakeErrorCode::Internal, e.to_string()))?;
        let committed = state
            .db
            .open_commitment(&o.maker_manager_id, &o.maker_token)
            .await
            .map_err(|e| reject(IntakeErrorCode::Internal, e.to_string()))?;
        if balance < committed.saturating_add(o.maker_amount) {
            return Err(reject(
                IntakeErrorCode::InsufficientEscrow,
                format!("escrow {balance} < committed {committed} + {}", o.maker_amount),
            ));
        }
    }

    // §5.4 step 5: write-ahead persist (status OPEN)
    let order_bytes = o.to_bcs();
    state
        .db
        .insert_order(&digest, signed, side, price_ticks, &order_bytes)
        .await
        .map_err(|e| {
            if e.is_unique_violation() {
                reject(IntakeErrorCode::Duplicate, "order already submitted")
            } else {
                reject(IntakeErrorCode::Internal, e.to_string())
            }
        })?;

    Ok((digest, side, price_ticks))
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange_types::order::{Order, SignatureScheme};
    use exchange_types::SuiAddress;
    use exchange_signing::keys::Ed25519Keypair;

    fn canonical(short: &str) -> String {
        canonicalize_move_type(short).unwrap()
    }

    fn market() -> Market {
        Market {
            symbol: "SUI/USDC".into(),
            registry_id: SuiAddress::parse("0x5c").unwrap(),
            base: canonical("0x2::sui::SUI"),
            quote: canonical("0xaa::usdc::USDC"),
            tick_size: 1_000,
            min_size: 100,
            lot_size: 1_000_000,
            current_fee_bps: 10,
        }
    }

    fn signed_order(kp: &Ed25519Keypair, m: &Market, now: u64) -> SignedOrder {
        let order = Order {
            maker_token: m.base.clone(),
            taker_token: m.quote.clone(),
            maker_amount: 10_000,
            taker_amount: 20_000, // price 2.0 => 2000 ticks
            max_fee_bps: 10,
            maker: kp.address(),
            maker_manager_id: SuiAddress::parse("0x71").unwrap(),
            taker: SuiAddress::ZERO,
            sender: SuiAddress::ZERO,
            expiry_ms: now + 60_000,
            salt: 1,
        };
        let digest = exchange_signing::order_digest(&order, &m.registry_id);
        let signature = kp.sign_personal_message(&digest.0);
        SignedOrder {
            order,
            registry_id: m.registry_id,
            scheme: SignatureScheme::Ed25519,
            signature,
            public_key: kp.public_key(),
        }
    }

    #[test]
    fn stateless_accepts_valid_order() {
        let m = market();
        let kp = Ed25519Keypair::from_seed([5u8; 32]);
        let now = 1_000_000;
        let s = signed_order(&kp, &m, now);
        let (digest, side, price) =
            validate_stateless(&m, &s, &IntakeConfig::default(), now).unwrap();
        assert_eq!(side, Side::Ask);
        assert_eq!(price, 2_000);
        assert_eq!(digest, exchange_signing::order_digest(&s.order, &m.registry_id));
    }

    #[test]
    fn stateless_rejections() {
        let m = market();
        let kp = Ed25519Keypair::from_seed([5u8; 32]);
        let now = 1_000_000;
        let cfg = IntakeConfig::default();

        // tampered economics after signing
        let mut s = signed_order(&kp, &m, now);
        s.order.taker_amount = 10_000;
        assert_eq!(
            validate_stateless(&m, &s, &cfg, now).unwrap_err().code,
            IntakeErrorCode::BadSignature
        );

        // expiry too soon
        let mut s = signed_order(&kp, &m, now);
        s.order.expiry_ms = now + 1_000;
        assert_eq!(
            validate_stateless(&m, &s, &cfg, now).unwrap_err().code,
            IntakeErrorCode::ExpiryTooSoon
        );

        // off-tick price
        let mut s = signed_order(&kp, &m, now);
        s.order.taker_amount = 20_001;
        assert_eq!(
            validate_stateless(&m, &s, &cfg, now).unwrap_err().code,
            IntakeErrorCode::OffTick
        );

        // non-canonical token string
        let mut s = signed_order(&kp, &m, now);
        s.order.maker_token = "0x2::sui::SUI".into();
        assert_eq!(
            validate_stateless(&m, &s, &cfg, now).unwrap_err().code,
            IntakeErrorCode::TokenMismatch
        );

        // wrong registry
        let mut s = signed_order(&kp, &m, now);
        s.registry_id = SuiAddress::parse("0x5d").unwrap();
        assert_eq!(
            validate_stateless(&m, &s, &cfg, now).unwrap_err().code,
            IntakeErrorCode::UnknownMarket
        );
    }
}
