//! Shared application state.

use crate::config::Config;
use crate::db::repo::Repo;
use crate::twitter_client::TwitterServiceClient;

pub struct AppState {
    pub repo: Repo,
    pub twitter: TwitterServiceClient,
    pub cfg: Config,
}
