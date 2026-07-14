//! HTTP handlers: [`health`], [`balance`], [`sponsor`], [`faucet`].

use std::sync::Arc;

use axum::extract::{Json, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use tracing::{error, warn};

use crate::faucet;
use crate::sponsor::{self, SponsorError};
use crate::state::AppState;

type ApiError = (StatusCode, String);

pub async fn health() -> &'static str {
    "ok"
}

// -------------------------------------------------------------------- balance

#[derive(Serialize)]
pub struct BalanceResp {
    /// Station (fee payer) pubkey, base58. The frontend builds sponsored
    /// transactions with this as the fee payer.
    pub address: String,
    /// Balance in lamports (string — can exceed JS safe-integer range).
    pub balance_lamports: String,
    /// Balance in SOL, for display.
    pub balance_sol: f64,
    /// Threshold below which sponsoring is considered unhealthy (lamports).
    pub threshold_lamports: String,
    /// `false` when balance < threshold — the frontend defaults the
    /// sponsor toggle off.
    pub healthy: bool,
}

/// `GET /balance` — the station wallet's balance + a health flag.
pub async fn balance(State(s): State<Arc<AppState>>) -> Result<Json<BalanceResp>, ApiError> {
    let addr = s.solana.signer.pubkey();
    let bal = s
        .solana
        .client
        .get_balance(&addr)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    let threshold = s.policy.min_balance_threshold_lamports;
    Ok(Json(BalanceResp {
        address: addr.to_string(),
        balance_lamports: bal.to_string(),
        balance_sol: bal as f64 / 1e9,
        threshold_lamports: threshold.to_string(),
        healthy: bal >= threshold,
    }))
}

// -------------------------------------------------------------------- sponsor

#[derive(Deserialize)]
pub struct SponsorReq {
    /// base64 serialized `VersionedTransaction` (unsigned or user-signed),
    /// fee payer = the station pubkey from `/balance`.
    pub transaction: String,
}

#[derive(Serialize)]
pub struct SponsorResp {
    /// base64 serialized `VersionedTransaction` with the station signature
    /// applied — the wallet signs these exact bytes, then submit raw.
    pub transaction: String,
    /// Station pubkey, base58.
    pub sponsor_pubkey: String,
    /// base58 station signature over the message.
    pub sponsor_signature: String,
}

fn sponsor_status(e: &SponsorError) -> (StatusCode, &'static str) {
    match e {
        SponsorError::BadRequest(_) => (StatusCode::BAD_REQUEST, "error"),
        SponsorError::Policy(_) => (StatusCode::UNPROCESSABLE_ENTITY, "rejected"),
        SponsorError::LowBalance(_) => (StatusCode::SERVICE_UNAVAILABLE, "rejected"),
        SponsorError::Upstream(_) => (StatusCode::BAD_GATEWAY, "error"),
    }
}

/// `POST /sponsor` — validate, simulate, and co-sign a fee-payer tx.
pub async fn sponsor(
    State(s): State<Arc<AppState>>,
    Json(req): Json<SponsorReq>,
) -> Result<Json<SponsorResp>, ApiError> {
    let out = sponsor::sponsor_transaction(
        &s.solana.client,
        &s.solana.signer.keypair,
        &s.templates,
        &s.policy,
        &req.transaction,
    )
    .await
    .map_err(|e| {
        let (status, outcome) = sponsor_status(&e);
        warn!(error = %e, outcome, "sponsorship refused");
        metrics::counter!("solana_gas_station_sponsorships_total", "outcome" => outcome)
            .increment(1);
        (status, e.to_string())
    })?;

    metrics::counter!("solana_gas_station_sponsorships_total", "outcome" => "ok").increment(1);
    metrics::histogram!("solana_gas_station_sponsor_lamports").record(out.lamport_delta as f64);

    Ok(Json(SponsorResp {
        transaction: out.transaction_b64,
        sponsor_pubkey: s.solana.signer.pubkey().to_string(),
        sponsor_signature: out.sponsor_signature_b58,
    }))
}

// --------------------------------------------------------------------- faucet

#[derive(Deserialize)]
pub struct FaucetReq {
    /// Recipient wallet, base58. The station creates its ATA if missing.
    pub recipient: String,
    /// Test-token ticker (`TBTC`, `TUSDC`, …), case-insensitive.
    pub ticker: String,
}

#[derive(Serialize)]
pub struct FaucetResp {
    /// Confirmed transaction signature, base58.
    pub signature: String,
}

/// `POST /faucet` — mint the configured amount of a test token.
pub async fn faucet(
    State(s): State<Arc<AppState>>,
    Json(req): Json<FaucetReq>,
) -> Result<Json<FaucetResp>, ApiError> {
    if !s.faucet_enabled {
        return Err((
            StatusCode::FORBIDDEN,
            "faucet is disabled on this deployment".into(),
        ));
    }
    let ticker = req.ticker.trim().to_ascii_uppercase();
    let token = s.faucet_tokens.get(&ticker).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!(
                "unknown test token {ticker} (have: {:?})",
                s.faucet_tokens.keys().collect::<Vec<_>>()
            ),
        )
    })?;
    let amount = token.amount.ok_or_else(|| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("no faucet_amounts entry configured for {ticker}"),
        )
    })?;
    if !token.authority_ok {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            format!("station key is not the mint authority for {ticker}"),
        ));
    }
    let recipient: Pubkey = req.recipient.trim().parse().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            format!("recipient is not a base58 pubkey: {}", req.recipient),
        )
    })?;

    let station = s.solana.signer.pubkey();
    let ixs = faucet::faucet_ixs(&station, &recipient, &token.mint, amount)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let signature = s
        .solana
        .send_and_confirm(&ixs, &[], "faucet mint")
        .await
        .map_err(|e| {
            error!(
                alert_id = "tx-failed-solana-gas-station-faucet",
                ticker, recipient = %recipient, error = %e,
                "faucet mint failed"
            );
            (StatusCode::BAD_GATEWAY, e.to_string())
        })?;

    metrics::counter!("solana_gas_station_faucet_mints_total", "ticker" => ticker.clone())
        .increment(1);

    Ok(Json(FaucetResp {
        signature: signature.to_string(),
    }))
}
