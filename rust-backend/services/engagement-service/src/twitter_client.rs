//! Client for twitter-service's internal read API (mentions + metrics).

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

pub struct TwitterServiceClient {
    http: reqwest::Client,
    base_url: String,
}

/// One tweet mentioning the account, as `GET /mentions` returns it.
#[derive(Debug, Deserialize)]
pub struct Mention {
    pub tweet_id: String,
    pub author_id: String,
    pub author_handle: String,
    pub text: String,
    /// RFC 3339.
    pub created_at: String,
    pub likes: i64,
    pub retweets: i64,
    pub replies: i64,
    pub quotes: i64,
}

#[derive(Debug, Deserialize)]
pub struct MentionsPage {
    pub newest_id: Option<String>,
    pub mentions: Vec<Mention>,
}

#[derive(Debug, Deserialize)]
pub struct TweetMetrics {
    pub tweet_id: String,
    pub likes: i64,
    pub retweets: i64,
    pub replies: i64,
    pub quotes: i64,
}

#[derive(Debug, Deserialize)]
struct MetricsResp {
    metrics: Vec<TweetMetrics>,
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

    /// `GET /mentions` — recent tweets mentioning `@account`.
    pub async fn mentions(&self, account: &str, since_id: Option<&str>) -> Result<MentionsPage> {
        let url = format!("{}/mentions", self.base_url);
        let mut query = vec![("account", account.to_string())];
        if let Some(id) = since_id {
            query.push(("since_id", id.to_string()));
        }
        let resp = observability::client::instrumented("twitter-service", "GET /mentions", {
            |headers| self.http.get(&url).headers(headers).query(&query).send()
        })
        .await
        .context("fetching mentions from twitter-service")?;
        resp.error_for_status()
            .context("twitter-service /mentions")?
            .json()
            .await
            .context("parsing mentions")
    }

    /// `GET /tweets/metrics` — refreshed counters for up to 100 known tweets.
    pub async fn tweets_metrics(&self, account: &str, ids: &[String]) -> Result<Vec<TweetMetrics>> {
        let url = format!("{}/tweets/metrics", self.base_url);
        let query = [("account", account.to_string()), ("ids", ids.join(","))];
        let resp =
            observability::client::instrumented("twitter-service", "GET /tweets/metrics", {
                |headers| self.http.get(&url).headers(headers).query(&query).send()
            })
            .await
            .context("fetching tweet metrics from twitter-service")?;
        let parsed: MetricsResp = resp
            .error_for_status()
            .context("twitter-service /tweets/metrics")?
            .json()
            .await
            .context("parsing tweet metrics")?;
        Ok(parsed.metrics)
    }
}
