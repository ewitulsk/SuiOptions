//! Bot credentials, loaded from the secrets TOML (rendered by
//! render-secrets.sh from AWS Secrets Manager in deployed envs).

use std::path::Path;

use anyhow::{ensure, Result};
use runtime_config::config_load;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct BotSecrets {
    /// Discord application public key (hex). Verifies the Ed25519 signature
    /// on `POST /discord/interactions`. This is the AIRDROP bot's own
    /// application — not social-bot's.
    pub discord_public_key: String,
}

impl BotSecrets {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let secrets: Self = config_load::load_toml(path)?;
        ensure!(
            !secrets.discord_public_key.trim().is_empty()
                && secrets.discord_public_key != "REPLACE_ME",
            "airdrop-bot secrets: discord_public_key is empty or a placeholder"
        );
        Ok(secrets)
    }
}
