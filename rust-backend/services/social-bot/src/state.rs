//! Shared application state.

use ed25519_dalek::VerifyingKey;

use crate::twitter_client::TwitterServiceClient;

pub struct AppState {
    /// Client for twitter-service (internal HTTP).
    pub twitter: TwitterServiceClient,
    /// For Slack `response_url` / Discord follow-up webhook posts.
    pub http: reqwest::Client,
    /// Verifies Slack request signatures (HMAC-SHA256).
    pub slack_signing_secret: String,
    /// Verifies Discord interaction signatures (Ed25519).
    pub discord_verify_key: VerifyingKey,
    /// Slack user ids allowed to tweet. Empty = nobody.
    pub slack_allowed_user_ids: Vec<String>,
    /// Discord user ids allowed to tweet. Empty = nobody.
    pub discord_allowed_user_ids: Vec<String>,
}
