//! Client for engagement-service's internal HTTP API.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub struct EngagementClient {
    http: reqwest::Client,
    base_url: String,
}

/// One leaderboard row (also the `GET /points/{handle}` payload).
#[derive(Debug, Clone, Deserialize)]
pub struct Entry {
    pub rank: usize,
    pub handle: String,
    pub ambassador: bool,
    pub tweets: i64,
    pub likes: i64,
    pub retweets: i64,
    pub replies: i64,
    pub quotes: i64,
    pub engagement_points: f64,
    pub airdrop_points: f64,
}

#[derive(Debug, Deserialize)]
struct LeaderboardResp {
    leaderboard: Vec<Entry>,
}

impl EngagementClient {
    pub fn new(base_url: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("building engagement-service http client")?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }

    /// `GET /leaderboard?limit=N`.
    pub async fn leaderboard(&self, limit: usize) -> Result<Vec<Entry>> {
        let url = format!("{}/leaderboard", self.base_url);
        let resp = observability::client::instrumented("engagement-service", "GET /leaderboard", {
            |headers| {
                self.http
                    .get(&url)
                    .headers(headers)
                    .query(&[("limit", limit)])
                    .send()
            }
        })
        .await
        .context("fetching leaderboard from engagement-service")?;
        let parsed: LeaderboardResp = resp
            .error_for_status()
            .context("engagement-service /leaderboard")?
            .json()
            .await
            .context("parsing leaderboard")?;
        Ok(parsed.leaderboard)
    }

    /// `GET /points/{handle}` — `None` when the handle has no tracked
    /// engagement (404).
    pub async fn points(&self, handle: &str) -> Result<Option<Entry>> {
        let url = format!("{}/points/{}", self.base_url, handle.trim_start_matches('@'));
        let resp = observability::client::instrumented("engagement-service", "GET /points", {
            |headers| self.http.get(&url).headers(headers).send()
        })
        .await
        .context("fetching points from engagement-service")?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let entry: Entry = resp
            .error_for_status()
            .context("engagement-service /points")?
            .json()
            .await
            .context("parsing points")?;
        Ok(Some(entry))
    }
}
