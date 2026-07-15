use crate::db::repo::Repo;

pub struct AppState {
    pub repo: Repo,
}

impl AppState {
    pub fn new(repo: Repo) -> Self {
        Self { repo }
    }
}
