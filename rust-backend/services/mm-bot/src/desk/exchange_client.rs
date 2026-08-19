//! Thin REST client + order signer for the in-house exchange (orderbook
//! service `/v1/*`), adapted from staging-mm-bot's `client.rs`/`signing.rs`.
//!
//! Only what the listings engine (SO-416) needs: market discovery, ask
//! placement, soft cancel, and open-order recovery after a restart.
//! Placement rejections come back as typed intake codes so the engine can
//! react (INSUFFICIENT_ESCROW means the mirror hasn't caught up or units
//! are already committed; everything else is a bug in our snapping math).

use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use exchange_signing::keys::Ed25519Keypair;
use exchange_signing::order_digest;
use exchange_types::order::{SignatureScheme, SignedOrder};
use exchange_types::{Digest, Market, ObjectId, Order, SuiAddress};
use serde::Deserialize;

/// Domain prefix for the orderbook's signed soft-cancel payload — mirrors
/// `handlers::CANCEL_DOMAIN_TAG` in the orderbook service.
pub const CANCEL_DOMAIN_TAG: &[u8] = b"SUI_HYBRID_EXCHANGE_CANCEL";

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

/// One row of `GET /v1/accounts/{addr}/orders`: the stored `SignedOrder`
/// plus its lifecycle status (`OPEN` while resting).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenOrderEntry {
    pub digest: String,
    pub order: SignedOrder,
    pub status: String,
}

#[derive(Debug, Deserialize)]
struct OrdersResponse {
    orders: Vec<OpenOrderEntry>,
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

    /// Orders by maker address (restart recovery: re-adopt asks that still
    /// rest on the book instead of double-listing the same inventory).
    pub async fn orders_by_account(&self, maker: &SuiAddress) -> Result<Vec<OpenOrderEntry>> {
        let url = format!("{}/v1/accounts/{}/orders", self.base, maker.to_hex());
        let resp = self.http.get(&url).send().await.context("GET account orders")?;
        let resp = resp.error_for_status().context("GET account orders status")?;
        let body: OrdersResponse = resp.json().await.context("decoding account orders")?;
        Ok(body.orders)
    }
}

// ── order signing ──────────────────────────────────────────────────────

/// Order signing with the bot's Sui wallet key — the same ed25519 key that
/// pays gas and holds the CuratorCap is delegated as an approved signer on
/// the vault's identity BalanceManager (`exchange_adapter::add_signer`),
/// so its `signPersonalMessage` signatures pass intake for the vault's
/// orders.
pub struct OrderSigner {
    kp: Ed25519Keypair,
    address: SuiAddress,
    public_key: Vec<u8>,
}

impl OrderSigner {
    /// Build from a Sui bech32 keypair export (`suiprivkey1…`). Fails
    /// closed on any non-ed25519 key — the desk's one-key design ties
    /// order signing to the gas/curator key.
    pub fn from_sui_bech32(raw: &str) -> Result<Self> {
        use sui_types::crypto::EncodeDecodeBase64 as _;
        let raw = raw.trim();
        let kp = sui_types::crypto::SuiKeyPair::decode(raw)
            .map_err(|e| anyhow!("decoding suiprivkey bech32 key: {e}"))?;
        // `encode_base64()` yields `base64(flag || 32-byte secret)` for
        // every variant (same extraction as sui-tx's QuoteSigner).
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(kp.encode_base64())
            .context("base64-decoding suiprivkey re-encoding")?;
        if bytes.len() != 33 {
            return Err(anyhow!(
                "suiprivkey decoded to {} bytes, expected 33 (1 flag + 32 secret)",
                bytes.len()
            ));
        }
        if bytes[0] != 0x00 {
            return Err(anyhow!(
                "sui key has scheme flag {:#04x}; the listings engine requires an ed25519 key",
                bytes[0]
            ));
        }
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&bytes[1..33]);
        let kp = Ed25519Keypair::from_seed(seed);
        let address = kp.address();
        let public_key = kp.public_key();
        Ok(Self { kp, address, public_key })
    }

    /// The signing wallet's address (identical bytes to the Sui address).
    pub fn address(&self) -> SuiAddress {
        self.address
    }

    /// Sign an order for one market: digest, personal-message signature,
    /// wire-ready `SignedOrder`.
    pub fn sign_order(&self, order: Order, registry_id: ObjectId) -> (Digest, SignedOrder) {
        let digest = order_digest(&order, &registry_id);
        let signature = self.kp.sign_personal_message(&digest.0);
        let signed = SignedOrder {
            order,
            registry_id,
            scheme: SignatureScheme::Ed25519,
            signature,
            public_key: self.public_key.clone(),
        };
        (digest, signed)
    }

    /// Signature over the soft-cancel payload `TAG ‖ digest_bytes`, plus
    /// the public key, both base64 as `DELETE /v1/orders/{digest}` expects.
    pub fn sign_cancel(&self, digest: &Digest) -> (String, String) {
        let mut message = CANCEL_DOMAIN_TAG.to_vec();
        message.extend_from_slice(&digest.0);
        let sig = self.kp.sign_personal_message(&message);
        let b64 = base64::engine::general_purpose::STANDARD;
        (b64.encode(sig), b64.encode(&self.public_key))
    }
}
