//! Shared application state.

use std::collections::BTreeMap;

use crate::secrets::TwitterAccount;
use crate::twitter::TwitterClient;

pub struct AppState {
    /// Signed Twitter API v2 client.
    pub twitter: TwitterClient,
    /// Account name → OAuth 1.0a credentials, from the secrets TOML.
    pub accounts: BTreeMap<String, TwitterAccount>,
}
