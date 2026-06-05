//! Shared application state for both routers.

use deployments::PackageInfo;
use token_info_client::SupportedToken;

use crate::db::Repo;

pub struct AppState {
    /// Durable, operator-managed catalog persistence.
    pub repo: Repo,
    /// `package_info` for the configured network, read once from
    /// `deployments.json` at boot and served verbatim from `/package-info`.
    pub package_info: PackageInfo,
    /// Read-time test-token overlay merged into `/tokens` on dev/staging
    /// (empty on prod). Static for the process lifetime.
    pub overlay: Vec<SupportedToken>,
}

impl AppState {
    pub fn new(repo: Repo, package_info: PackageInfo, overlay: Vec<SupportedToken>) -> Self {
        Self {
            repo,
            package_info,
            overlay,
        }
    }
}
