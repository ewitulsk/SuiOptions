//! Treasury: the admin's non-custodial Dakota wallet.
//!
//! Setup is a five-call chain that has to run in order, because each step
//! references the last:
//!
//! ```text
//! POST /signers        (our P-256 public key)
//!   └─▶ POST /signer-groups   (member_keys = the PUBLIC KEY, not the signer id)
//!         └─▶ POST /policies
//!               └─▶ POST /wallets   (signer_groups + policies)
//! ```
//!
//! Sending is an endorsed request: we sign a canonical intent with the private
//! half. See [`crate::wallet`] for why the canonicalization is exact.
//!
//! Admin-only throughout. This is our own treasury, not a per-customer wallet.

use std::sync::Arc;

use auth_client::VerifiedClaims;
use axum::extract::{Path, State};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use tracing::{error, info};
use uuid::Uuid;

use super::{bad_request, internal, ApiError};
use crate::authz::Caller;
use crate::db::models::{NewWallet, Wallet};
use crate::state::AppState;
use crate::wallet::{normalize_amount, SendTransactionIntent, TransferOperation};

/// Dakota addresses chains by CAIP-2, but its network ids are its own strings,
/// and nothing in the API converts between them. Wrong chain id means the
/// transfer either fails or — worse — targets a chain the operator did not
/// mean, so this map is explicit rather than derived.
fn caip2_for(network_id: &str) -> Option<&'static str> {
    Some(match network_id {
        "ethereum-sepolia" => "eip155:11155111",
        "base-sepolia" => "eip155:84532",
        "arbitrum-sepolia" => "eip155:421614",
        "optimism-sepolia" => "eip155:11155420",
        "polygon-amoy" => "eip155:80002",
        "solana-devnet" => "solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1",
        _ => return None,
    })
}

fn signer(state: &Arc<AppState>) -> Result<&crate::wallet::WalletSigner, ApiError> {
    state.wallet_signer.as_ref().ok_or_else(|| {
        bad_request(
            "no treasury key configured — set dakota.wallet_p256_pem in the secrets file",
        )
    })
}

// -------------------------------------------------------------------- setup

#[derive(Deserialize)]
pub struct SetupBody {
    #[serde(default = "default_label")]
    pub label: String,
    /// `evm` | `solana`. One wallet per family; an EVM wallet's address is
    /// shared across every EVM chain.
    #[serde(default = "default_family")]
    pub family: String,
}

fn default_label() -> String {
    "treasury".to_string()
}
fn default_family() -> String {
    "evm".to_string()
}

#[derive(Serialize)]
pub struct SetupResult {
    pub wallet: Wallet,
    pub signer_id: String,
}

/// `POST /admin/treasury/setup` — run the whole chain and record the result.
///
/// Not idempotent on Dakota's side: calling it twice creates a second wallet.
/// It is an explicit admin action for exactly that reason.
pub async fn setup(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<VerifiedClaims>,
    Json(body): Json<SetupBody>,
) -> Result<Json<SetupResult>, ApiError> {
    Caller::from_claims(&claims)?.require_admin()?;
    let signer = signer(&state)?;
    let public_key = signer.public_key_b64().map_err(internal)?;

    #[derive(Deserialize)]
    struct Created {
        id: String,
    }

    let registered: Created = state
        .dakota
        .post(
            "POST /signers",
            "/signers",
            &serde_json::json!({
                "name": format!("{}-signer", body.label),
                "public_key": public_key,
                "key_type": "ES256",
            }),
        )
        .await
        .map_err(|e| (e.client_status(), e.to_string()))?;

    // `member_keys` takes public keys, not signer ids — passing the id here is
    // accepted and produces a group that can never authorize anything.
    let group: Created = state
        .dakota
        .post(
            "POST /signer-groups",
            "/signer-groups",
            &serde_json::json!({
                "name": format!("{}-group", body.label),
                "member_keys": [public_key],
            }),
        )
        .await
        .map_err(|e| (e.client_status(), e.to_string()))?;

    let policy: Created = state
        .dakota
        .post(
            "POST /policies",
            "/policies",
            &serde_json::json!({
                "name": format!("{}-policy", body.label),
                "description": "single-approval treasury policy",
                "signer_group_id": group.id,
                // One approval: we hold exactly one key. Raising this without
                // registering more signers would lock the wallet.
                "rules": [{
                    "rule_type": "approval_threshold",
                    "action": "allow",
                    "definition": { "threshold": 1 }
                }],
            }),
        )
        .await
        .map_err(|e| (e.client_status(), e.to_string()))?;

    #[derive(Deserialize)]
    struct CreatedWallet {
        id: String,
        #[serde(default)]
        address: Option<String>,
    }

    let created: CreatedWallet = state
        .dakota
        .post(
            "POST /wallets",
            "/wallets",
            &serde_json::json!({
                "name": body.label,
                "family": body.family,
                "signer_groups": [group.id],
                "policies": [policy.id],
            }),
        )
        .await
        .map_err(|e| (e.client_status(), e.to_string()))?;

    let wallet = state
        .repo
        .insert_wallet(&NewWallet {
            dakota_wallet_id: created.id.clone(),
            address: created.address.clone(),
            family: body.family,
            signer_group_id: Some(group.id),
            policy_id: Some(policy.id),
            label: Some(body.label),
        })
        .map_err(internal)?;

    info!(
        wallet_id = %created.id,
        address = created.address.as_deref().unwrap_or("-"),
        "treasury wallet created"
    );
    Ok(Json(SetupResult { wallet, signer_id: registered.id }))
}

/// `GET /admin/treasury` — wallets we know about, each with live balances.
pub async fn list(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<VerifiedClaims>,
) -> Result<Json<serde_json::Value>, ApiError> {
    Caller::from_claims(&claims)?.require_admin()?;

    let wallets = state.repo.list_wallets().map_err(internal)?;
    let mut out = Vec::with_capacity(wallets.len());
    for w in wallets {
        // Best-effort per wallet: one unreachable balance should not blank the
        // whole treasury page.
        let balances = state
            .dakota
            .get::<serde_json::Value>(
                "GET /wallets/{id}/balances",
                &format!("/wallets/{}/balances", w.dakota_wallet_id),
            )
            .await
            .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }));
        out.push(serde_json::json!({ "wallet": w, "balances": balances }));
    }
    Ok(Json(serde_json::json!({ "treasury": out })))
}

/// `GET /admin/treasury/:id/balances`
pub async fn balances(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<VerifiedClaims>,
    Path(wallet_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    Caller::from_claims(&claims)?.require_admin()?;
    state
        .dakota
        .get(
            "GET /wallets/{id}/balances",
            &format!("/wallets/{wallet_id}/balances"),
        )
        .await
        .map(Json)
        .map_err(|e| (e.client_status(), e.to_string()))
}

// --------------------------------------------------------------------- send

#[derive(Deserialize)]
pub struct SendBody {
    pub to: String,
    /// Decimal string, e.g. "1.50".
    pub amount: String,
    pub asset_id: String,
    /// Our network id; converted to CAIP-2 before signing.
    pub network_id: String,
}

/// `POST /admin/treasury/:id/send` — sign and submit a transfer.
pub async fn send(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<VerifiedClaims>,
    Path(wallet_id): Path<String>,
    Json(body): Json<SendBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    Caller::from_claims(&claims)?.require_admin()?;
    let signer = signer(&state)?;

    let wallet = state
        .repo
        .list_wallets()
        .map_err(internal)?
        .into_iter()
        .find(|w| w.dakota_wallet_id == wallet_id)
        .ok_or_else(|| bad_request("unknown treasury wallet"))?;
    let from = wallet
        .address
        .clone()
        .ok_or_else(|| bad_request("treasury wallet has no recorded address"))?;

    if !state.cfg.network_allowed(&body.network_id) {
        return Err(bad_request(format!(
            "network {} is not permitted in this environment",
            body.network_id
        )));
    }
    let caip2 = caip2_for(&body.network_id)
        .ok_or_else(|| bad_request(format!("no CAIP-2 mapping for {}", body.network_id)))?;

    let minor = parse_minor(&body.amount)
        .ok_or_else(|| bad_request(format!("amount {:?} is not a decimal string", body.amount)))?;
    if minor > state.cfg.dakota.max_amount_minor {
        return Err(bad_request(format!(
            "amount {} exceeds the configured cap of {}.{:02}",
            body.amount,
            state.cfg.dakota.max_amount_minor / 100,
            state.cfg.dakota.max_amount_minor % 100
        )));
    }

    let intent = SendTransactionIntent {
        wallet_id: wallet_id.clone(),
        caip2: caip2.to_string(),
        operation: TransferOperation {
            kind: "transfer".into(),
            from,
            to: body.to.clone(),
            // Must be Dakota's normalized form or the signature will not
            // verify — see `wallet::normalize_amount`.
            amount: normalize_amount(&body.amount),
            asset_id: body.asset_id.clone(),
        },
        idempotency_key: Uuid::new_v4().to_string(),
    };
    let endorsed = signer.endorse(intent).map_err(internal)?;

    // Post the envelope as a `Value`, not as the struct.
    //
    // `serde_json::Value` orders its keys, so this transmits the whole request
    // in canonical form. Sending the struct instead puts `signatures` before
    // `intent` (declaration order) and Dakota answers `endorsement validation
    // failed` — verified against the live sandbox, where the identical intent
    // and key succeed one way and fail the other.
    let envelope = serde_json::to_value(&endorsed).map_err(internal)?;
    tracing::debug!(
        wire = %serde_json::to_string(&envelope).unwrap_or_default(),
        "endorsed request"
    );

    match state
        .dakota
        .post::<_, serde_json::Value>(
            "POST /wallets/{id}/transactions",
            &format!("/wallets/{wallet_id}/transactions"),
            &envelope,
        )
        .await
    {
        Ok(resp) => {
            info!(
                %wallet_id,
                to = %body.to,
                amount = %body.amount,
                asset = %body.asset_id,
                "treasury transfer submitted"
            );
            Ok(Json(resp))
        }
        Err(e) => {
            // Per docs/tx-alerting.md, a submission failure alerts at the
            // service handler rather than inside the client. Insufficient
            // balance and policy rejection are expected outcomes of a
            // human-driven action, not incidents — only a genuine submission
            // failure pages.
            let detail = e.to_string();
            let benign = detail.contains("insufficient")
                || detail.contains("policy")
                || e.client_status() == axum::http::StatusCode::BAD_REQUEST;
            if !benign {
                error!(
                    alert_id = "tx-failed-dakota-wallet",
                    %wallet_id,
                    asset = %body.asset_id,
                    amount = %body.amount,
                    dakota_request_id = e.request_id().unwrap_or("-"),
                    error = %detail,
                    "treasury transfer submission failed"
                );
            }
            Err((e.client_status(), detail))
        }
    }
}

/// Decimal string -> minor units. Rejects negatives: a transfer is not a
/// refund, and a negative would sail under the cap check.
fn parse_minor(s: &str) -> Option<i64> {
    let s = s.trim();
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    if whole.is_empty() || !whole.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if !frac.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let units: i64 = whole.parse().ok()?;
    let cents: i64 = frac
        .chars()
        .chain(std::iter::repeat('0'))
        .take(2)
        .collect::<String>()
        .parse()
        .ok()?;
    units.checked_mul(100)?.checked_add(cents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caip2_covers_every_sandbox_network() {
        // These are exactly the networks config.staging.toml allows; a missing
        // entry here would make a permitted network un-sendable.
        for n in [
            "ethereum-sepolia",
            "base-sepolia",
            "arbitrum-sepolia",
            "optimism-sepolia",
            "polygon-amoy",
            "solana-devnet",
        ] {
            assert!(caip2_for(n).is_some(), "no CAIP-2 mapping for {n}");
        }
    }

    #[test]
    fn caip2_values_are_the_real_chain_ids() {
        assert_eq!(caip2_for("base-sepolia"), Some("eip155:84532"));
        assert_eq!(caip2_for("ethereum-sepolia"), Some("eip155:11155111"));
        assert_eq!(caip2_for("polygon-amoy"), Some("eip155:80002"));
    }

    #[test]
    fn unknown_network_has_no_mapping() {
        // Better to refuse than to guess a chain id and send somewhere real.
        assert_eq!(caip2_for("ethereum-mainnet"), None);
        assert_eq!(caip2_for("nonsense"), None);
    }

    #[test]
    fn parse_minor_rejects_negatives_and_garbage() {
        assert_eq!(parse_minor("1.50"), Some(150));
        assert_eq!(parse_minor("2"), Some(200));
        assert_eq!(parse_minor("-1.00"), None);
        assert_eq!(parse_minor("abc"), None);
        assert_eq!(parse_minor(""), None);
    }
}
