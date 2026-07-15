//! HTTP handlers: [`health`], [`leaderboard`], [`points`].

use std::sync::Arc;

use axum::extract::{Json, Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::db::repo::AuthorTotals;
use crate::points::PointsConfig;
use crate::state::AppState;

type ApiError = (StatusCode, String);

pub async fn health() -> &'static str {
    "ok"
}

/// One leaderboard row (also the `GET /points/{handle}` payload).
#[derive(Serialize, Debug, Clone)]
pub struct LeaderboardEntry {
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

fn ranked_entries(points: &PointsConfig, totals: Vec<AuthorTotals>) -> Vec<LeaderboardEntry> {
    let mut entries: Vec<LeaderboardEntry> = totals
        .into_iter()
        .map(|t| {
            let engagement_points = points.engagement_points(t.engagement());
            LeaderboardEntry {
                rank: 0,
                ambassador: points.is_ambassador(&t.author_handle),
                airdrop_points: points.airdrop_points(&t.author_handle, engagement_points),
                engagement_points,
                tweets: t.tweets,
                likes: t.likes,
                retweets: t.retweets,
                replies: t.replies,
                quotes: t.quotes,
                handle: t.author_handle,
            }
        })
        .collect();
    entries.sort_by(|a, b| {
        b.airdrop_points
            .total_cmp(&a.airdrop_points)
            .then_with(|| a.handle.cmp(&b.handle))
    });
    for (i, e) in entries.iter_mut().enumerate() {
        e.rank = i + 1;
    }
    entries
}

fn load_entries(state: &AppState) -> Result<Vec<LeaderboardEntry>, ApiError> {
    let totals = state.repo.author_totals().map_err(|e| {
        warn!(error = %format!("{e:#}"), "loading author totals failed");
        (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    })?;
    Ok(ranked_entries(&state.cfg.points, totals))
}

fn default_limit() -> usize {
    10
}

#[derive(Deserialize)]
pub struct LeaderboardQuery {
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[derive(Serialize)]
pub struct LeaderboardResp {
    pub leaderboard: Vec<LeaderboardEntry>,
}

/// `GET /leaderboard?limit=N` — authors ranked by airdrop points.
pub async fn leaderboard(
    State(s): State<Arc<AppState>>,
    Query(q): Query<LeaderboardQuery>,
) -> Result<Json<LeaderboardResp>, ApiError> {
    let mut entries = load_entries(&s)?;
    entries.truncate(q.limit.clamp(1, 100));
    Ok(Json(LeaderboardResp {
        leaderboard: entries,
    }))
}

/// `GET /points/{handle}` — one author's totals, points and rank.
pub async fn points(
    State(s): State<Arc<AppState>>,
    Path(handle): Path<String>,
) -> Result<Json<LeaderboardEntry>, ApiError> {
    let handle = handle.trim_start_matches('@').to_lowercase();
    let entries = load_entries(&s)?;
    entries
        .into_iter()
        .find(|e| e.handle == handle)
        .map(Json)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("no tracked engagement for `{handle}`"),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn points_cfg(ambassadors: Vec<String>) -> PointsConfig {
        PointsConfig {
            like_weight: 1.0,
            reply_weight: 2.0,
            retweet_weight: 3.0,
            quote_weight: 4.0,
            airdrop_points_per_engagement_point: 10.0,
            ambassador_multiplier: 1.5,
            ambassadors,
        }
    }

    fn totals(handle: &str, likes: i64) -> AuthorTotals {
        AuthorTotals {
            author_handle: handle.to_string(),
            tweets: 1,
            likes,
            retweets: 0,
            replies: 0,
            quotes: 0,
        }
    }

    #[test]
    fn ranks_by_airdrop_points_with_ambassador_boost() {
        let cfg = points_cfg(vec!["amber".to_string()]);
        // amber: 10 likes * 1 * 10 * 1.5 = 150; bob: 12 likes * 1 * 10 = 120.
        let entries = ranked_entries(&cfg, vec![totals("bob", 12), totals("amber", 10)]);
        assert_eq!(entries[0].handle, "amber");
        assert_eq!(entries[0].rank, 1);
        assert!(entries[0].ambassador);
        assert_eq!(entries[0].airdrop_points, 150.0);
        assert_eq!(entries[1].handle, "bob");
        assert_eq!(entries[1].rank, 2);
        assert!(!entries[1].ambassador);
        assert_eq!(entries[1].airdrop_points, 120.0);
    }

    #[test]
    fn ties_break_alphabetically() {
        let cfg = points_cfg(vec![]);
        let entries = ranked_entries(&cfg, vec![totals("zed", 5), totals("ana", 5)]);
        assert_eq!(entries[0].handle, "ana");
        assert_eq!(entries[1].handle, "zed");
    }
}
