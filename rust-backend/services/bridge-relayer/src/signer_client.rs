//! Client for the signer node's async signing API (bridge-spec.md §5.3):
//! `POST /sign_requests` to submit, then poll `GET /sign_requests/{hash}` until
//! the session is `signed`. The submit-then-poll is hidden behind the same
//! [`RemoteSigner::sign`] method, so the relay loop is unchanged and the M3 MPC
//! turn-on (where signing genuinely takes multiple rounds) needs no client edit.

use std::time::Duration;

use anyhow::{bail, Context, Result};
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
    poll_interval: Duration,
    poll_attempts: u32,
}

impl HttpSigner {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
            poll_interval: Duration::from_millis(500),
            poll_attempts: 60, // ~30s ceiling; M1 completes on the first poll
        }
    }
}

#[derive(Serialize)]
struct SignRequestBody<'a> {
    message: &'a CrossChainMessage,
}

#[derive(Deserialize)]
struct SignRequestResponse {
    message_hash: String,
    status: String,
    envelope: Option<SignatureEnvelope>,
}

#[async_trait]
impl RemoteSigner for HttpSigner {
    async fn sign(&self, message: &CrossChainMessage) -> Result<SignatureEnvelope> {
        // Submit (idempotent per hash). A 202 may already carry the signature.
        let url = format!("{}/sign_requests", self.base_url);
        let submitted: SignRequestResponse = self
            .http
            .post(&url)
            .json(&SignRequestBody { message })
            .send()
            .await
            .with_context(|| format!("POST {url}"))?
            .error_for_status()
            .context("signer rejected the request")?
            .json()
            .await
            .context("decoding submit response")?;
        if let Some(env) = signed_envelope(&submitted)? {
            return Ok(env);
        }

        // Otherwise poll the session to completion.
        let poll_url = format!("{}/sign_requests/{}", self.base_url, submitted.message_hash);
        for _ in 0..self.poll_attempts {
            tokio::time::sleep(self.poll_interval).await;
            let polled: SignRequestResponse = self
                .http
                .get(&poll_url)
                .send()
                .await
                .with_context(|| format!("GET {poll_url}"))?
                .error_for_status()
                .context("polling signer session")?
                .json()
                .await
                .context("decoding poll response")?;
            if let Some(env) = signed_envelope(&polled)? {
                return Ok(env);
            }
        }
        bail!("signer session {} did not complete within the poll budget", submitted.message_hash);
    }
}

/// Extract the envelope from a `signed` response, or None if still `pending`.
fn signed_envelope(r: &SignRequestResponse) -> Result<Option<SignatureEnvelope>> {
    match r.status.as_str() {
        "signed" => Ok(Some(
            r.envelope.clone().context("signer reported signed but returned no envelope")?,
        )),
        "pending" => Ok(None),
        other => bail!("unexpected signer session status: {other}"),
    }
}
