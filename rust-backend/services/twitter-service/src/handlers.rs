//! HTTP handlers: [`health`], [`accounts`], [`post_tweet`].

use std::sync::Arc;

use axum::extract::{Json, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use tracing::{error, info};

use crate::state::AppState;

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

/// `POST /tweets` — post a tweet from the named account.
pub async fn post_tweet(
    State(s): State<Arc<AppState>>,
    Json(req): Json<PostTweetReq>,
) -> Result<Json<PostTweetResp>, ApiError> {
    let creds = s.accounts.get(&req.account).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("unknown account `{}`", req.account),
        )
    })?;
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
