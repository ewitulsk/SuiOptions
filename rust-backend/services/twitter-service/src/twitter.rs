//! Thin Twitter API v2 client: create-tweet, signed per-account.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use crate::oauth1;
use crate::secrets::TwitterAccount;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub struct TwitterClient {
    http: reqwest::Client,
    api_base: String,
}

/// The created tweet, from the v2 response `data` object.
#[derive(Debug, Deserialize)]
pub struct PostedTweet {
    pub id: String,
    pub text: String,
}

#[derive(Deserialize)]
struct CreateTweetResponse {
    data: PostedTweet,
}

impl TwitterClient {
    pub fn new(api_base: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("building twitter http client")?;
        Ok(Self {
            http,
            api_base: api_base.trim_end_matches('/').to_string(),
        })
    }

    /// `POST /2/tweets` as the given account.
    pub async fn post_tweet(&self, creds: &TwitterAccount, text: &str) -> Result<PostedTweet> {
        let url = format!("{}/2/tweets", self.api_base);
        // JSON body params are not part of the OAuth 1.0a signature.
        let auth = oauth1::authorization_header(creds, "POST", &url, &BTreeMap::new());

        let send = |headers| {
            self.http
                .post(&url)
                .headers(headers)
                .header(reqwest::header::AUTHORIZATION, auth.clone())
                .json(&serde_json::json!({ "text": text }))
                .send()
        };
        let resp = observability::client::instrumented("twitter", "POST /2/tweets", send)
            .await
            .context("sending create-tweet request")?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("twitter api {status}: {body}"));
        }
        let parsed: CreateTweetResponse =
            serde_json::from_str(&body).with_context(|| format!("parsing response: {body}"))?;
        Ok(parsed.data)
    }
}
