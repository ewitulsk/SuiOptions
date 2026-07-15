//! HTTP handlers: [`health`], [`accounts`], [`post_tweet`], [`mentions`],
//! [`tweets_metrics`].

use std::sync::Arc;

use axum::extract::{Json, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::secrets::TwitterAccount;
use crate::state::AppState;
use crate::twitter::PublicMetrics;

type ApiError = (StatusCode, String);

pub async fn health() -> &'static str {
    "ok"
}

/// `GET /accounts` — the configured account names.
pub async fn accounts(State(s): State<Arc<AppState>>) -> Json<Vec<String>> {
    Json(s.accounts.keys().cloned().collect())
}

#[derive(Deserialize)]
pub struct PostTweetReq {
    /// Account name, one of `GET /accounts`.
    pub account: String,
    pub text: String,
}

#[derive(Serialize)]
pub struct PostTweetResp {
    pub account: String,
    /// The created tweet's id (string — exceeds JS safe-integer range).
    pub tweet_id: String,
    pub text: String,
}

fn account_creds<'a>(s: &'a AppState, account: &str) -> Result<&'a TwitterAccount, ApiError> {
    s.accounts.get(account).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("unknown account `{account}`"),
        )
    })
}

/// `POST /tweets` — post a tweet from the named account.
pub async fn post_tweet(
    State(s): State<Arc<AppState>>,
    Json(req): Json<PostTweetReq>,
) -> Result<Json<PostTweetResp>, ApiError> {
    let creds = account_creds(&s, &req.account)?;
    if req.text.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "tweet text is empty".to_string()));
    }

    match s.twitter.post_tweet(creds, &req.text).await {
        Ok(tweet) => {
            info!(account = %req.account, tweet_id = %tweet.id, "tweet posted");
            Ok(Json(PostTweetResp {
                account: req.account,
                tweet_id: tweet.id,
                text: tweet.text,
            }))
        }
        Err(e) => {
            // Grouped Grafana alert (see crates/observability); the account
            // rides along as a structured field to keep alert_id
            // low-cardinality.
            error!(
                alert_id = "tweet-failed",
                account = %req.account,
                error = %format!("{e:#}"),
                "posting tweet failed"
            );
            Err((StatusCode::BAD_GATEWAY, format!("{e:#}")))
        }
    }
}

/// Engagement counters flattened to friendly names (consumed by
/// engagement-service).
#[derive(Serialize)]
pub struct MetricsDto {
    pub likes: i64,
    pub retweets: i64,
    pub replies: i64,
    pub quotes: i64,
}

impl From<PublicMetrics> for MetricsDto {
    fn from(m: PublicMetrics) -> Self {
        Self {
            likes: m.like_count,
            retweets: m.retweet_count,
            replies: m.reply_count,
            quotes: m.quote_count,
        }
    }
}

#[derive(Deserialize)]
pub struct MentionsQuery {
    /// Account name (= handle) whose mentions to search.
    pub account: String,
    /// Only return tweets newer than this id (the previous page's
    /// `newest_id`).
    pub since_id: Option<String>,
}

#[derive(Serialize)]
pub struct MentionDto {
    pub tweet_id: String,
    pub author_id: String,
    pub author_handle: String,
    pub text: String,
    /// RFC 3339, as Twitter returns it.
    pub created_at: String,
    #[serde(flatten)]
    pub metrics: MetricsDto,
}

#[derive(Serialize)]
pub struct MentionsResp {
    pub account: String,
    /// `since_id` for the next poll. Absent when nothing matched.
    pub newest_id: Option<String>,
    pub mentions: Vec<MentionDto>,
}

/// `GET /mentions?account=<name>[&since_id=<id>]` — recent (≤7 days)
/// original tweets mentioning `@account`.
pub async fn mentions(
    State(s): State<Arc<AppState>>,
    Query(q): Query<MentionsQuery>,
) -> Result<Json<MentionsResp>, ApiError> {
    let creds = account_creds(&s, &q.account)?;
    let page = s
        .twitter
        .search_mentions(creds, &q.account, q.since_id.as_deref())
        .await
        .map_err(|e| {
            warn!(account = %q.account, error = %format!("{e:#}"), "mention search failed");
            (StatusCode::BAD_GATEWAY, format!("{e:#}"))
        })?;
    Ok(Json(MentionsResp {
        account: q.account,
        newest_id: page.newest_id,
        mentions: page
            .mentions
            .into_iter()
            .map(|m| MentionDto {
                tweet_id: m.tweet_id,
                author_id: m.author_id,
                author_handle: m.author_handle,
                text: m.text,
                created_at: m.created_at,
                metrics: m.metrics.into(),
            })
            .collect(),
    }))
}

#[derive(Deserialize)]
pub struct TweetMetricsQuery {
    /// Account name whose credentials sign the lookup.
    pub account: String,
    /// Comma-separated tweet ids, at most 100.
    pub ids: String,
}

#[derive(Serialize)]
pub struct TweetMetricsDto {
    pub tweet_id: String,
    #[serde(flatten)]
    pub metrics: MetricsDto,
}

#[derive(Serialize)]
pub struct TweetMetricsResp {
    /// Deleted/protected tweets are absent.
    pub metrics: Vec<TweetMetricsDto>,
}

/// `GET /tweets/metrics?account=<name>&ids=<a,b,c>` — current engagement
/// counters for up to 100 tweets.
pub async fn tweets_metrics(
    State(s): State<Arc<AppState>>,
    Query(q): Query<TweetMetricsQuery>,
) -> Result<Json<TweetMetricsResp>, ApiError> {
    let creds = account_creds(&s, &q.account)?;
    let ids: Vec<String> = q
        .ids
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if ids.is_empty() || ids.len() > 100 {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("ids must be 1..=100 comma-separated tweet ids, got {}", ids.len()),
        ));
    }

    let metrics = s.twitter.tweets_metrics(creds, &ids).await.map_err(|e| {
        warn!(account = %q.account, error = %format!("{e:#}"), "metrics lookup failed");
        (StatusCode::BAD_GATEWAY, format!("{e:#}"))
    })?;
    Ok(Json(TweetMetricsResp {
        metrics: metrics
            .into_iter()
            .map(|t| TweetMetricsDto {
                tweet_id: t.tweet_id,
                metrics: t.metrics.into(),
            })
            .collect(),
    }))
}
