//! Circle attestation API (iris) client.
//!
//! `GET {base}/v1/messages/{sourceDomain}/{txHash}` returns every CCTP
//! message emitted by the burn tx with its attestation. Attestation is the
//! literal string `"PENDING"` until Circle reaches hard finality on the
//! source chain. 404 means the tx hasn't been observed yet — both map to
//! `NotReady`. Rate limit: 35 req/s (429 → 5-minute block), so the poller
//! stays far below it.

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Clone)]
pub struct IrisClient {
    http: reqwest::Client,
    base: String,
}

#[derive(Debug)]
pub enum Attestation {
    NotReady,
    Ready { message_hex: String, attestation_hex: String },
}

#[derive(Deserialize)]
struct MessagesResponse {
    #[serde(default)]
    messages: Vec<IrisMessage>,
}

#[derive(Deserialize)]
struct IrisMessage {
    message: Option<String>,
    attestation: Option<String>,
}

impl IrisClient {
    pub fn new(base: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: base.trim_end_matches('/').to_string(),
        }
    }

    pub async fn attestation(&self, source_domain: u32, tx_hash: &str) -> Result<Attestation> {
        let url = format!("{}/v1/messages/{}/{}", self.base, source_domain, tx_hash);
        let resp = self.http.get(&url).send().await.context("calling iris")?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(Attestation::NotReady);
        }
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            anyhow::bail!("iris rate limited (429)");
        }
        let resp = resp.error_for_status().context("iris error status")?;
        let body: MessagesResponse = resp.json().await.context("parsing iris response")?;

        // A burn tx from our entry points carries exactly one CCTP message.
        let Some(m) = body.messages.into_iter().next() else {
            return Ok(Attestation::NotReady);
        };
        match (m.message, m.attestation) {
            (Some(message), Some(attestation))
                if attestation.starts_with("0x") && message.starts_with("0x") =>
            {
                Ok(Attestation::Ready { message_hex: message, attestation_hex: attestation })
            }
            _ => Ok(Attestation::NotReady),
        }
    }
}
