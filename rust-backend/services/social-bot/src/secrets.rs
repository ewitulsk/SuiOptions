//! Bot credentials, loaded from the secrets TOML (rendered by
//! render-secrets.sh from AWS Secrets Manager in deployed envs).

use std::path::Path;

use anyhow::{ensure, Result};
use runtime_config::config_load;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct BotSecrets {
    /// Slack app signing secret (Basic Information → App Credentials).
    /// Verifies `POST /slack/command` request signatures.
    pub slack_signing_secret: String,
    /// Discord application public key (hex). Verifies the Ed25519 signature
    /// on `POST /discord/interactions`.
    pub discord_public_key: String,
}

impl BotSecrets {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let secrets: Self = config_load::load_toml(path)?;
        for (field, value) in [
            ("slack_signing_secret", &secrets.slack_signing_secret),
            ("discord_public_key", &secrets.discord_public_key),
        ] {
            ensure!(
                !value.trim().is_empty() && value != "REPLACE_ME",
                "social-bot secrets: {field} is empty or a placeholder"
            );
        }
        Ok(secrets)
    }
}
