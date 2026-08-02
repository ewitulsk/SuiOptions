//! Client for auth-service's internal invite-minting route.
//!
//! Direction of dependency matters: dakota-service calls auth-service, never
//! the reverse. auth-service stays domain-agnostic — it stores an opaque
//! `scope_id` and has no idea it means a Dakota customer.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct InviteClient {
    base_url: String,
    ttl_secs: i64,
    http: reqwest::Client,
}

#[derive(Debug, Serialize)]
struct CreateInviteReq<'a> {
    role: &'a str,
    scope_id: Option<&'a str>,
    label: Option<&'a str>,
    ttl_secs: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Invite {
    pub invite_id: String,
    pub role: String,
    pub expires_at: String,
}

impl InviteClient {
    pub fn new(base_url: impl Into<String>, ttl_secs: i64) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            ttl_secs,
            http: reqwest::Client::new(),
        }
    }

    /// Mint a signup grant for `role`, scoped to a Dakota customer id.
    ///
    /// `label` is shown on the signup page, so keep it non-identifying — it is
    /// the one free-text field that crosses into auth-service.
    pub async fn mint(
        &self,
        role: &str,
        scope_id: Option<&str>,
        label: Option<&str>,
    ) -> Result<Invite> {
        let url = format!("{}/invites", self.base_url);
        let body = CreateInviteReq { role, scope_id, label, ttl_secs: self.ttl_secs };

        let resp = observability::client::instrumented("auth-service", "POST /invites", |h| {
            self.http.post(&url).headers(h).json(&body).send()
        })
        .await
        .context("calling auth-service /invites")?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("auth-service /invites → {status}: {text}");
        }
        serde_json::from_str(&text).context("parsing invite response")
    }
}
