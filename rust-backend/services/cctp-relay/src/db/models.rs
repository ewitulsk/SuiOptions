//! Diesel row types + status constants.

use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use diesel::prelude::*;

use super::schema::cctp_transfers;

/// Transfer status state machine:
/// `pending_attestation → attested → minting → complete`, terminal `failed`.
pub mod status {
    pub const PENDING_ATTESTATION: &str = "pending_attestation";
    pub const ATTESTED: &str = "attested";
    pub const MINTING: &str = "minting";
    pub const COMPLETE: &str = "complete";
    pub const FAILED: &str = "failed";
}

pub mod chain {
    pub const SUI: &str = "sui";
    pub const SOLANA: &str = "solana";
}

#[derive(Queryable, Identifiable, Debug, Clone)]
#[diesel(table_name = cctp_transfers)]
pub struct TransferRow {
    pub id: i64,
    pub origin_chain: String,
    pub origin_tx_hash: String,
    pub origin_wallet: String,
    pub destination_wallet: Option<String>,
    pub mint_recipient: Option<String>,
    pub amount: Option<BigDecimal>,
    pub status: String,
    pub message_hex: Option<String>,
    pub attestation_hex: Option<String>,
    pub mint_tx_hash: Option<String>,
    pub error: Option<String>,
    pub attempts: i32,
    pub burned_at: Option<DateTime<Utc>>,
    pub attested_at: Option<DateTime<Utc>>,
    pub minted_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TransferRow {
    /// Destination chain is always the other side of the pair.
    pub fn destination_chain(&self) -> &'static str {
        if self.origin_chain == chain::SUI {
            chain::SOLANA
        } else {
            chain::SUI
        }
    }
}

#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = cctp_transfers)]
pub struct NewTransfer {
    pub origin_chain: String,
    pub origin_tx_hash: String,
    pub origin_wallet: String,
    pub destination_wallet: Option<String>,
    pub status: String,
}
