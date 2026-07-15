//! The /leaderboard and /points commands: fetch from engagement-service and
//! format the chat reply.

use std::sync::Arc;

use tracing::warn;

use crate::engagement_client::Entry;
use crate::state::AppState;

const DEFAULT_LEADERBOARD_COUNT: usize = 10;
const MAX_LEADERBOARD_COUNT: usize = 25;

fn fmt_entry_line(e: &Entry) -> String {
    let star = if e.ambassador { " ⭐" } else { "" };
    format!(
        "{}. @{} — {:.0} airdrop pts ({:.0} engagement pts, {} tweet{}){}",
        e.rank,
        e.handle,
        e.airdrop_points,
        e.engagement_points,
        e.tweets,
        if e.tweets == 1 { "" } else { "s" },
        star
    )
}

pub fn format_leaderboard(entries: &[Entry]) -> String {
    if entries.is_empty() {
        return "No tracked engagement yet — mention us on X to get on the board!".to_string();
    }
    let mut lines = vec!["🏆 **Airdrop leaderboard** (⭐ = ambassador)".to_string()];
    lines.extend(entries.iter().map(fmt_entry_line));
    lines.join("\n")
}

pub fn format_points(handle: &str, entry: Option<&Entry>) -> String {
    match entry {
        Some(e) => format!(
            "@{} is rank #{} with {:.0} airdrop pts{} — {:.0} engagement pts from {} tweet{} \
             ({} likes, {} retweets, {} replies, {} quotes).",
            e.handle,
            e.rank,
            e.airdrop_points,
            if e.ambassador { " ⭐ (ambassador)" } else { "" },
            e.engagement_points,
            e.tweets,
            if e.tweets == 1 { "" } else { "s" },
            e.likes,
            e.retweets,
            e.replies,
            e.quotes,
        ),
        None => format!(
            "No tracked engagement for `@{}` yet — tweets mentioning us start counting \
             within a few minutes of posting.",
            handle.trim_start_matches('@')
        ),
    }
}

/// Run /leaderboard and produce the user-facing message.
pub async fn run_leaderboard(state: &Arc<AppState>, count: Option<usize>) -> String {
    let count = count
        .unwrap_or(DEFAULT_LEADERBOARD_COUNT)
        .clamp(1, MAX_LEADERBOARD_COUNT);
    match state.engagement.leaderboard(count).await {
        Ok(entries) => format_leaderboard(&entries),
        Err(e) => {
            warn!(error = %format!("{e:#}"), "leaderboard fetch failed");
            "❌ Couldn't reach the engagement service — try again in a minute.".to_string()
        }
    }
}

/// Run /points and produce the user-facing message.
pub async fn run_points(state: &Arc<AppState>, handle: &str) -> String {
    match state.engagement.points(handle).await {
        Ok(entry) => format_points(handle, entry.as_ref()),
        Err(e) => {
            warn!(handle, error = %format!("{e:#}"), "points fetch failed");
            "❌ Couldn't reach the engagement service — try again in a minute.".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(rank: usize, handle: &str, ambassador: bool) -> Entry {
        Entry {
            rank,
            handle: handle.to_string(),
            ambassador,
            tweets: 2,
            likes: 10,
            retweets: 1,
            replies: 3,
            quotes: 0,
            engagement_points: 19.0,
            airdrop_points: if ambassador { 285.0 } else { 190.0 },
        }
    }

    #[test]
    fn formats_leaderboard_with_ambassador_star() {
        let out = format_leaderboard(&[entry(1, "amber", true), entry(2, "bob", false)]);
        assert!(out.contains("1. @amber — 285 airdrop pts (19 engagement pts, 2 tweets) ⭐"));
        assert!(out.contains("2. @bob — 190 airdrop pts"));
        assert!(!out.contains("bob — 190 airdrop pts (19 engagement pts, 2 tweets) ⭐"));
    }

    #[test]
    fn formats_empty_leaderboard() {
        assert!(format_leaderboard(&[]).contains("No tracked engagement yet"));
    }

    #[test]
    fn formats_points_and_unknown_handle() {
        let e = entry(3, "amber", true);
        let out = format_points("amber", Some(&e));
        assert!(out.contains("rank #3"));
        assert!(out.contains("ambassador"));
        assert!(format_points("@ghost", None).contains("`@ghost`"));
    }
}
