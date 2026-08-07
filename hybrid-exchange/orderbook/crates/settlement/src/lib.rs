//! Settlement submitter (spec §5.6): consumes match intents, builds and
//! submits `settlement::match_orders` transactions, and decodes aborts into
//! targeted reactions (prune the over-committed maker, drop the dead order,
//! restore the counterparty, bounded retries on transport errors).
//!
//! v1 keeps one in-flight pipeline per market (single worker): every fill in
//! a market serializes on its shared registry anyway, and local ordering
//! gives deterministic failure attribution.

use base64::Engine;
use orderbook_book::MatchIntent;
use orderbook_core::{Digest, SuiAddress};
use orderbook_signing::keys::Ed25519Keypair;
use orderbook_store::StoredOrder;
use orderbook_suirpc::{parse_move_abort, SuiRpcClient};
use serde_json::{json, Value};
use std::time::Duration;

pub const CLOCK_OBJECT: &str = "0x0000000000000000000000000000000000000000000000000000000000000006";

/// Everything needed to settle one match on-chain. The service layer
/// resolves the intent's digests into stored signed orders; `ask` is always
/// the Base-selling order (`order_a` on-chain).
#[derive(Clone, Debug)]
pub struct MatchJob {
    pub intent: MatchIntent,
    pub ask: StoredOrder,
    pub bid: StoredOrder,
    /// Canonical type strings of the market pair.
    pub base_type: String,
    pub quote_type: String,
}

/// Decoded settlement outcome. The abort mapping mirrors the Move modules'
/// error constants — keep in sync with settlement.move / balance_manager.move
/// / registry.move.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettleOutcome {
    Confirmed { tx_digest: String },
    /// Order-level death: drop this digest, restore + re-match the other.
    OrderDead { digest: Digest, reason: DeadReason },
    /// Maker escrow can't cover: prune/downsize that maker's orders.
    InsufficientEscrow,
    /// Stale fill state (someone raced us): restore both, refresh, re-match.
    Stale,
    /// Transport/gas failure after bounded retries.
    Failed { error: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeadReason {
    Expired,
    Cancelled,
    SaltVoided,
    BadSignature,
}

// settlement.move abort codes
const E_EXPIRED: u64 = 3;
const E_SALT_VOIDED: u64 = 7;
const E_CANCELLED: u64 = 8;
const E_ALREADY_FILLED: u64 = 9;
const E_BAD_SIGNATURE: u64 = 10;
const E_NOT_CROSSING: u64 = 15;
const E_LIMIT_VIOLATED: u64 = 16;
// registry.move
const E_OVERFILL: u64 = 3;
// balance_manager.move
const E_INSUFFICIENT_ESCROW: u64 = 2;

pub struct SubmitterConfig {
    pub package: String,
    pub gas_budget: u64,
    pub max_retries: u32,
    pub retry_base_delay: Duration,
}

impl Default for SubmitterConfig {
    fn default() -> Self {
        SubmitterConfig {
            package: String::new(),
            gas_budget: 50_000_000,
            max_retries: 4,
            retry_base_delay: Duration::from_millis(250),
        }
    }
}

pub struct Submitter {
    rpc: SuiRpcClient,
    key: Ed25519Keypair,
    relayer: SuiAddress,
    config: SubmitterConfig,
}

/// JSON encoding of a byte vector for SuiJSON (array of numbers).
fn bytes_arg(bytes: &[u8]) -> Value {
    Value::Array(bytes.iter().map(|b| json!(*b)).collect())
}

impl Submitter {
    pub fn new(rpc: SuiRpcClient, key: Ed25519Keypair, config: SubmitterConfig) -> Self {
        let relayer = key.address();
        Submitter { rpc, key, relayer, config }
    }

    pub fn relayer_address(&self) -> SuiAddress {
        self.relayer
    }

    /// Settle one match intent: build (server-side), sign, execute, decode.
    pub async fn submit_match(&self, job: &MatchJob) -> SettleOutcome {
        let mut attempt = 0u32;
        loop {
            match self.try_submit(job).await {
                Ok(outcome) => return outcome,
                Err(e) => {
                    attempt += 1;
                    if attempt > self.config.max_retries {
                        return SettleOutcome::Failed { error: e.to_string() };
                    }
                    // jittered exponential backoff
                    let base = self.config.retry_base_delay * 2u32.pow(attempt - 1);
                    let jitter_ms = (attempt as u64 * 37) % 100;
                    tokio::time::sleep(base + Duration::from_millis(jitter_ms)).await;
                }
            }
        }
    }

    async fn try_submit(
        &self,
        job: &MatchJob,
    ) -> Result<SettleOutcome, orderbook_suirpc::RpcError> {
        let ask = &job.ask.signed;
        let bid = &job.bid.signed;
        let args = vec![
            json!(ask.registry_id.to_hex()),
            json!(ask.order.maker_manager_id.to_hex()),
            json!(bid.order.maker_manager_id.to_hex()),
            bytes_arg(&ask.order.to_bcs()),
            bytes_arg(&ask.prefixed_signature()),
            bytes_arg(&ask.public_key),
            bytes_arg(&bid.order.to_bcs()),
            bytes_arg(&bid.prefixed_signature()),
            bytes_arg(&bid.public_key),
            json!(job.intent.fill_base_amount.to_string()),
            json!(CLOCK_OBJECT),
        ];
        let tx_bytes_b64 = self
            .rpc
            .unsafe_move_call(
                &self.relayer.to_hex(),
                &self.config.package,
                "settlement",
                "match_orders",
                &[job.base_type.clone(), job.quote_type.clone()],
                args,
                self.config.gas_budget,
            )
            .await?;
        let tx_bytes = base64::engine::general_purpose::STANDARD
            .decode(&tx_bytes_b64)
            .map_err(|e| orderbook_suirpc::RpcError::Malformed(e.to_string()))?;
        let sig = base64::engine::general_purpose::STANDARD
            .encode(self.key.sign_transaction_data(&tx_bytes));
        let result = self.rpc.execute_tx(&tx_bytes_b64, &[sig]).await?;
        if result.success {
            return Ok(SettleOutcome::Confirmed { tx_digest: result.tx_digest });
        }
        let error = result.error.unwrap_or_else(|| "unknown".into());
        Ok(decode_failure(&error, job))
    }
}

/// Map an on-chain failure to the submitter's reaction (spec §5.6).
///
/// For per-order aborts (expiry, cancel, salt) the failing order is not
/// identified by the abort itself; the caller re-validates both orders
/// against local state. We attribute by inspecting the orders' own fields
/// where possible, defaulting to `Stale` when ambiguous.
pub fn decode_failure(error: &str, job: &MatchJob) -> SettleOutcome {
    let Some(abort) = parse_move_abort(error) else {
        return SettleOutcome::Failed { error: error.to_string() };
    };
    match (abort.module.as_str(), abort.code) {
        ("balance_manager", E_INSUFFICIENT_ESCROW) => SettleOutcome::InsufficientEscrow,
        ("registry", E_OVERFILL) => SettleOutcome::Stale,
        ("settlement", E_ALREADY_FILLED) => SettleOutcome::Stale,
        ("settlement", E_NOT_CROSSING) | ("settlement", E_LIMIT_VIOLATED) => SettleOutcome::Stale,
        ("settlement", E_EXPIRED) => {
            // earlier-expiring order is the dead one
            let dead = if job.ask.signed.order.expiry_ms <= job.bid.signed.order.expiry_ms {
                job.ask.digest
            } else {
                job.bid.digest
            };
            SettleOutcome::OrderDead { digest: dead, reason: DeadReason::Expired }
        }
        ("settlement", E_CANCELLED) => SettleOutcome::OrderDead {
            // ask is validated first on-chain; caller re-checks both anyway
            digest: job.ask.digest,
            reason: DeadReason::Cancelled,
        },
        ("settlement", E_SALT_VOIDED) => SettleOutcome::OrderDead {
            digest: job.ask.digest,
            reason: DeadReason::SaltVoided,
        },
        ("settlement", E_BAD_SIGNATURE) => SettleOutcome::OrderDead {
            digest: job.ask.digest,
            reason: DeadReason::BadSignature,
        },
        _ => SettleOutcome::Failed { error: error.to_string() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orderbook_core::order::{Order, SignatureScheme, SignedOrder};
    use orderbook_core::Side;

    fn stored(expiry: u64) -> StoredOrder {
        let order = Order {
            maker_token: "B".into(),
            taker_token: "Q".into(),
            maker_amount: 10,
            taker_amount: 20,
            max_fee_bps: 10,
            maker: SuiAddress::ZERO,
            maker_manager_id: SuiAddress::ZERO,
            taker: SuiAddress::ZERO,
            sender: SuiAddress::ZERO,
            expiry_ms: expiry,
            salt: 1,
        };
        StoredOrder {
            digest: Digest([expiry as u8; 32]),
            signed: SignedOrder {
                order,
                registry_id: SuiAddress::ZERO,
                scheme: SignatureScheme::Ed25519,
                signature: vec![0; 64],
                public_key: vec![0; 32],
            },
            side: Side::Ask,
            price_ticks: 1,
            filled_taker: 0,
            status: "OPEN".into(),
        }
    }

    fn job() -> MatchJob {
        MatchJob {
            intent: MatchIntent {
                market: SuiAddress::ZERO,
                ask_digest: Digest([1; 32]),
                bid_digest: Digest([2; 32]),
                fill_base_amount: 10,
                exec_price_ticks: 1,
            },
            ask: stored(1),
            bid: stored(2),
            base_type: "B".into(),
            quote_type: "Q".into(),
        }
    }

    fn abort(module: &str, code: u64) -> String {
        format!(
            "MoveAbort(MoveLocation {{ module: ModuleId {{ address: x, name: Identifier(\"{module}\") }}, function: 1, instruction: 1, function_name: Some(\"f\") }}, {code}) in command 0"
        )
    }

    #[test]
    fn decode_targeted_reactions() {
        let j = job();
        assert_eq!(
            decode_failure(&abort("balance_manager", 2), &j),
            SettleOutcome::InsufficientEscrow
        );
        assert_eq!(decode_failure(&abort("settlement", 9), &j), SettleOutcome::Stale);
        assert_eq!(decode_failure(&abort("registry", 3), &j), SettleOutcome::Stale);
        // expiry attributes the earlier-expiring order (the ask, expiry 1)
        assert_eq!(
            decode_failure(&abort("settlement", 3), &j),
            SettleOutcome::OrderDead { digest: Digest([1; 32]), reason: DeadReason::Expired }
        );
        assert!(matches!(
            decode_failure("InsufficientGas", &j),
            SettleOutcome::Failed { .. }
        ));
    }
}
