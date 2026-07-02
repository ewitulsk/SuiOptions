//! Client for the signer node's `POST /sign_message` (bridge-spec.md §5.3). The
//! request/response shapes mirror `bridge-signer-service`'s handlers; the JSON
//! hex encoding of the message + envelope is provided by `bridge-types`'s serde.

use anyhow::{Context, Result};
use async_trait::async_trait;
use bridge_types::{CrossChainMessage, SignatureEnvelope};
use serde::{Deserialize, Serialize};

#[async_trait]
pub trait RemoteSigner: Send + Sync {
    /// Request an aggregated signature envelope for `message`.
    async fn sign(&self, message: &CrossChainMessage) -> Result<SignatureEnvelope>;
}

pub struct HttpSigner {
    base_url: String,
    http: reqwest::Client,
}

impl HttpSigner {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self { base_url: base_url.into().trim_end_matches('/').to_string(), http: reqwest::Client::new() }
    }
}

#[derive(Serialize)]
struct SignRequest<'a> {
    message: &'a CrossChainMessage,
}

#[derive(Deserialize)]
struct SignResponse {
    #[allow(dead_code)]
    message_hash: String,
    envelope: SignatureEnvelope,
}

#[async_trait]
impl RemoteSigner for HttpSigner {
    async fn sign(&self, message: &CrossChainMessage) -> Result<SignatureEnvelope> {
        let url = format!("{}/sign_message", self.base_url);
        let resp = self
            .http
            .post(&url)
            .json(&SignRequest { message })
            .send()
            .await
            .with_context(|| format!("POST {url}"))?
            .error_for_status()
            .context("signer returned an error status")?;
        let body: SignResponse = resp.json().await.context("decoding signer response")?;
        Ok(body.envelope)
    }
}
