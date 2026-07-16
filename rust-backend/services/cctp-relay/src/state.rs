use crate::db::repo::Repo;
use crate::router::CctpConfigDto;

pub struct AppState {
    pub repo: Repo,
    /// Served verbatim by `GET /config`; built from the service config at boot.
    pub cctp_config: CctpConfigDto,
}

impl AppState {
    pub fn new(repo: Repo, cctp_config: CctpConfigDto) -> Self {
        Self { repo, cctp_config }
    }
}
