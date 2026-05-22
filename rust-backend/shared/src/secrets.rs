//! Single source of truth for every off-chain secret the workspace uses.
//!
//! All binaries that need to sign anything (Sui transactions, MM quote
//! payloads) read this file at startup. There is **no environment-variable
//! fallback** — if a key is missing from the TOML the binary refuses to
//! start. Operators commit `secrets.example.toml` to source control and
//! keep their real `secrets.toml` out (the workspace `.gitignore` already
//! covers `secrets*.toml`).
//!
//! Expected shape:
//!
//! ```toml
//! [sui]
//! # Per-network keys. Use Sui's `suiprivkey1…` bech32 encoding.
//! testnet = "suiprivkey1..."
//! mainnet = "suiprivkey1..."
//! devnet  = "suiprivkey1..."
//! # Optional shared fallback used when the per-network slot is unset.
//! default = "suiprivkey1..."
//!
//! [mm_bot]
//! # 32-byte secret used to sign Quotes. Interpretation depends on the
//! # MM bot's configured `signing_scheme`: Ed25519 seed, Secp256k1 scalar,
//! # or Secp256r1 scalar.
//! quote_key = "0xabcdef..."
//! ```

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use tracing::{debug, info};

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Secrets {
    #[serde(default)]
    pub sui: SuiSecrets,
    #[serde(default)]
    pub mm_bot: MmBotSecrets,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SuiSecrets {
    pub testnet: Option<String>,
    pub mainnet: Option<String>,
    pub devnet: Option<String>,
    /// Used when the per-network slot is unset.
    pub default: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MmBotSecrets {
    /// 32-byte secret in hex (`0x…` prefix optional). Interpreted per the
    /// MM bot's configured `signing_scheme`.
    pub quote_key: Option<String>,
}

impl Secrets {
    /// Load and parse the secrets file at `path`. Errors if the file is
    /// missing — there's no env fallback by design.
    pub fn load(path: &Path) -> Result<Self> {
        info!(path = %path.display(), "loading secrets file");
        let settings = config::Config::builder()
            .add_source(config::File::from(path).required(true))
            .build()
            .with_context(|| format!("loading secrets file {}", path.display()))?;
        let result = settings
            .try_deserialize::<Self>()
            .with_context(|| format!("parsing secrets file {}", path.display()))?;
        debug!(
            has_testnet = result.sui.testnet.is_some(),
            has_mainnet = result.sui.mainnet.is_some(),
            has_devnet = result.sui.devnet.is_some(),
            has_default = result.sui.default.is_some(),
            has_quote_key = result.mm_bot.quote_key.is_some(),
            "secrets loaded"
        );
        Ok(result)
    }

    /// Sui private key for `network` (case-insensitive: `mainnet` /
    /// `testnet` / `devnet`). Falls back to `sui.default` if the slot is
    /// unset. Returns an error — never reads env.
    pub fn sui_private_key(&self, network: &str) -> Result<&str> {
        debug!(network, "resolving sui private key");
        let per_net = match network.to_ascii_lowercase().as_str() {
            "mainnet" => &self.sui.mainnet,
            "testnet" => &self.sui.testnet,
            "devnet" => &self.sui.devnet,
            other => return Err(anyhow!("unknown network slot: {other}")),
        };
        per_net
            .as_deref()
            .or(self.sui.default.as_deref())
            .ok_or_else(|| {
                anyhow!(
                    "secrets.toml has no sui.{network} key and no sui.default \
                     fallback"
                )
            })
    }

    pub fn mm_quote_key(&self) -> Result<&str> {
        self.mm_bot
            .quote_key
            .as_deref()
            .ok_or_else(|| anyhow!("secrets.toml is missing mm_bot.quote_key"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(name: &str, body: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("secrets-{}-{}.toml", std::process::id(), name));
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn parses_full_shape() {
        let p = write_tmp(
            "full",
            r#"
[sui]
testnet = "suiprivkey1testnet"
mainnet = "suiprivkey1mainnet"
default = "suiprivkey1default"

[mm_bot]
quote_key = "0xdeadbeef"
"#,
        );
        let s = Secrets::load(&p).unwrap();
        assert_eq!(s.sui_private_key("testnet").unwrap(), "suiprivkey1testnet");
        assert_eq!(s.sui_private_key("mainnet").unwrap(), "suiprivkey1mainnet");
        // No devnet entry; falls back to default.
        assert_eq!(s.sui_private_key("devnet").unwrap(), "suiprivkey1default");
        assert_eq!(s.mm_quote_key().unwrap(), "0xdeadbeef");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn missing_slot_with_no_default_errors() {
        let p = write_tmp(
            "no_default",
            r#"
[sui]
testnet = "suiprivkey1testnet"
"#,
        );
        let s = Secrets::load(&p).unwrap();
        assert!(s.sui_private_key("devnet").is_err());
        assert!(s.mm_quote_key().is_err());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn unknown_network_errors() {
        let p = write_tmp("unknown_net", "[sui]\ndefault = \"k\"\n");
        let s = Secrets::load(&p).unwrap();
        assert!(s.sui_private_key("localnet").is_err());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn case_insensitive_network() {
        let p = write_tmp("case", "[sui]\ntestnet = \"k\"\n");
        let s = Secrets::load(&p).unwrap();
        assert_eq!(s.sui_private_key("TESTNET").unwrap(), "k");
        assert_eq!(s.sui_private_key("TestNet").unwrap(), "k");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn missing_file_errors() {
        let p = std::path::PathBuf::from("/this/should/not/exist/secrets.toml");
        assert!(Secrets::load(&p).is_err());
    }
}
