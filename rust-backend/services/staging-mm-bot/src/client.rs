//! Thin REST client for the orderbook service (`/v1/*`).
//!
//! Only what the maker loop needs: market discovery, order placement, soft
//! cancel, and mirrored escrow balances. Placement rejections come back as
//! typed intake codes so the quoter can react (INSUFFICIENT_ESCROW triggers
//! a watermark reset, everything else is a bug in our ladder math).

use anyhow::{anyhow, Context, Result};
use exchange_types::order::SignedOrder;
use exchange_types::{Digest, Market, SuiAddress};
use serde::Deserialize;

pub struct OrderbookClient {
    base: String,
    http: reqwest::Client,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketsResponse {
    pub package_id: String,
    pub markets: Vec<Market>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaceResponse {
    pub digest: String,
    pub status: String,
    pub matches: u64,
}

/// An intake rejection (`422`) with its stable code, e.g.
/// `INSUFFICIENT_ESCROW`, `OFF_TICK`, `SALT_NOT_MONOTONIC`.
#[derive(Debug, thiserror::Error)]
#[error("orderbook rejected order: {code}: {detail}")]
pub struct IntakeReject {
    pub code: String,
    pub detail: String,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    error: ErrorInner,
}

#[derive(Debug, Deserialize)]
struct ErrorInner {
    code: String,
    detail: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BalanceEntry {
    /// Canonical coin type string.
    pub token: String,
    /// Raw units, decimal string on the wire.
    pub amount: String,
}

impl BalanceEntry {
    pub fn amount_raw(&self) -> u64 {
        self.amount.parse().unwrap_or(0)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BalanceResponse {
    balances: Vec<BalanceEntry>,
}

impl OrderbookClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            base: base_url.trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
        }
    }

    pub async fn markets(&self) -> Result<MarketsResponse> {
        let url = format!("{}/v1/markets", self.base);
        let resp = self.http.get(&url).send().await.context("GET /v1/markets")?;
        let resp = resp.error_for_status().context("GET /v1/markets status")?;
        resp.json().await.context("decoding /v1/markets")
    }

    /// Place a signed order. `Ok(Err(reject))` is an intake rejection (the
    /// request was understood and refused); `Err(_)` is transport/serving
    /// failure.
    pub async fn place_order(
        &self,
        signed: &SignedOrder,
    ) -> Result<std::result::Result<PlaceResponse, IntakeReject>> {
        let url = format!("{}/v1/orders", self.base);
        let resp = self
            .http
            .post(&url)
            .json(signed)
            .send()
            .await
            .context("POST /v1/orders")?;
        let status = resp.status();
        if status.is_success() {
            return Ok(Ok(resp.json().await.context("decoding place response")?));
        }
        let body: ErrorBody = resp
            .json()
            .await
            .with_context(|| format!("decoding error body (status {status})"))?;
        Ok(Err(IntakeReject { code: body.error.code, detail: body.error.detail }))
    }

    /// Soft cancel. Best-effort by design: the order stays fillable on-chain
    /// until the salt watermark passes it, so failures here are logged, not
    /// fatal.
    pub async fn cancel_order(
        &self,
        digest: &Digest,
        signature_b64: &str,
        public_key_b64: &str,
    ) -> Result<()> {
        let url = format!("{}/v1/orders/{}", self.base, digest.to_hex());
        let resp = self
            .http
            .delete(&url)
            .json(&serde_json::json!({
                "scheme": "ed25519",
                "signature": signature_b64,
                "publicKey": public_key_b64,
            }))
            .send()
            .await
            .context("DELETE /v1/orders")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("cancel {} failed: {status}: {body}", digest.to_hex()));
        }
        Ok(())
    }

    /// Mirrored escrow balances by BalanceManager id (chain-event lag
    /// applies: a fresh deposit shows up once the orderbook's sync sees it).
    pub async fn balances(&self, manager: &SuiAddress) -> Result<Vec<BalanceEntry>> {
        let url = format!("{}/v1/accounts/{}/balance", self.base, manager.to_hex());
        let resp = self.http.get(&url).send().await.context("GET balance")?;
        let resp = resp.error_for_status().context("GET balance status")?;
        let body: BalanceResponse = resp.json().await.context("decoding balance")?;
        Ok(body.balances)
    }
}
