//! Thin Twitter API v2 client: create-tweet, recent-mention search and
//! tweet-metrics lookup, signed per-account.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{anyhow, ensure, Context, Result};
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

/// Public engagement counters (`tweet.fields=public_metrics`).
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct PublicMetrics {
    #[serde(default)]
    pub like_count: i64,
    #[serde(default)]
    pub retweet_count: i64,
    #[serde(default)]
    pub reply_count: i64,
    #[serde(default)]
    pub quote_count: i64,
}

/// One tweet mentioning the account, author resolved from the response's
/// `includes.users`.
#[derive(Debug)]
pub struct Mention {
    pub tweet_id: String,
    pub author_id: String,
    /// Author's @handle, without the `@`. Empty if the expansion was missing.
    pub author_handle: String,
    pub text: String,
    /// RFC 3339, as Twitter returns it.
    pub created_at: String,
    pub metrics: PublicMetrics,
}

/// One page of recent-search results (Twitter caps recent search at the
/// last 7 days; `newest_id` is the next poll's `since_id`).
#[derive(Debug)]
pub struct MentionsPage {
    pub mentions: Vec<Mention>,
    pub newest_id: Option<String>,
}

/// Refreshed counters for one tweet (`GET /2/tweets?ids=…`).
#[derive(Debug)]
pub struct TweetMetrics {
    pub tweet_id: String,
    pub metrics: PublicMetrics,
}

#[derive(Deserialize)]
struct SearchTweet {
    id: String,
    text: String,
    #[serde(default)]
    author_id: String,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    public_metrics: PublicMetrics,
}

#[derive(Deserialize)]
struct IncludedUser {
    id: String,
    username: String,
}

#[derive(Deserialize, Default)]
struct SearchIncludes {
    #[serde(default)]
    users: Vec<IncludedUser>,
}

#[derive(Deserialize)]
struct SearchMeta {
    newest_id: Option<String>,
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    data: Vec<SearchTweet>,
    #[serde(default)]
    includes: Option<SearchIncludes>,
    meta: Option<SearchMeta>,
}

#[derive(Deserialize)]
struct LookupResponse {
    #[serde(default)]
    data: Vec<SearchTweet>,
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

    /// `GET /2/tweets/search/recent` — original (non-retweet) tweets from
    /// the last 7 days mentioning `@handle`, excluding the account's own
    /// tweets. The account name in secrets doubles as the handle.
    pub async fn search_mentions(
        &self,
        creds: &TwitterAccount,
        handle: &str,
        since_id: Option<&str>,
    ) -> Result<MentionsPage> {
        let url = format!("{}/2/tweets/search/recent", self.api_base);
        let mut params = BTreeMap::new();
        params.insert(
            "query".to_string(),
            format!("@{handle} -is:retweet -from:{handle}"),
        );
        params.insert("max_results".to_string(), "100".to_string());
        params.insert(
            "tweet.fields".to_string(),
            "public_metrics,created_at,author_id".to_string(),
        );
        params.insert("expansions".to_string(), "author_id".to_string());
        params.insert("user.fields".to_string(), "username".to_string());
        if let Some(id) = since_id {
            params.insert("since_id".to_string(), id.to_string());
        }

        let body = self
            .signed_get(creds, "GET /2/tweets/search/recent", &url, &params)
            .await?;
        let parsed: SearchResponse =
            serde_json::from_str(&body).with_context(|| format!("parsing response: {body}"))?;

        let users: BTreeMap<String, String> = parsed
            .includes
            .unwrap_or_default()
            .users
            .into_iter()
            .map(|u| (u.id, u.username))
            .collect();
        let mentions = parsed
            .data
            .into_iter()
            .map(|t| Mention {
                author_handle: users.get(&t.author_id).cloned().unwrap_or_default(),
                tweet_id: t.id,
                author_id: t.author_id,
                text: t.text,
                created_at: t.created_at,
                metrics: t.public_metrics,
            })
            .collect();
        Ok(MentionsPage {
            mentions,
            newest_id: parsed.meta.and_then(|m| m.newest_id),
        })
    }

    /// `GET /2/tweets?ids=…` — current engagement counters for up to 100
    /// tweets. Deleted/protected tweets come back under `errors` and are
    /// silently absent from the result.
    pub async fn tweets_metrics(
        &self,
        creds: &TwitterAccount,
        ids: &[String],
    ) -> Result<Vec<TweetMetrics>> {
        ensure!(
            !ids.is_empty() && ids.len() <= 100,
            "ids must be 1..=100 per lookup, got {}",
            ids.len()
        );
        let url = format!("{}/2/tweets", self.api_base);
        let mut params = BTreeMap::new();
        params.insert("ids".to_string(), ids.join(","));
        params.insert("tweet.fields".to_string(), "public_metrics".to_string());

        let body = self.signed_get(creds, "GET /2/tweets", &url, &params).await?;
        let parsed: LookupResponse =
            serde_json::from_str(&body).with_context(|| format!("parsing response: {body}"))?;
        Ok(parsed
            .data
            .into_iter()
            .map(|t| TweetMetrics {
                tweet_id: t.id,
                metrics: t.public_metrics,
            })
            .collect())
    }

    /// Signed GET. Query params are part of the OAuth 1.0a signature base
    /// string, so the same map feeds both the header and the URL.
    async fn signed_get(
        &self,
        creds: &TwitterAccount,
        op: &'static str,
        url: &str,
        params: &BTreeMap<String, String>,
    ) -> Result<String> {
        let auth = oauth1::authorization_header(creds, "GET", url, params);
        let send = |headers| {
            self.http
                .get(url)
                .headers(headers)
                .header(reqwest::header::AUTHORIZATION, auth.clone())
                .query(params)
                .send()
        };
        let resp = observability::client::instrumented("twitter", op, send)
            .await
            .with_context(|| format!("sending {op} request"))?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("twitter api {status}: {body}"));
        }
        Ok(body)
    }
}
