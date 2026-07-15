//! Per-account Twitter credentials, loaded from the secrets TOML.
//!
//! Shape (see config/secrets.example.toml):
//!
//! ```toml
//! [accounts.main]
//! api_key             = "..."   # consumer key
//! api_key_secret      = "..."   # consumer secret
//! access_token        = "..."   # user-context access token
//! access_token_secret = "..."
//! ```
//!
//! In deployed envs the file is rendered by render-secrets.sh from AWS
//! Secrets Manager (options/<env>/twitter-service).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{ensure, Result};
use runtime_config::config_load;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct TwitterAccount {
    /// OAuth 1.0a consumer key ("API Key" in the developer portal).
    pub api_key: String,
    /// OAuth 1.0a consumer secret ("API Key Secret").
    pub api_key_secret: String,
    /// User-context access token for the account.
    pub access_token: String,
    /// User-context access token secret.
    pub access_token_secret: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TwitterSecrets {
    /// Account name → credentials. The name is the handle the API exposes
    /// (`GET /accounts`, `POST /tweets` `account` field).
    pub accounts: BTreeMap<String, TwitterAccount>,
}

impl TwitterSecrets {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let secrets: Self = config_load::load_toml(path)?;
        ensure!(
            !secrets.accounts.is_empty(),
            "no [accounts.<name>] entries in twitter secrets"
        );
        for (name, acct) in &secrets.accounts {
            for (field, value) in [
                ("api_key", &acct.api_key),
                ("api_key_secret", &acct.api_key_secret),
                ("access_token", &acct.access_token),
                ("access_token_secret", &acct.access_token_secret),
            ] {
                ensure!(
                    !value.trim().is_empty() && value != "REPLACE_ME",
                    "twitter account `{name}`: {field} is empty or a placeholder"
                );
            }
        }
        Ok(secrets)
    }
}
