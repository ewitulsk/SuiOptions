//! Client for twitter-service's internal HTTP API.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

pub struct TwitterServiceClient {
    http: reqwest::Client,
    base_url: String,
}

#[derive(Debug, Deserialize)]
pub struct PostedTweet {
    pub account: String,
    pub tweet_id: String,
    pub text: String,
}

impl TwitterServiceClient {
    pub fn new(base_url: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("building twitter-service http client")?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }

    /// `GET /accounts` — the account names tweets can be posted from.
    pub async fn accounts(&self) -> Result<Vec<String>> {
        let url = format!("{}/accounts", self.base_url);
        let resp = observability::client::instrumented("twitter-service", "GET /accounts", {
            |headers| self.http.get(&url).headers(headers).send()
        })
        .await
        .context("fetching accounts from twitter-service")?;
        resp.error_for_status()
            .context("twitter-service /accounts")?
            .json()
            .await
            .context("parsing accounts")
    }

    /// `POST /tweets` — post `text` from `account`.
    pub async fn post_tweet(&self, account: &str, text: &str) -> Result<PostedTweet> {
        let url = format!("{}/tweets", self.base_url);
        let body = serde_json::json!({ "account": account, "text": text });
        let resp = observability::client::instrumented("twitter-service", "POST /tweets", {
            |headers| self.http.post(&url).headers(headers).json(&body).send()
        })
        .await
        .context("posting tweet via twitter-service")?;

        let status = resp.status();
        if !status.is_success() {
            let detail = resp.text().await.unwrap_or_default();
            return Err(anyhow!("twitter-service {status}: {detail}"));
        }
        resp.json().await.context("parsing posted tweet")
    }
}
