//! The /tweet command, shared by both platforms.

use std::sync::Arc;
use std::time::Duration;

use tracing::{info, warn};

use crate::state::AppState;

/// `/tweet <account> <text…>` parsed into its two arguments.
pub fn parse_tweet_args(input: &str) -> Option<(&str, &str)> {
    let input = input.trim();
    let (account, text) = input.split_once(char::is_whitespace)?;
    let text = text.trim();
    if account.is_empty() || text.is_empty() {
        return None;
    }
    Some((account, text))
}

/// Usage string, with the live account list when twitter-service is
/// reachable. Bounded well inside the platforms' 3s ack window.
pub async fn usage(state: &AppState) -> String {
    let accounts = tokio::time::timeout(Duration::from_secs(2), state.twitter.accounts()).await;
    let accounts = match accounts {
        Ok(Ok(a)) if !a.is_empty() => format!(" Accounts: {}.", a.join(", ")),
        _ => String::new(),
    };
    format!("Usage: /tweet <account> <text>.{accounts}")
}

/// Post the tweet and produce the user-facing result message.
pub async fn run_tweet(state: &Arc<AppState>, user: &str, account: &str, text: &str) -> String {
    match state.twitter.post_tweet(account, text).await {
        Ok(tweet) => {
            info!(user, account, tweet_id = %tweet.tweet_id, "tweet posted via bot");
            format!(
                "✅ Tweet posted from `{}`: https://x.com/i/web/status/{}",
                tweet.account, tweet.tweet_id
            )
        }
        Err(e) => {
            // twitter-service already fires the grouped `tweet-failed` alert
            // at its failure handler; here the chat reply is the feedback.
            warn!(user, account, error = %format!("{e:#}"), "bot tweet failed");
            format!("❌ Failed to post from `{account}`: {e:#}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_tweet_args;

    #[test]
    fn parses_account_and_text() {
        assert_eq!(
            parse_tweet_args("suioptions gm from staging"),
            Some(("suioptions", "gm from staging"))
        );
        assert_eq!(
            parse_tweet_args("  suioptions   spaced  "),
            Some(("suioptions", "spaced"))
        );
    }

    #[test]
    fn rejects_missing_parts() {
        assert_eq!(parse_tweet_args(""), None);
        assert_eq!(parse_tweet_args("onlyaccount"), None);
        assert_eq!(parse_tweet_args("account   "), None);
    }
}
