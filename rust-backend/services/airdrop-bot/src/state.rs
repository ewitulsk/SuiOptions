//! Shared application state.

use ed25519_dalek::VerifyingKey;

use crate::engagement_client::EngagementClient;

pub struct AppState {
    /// engagement-service read API.
    pub engagement: EngagementClient,
    /// Follow-up client for editing deferred Discord responses.
    pub http: reqwest::Client,
    /// Discord application public key (this bot's own application).
    pub discord_verify_key: VerifyingKey,
}
