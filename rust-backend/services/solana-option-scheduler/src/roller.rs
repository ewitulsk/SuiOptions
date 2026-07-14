//! Bucket-roll executor.
//!
//! The big simplification vs the Sui twin: no codegen, no in-process Move
//! compile, no coin publish — `call_mint`/`put_mint` are PDAs the program
//! creates, so a roll is just N `create_bucket(salt, expiry_ms, strike,
//! strike_scale)` instructions.
//!
//! One transaction per `create_bucket` (Anchor init of bucket + mint + two
//! vault ATAs is compute/size-heavy); a roll of N strikes = N txs submitted
//! **sequentially with per-tx confirm**. Salts are deterministic
//! ([`crate::salt::bucket_salt`]), so a re-run collides on-chain
//! (`already in use`) instead of duplicating — such a collision is Benign
//! and the loop resumes with the next strike.

use anyhow::{anyhow, Result};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::Transaction;
use tracing::{debug, info};

use solana_tx::SolanaClientWrapper;

use crate::salt;

/// Which option product a roll creates. Calls use `create_bucket` /
/// `pda::bucket`; puts use `create_put_bucket` / `pda::put_bucket`.
/// Defaults to `Call` so existing configs keep their behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProductType {
    #[default]
    Call,
    Put,
}

impl ProductType {
    /// Lowercase wire/DB tag (`"call"` / `"put"`) — also the indexer's
    /// `option_kind` and the salt-domain separator.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Put => "put",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "call" => Some(Self::Call),
            "put" => Some(Self::Put),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RollPlan {
    pub underlying_symbol: String,
    pub settlement_symbol: String,
    pub underlying_mint: Pubkey,
    pub settlement_mint: Pubkey,
    pub expiry_ms: u64,
    /// Explicit per-bucket strikes, ascending, in scaled chain units.
    pub strikes: Vec<u128>,
    pub strike_scale: u8,
    pub product_type: ProductType,
}

impl RollPlan {
    /// Deterministic per-strike salts (see [`crate::salt::bucket_salt`]).
    pub fn bucket_salts(&self) -> Vec<u64> {
        self.strikes
            .iter()
            .map(|k| {
                salt::bucket_salt(
                    &self.underlying_mint,
                    &self.settlement_mint,
                    self.expiry_ms,
                    *k,
                    self.strike_scale,
                    self.product_type,
                )
            })
            .collect()
    }

    /// The bucket PDAs this roll will create, derivable before any tx is
    /// sent — recorded up front in `scheduler_rolls.bucket_ids`.
    pub fn bucket_pdas(&self) -> Vec<Pubkey> {
        let core = options_core::ID;
        self.bucket_salts()
            .into_iter()
            .map(|s| match self.product_type {
                ProductType::Call => solana_tx::pda::bucket(
                    &core,
                    &self.underlying_mint,
                    &self.settlement_mint,
                    s,
                ),
                ProductType::Put => solana_tx::pda::put_bucket(
                    &core,
                    &self.underlying_mint,
                    &self.settlement_mint,
                    s,
                ),
            })
            .collect()
    }

    pub fn log_intent(&self, dry_run: bool) {
        info!(
            pair = %format!("{}/{}", self.underlying_symbol, self.settlement_symbol),
            product = self.product_type.as_str(),
            expiry_ms = self.expiry_ms,
            strikes = ?self.strikes,
            count = self.strikes.len(),
            strike_scale = self.strike_scale,
            dry_run,
            "rolling new bucket family"
        );
    }
}

pub struct RollOutcome {
    /// Signature of the last create_bucket tx we landed ourselves. `None`
    /// when every strike collided benignly (the family already existed).
    pub signature: Option<String>,
    /// The derived bucket PDAs (base58), one per strike.
    pub bucket_ids: Vec<String>,
}

/// Classification of a submit error for the local-rolls retry policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Build, sign, blockhash-fetch or preflight-reject: the tx never
    /// reached consensus. Safe to delete the pending row and retry on the
    /// next tick (existing buckets collide Benign on the resume).
    DefinitelyNotSent,
    /// Confirm timeout, transport error after transmit, unknown: the tx may
    /// or may not have been accepted. Must go to `needs_reconciliation`;
    /// the reconciler resolves it via `getSignatureStatuses` first.
    Ambiguous,
}

/// Inspect an error from `submit` and decide whether the tx definitely
/// never reached consensus or might have.
pub fn classify_error(err: &anyhow::Error) -> ErrorClass {
    classify_error_text(&format!("{err:#}"))
}

/// Text-level classifier (also used against litesvm/preflight log dumps).
pub fn classify_error_text(text: &str) -> ErrorClass {
    let msg = text.to_lowercase();
    // Build/sign/preflight errors — the tx was never transmitted (preflight
    // simulation rejections come back before broadcast).
    let definitely_not_sent = [
        "fetching latest blockhash",
        "transaction simulation failed",
        "simulation failed",
        "custom program error",
        "blockhash not found",
        "invalid param",
        "signature verification failure",
        "insufficient funds for",
        "attempt to debit an account",
        "already in use",
    ];
    for pat in &definitely_not_sent {
        if msg.contains(pat) {
            return ErrorClass::DefinitelyNotSent;
        }
    }
    // Everything else (confirm timeout, transport, unknown) is ambiguous.
    ErrorClass::Ambiguous
}

/// Is this the deterministic-salt collision ("account already in use")?
/// A re-submitted create over an existing bucket/vault PDA fails preflight
/// with the system program's allocate error — Benign: the account we wanted
/// already exists.
pub fn is_already_in_use(err: &anyhow::Error) -> bool {
    is_already_in_use_text(&format!("{err:#}"))
}

pub fn is_already_in_use_text(text: &str) -> bool {
    text.to_lowercase().contains("already in use")
}

/// A submit failure, carrying enough context for the DB bookkeeping: the
/// class, and the signature of the failed tx when it was actually signed
/// and broadcast (Ambiguous — the reconciler resolves it).
#[derive(Debug)]
pub struct SubmitFailure {
    pub class: ErrorClass,
    pub signature: Option<String>,
    pub error: anyhow::Error,
}

impl std::fmt::Display for SubmitFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#} (class {:?})", self.error, self.class)
    }
}

/// Submit the roll: one `create_bucket` (or `create_put_bucket`) tx per
/// strike, sequential, each confirmed before the next. "already in use"
/// collisions are Benign resumes. Aborts on the first real failure — the
/// caller classifies and either deletes the pending row (retry next tick)
/// or parks it in needs_reconciliation.
pub async fn submit(wrap: &SolanaClientWrapper, plan: &RollPlan) -> Result<RollOutcome, SubmitFailure> {
    let admin = wrap.signer.pubkey();
    let salts = plan.bucket_salts();
    let pdas = plan.bucket_pdas();
    let bucket_ids: Vec<String> = pdas.iter().map(|p| p.to_string()).collect();
    let mut last_signature: Option<String> = None;

    for (i, (&strike, &bucket_salt)) in plan.strikes.iter().zip(&salts).enumerate() {
        let ix = match plan.product_type {
            ProductType::Call => solana_tx::ix::create_bucket(
                &admin,
                &plan.underlying_mint,
                &plan.settlement_mint,
                bucket_salt,
                plan.expiry_ms,
                strike,
                plan.strike_scale,
            ),
            ProductType::Put => solana_tx::ix::create_put_bucket(
                &admin,
                &plan.underlying_mint,
                &plan.settlement_mint,
                bucket_salt,
                plan.expiry_ms,
                strike,
                plan.strike_scale,
            ),
        };
        let label = format!(
            "create_{}_bucket {}/{} strike {} ({} of {})",
            plan.product_type.as_str(),
            plan.underlying_symbol,
            plan.settlement_symbol,
            strike,
            i + 1,
            plan.strikes.len()
        );

        // Build + sign here (not via the wrapper's send_and_confirm) so the
        // signature is known even when the send outcome is ambiguous.
        let blockhash = match wrap.client.get_latest_blockhash().await {
            Ok(b) => b,
            Err(e) => {
                return Err(SubmitFailure {
                    class: ErrorClass::DefinitelyNotSent,
                    signature: None,
                    error: anyhow!("fetching latest blockhash for {label}: {e}"),
                })
            }
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&admin),
            &[&wrap.signer.keypair],
            blockhash,
        );
        let signature: Signature = tx.signatures[0];

        match wrap.client.send_and_confirm_transaction(&tx).await {
            Ok(sig) => {
                debug!(%sig, bucket = %pdas[i], label, "bucket created");
                last_signature = Some(sig.to_string());
            }
            Err(e) => {
                let err = anyhow!("{label} failed: {e}");
                if is_already_in_use(&err) {
                    // Deterministic salt collided with an existing bucket —
                    // a previous (partial) roll already created it. Benign:
                    // resume with the next strike.
                    info!(bucket = %pdas[i], label, "bucket already exists on-chain; resuming");
                    continue;
                }
                let class = classify_error(&err);
                return Err(SubmitFailure {
                    class,
                    // Only an ambiguous failure can have reached the chain.
                    signature: matches!(class, ErrorClass::Ambiguous)
                        .then(|| signature.to_string()),
                    error: err,
                });
            }
        }
    }

    Ok(RollOutcome {
        signature: last_signature,
        bucket_ids,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(product_type: ProductType) -> RollPlan {
        RollPlan {
            underlying_symbol: "TBTC".into(),
            settlement_symbol: "TUSDC".into(),
            underlying_mint: Pubkey::new_from_array([7u8; 32]),
            settlement_mint: Pubkey::new_from_array([9u8; 32]),
            expiry_ms: 1_760_000_000_000,
            strikes: vec![61_600, 65_450, 69_300],
            strike_scale: 2,
            product_type,
        }
    }

    #[test]
    fn bucket_pdas_are_deterministic_and_per_strike() {
        let p = plan(ProductType::Call);
        let a = p.bucket_pdas();
        let b = p.bucket_pdas();
        assert_eq!(a, b, "same plan ⇒ same PDAs");
        assert_eq!(a.len(), 3);
        assert_ne!(a[0], a[1]);
        assert_ne!(a[1], a[2]);
    }

    #[test]
    fn call_and_put_pdas_never_collide() {
        // Same pair/expiry/strikes: the product type separates both the
        // salt domain AND the PDA seed prefix.
        let call = plan(ProductType::Call).bucket_pdas();
        let put = plan(ProductType::Put).bucket_pdas();
        for (c, p) in call.iter().zip(&put) {
            assert_ne!(c, p);
        }
    }

    #[test]
    fn product_type_round_trips() {
        assert_eq!(ProductType::parse("call"), Some(ProductType::Call));
        assert_eq!(ProductType::parse("put"), Some(ProductType::Put));
        assert_eq!(ProductType::parse("swap"), None);
        assert_eq!(ProductType::default(), ProductType::Call);
    }

    // ── ErrorClass tests ────────────────────────────────────────────

    #[test]
    fn classify_blockhash_fetch_as_definitely_not_sent() {
        let err = anyhow!("fetching latest blockhash for create_call_bucket: connection refused");
        assert_eq!(classify_error(&err), ErrorClass::DefinitelyNotSent);
    }

    #[test]
    fn classify_preflight_reject_as_definitely_not_sent() {
        // Preflight simulation failures come back before broadcast.
        let err = anyhow!(
            "create_call_bucket failed: RPC response error -32002: \
             Transaction simulation failed: Error processing Instruction 0: \
             custom program error: 0x1772"
        );
        assert_eq!(classify_error(&err), ErrorClass::DefinitelyNotSent);
    }

    #[test]
    fn classify_expired_blockhash_as_definitely_not_sent() {
        let err = anyhow!("create_call_bucket failed: Blockhash not found");
        assert_eq!(classify_error(&err), ErrorClass::DefinitelyNotSent);
    }

    #[test]
    fn classify_confirm_timeout_as_ambiguous() {
        let err = anyhow!(
            "create_call_bucket failed: unable to confirm transaction. \
             This can happen in situations such as transaction expiration"
        );
        assert_eq!(classify_error(&err), ErrorClass::Ambiguous);
    }

    #[test]
    fn classify_unknown_error_as_ambiguous() {
        let err = anyhow!("something completely unexpected happened");
        assert_eq!(classify_error(&err), ErrorClass::Ambiguous);
    }

    #[test]
    fn already_in_use_is_detected_from_preflight_logs() {
        let err = anyhow!(
            "create_call_bucket failed: Transaction simulation failed: \
             Error processing Instruction 0: instruction error: \
             Allocate: account Address {{ address: 9xQeWv..., base: None }} already in use"
        );
        assert!(is_already_in_use(&err));
        // And it's inside the DefinitelyNotSent family, so even if a caller
        // skipped the collision check the row would be retried, not parked.
        assert_eq!(classify_error(&err), ErrorClass::DefinitelyNotSent);
        assert!(!is_already_in_use(&anyhow!("unable to confirm transaction")));
    }
}
