//! Shared application state for both routers.

use solana_deployments::ProgramInfo;
use solana_token_info_client::SupportedToken;

use crate::db::Repo;

pub struct AppState {
    /// Durable, operator-managed catalog persistence.
    pub repo: Repo,
    /// `program_info` for the configured environment, read once from
    /// `solana-deployments.json` at boot and served verbatim from
    /// `/program-info`.
    pub program_info: ProgramInfo,
    /// Read-time test-token overlay merged into `/tokens` on non-mainnet-beta
    /// networks (empty on mainnet-beta). Static for the process lifetime.
    pub overlay: Vec<SupportedToken>,
}

impl AppState {
    pub fn new(repo: Repo, program_info: ProgramInfo, overlay: Vec<SupportedToken>) -> Self {
        Self {
            repo,
            program_info,
            overlay,
        }
    }
}
