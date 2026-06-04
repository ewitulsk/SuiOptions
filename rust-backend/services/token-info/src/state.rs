//! Shared application state for both routers.

use deployments::PackageInfo;

use crate::db::Repo;

pub struct AppState {
    /// Catalog persistence.
    pub repo: Repo,
    /// `package_info` for the configured network, read once from
    /// `deployments.json` at boot and served verbatim from `/package-info`.
    /// Static for the process lifetime — it only changes on a redeploy, which
    /// restarts the service.
    pub package_info: PackageInfo,
}

impl AppState {
    pub fn new(repo: Repo, package_info: PackageInfo) -> Self {
        Self {
            repo,
            package_info,
        }
    }
}
