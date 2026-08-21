//! Wire types for the Dakota platform API.
//!
//! Shapes here were confirmed against the live sandbox (see
//! `docs/dakota-sandbox-notes.md`), not just read off the docs — several
//! documented shapes are wrong. Where the two disagree the live behaviour wins,
//! and the difference is called out in a comment.
//!
//! Response structs are deliberately partial: we deserialize only what we act
//! on. Dakota bodies carry PII (`email`, `account_holder_name`,
//! `sender_account_number`) and anything named here is a field we could
//! accidentally persist, so the smaller this file is, the safer it is. Handlers
//! that need to relay a full body to the browser pass `serde_json::Value`
//! through without ever binding it to a struct.

use serde::{Deserialize, Serialize};

// ----------------------------------------------------------------- customers

#[derive(Debug, Clone, Serialize)]
pub struct CreateCustomerReq {
    pub name: String,
    pub customer_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    /// Mutually exclusive with `sub_client_id`, and immutable after creation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_sub_client: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub_client_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateCustomerResp {
    pub id: String,
    pub application_id: String,
    /// Hosted onboarding form with an embedded token. This is the whole
    /// no-PII strategy: we hand the customer here and never see what they type.
    pub application_url: String,
    /// NANOseconds since epoch, unlike every other timestamp in this API.
    #[serde(default)]
    pub application_expires_at: Option<i64>,
}

/// A customer as Dakota returns it. `name` is present in the real payload and
/// deliberately absent here — it is PII, it is never stored, and handlers that
/// display it relay the raw body instead.
#[derive(Debug, Clone, Deserialize)]
pub struct CustomerStatus {
    pub id: String,
    pub customer_type: String,
    #[serde(default)]
    pub is_sub_client: bool,
    #[serde(default)]
    pub sub_client_id: Option<String>,
    #[serde(default)]
    pub external_id: Option<String>,
    #[serde(default)]
    pub kyb_status: Option<String>,
    #[serde(default)]
    pub kyc_status: Option<String>,
    #[serde(default)]
    pub application_id: Option<String>,
    #[serde(default)]
    pub application_status: Option<String>,
}

impl CustomerStatus {
    /// Whether Dakota will let this customer open a ramp account.
    ///
    /// `kyb_status == "active"` is the real gate, for individuals as much as
    /// businesses — `POST /accounts` fails with "Customer is not KYB-approved
    /// by Dakota" otherwise, and an individual sits at `kyc_status: "pending"`
    /// even once approved. Checking `kyc_status` here would reject every
    /// perfectly good individual.
    pub fn can_transact(&self) -> bool {
        self.kyb_status.as_deref() == Some("active")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Paginated<T> {
    #[serde(default = "Vec::new")]
    pub data: Vec<T>,
    #[serde(default)]
    pub meta: Option<PageMeta>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PageMeta {
    #[serde(default)]
    pub has_more_after: bool,
    #[serde(default)]
    pub has_more_before: bool,
    #[serde(default)]
    pub total_count: Option<i64>,
}

// ---------------------------------------------------- recipients + destinations

#[derive(Debug, Clone, Serialize)]
pub struct CreateRecipientReq {
    pub name: String,
    /// Optional for crypto-only recipients; Dakota requires it before any
    /// fiat destination can be attached, so an offramp needs it up front.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CreatedId {
    pub id: String,
}

// ------------------------------------------------------------------ accounts

#[derive(Debug, Clone, Serialize)]
pub struct CreateAccountReq {
    pub account_type: String,
    /// Required for onramps. Undocumented as required — Dakota 400s with
    /// "capabilities are required" when it is missing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crypto_destination_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fiat_destination_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination_network_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_network_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_asset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination_asset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub developer_fee_bps: Option<i32>,
}

/// Only the fields we index on. The full response also carries `bank_account`
/// (routing + account number + holder name) — pure PII, relayed to the browser
/// and never bound here.
#[derive(Debug, Clone, Deserialize)]
pub struct AccountSummary {
    pub id: String,
    pub account_type: String,
    #[serde(default)]
    pub source_asset: Option<String>,
    #[serde(default)]
    pub destination_asset: Option<String>,
    #[serde(default)]
    pub source_network_id: Option<String>,
    #[serde(default)]
    pub rail: Option<String>,
    /// Deposit address for offramps and swaps.
    #[serde(default)]
    pub source_crypto_address: Option<String>,
}

// -------------------------------------------------------------- transactions

/// Fee and rate breakdown Dakota attaches to each transaction. This is the
/// only place real rates are observable — there is no pricing endpoint
/// available to our client tier.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Receipt {
    #[serde(default)]
    pub input: Option<Amount>,
    #[serde(default)]
    pub output: Option<Amount>,
    #[serde(default)]
    pub exchange_rate: Option<String>,
    #[serde(default)]
    pub dakota_fee: Option<Amount>,
    #[serde(default)]
    pub client_fee: Option<Amount>,
    #[serde(default)]
    pub external_fee: Option<Amount>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Amount {
    #[serde(default)]
    pub amount: Option<String>,
    #[serde(default)]
    pub asset: Option<String>,
}

/// An auto-account transaction. `sender_details` exists on the wire and is
/// omitted here on purpose — it holds the sender's name and bank account.
#[derive(Debug, Clone, Deserialize)]
pub struct AutoTransaction {
    pub id: String,
    #[serde(default)]
    pub auto_account_id: Option<String>,
    #[serde(default)]
    pub destination_id: Option<String>,
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub receipt: Option<Receipt>,
    #[serde(default)]
    pub failure_reason: Option<String>,
    #[serde(default)]
    pub created_at: Option<i64>,
    #[serde(default)]
    pub updated_at: Option<i64>,
}

// ------------------------------------------------------------------- sandbox

#[derive(Debug, Clone, Serialize)]
pub struct SimulateInboundReq {
    pub simulation_id: String,
    /// `ach_inbound` | `fedwire_inbound` | `fednow_inbound` | `crypto_inbound`
    /// and the outbound/reversal variants.
    pub r#type: String,
    pub amount: String,
    pub currency: String,
    /// Required for the fiat inbound types.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Required for `crypto_inbound`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wallet_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scenario: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SimulateOnboardingReq {
    /// `kyb_approve` drives the state machine for individuals as well as
    /// businesses; `kyc_approve` on a fresh individual is a no-op.
    pub r#type: String,
    /// The **application** id, not the customer id — the docs' example is wrong.
    pub applicant_id: String,
    pub simulation_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SimulationResp {
    #[serde(default)]
    pub simulation_id: Option<String>,
    #[serde(default)]
    pub previous_state: Option<String>,
    #[serde(default)]
    pub new_state: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
}

// ------------------------------------------------------------------ webhooks

#[derive(Debug, Clone, Serialize)]
pub struct CreateWebhookTargetReq {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_types: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kyb_active_is_the_transact_gate_even_for_individuals() {
        // Captured from the sandbox after `kyb_approve` on an individual: the
        // customer transacts fine while kyc_status is still "pending".
        let raw = r#"{"id":"c1","customer_type":"individual","kyb_status":"active",
                      "kyc_status":"pending","application_status":"approved"}"#;
        let c: CustomerStatus = serde_json::from_str(raw).unwrap();
        assert!(c.can_transact(), "gating on kyc_status would reject this customer");
    }

    #[test]
    fn pending_customer_cannot_transact() {
        let raw = r#"{"id":"c1","customer_type":"individual","kyb_status":"pending"}"#;
        let c: CustomerStatus = serde_json::from_str(raw).unwrap();
        assert!(!c.can_transact());
    }

    #[test]
    fn customer_parses_without_optional_fields() {
        let c: CustomerStatus =
            serde_json::from_str(r#"{"id":"c1","customer_type":"business"}"#).unwrap();
        assert!(!c.is_sub_client);
        assert!(!c.can_transact());
    }

    #[test]
    fn create_customer_omits_unset_discriminators() {
        // `is_sub_client` and `sub_client_id` are mutually exclusive; sending
        // either as null would be a 400.
        let body = serde_json::to_value(CreateCustomerReq {
            name: "Acme".into(),
            customer_type: "business".into(),
            external_id: None,
            is_sub_client: Some(true),
            sub_client_id: None,
        })
        .unwrap();
        assert_eq!(body.get("is_sub_client").and_then(|v| v.as_bool()), Some(true));
        assert!(body.get("sub_client_id").is_none());
        assert!(body.get("external_id").is_none());
    }

    #[test]
    fn paginated_tolerates_a_missing_data_array() {
        let p: Paginated<CustomerStatus> = serde_json::from_str(r#"{"meta":{}}"#).unwrap();
        assert!(p.data.is_empty());
    }

    #[test]
    fn auto_transaction_parses_the_real_sandbox_payload() {
        // Trimmed from a live onramp; sender_details intentionally not bound.
        let raw = r#"{"auto_account_id":"3HNCN914HGh2Sr95XpcJBgMPLAT","status":"processing",
            "id":"3HNCPYPeEZCpScXWQbvFJwUeIxB","type":"onramp","failure_reason":"",
            "receipt":{"exchange_rate":"1","input":{"amount":"2","asset":"USD"},
                       "output":{"amount":"2","asset":"USDC"},
                       "dakota_fee":{"amount":"0","asset":"USD"}},
            "created_at":1785699116,"updated_at":1785699116}"#;
        let t: AutoTransaction = serde_json::from_str(raw).unwrap();
        assert_eq!(t.status.as_deref(), Some("processing"));
        let r = t.receipt.unwrap();
        assert_eq!(r.exchange_rate.as_deref(), Some("1"));
        assert_eq!(r.output.unwrap().asset.as_deref(), Some("USDC"));
    }
}
