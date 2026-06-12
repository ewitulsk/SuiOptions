//! HTTP handlers: [`health`], [`balance`], [`sponsor`].

use std::str::FromStr;
use std::sync::Arc;

use axum::extract::{Json, State};
use axum::http::StatusCode;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sui_tx::tx::sponsor;
use sui_types::base_types::SuiAddress;
use tracing::warn;

use crate::state::AppState;

type ApiError = (StatusCode, String);

pub async fn health() -> &'static str {
    "ok"
}

// -------------------------------------------------------------------- balance

#[derive(Serialize)]
pub struct BalanceResp {
    /// Sponsor (gas payer) address.
    pub address: String,
    /// Balance in MIST (string — can exceed JS safe-integer range).
    pub balance_mist: String,
    /// Balance in SUI, for display.
    pub balance_sui: f64,
    /// Threshold below which sponsoring is considered unhealthy (MIST).
    pub threshold_mist: String,
    /// `false` when balance < threshold — the frontend defaults the toggle off.
    pub healthy: bool,
}

/// `GET /balance` — the sponsor wallet's balance + a health flag.
pub async fn balance(State(s): State<Arc<AppState>>) -> Result<Json<BalanceResp>, ApiError> {
    let addr = s.sui.signer.address;
    let bal = sponsor::sponsor_balance(&s.sui.client, addr)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    let healthy = bal >= s.min_balance_threshold_mist as u128;
    Ok(Json(BalanceResp {
        address: addr.to_string(),
        balance_mist: bal.to_string(),
        balance_sui: bal as f64 / 1e9,
        threshold_mist: s.min_balance_threshold_mist.to_string(),
        healthy,
    }))
}

// -------------------------------------------------------------------- sponsor

#[derive(Deserialize)]
pub struct SponsorReq {
    /// User's address (the transaction sender).
    pub sender: String,
    /// base64(bcs(TransactionKind)) — dapp-kit
    /// `tx.build({ onlyTransactionKind: true })`.
    pub kind_bytes: String,
}

#[derive(Serialize)]
pub struct SponsorResp {
    /// base64(bcs(TransactionData)) — submit as `transactionBlock`; the wallet
    /// co-signs these exact bytes.
    pub tx_bytes: String,
    /// base64 sponsor signature over `tx_bytes`.
    pub sponsor_signature: String,
    pub gas_budget: String,
    pub gas_price: String,
}

/// `POST /sponsor` — attach sponsor gas, dry-run for budget, and sign.
pub async fn sponsor(
    State(s): State<Arc<AppState>>,
    Json(req): Json<SponsorReq>,
) -> Result<Json<SponsorResp>, ApiError> {
    let sender = SuiAddress::from_str(req.sender.trim()).map_err(|e| {
        metrics::counter!("gas_station_sponsorships_total", "outcome" => "error").increment(1);
        (StatusCode::BAD_REQUEST, format!("invalid sender: {e}"))
    })?;
    let kind_bytes = base64::engine::general_purpose::STANDARD
        .decode(req.kind_bytes.trim())
        .map_err(|_| {
            metrics::counter!("gas_station_sponsorships_total", "outcome" => "error").increment(1);
            (StatusCode::BAD_REQUEST, "kind_bytes is not base64".into())
        })?;

    let out = sponsor::sponsor_transaction(
        &s.sui.client,
        &s.sui.signer,
        &s.templates,
        &s.policy,
        sender,
        &kind_bytes,
    )
    .await
    .map_err(|e| {
        warn!(%sender, error = %e, "sponsorship refused");
        metrics::counter!("gas_station_sponsorships_total", "outcome" => "rejected").increment(1);
        (StatusCode::UNPROCESSABLE_ENTITY, e.to_string())
    })?;

    metrics::counter!("gas_station_sponsorships_total", "outcome" => "ok").increment(1);
    metrics::histogram!("gas_station_sponsor_gas_budget").record(out.gas_budget as f64);

    Ok(Json(SponsorResp {
        tx_bytes: out.tx_bytes_b64,
        sponsor_signature: out.sponsor_signature_b64,
        gas_budget: out.gas_budget.to_string(),
        gas_price: out.gas_price.to_string(),
    }))
}
