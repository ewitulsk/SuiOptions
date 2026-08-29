use std::sync::atomic::AtomicBool;

use crate::db::repo::Repo;

pub struct AppState {
    pub repo: Repo,
    /// Raised by the hub watcher on observed pause/risk/identity events;
    /// consumed by the ConfigSync crank for an immediate push.
    pub config_sync_due: AtomicBool,
    /// Served by GET /lanes.
    pub spoke_id: i64,
}

impl AppState {
    pub fn new(repo: Repo, spoke_id: i64) -> Self {
        Self { repo, config_sync_due: AtomicBool::new(false), spoke_id }
    }
}
