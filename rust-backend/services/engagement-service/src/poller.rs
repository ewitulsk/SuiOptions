//! Poll loop: ingest new mentions, refresh counters on known tweets.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use tracing::{error, info, warn};

use crate::db::models::TweetRow;
use crate::points::Engagement;
use crate::state::AppState;

/// Tweets per metrics-refresh call (Twitter's `GET /2/tweets` id cap).
const REFRESH_BATCH: i64 = 100;

pub fn spawn(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(Duration::from_secs(state.cfg.poll_interval_secs.max(30)));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            if let Err(e) = tick(&state).await {
                // Grouped Grafana alert: engagement silently not accruing is
                // the failure mode this service exists to avoid.
                error!(
                    alert_id = "engagement-poll-failed",
                    error = %format!("{e:#}"),
                    "engagement poll tick failed"
                );
            }
        }
    });
}

async fn tick(state: &AppState) -> Result<()> {
    let account = &state.cfg.twitter_account;

    // New mentions since the stored cursor.
    let since_id = state.repo.load_since_id()?;
    let page = state
        .twitter
        .mentions(account, since_id.as_deref())
        .await
        .context("fetching mentions")?;
    let now = Utc::now();
    let rows: Vec<TweetRow> = page
        .mentions
        .into_iter()
        .filter_map(|m| {
            let created = DateTime::parse_from_rfc3339(&m.created_at)
                .map(|t| t.with_timezone(&Utc))
                .ok();
            if created.is_none() || m.author_handle.is_empty() {
                warn!(tweet_id = %m.tweet_id, "skipping mention with missing author/timestamp");
                return None;
            }
            Some(TweetRow {
                tweet_id: m.tweet_id,
                author_id: m.author_id,
                // Handles are case-insensitive; normalize so grouping and
                // ambassador matching never split on case.
                author_handle: m.author_handle.to_lowercase(),
                text: m.text,
                tweet_created_at: created.unwrap(),
                first_seen_at: now,
                metrics_updated_at: now,
                likes: m.likes,
                retweets: m.retweets,
                replies: m.replies,
                quotes: m.quotes,
            })
        })
        .collect();
    let ingested = state.repo.upsert_mentions(&rows, page.newest_id.as_deref())?;

    // Refresh the stalest counters among tweets still young enough to move.
    let ids = state
        .repo
        .refresh_candidates(state.cfg.refresh_max_age_hours, REFRESH_BATCH)?;
    let mut refreshed = 0;
    if !ids.is_empty() {
        let metrics = state
            .twitter
            .tweets_metrics(account, &ids)
            .await
            .context("refreshing tweet metrics")?;
        let updates: Vec<(String, Engagement)> = metrics
            .into_iter()
            .map(|m| {
                (
                    m.tweet_id,
                    Engagement {
                        likes: m.likes,
                        retweets: m.retweets,
                        replies: m.replies,
                        quotes: m.quotes,
                    },
                )
            })
            .collect();
        refreshed = updates.len();
        state.repo.update_metrics(&updates)?;
    }

    info!(ingested, refreshed, "engagement poll tick");
    Ok(())
}
